//! The driver against a device that answers.
//!
//! Everything here is bytes and a clock. There is no socket, no runtime and no
//! sleep: the "device" is a function from a request to a reply, so a discovery
//! walk, a poll and a silent inverter are all ordinary assertions.
//!
//! This is what the sans-I/O contract is *for*, and it is what proves
//! `hems_drv::Driver` is a trait somebody can actually implement.

#![cfg(feature = "modbus")]

use hems_core::prelude::{AssetId, Current, Power};
use hems_core::setpoint::Command;
use hems_drv::modbus::frame::{self, RequestBody, ResponseBody};
use hems_drv::modbus::{Cadence, Kind, SunSpec};
use hems_drv::{Driver, DriverError, DriverEvent, LinkState};
use time::macros::datetime;
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = datetime!(2026-05-15 12:00:00 UTC);

/// A SunSpec inverter that answers on base 40 000.
///
/// Publishes model 1 (common), 103 (inverter) and 123 (immediate controls), in
/// the chain layout a real device uses: `SunS`, then `(id, len, body…)` blocks,
/// then the `0xffff` sentinel.
struct Inverter {
    map: std::collections::BTreeMap<u16, u16>,
    curtailed_to: Option<u16>,
}

impl Inverter {
    fn new() -> Self {
        let mut map = std::collections::BTreeMap::new();
        map.insert(40_000, 0x5375);
        map.insert(40_001, 0x6e53);

        // Model 1, common.
        map.insert(40_002, 1);
        map.insert(40_003, 66);
        for i in 0..66u16 {
            map.insert(40_004 + i, 0);
        }

        // Model 103 at 40 070.
        map.insert(40_070, 103);
        map.insert(40_071, 50);
        for i in 0..50u16 {
            map.insert(40_072 + i, 0);
        }
        // Model 103's real layout, which is the point of writing it out: A,
        // AphA, AphB, AphC, A_SF, PPVphAB, PPVphBC, PPVphCA, PhVphA, PhVphB,
        // PhVphC, V_SF, **W**, **W_SF**, **Hz**, **Hz_SF**, …
        map.insert(40_072 + 12, 231); // W
        map.insert(40_072 + 13, 1); // W_SF: ×10¹ = 2 310 W
        map.insert(40_072 + 14, 4998); // Hz
        map.insert(40_072 + 15, (-2i16) as u16); // Hz_SF: ×10⁻² = 49,98 Hz

        // The next header sits at `header + 2 + len`, which is the whole of how
        // a chain is walked: 40 070 + 2 + 50.
        map.insert(40_122, 123);
        map.insert(40_123, 24);
        for i in 0..24u16 {
            map.insert(40_124 + i, 0);
        }

        // …and the sentinel after that one: 40 122 + 2 + 24.
        map.insert(40_148, 0xffff);
        map.insert(40_149, 0);

        Self {
            map,
            curtailed_to: None,
        }
    }

    /// Answer one request, the way a device would.
    fn answer(&mut self, bytes: &[u8]) -> Vec<u8> {
        let transaction = u16::from_be_bytes([bytes[0], bytes[1]]);
        let unit = bytes[6];
        let function = bytes[7];
        let address = u16::from_be_bytes([bytes[8], bytes[9]]);

        let pdu = match function {
            0x03 => {
                let count = u16::from_be_bytes([bytes[10], bytes[11]]);
                // An unmapped address is `0x02`, illegal data address — which is
                // how a real device ends a model walk.
                if self.map.contains_key(&address) {
                    let mut pdu = vec![0x03, u8::try_from(count * 2).unwrap()];
                    for i in 0..count {
                        let v = self.map.get(&(address + i)).copied().unwrap_or(0);
                        pdu.extend_from_slice(&v.to_be_bytes());
                    }
                    pdu
                } else {
                    vec![0x83, 0x02]
                }
            }
            0x10 => {
                let count = u16::from_be_bytes([bytes[10], bytes[11]]);
                self.curtailed_to = Some(u16::from_be_bytes([bytes[13], bytes[14]]));
                let mut pdu = vec![0x10];
                pdu.extend_from_slice(&address.to_be_bytes());
                pdu.extend_from_slice(&count.to_be_bytes());
                pdu
            }
            _ => vec![function | 0x80, 0x01],
        };

        let mut out = Vec::new();
        out.extend_from_slice(&transaction.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&(u16::try_from(pdu.len() + 1).unwrap()).to_be_bytes());
        out.push(unit);
        out.extend_from_slice(&pdu);
        out
    }
}

/// Run the driver against the device until it stops asking, or `limit` rounds.
fn exchange(driver: &mut SunSpec, device: &mut Inverter, now: OffsetDateTime, limit: usize) {
    for _ in 0..limit {
        let Some(out) = driver.poll_transmit() else {
            return;
        };
        let reply = device.answer(&out);
        driver.on_bytes(&reply, now).expect("a well-formed reply");
    }
}

fn driver() -> SunSpec {
    SunSpec::new(
        AssetId::new("wechselrichter").expect("a valid identifier"),
        1,
        Cadence::default(),
    )
}

#[test]
fn a_device_is_discovered_by_walking_its_own_chain() {
    // Where model 103 lives differs between manufacturers and between firmware
    // versions of the same one, so it is read off the device rather than
    // configured.
    let mut d = driver();
    let mut device = Inverter::new();
    assert_eq!(d.kind(), None, "nothing is known before the first byte");

    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);

    assert!(d.models().has(1), "the common model");
    assert!(d.models().has(103), "the inverter model");
    assert!(d.models().has(123), "the curtailment control");
    assert!(!d.models().has(203), "and not a meter it does not have");
    assert_eq!(d.kind(), Some(Kind::Inverter));
}

#[test]
fn a_reading_comes_back_with_its_scale_factor_applied() {
    // The single most common way a SunSpec integration is wrong: `W = 231` with
    // `W_SF = 1` is 2 310 watts, not 231. A factor of ten still looks like a
    // plausible household, which is why it survives.
    let mut d = driver();
    let mut device = Inverter::new();
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);

    let measured = std::iter::from_fn(|| d.poll_event())
        .filter_map(|e| match e {
            DriverEvent::Measured(m) => Some(m),
            _ => None,
        })
        .last()
        .expect("a measurement");

    assert_eq!(
        measured.power,
        Some(Power::new(-2310.0)),
        "2 310 W, produced — negative in the load convention, because an \
         inverter is a source"
    );
    let hz = measured.frequency_hz.expect("a frequency");
    assert!((hz - 49.98).abs() < 0.01, "{hz}");
}

#[test]
fn an_inverter_with_no_model_701_does_not_claim_to_know_what_it_could_produce() {
    // The capability that cannot be worked around. A curtailed inverter asked
    // what it is producing answers with what the manager already commanded, so a
    // controller reading that alone never lifts its own curtailment. This device
    // publishes 103 and not 701, so it says it cannot tell — and the caller
    // falls back to a nameplate *knowing* that it has.
    let mut d = driver();
    let mut device = Inverter::new();
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);

    assert!(!d.models().reports_available_power());
    assert!(
        !d.capabilities().reports_available_power,
        "a driver may not claim a figure its protocol cannot give it"
    );
    assert!(d.capabilities().measures);
}

#[test]
fn a_device_that_stops_answering_makes_the_link_stale() {
    // The guard's whole defence against a silent device is that it stops
    // believing the last measurement. A driver that never said the link had gone
    // would leave a reading looking fresh because nothing contradicted it.
    let mut d = driver();
    let mut device = Inverter::new();
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);
    while d.poll_event().is_some() {}

    let later = T0 + Duration::seconds(2);
    d.on_timeout(later);
    assert!(d.poll_transmit().is_some(), "the next poll went out");

    d.on_timeout(later + Duration::seconds(30));
    let links: Vec<LinkState> = std::iter::from_fn(|| d.poll_event())
        .filter_map(|e| match e {
            DriverEvent::Link(l) => Some(l),
            _ => None,
        })
        .collect();
    assert!(
        links.contains(&LinkState::Stale),
        "silence has to be reported: {links:?}"
    );
}

#[test]
fn a_deadline_is_always_offered() {
    // A driver that returned `None` here would be woken only by bytes, and a
    // silent device sends none — so the timeout that notices the silence would
    // never fire.
    let mut d = driver();
    d.on_timeout(T0);
    assert!(d.poll_deadline().is_some(), "with a request outstanding");

    let mut device = Inverter::new();
    exchange(&mut d, &mut device, T0, 60);
    assert!(d.poll_deadline().is_some(), "and between polls");
}

#[test]
fn a_command_the_protocol_cannot_express_is_refused_rather_than_dropped() {
    // A command nobody sends and nobody reports is how a device quietly stops
    // being managed.
    let mut d = driver();
    let mut device = Inverter::new();
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);

    let err = d
        .command(&Command::ChargingCurrent(Current::new(16.0)), T0)
        .expect_err("an inverter has no charging current");
    assert!(matches!(err, DriverError::Unsupported(_)), "{err:?}");
}

#[test]
fn a_curtailment_needs_a_rating_because_the_model_is_a_percentage() {
    // Model 123 curtails in **per cent of WMax**, not in watts. A driver that
    // did not know the rating would have to invent one, and a percentage
    // computed from a guess is a curtailment wrong by whatever the guess was.
    let mut d = driver();
    let mut device = Inverter::new();
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);

    let err = d
        .command(&Command::ProductionCeiling(Power::from_kw(3.0)), T0)
        .expect_err("no rating was published, so no percentage can be computed");
    assert!(matches!(err, DriverError::Unsupported(_)), "{err:?}");
}

#[test]
fn a_curtailment_reaches_the_device_as_a_percentage_of_its_rating() {
    // The other half of the previous test, and the one that would have caught a
    // driver whose curtailment could never fire: nothing populated the rating,
    // so `with_rating` had no caller and every production ceiling was refused.
    // A control path that always refuses is indistinguishable from one that
    // works until the day a roof has to be turned down.
    let mut d = SunSpec::new(
        AssetId::new("wechselrichter").expect("a valid identifier"),
        1,
        Cadence::default(),
    )
    .with_rating(Power::from_kw(10.0));
    let mut device = Inverter::new();
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);
    while d.poll_event().is_some() {}

    d.command(&Command::ProductionCeiling(Power::from_kw(6.0)), T0)
        .expect("a ceiling on a device with a rating and a model 123");
    let out = d.poll_transmit().expect("a write went out");
    let reply = device.answer(&out);
    d.on_bytes(&reply, T0).expect("the device acknowledged");

    assert_eq!(
        device.curtailed_to,
        Some(60),
        "6 kW of a 10 kW inverter is sixty per cent, because model 123 curtails          in per cent of WMax and not in watts"
    );
    let accepted = std::iter::from_fn(|| d.poll_event())
        .any(|e| matches!(e, DriverEvent::Command(o) if o.accepted));
    assert!(accepted, "and the acknowledgement is reported");
}

#[test]
fn a_garbage_scale_factor_does_not_take_the_box_down() {
    // `10^exponent` with an exponent read off a wire is an infinity waiting to
    // happen, and `Power` rejects a value that is not finite — so one misaligned
    // register would panic the control loop. The input comes from a device
    // nobody in this workspace controls, so it has to be survivable.
    let mut d = driver();
    let mut device = Inverter::new();
    // W_SF far outside SunSpec's own −10..10.
    device.map.insert(40_072 + 13, 4998);
    d.on_timeout(T0);
    exchange(&mut d, &mut device, T0, 60);

    let measured = std::iter::from_fn(|| d.poll_event())
        .filter_map(|e| match e {
            DriverEvent::Measured(m) => Some(m),
            _ => None,
        })
        .last()
        .expect("a measurement, rather than a panic");
    let w = measured.power.expect("a power").get();
    assert!(w.is_finite(), "{w}");
}

#[test]
fn a_frame_split_across_reads_is_acted_on_only_once_it_is_whole() {
    // A TCP read is not a message boundary.
    let mut d = driver();
    let mut device = Inverter::new();
    d.on_timeout(T0);

    let first = d.poll_transmit().expect("a probe");
    let reply = device.answer(&first);
    for byte in &reply[..reply.len() - 1] {
        d.on_bytes(&[*byte], T0).expect("well formed");
        assert!(
            d.poll_transmit().is_none(),
            "a partial frame must not move the walk on"
        );
    }
    d.on_bytes(&[reply[reply.len() - 1]], T0)
        .expect("well formed");
    assert!(
        d.poll_transmit().is_some(),
        "and the whole one must, at once"
    );
}

#[test]
fn rubbish_on_the_wire_is_an_error_and_not_a_measurement() {
    let mut d = driver();
    let err = d
        .on_bytes(&[0, 1, 0, 7, 0, 3, 1, 0x03, 0x00], T0)
        .expect_err("protocol identifier 7 is not Modbus");
    assert!(matches!(err, DriverError::Malformed(_)), "{err:?}");
}

#[test]
fn the_frame_codec_and_the_device_agree() {
    // A guard against this test's own device being wrong: what the encoder
    // produces is what the decoder reads.
    let request = frame::Request {
        transaction: 42,
        unit: 1,
        body: RequestBody::Read {
            address: 40_000,
            count: 2,
        },
    };
    let mut device = Inverter::new();
    let reply = device.answer(&request.encode());
    let (response, used) = frame::decode(&reply).unwrap().expect("a whole frame");
    assert_eq!(used, reply.len());
    assert_eq!(response.transaction, 42);
    assert_eq!(
        response.body,
        ResponseBody::Registers(vec![0x5375, 0x6e53]),
        "the device says SunS"
    );
}
