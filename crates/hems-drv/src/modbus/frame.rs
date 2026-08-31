//! Modbus TCP framing, as bytes and nothing else.
//!
//! The wire format is deliberately small: a seven-byte MBAP header and a
//! protocol data unit behind it.
//!
//! ```text
//! ┌──────────────┬──────────────┬────────┬──────┬─────────────┐
//! │ transaction  │ protocol (0) │ length │ unit │ PDU         │
//! │ 2 bytes      │ 2 bytes      │ 2      │ 1    │ length − 1  │
//! └──────────────┴──────────────┴────────┴──────┴─────────────┘
//! ```
//!
//! Everything here is a pure function of a byte slice. There is no socket, no
//! retry and no timeout: those are decisions, and decisions live one layer up
//! where they can be tested against a clock that does not tick by itself.

use core::fmt;

/// The function codes this driver uses.
///
/// SunSpec lives in holding registers, so `0x03` reads and `0x10` writes. A
/// device that only implements input registers (`0x04`) is out of scope and
/// says so rather than being guessed at: reading the wrong space returns
/// plausible numbers from somewhere else in the map, which is worse than an
/// error.
pub mod function {
    /// Read holding registers.
    pub const READ_HOLDING: u8 = 0x03;
    /// Write multiple holding registers.
    pub const WRITE_MULTIPLE: u8 = 0x10;
}

/// The most registers a single read may ask for.
///
/// The protocol's own ceiling: a response carries its byte count in one byte,
/// so 125 registers is 250 bytes and the largest that can be described. A
/// SunSpec model longer than that is read in several passes.
pub const MAX_REGISTERS_PER_READ: u16 = 125;

/// Why a frame could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The header says this is not Modbus TCP.
    #[error("protocol identifier {0} is not Modbus TCP")]
    NotModbus(u16),
    /// The length field is impossible.
    #[error("declared length {0} is not a frame")]
    BadLength(u16),
    /// The device answered with an exception.
    ///
    /// Carried through rather than flattened into a string: a `0x02` (illegal
    /// data address) while walking the SunSpec model chain is the ordinary way
    /// a device says "that is the end of the list", and a driver that could not
    /// tell it from a `0x04` (device failure) would either give up early or
    /// hammer a broken inverter for ever.
    #[error("the device answered function {function:#04x} with exception {code:#04x}")]
    Exception {
        /// The function that failed, without the exception bit.
        function: u8,
        /// The exception code.
        code: u8,
    },
    /// The response is not the shape its function code implies.
    #[error("malformed {0}")]
    Malformed(&'static str),
}

/// One decoded response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The transaction identifier the request carried.
    pub transaction: u16,
    /// What came back.
    pub body: ResponseBody,
}

/// The part of a response that differs by function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseBody {
    /// Register values, in the order they were asked for.
    Registers(Vec<u16>),
    /// A write was accepted.
    WriteAccepted {
        /// The first register written.
        address: u16,
        /// How many.
        count: u16,
    },
    /// The device refused.
    Exception {
        /// The function that failed.
        function: u8,
        /// Why.
        code: u8,
    },
}

/// A request to put on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Matches the response to the request that caused it.
    pub transaction: u16,
    /// Which device behind the gateway.
    pub unit: u8,
    /// What to ask.
    pub body: RequestBody,
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    /// Read `count` holding registers from `address`.
    Read {
        /// The first register.
        address: u16,
        /// How many, at most [`MAX_REGISTERS_PER_READ`].
        count: u16,
    },
    /// Write `values` starting at `address`.
    Write {
        /// The first register.
        address: u16,
        /// What to put there.
        values: Vec<u16>,
    },
}

impl Request {
    /// The bytes for this request.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut pdu = Vec::with_capacity(16);
        match &self.body {
            RequestBody::Read { address, count } => {
                pdu.push(function::READ_HOLDING);
                pdu.extend_from_slice(&address.to_be_bytes());
                pdu.extend_from_slice(&count.min(&MAX_REGISTERS_PER_READ).to_be_bytes());
            }
            RequestBody::Write { address, values } => {
                pdu.push(function::WRITE_MULTIPLE);
                pdu.extend_from_slice(&address.to_be_bytes());
                let count = u16::try_from(values.len()).unwrap_or(u16::MAX);
                pdu.extend_from_slice(&count.to_be_bytes());
                // The byte count is one byte, so a write is bounded the same way
                // a read is. Truncating here rather than at the call site would
                // silently write half a setpoint.
                pdu.push(u8::try_from(values.len() * 2).unwrap_or(u8::MAX));
                for v in values {
                    pdu.extend_from_slice(&v.to_be_bytes());
                }
            }
        }

        let mut out = Vec::with_capacity(7 + pdu.len());
        out.extend_from_slice(&self.transaction.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes()); // protocol: Modbus
        // Length counts the unit identifier and the PDU.
        let length = u16::try_from(pdu.len() + 1).unwrap_or(u16::MAX);
        out.extend_from_slice(&length.to_be_bytes());
        out.push(self.unit);
        out.extend_from_slice(&pdu);
        out
    }
}

/// Take one response off the front of `buffer`, if a whole one is there.
///
/// Returns how many bytes were consumed alongside the response, so the caller
/// can keep the remainder: a TCP read is not a message boundary, and a driver
/// that assumed one would work on a desk and fail on a busy gateway that
/// coalesces two answers into a segment.
///
/// `Ok(None)` means "not yet, ask again when more has arrived" — which is not an
/// error and must not be logged as one.
///
/// # Errors
/// [`FrameError`] when the bytes present are a frame and it is a broken one.
pub fn decode(buffer: &[u8]) -> Result<Option<(Response, usize)>, FrameError> {
    const HEADER: usize = 7;
    if buffer.len() < HEADER {
        return Ok(None);
    }
    let transaction = u16::from_be_bytes([buffer[0], buffer[1]]);
    let protocol = u16::from_be_bytes([buffer[2], buffer[3]]);
    if protocol != 0 {
        return Err(FrameError::NotModbus(protocol));
    }
    let length = u16::from_be_bytes([buffer[4], buffer[5]]);
    if length < 2 {
        return Err(FrameError::BadLength(length));
    }
    // `length` counts the unit byte and the PDU, and the unit byte is inside the
    // seven-byte header, so the whole frame is `6 + length`.
    let total = 6 + usize::from(length);
    if buffer.len() < total {
        return Ok(None);
    }
    let pdu = &buffer[HEADER..total];
    let body = decode_pdu(pdu)?;
    Ok(Some((Response { transaction, body }, total)))
}

/// The protocol data unit, once a whole one is in hand.
fn decode_pdu(pdu: &[u8]) -> Result<ResponseBody, FrameError> {
    let Some((&code, rest)) = pdu.split_first() else {
        return Err(FrameError::Malformed("an empty protocol data unit"));
    };

    // The exception bit. A refusal is a well-formed answer, not a broken frame:
    // the model walk *ends* on one.
    if code & 0x80 != 0 {
        let Some(&exception) = rest.first() else {
            return Err(FrameError::Malformed("an exception with no code"));
        };
        return Ok(ResponseBody::Exception {
            function: code & 0x7f,
            code: exception,
        });
    }

    match code {
        function::READ_HOLDING => {
            let Some((&byte_count, values)) = rest.split_first() else {
                return Err(FrameError::Malformed("a read with no byte count"));
            };
            if usize::from(byte_count) != values.len() || byte_count % 2 != 0 {
                return Err(FrameError::Malformed("a read whose byte count disagrees"));
            }
            Ok(ResponseBody::Registers(
                values
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect(),
            ))
        }
        function::WRITE_MULTIPLE => {
            if rest.len() < 4 {
                return Err(FrameError::Malformed("a short write acknowledgement"));
            }
            Ok(ResponseBody::WriteAccepted {
                address: u16::from_be_bytes([rest[0], rest[1]]),
                count: u16::from_be_bytes([rest[2], rest[3]]),
            })
        }
        other => Err(FrameError::Malformed(match other {
            0x04 => "an input-register response, which SunSpec does not live in",
            _ => "an unexpected function code",
        })),
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.body {
            RequestBody::Read { address, count } => {
                write!(f, "read {count} registers from {address}")
            }
            RequestBody::Write { address, values } => {
                write!(f, "write {} registers at {address}", values.len())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_round_trips_through_the_wire() {
        let request = Request {
            transaction: 7,
            unit: 1,
            body: RequestBody::Read {
                address: 40_000,
                count: 2,
            },
        };
        let bytes = request.encode();
        assert_eq!(
            bytes,
            vec![0, 7, 0, 0, 0, 6, 1, 0x03, 0x9c, 0x40, 0x00, 0x02]
        );

        // The answer the device would give.
        let reply = [0, 7, 0, 0, 0, 7, 1, 0x03, 4, 0x53, 0x75, 0x6e, 0x53];
        let (response, used) = decode(&reply).unwrap().expect("a whole frame");
        assert_eq!(used, reply.len());
        assert_eq!(response.transaction, 7);
        assert_eq!(
            response.body,
            ResponseBody::Registers(vec![0x5375, 0x6e53]),
            "\"SunS\", which is how a SunSpec device says hello"
        );
    }

    #[test]
    fn a_partial_frame_is_not_an_error() {
        // A TCP read is not a message boundary. Treating a short buffer as a
        // fault is how a driver works on a desk and fails on a busy gateway.
        let whole = [0, 7, 0, 0, 0, 7, 1, 0x03, 4, 0x53, 0x75, 0x6e, 0x53];
        for cut in 0..whole.len() {
            assert_eq!(
                decode(&whole[..cut]),
                Ok(None),
                "{cut} bytes is not yet a frame, and not yet an error either"
            );
        }
        assert!(decode(&whole).unwrap().is_some());
    }

    #[test]
    fn two_answers_in_one_segment_are_both_read() {
        // The other half of the same fact: a gateway may coalesce.
        let one = [0, 1, 0, 0, 0, 5, 1, 0x03, 2, 0x00, 0x2a];
        let two = [0, 2, 0, 0, 0, 5, 1, 0x03, 2, 0x00, 0x2b];
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&one);
        buffer.extend_from_slice(&two);

        let (first, used) = decode(&buffer).unwrap().expect("the first");
        assert_eq!(first.transaction, 1);
        let (second, _) = decode(&buffer[used..]).unwrap().expect("the second");
        assert_eq!(second.transaction, 2);
        assert_eq!(second.body, ResponseBody::Registers(vec![0x2b]));
    }

    #[test]
    fn an_exception_is_an_answer_rather_than_a_broken_frame() {
        // `0x02`, illegal data address, is the ordinary way a device says "that
        // is the end of the model list". A driver that treated it as a fault
        // would either stop walking early or hammer a broken inverter for ever,
        // depending on which way it guessed.
        let reply = [0, 9, 0, 0, 0, 3, 1, 0x83, 0x02];
        let (response, _) = decode(&reply).unwrap().expect("a whole frame");
        assert_eq!(
            response.body,
            ResponseBody::Exception {
                function: 0x03,
                code: 0x02
            }
        );
    }

    #[test]
    fn a_byte_count_that_disagrees_with_the_payload_is_refused() {
        // The one malformation worth catching by hand: a device that says four
        // bytes and sends two would otherwise decode as a shorter register block
        // and land plausible-looking rubbish in a measurement.
        let reply = [0, 1, 0, 0, 0, 5, 1, 0x03, 4, 0x00, 0x2a];
        assert!(matches!(decode(&reply), Err(FrameError::Malformed(_))));
    }

    #[test]
    fn a_write_is_encoded_with_its_byte_count_and_acknowledged() {
        let request = Request {
            transaction: 3,
            unit: 2,
            body: RequestBody::Write {
                address: 40_100,
                values: vec![0x0064, 0x0001],
            },
        };
        assert_eq!(
            request.encode(),
            vec![
                0, 3, 0, 0, 0, 11, 2, 0x10, 0x9c, 0xa4, 0x00, 0x02, 4, 0x00, 0x64, 0x00, 0x01
            ]
        );

        let reply = [0, 3, 0, 0, 0, 6, 2, 0x10, 0x9c, 0xa4, 0x00, 0x02];
        let (response, _) = decode(&reply).unwrap().expect("a whole frame");
        assert_eq!(
            response.body,
            ResponseBody::WriteAccepted {
                address: 40_100,
                count: 2
            }
        );
    }

    #[test]
    fn something_that_is_not_modbus_says_so() {
        let reply = [0, 1, 0, 7, 0, 3, 1, 0x03, 0x00];
        assert_eq!(decode(&reply), Err(FrameError::NotModbus(7)));
    }
}
