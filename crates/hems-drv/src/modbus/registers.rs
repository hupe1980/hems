//! A vendor's own register map, declared rather than discovered.
//!
//! [`SunSpec`](super::SunSpec) works because SunSpec is a *standard*: a device
//! publishes a model list and the driver walks it, so nothing has to be typed
//! into a file and nothing can be typed wrongly. Most of the German market is
//! not that. A Stiebel, Vaillant, Viessmann or Bosch heat pump answers Modbus
//! all day and publishes no model list at all — the register numbers are in a
//! PDF, and every unit's are different.
//!
//! This is the driver for that case, and it exists because of one measurement.
//! The planner models the building, learns it from the household's own record
//! and can start the compressor over EEBUS OHPCF — and all of it is gated on an
//! **indoor temperature**, which no EEBUS use case carries and which a heat pump
//! has been measuring in a holding register the whole time.
//!
//! # What is declared, and why each part has to be
//!
//! A point is a register number, a width, a **space**, a scale and which field
//! of a [`Measurement`] it becomes. None of the five is guessable:
//!
//! * the **space** because holding and input registers are separately addressed,
//!   and most vendors put sensors in the input space that SunSpec never touches;
//! * the **width and word order** because a 32-bit value spans two registers and
//!   vendors disagree about which comes first — the same bytes read the other
//!   way round are a plausible number several orders of magnitude out;
//! * the **scale** because a temperature is published in tenths of a kelvin as
//!   often as in kelvin, and a sign flip is how a vendor that reports generation
//!   as positive is turned into this workspace's load convention.
//!
//! # What it does not do
//!
//! It does not command. A register map that could write is a register map that
//! can turn a compressor on from a number in a configuration file, and the
//! consequence of a typo there is not a bad reading — it is a heat pump doing
//! something nobody asked for. Commanding a heat pump is
//! [`eebus_heat_pump`](crate::eebus_heat_pump)'s, over a protocol that says what a value
//! means; the registry lets both drivers speak for one asset precisely so that
//! this one never has to.

use hems_core::prelude::{AssetId, Measurement, Power, Soc};
use hems_core::setpoint::Command;
use time::OffsetDateTime;

use super::Cadence;
use super::frame::{self, Request, RequestBody, Response, ResponseBody, Space};
use crate::{Driver, DriverCapabilities, DriverError, DriverEvent, LinkState};

/// How wide a value is, and in which order its registers arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Word {
    /// One register, unsigned.
    #[default]
    U16,
    /// One register, two's complement.
    S16,
    /// Two registers, unsigned, most significant first.
    U32,
    /// Two registers, two's complement, most significant first.
    S32,
    /// Two registers, unsigned, **least** significant first.
    ///
    /// The word-swapped order a good many gateways and inverters use. It is a
    /// separate variant rather than a flag because it is the single most common
    /// way a register map is read wrongly and still looks plausible.
    U32Swapped,
    /// The same, two's complement.
    S32Swapped,
}

impl Word {
    /// How many registers it occupies.
    #[must_use]
    pub const fn registers(self) -> u16 {
        match self {
            Self::U16 | Self::S16 => 1,
            _ => 2,
        }
    }

    /// The raw value in `words`, or `None` where there are not enough of them.
    #[must_use]
    pub fn read(self, words: &[u16]) -> Option<f64> {
        let (&first, rest) = words.split_first()?;
        // `cast_signed` rather than `as`: the wrap is the point — a register
        // holding `0xFFFF` *is* −1 where the vendor declared it signed — and
        // saying so explicitly is what tells it apart from an `as` somebody
        // wrote without thinking about the top bit.
        let wide = |high: u16, low: u16| (u32::from(high) << 16) | u32::from(low);
        Some(match self {
            Self::U16 => f64::from(first),
            Self::S16 => f64::from(first.cast_signed()),
            Self::U32 => f64::from(wide(first, *rest.first()?)),
            Self::S32 => f64::from(wide(first, *rest.first()?).cast_signed()),
            Self::U32Swapped => f64::from(wide(*rest.first()?, first)),
            Self::S32Swapped => f64::from(wide(*rest.first()?, first).cast_signed()),
        })
    }
}

/// Which part of a [`Measurement`] a point becomes.
///
/// Deliberately a short list. Every entry is a field some driver in this
/// workspace already fills from a standard, so a register map can only ever say
/// a thing the rest of the box already knows how to use — rather than becoming a
/// second, parallel device model that only this driver understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Field {
    /// Active power in watts, **load convention** — positive into the asset.
    ///
    /// A vendor that publishes generation as positive is corrected with a
    /// negative scale, which is the whole of what the scale is for.
    Power,
    /// A temperature in degrees Celsius.
    ///
    /// For a heat pump this is the **indoor** temperature: the one state the
    /// planner's thermal model is integrated from, and the reason this driver
    /// exists.
    TemperatureC,
    /// A state of charge, as a fraction — so a register in per cent has a scale
    /// of `0.01`.
    Soc,
}

/// One value in a vendor's map.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Point {
    /// Which register space it lives in.
    #[cfg_attr(feature = "serde", serde(default))]
    pub space: Space,
    /// The first register.
    pub register: u16,
    /// How wide it is, and in which word order.
    #[cfg_attr(feature = "serde", serde(default))]
    pub word: Word,
    /// What to multiply the raw value by to reach the field's own unit.
    #[cfg_attr(feature = "serde", serde(default = "one"))]
    pub scale: f64,
    /// Which field of a measurement it becomes.
    pub field: Field,
}

fn one() -> f64 {
    1.0
}

/// A device read through a declared register map.
#[derive(Debug)]
pub struct Registers {
    asset: AssetId,
    unit: u8,
    cadence: Cadence,
    points: Vec<Point>,
    link: LinkState,
    inbox: Vec<u8>,
    outbox: Vec<Request>,
    /// Which point each outstanding transaction is for.
    pending: Option<(u16, usize, OffsetDateTime)>,
    /// The next point to read this round.
    next: usize,
    next_transaction: u16,
    /// The measurement being assembled from this round's points.
    partial: Option<Measurement>,
    events: Vec<DriverEvent>,
    due: Option<OffsetDateTime>,
}

impl Registers {
    /// A device on `unit` behind a Modbus TCP gateway, read as `points`.
    ///
    /// # Errors
    /// [`DriverError::Unsupported`] where `points` is empty — a driver with no
    /// points would connect, poll nothing and report a device that is perfectly
    /// reachable and says nothing, which is the one failure this workspace keeps
    /// finding in itself.
    pub fn new(
        asset: AssetId,
        unit: u8,
        cadence: Cadence,
        points: Vec<Point>,
    ) -> Result<Self, DriverError> {
        if points.is_empty() {
            return Err(DriverError::Unsupported(format!(
                "the register map for `{asset}` names no points, so this driver \
                 would poll nothing and report a device that says nothing"
            )));
        }
        Ok(Self {
            asset,
            unit,
            cadence,
            points,
            link: LinkState::Down,
            inbox: Vec::new(),
            outbox: Vec::new(),
            pending: None,
            next: 0,
            next_transaction: 1,
            partial: None,
            events: Vec::new(),
            due: None,
        })
    }

    fn transaction(&mut self) -> u16 {
        let t = self.next_transaction;
        self.next_transaction = self.next_transaction.wrapping_add(1).max(1);
        t
    }

    /// Ask for the point at `index`.
    ///
    /// One point per request rather than one read spanning several. Coalescing
    /// is the obvious optimisation and it is wrong here: a vendor map is sparse
    /// and its gaps are not always readable — a device answers a read that
    /// crosses an unimplemented register with an exception, and the reply
    /// carries no way to say which register was the problem, so one bad number
    /// in the file would silently cost every point that shares its block.
    fn ask(&mut self, index: usize, now: OffsetDateTime) {
        let Some(point) = self.points.get(index).copied() else {
            return;
        };
        let transaction = self.transaction();
        self.outbox.push(Request {
            transaction,
            unit: self.unit,
            body: RequestBody::Read {
                space: point.space,
                address: point.register,
                count: point.word.registers(),
            },
        });
        self.pending = Some((transaction, index, now));
    }

    /// Start a fresh round.
    fn poll(&mut self, now: OffsetDateTime) {
        self.next = 0;
        self.partial = Some(Measurement::at(now));
        self.ask(0, now);
    }

    /// Fold one answer in and ask for the next point, or publish the round.
    fn absorb(&mut self, response: &Response, now: OffsetDateTime) {
        let Some((transaction, index, _)) = self.pending else {
            return;
        };
        if response.transaction != transaction {
            return;
        }
        self.pending = None;
        if let ResponseBody::Registers(words) = &response.body {
            // A point the device refuses is left absent rather than defaulted:
            // every field is an `Option` precisely so a driver can say "this one
            // did not arrive" instead of saying nought.
            if let (Some(point), Some(into)) = (self.points.get(index), self.partial.as_mut())
                && let Some(raw) = point.word.read(words)
            {
                let value = raw * point.scale;
                match point.field {
                    Field::Power => into.power = Some(Power::new(value)),
                    Field::TemperatureC => into.temperature_c = Some(value),
                    Field::Soc => into.soc = Soc::new(value).ok(),
                }
            }
        }
        let next = index + 1;
        if next < self.points.len() {
            self.next = next;
            self.ask(next, now);
        } else if let Some(measurement) = self.partial.take() {
            self.events.push(DriverEvent::Measured(measurement));
            self.due = Some(now + self.cadence.poll);
        }
    }
}

impl Driver for Registers {
    fn asset(&self) -> &AssetId {
        &self.asset
    }

    fn capabilities(&self) -> DriverCapabilities {
        // It reads and never writes. See the module note: a register map that
        // could command is one where a typo starts a compressor.
        DriverCapabilities::meter()
    }

    fn on_bytes(&mut self, bytes: &[u8], now: OffsetDateTime) -> Result<(), DriverError> {
        self.inbox.extend_from_slice(bytes);
        loop {
            match frame::decode(&self.inbox) {
                Ok(Some((response, consumed))) => {
                    self.inbox.drain(..consumed);
                    self.absorb(&response, now);
                }
                Ok(None) => return Ok(()),
                Err(error) => {
                    // The stream is no longer trustworthy: a frame boundary that
                    // has been lost cannot be recovered by reading further, and
                    // every byte after it would be decoded against the wrong
                    // offset.
                    self.inbox.clear();
                    self.pending = None;
                    self.partial = None;
                    return Err(DriverError::Malformed(error.to_string()));
                }
            }
        }
    }

    fn on_link(&mut self, state: LinkState, now: OffsetDateTime) {
        self.inbox.clear();
        self.outbox.clear();
        self.pending = None;
        self.partial = None;
        if self.link != state {
            self.link = state;
            self.events.push(DriverEvent::Link(state));
        }
        if state == LinkState::Up {
            self.poll(now);
        } else {
            self.due = None;
        }
    }

    fn on_timeout(&mut self, now: OffsetDateTime) {
        // A request that was never answered. The round is abandoned rather than
        // published half-filled: a measurement carrying the two points that did
        // arrive and not the third reads downstream as a device that has stopped
        // reporting the third, which is a different fault from a slow one.
        if let Some((_, _, sent)) = self.pending
            && now - sent >= self.cadence.timeout
        {
            self.pending = None;
            self.partial = None;
            if self.link == LinkState::Up {
                self.link = LinkState::Stale;
                self.events.push(DriverEvent::Link(LinkState::Stale));
            }
            self.due = Some(now + self.cadence.poll);
        }
        if self.pending.is_none() && self.due.is_some_and(|due| now >= due) && self.link.is_usable()
        {
            self.poll(now);
        }
    }

    fn command(&mut self, command: &Command, _: OffsetDateTime) -> Result<(), DriverError> {
        Err(DriverError::Unsupported(format!(
            "`{}` is read through a register map, which never writes: {command:?}",
            self.asset
        )))
    }

    fn poll_event(&mut self) -> Option<DriverEvent> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }

    fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        if self.outbox.is_empty() {
            None
        } else {
            Some(self.outbox.remove(0).encode())
        }
    }

    fn poll_deadline(&self) -> Option<OffsetDateTime> {
        match self.pending {
            Some((_, _, sent)) => Some(sent + self.cadence.timeout),
            None => self.due,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const START: OffsetDateTime = datetime!(2026-01-15 08:00:00 UTC);

    fn at(seconds: i64) -> OffsetDateTime {
        START + time::Duration::seconds(seconds)
    }

    fn asset() -> AssetId {
        AssetId::new("waermepumpe").expect("a valid identifier")
    }

    /// A Stiebel-ish map: the room temperature in tenths of a kelvin, and the
    /// compressor's draw as a 32-bit watt figure.
    fn points() -> Vec<Point> {
        vec![
            Point {
                space: Space::Input,
                register: 507,
                word: Word::S16,
                scale: 0.1,
                field: Field::TemperatureC,
            },
            Point {
                space: Space::Holding,
                register: 2_240,
                word: Word::U32,
                scale: 1.0,
                field: Field::Power,
            },
        ]
    }

    fn driver() -> Registers {
        Registers::new(asset(), 1, Cadence::default(), points()).expect("two points is a map")
    }

    /// The answer a device gives to whatever was last asked.
    fn answer(driver: &mut Registers, words: &[u16]) -> Vec<u8> {
        let request = driver.poll_transmit().expect("a request went out");
        // The transaction identifier is the first two bytes of the request, and
        // echoing it is what makes the answer *this* answer.
        let transaction = u16::from_be_bytes([request[0], request[1]]);
        let function = request[7];
        let mut pdu = vec![
            function,
            u8::try_from(words.len() * 2).expect("a short read"),
        ];
        for w in words {
            pdu.extend_from_slice(&w.to_be_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(&transaction.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(pdu.len() + 1).expect("short").to_be_bytes());
        out.push(1);
        out.extend_from_slice(&pdu);
        out
    }

    fn measurements(driver: &mut Registers) -> Vec<Measurement> {
        let mut out = Vec::new();
        while let Some(event) = driver.poll_event() {
            if let DriverEvent::Measured(m) = event {
                out.push(m);
            }
        }
        out
    }

    #[test]
    fn a_declared_map_becomes_the_measurement_the_rest_of_the_box_already_reads() {
        // The whole point, and the one measurement the planner's thermal model
        // is gated on: a heat pump has been reporting the room temperature in a
        // register the entire time, and nothing in this workspace could read it.
        let mut d = driver();
        d.on_link(LinkState::Up, at(0));
        let reply = answer(&mut d, &[213]);
        d.on_bytes(&reply, at(0)).expect("a well-formed frame");
        assert!(
            measurements(&mut d).is_empty(),
            "one point of two is not a round"
        );
        let reply = answer(&mut d, &[0, 1_800]);
        d.on_bytes(&reply, at(0)).expect("a well-formed frame");

        let m = measurements(&mut d).pop().expect("the round completed");
        assert_eq!(m.temperature_c, Some(21.3), "tenths of a kelvin, scaled");
        assert_eq!(m.power, Some(Power::from_kw(1.8)));
    }

    #[test]
    fn each_point_is_read_from_the_space_it_was_declared_in() {
        // Holding and input registers are separately addressed, so a device that
        // implements both keeps different values at the same number in each.
        // Reading the wrong space returns a plausible number from somewhere else
        // in the map, which is worse than an error.
        let mut d = driver();
        d.on_link(LinkState::Up, at(0));
        let first = d.poll_transmit().expect("a request");
        assert_eq!(first[7], frame::function::READ_INPUT, "the temperature");
        assert_eq!(u16::from_be_bytes([first[8], first[9]]), 507);

        let reply = answer_to(&first, &[213]);
        d.on_bytes(&reply, at(0)).expect("a frame");
        let second = d.poll_transmit().expect("the next request");
        assert_eq!(second[7], frame::function::READ_HOLDING, "the power");
        assert_eq!(u16::from_be_bytes([second[8], second[9]]), 2_240);
    }

    /// The same as `answer`, for a request already taken off the driver.
    fn answer_to(request: &[u8], words: &[u16]) -> Vec<u8> {
        let transaction = u16::from_be_bytes([request[0], request[1]]);
        let mut pdu = vec![request[7], u8::try_from(words.len() * 2).expect("short")];
        for w in words {
            pdu.extend_from_slice(&w.to_be_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(&transaction.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&u16::try_from(pdu.len() + 1).expect("short").to_be_bytes());
        out.push(1);
        out.extend_from_slice(&pdu);
        out
    }

    #[test]
    fn the_word_order_of_a_thirty_two_bit_value_is_declared_and_not_assumed() {
        // The single most common way a register map is read wrongly and still
        // looks plausible. 1,8 kW read the other way round is 117 964 800 W —
        // which a guard would treat as a household drawing a hundred megawatts,
        // and which no bounds check would call impossible in the way a negative
        // temperature is impossible.
        assert_eq!(Word::U32.read(&[0, 1_800]), Some(1_800.0));
        assert_eq!(Word::U32Swapped.read(&[1_800, 0]), Some(1_800.0));
        assert_eq!(Word::U32.read(&[1_800, 0]), Some(117_964_800.0));

        // And a negative one, which is how a vendor reports export.
        assert_eq!(Word::S16.read(&[0xFFFF]), Some(-1.0));
        assert_eq!(Word::S32.read(&[0xFFFF, 0xF8F8]), Some(-1_800.0));
        assert_eq!(Word::S32Swapped.read(&[0xF8F8, 0xFFFF]), Some(-1_800.0));
    }

    #[test]
    fn a_value_whose_registers_did_not_all_arrive_is_absent_rather_than_half_read() {
        // A short answer to a 32-bit read. Taking the one register that came is
        // a number three orders of magnitude out; every field is an `Option`
        // precisely so a driver can say "this did not arrive".
        assert_eq!(Word::U32.read(&[7]), None);
        assert_eq!(Word::U32.read(&[]), None);
    }

    #[test]
    fn a_map_with_no_points_is_refused_at_construction() {
        // It would connect, poll nothing, and report a device that is perfectly
        // reachable and says nothing — which is the shape of defect this
        // workspace keeps finding in itself.
        let err = Registers::new(asset(), 1, Cadence::default(), Vec::new())
            .expect_err("a map with no points is not a map");
        assert!(matches!(err, DriverError::Unsupported(_)), "{err}");
    }

    #[test]
    fn a_register_map_refuses_every_command() {
        // A map that could write is one where a typo in a configuration file
        // starts a compressor. Commanding a heat pump belongs to a protocol that
        // says what a value *means*.
        let mut d = driver();
        let refused = d.command(&Command::OnOff(true), at(0));
        assert!(refused.is_err());
        assert!(!d.capabilities().accepts_commands);
    }

    #[test]
    fn a_round_that_never_finished_is_abandoned_rather_than_published_half_filled() {
        // A measurement carrying the point that arrived and not the one that did
        // not reads downstream as a device that has stopped reporting the
        // second, which is a different fault from a slow one — and the guard's
        // response to the two is different.
        let mut d = driver();
        d.on_link(LinkState::Up, at(0));
        let reply = answer(&mut d, &[213]);
        d.on_bytes(&reply, at(0)).expect("a frame");
        let _ = d.poll_transmit();

        d.on_timeout(at(30));
        assert!(
            measurements(&mut d).is_empty(),
            "half a round is not a measurement"
        );
        assert_eq!(
            d.link,
            LinkState::Stale,
            "and a device that stopped answering mid-round is one the guard has \
             to stop believing"
        );
    }
}
