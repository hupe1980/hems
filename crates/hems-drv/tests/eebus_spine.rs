//! A network operator's Steuerbox limits the household, over SPINE datagrams.
//!
//! Everything in `hems-drv`'s own unit tests reaches the state machine directly
//! — `on_limit`, `on_heartbeat` — which proves the § 14a *rules* and proves
//! nothing about the wire. This one goes the other way round: an Energy Guard
//! built out of `eebus`'s own actor discovers the box, binds to its
//! `LoadControl` feature and writes 4,2 kW, and every message between the two
//! travels as the bytes a SHIP data frame carries.
//!
//! That seam is exactly the one `hemsd` owns. What it adds under this test is
//! TCP, TLS with mutual authentication, the WebSocket upgrade and the SHIP
//! handshake; what it adds *above* it is nothing at all, which is the point —
//! if a limit arrives here it arrives on a real box, and if it does not, no
//! amount of socket code will help.

#![cfg(feature = "eebus")]

use core::time::Duration as StdDuration;

use eebus::model::{DeviceType, EntityType};
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::usecases::limitation::{self, EnergyGuardActor, GuardEvent, LimitWrite};
use eebus::usecases::lpc;
use hems_core::prelude::{AssetId, Power};
use hems_drv::eebus::{Lpc, Use};
use hems_drv::{Driver, DriverEvent, LimitSource, LinkState};
use time::OffsetDateTime;
use time::macros::datetime;

const START: OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

/// The wall clock, `seconds` after the box started.
fn at(seconds: i64) -> OffsetDateTime {
    START + time::Duration::seconds(seconds)
}

/// `eebus`'s monotonic clock at the same instant.
fn elapsed(seconds: i64) -> StdDuration {
    StdDuration::from_secs(seconds.unsigned_abs())
}

/// The network operator's box: an Energy Guard on a `GridGuard` entity.
fn steuerbox() -> (Engine, EnergyGuardActor) {
    let mut device = LocalDevice::new("n:dso", "Steuerbox-1", DeviceType::ElectricitySupplySystem)
        .expect("a valid device address");
    device
        .add_entity(
            LocalEntity::new([1], EntityType::GridGuard)
                .with_feature(limitation::client_feature(1))
                .with_feature(limitation::device_diagnosis_feature(2)),
        )
        .expect("a fresh entity");
    let client = device.address_of(&[1], 1);
    let diagnosis = device.address_of(&[1], 2);
    let mut engine = Engine::new(device);
    engine.add_use_case([1], 1, &lpc::ENERGY_GUARD);
    let actor = EnergyGuardActor::new(lpc::DIRECTION, client, diagnosis, StdDuration::ZERO);
    (engine, actor)
}

/// The household: hems as a Controllable System.
fn household() -> Lpc {
    Lpc::new(
        AssetId::new("netzanschluss").expect("a valid identifier"),
        Use::Lpc,
        // The household's own § 14a minimum, not a vendor default.
        Power::from_kw(10.5),
        StdDuration::from_secs(2 * 3600),
        START,
    )
}

/// One turn of the loop `hemsd` runs, with the socket replaced by a `Vec`.
///
/// Datagrams go across as JSON, which is what a SHIP data frame carries and
/// what [`Driver::poll_transmit`] and [`Driver::on_bytes`] therefore speak.
/// Nothing here is a shortcut around the protocol: both sides are the real
/// engines, and a message either side refuses to encode simply does not arrive.
struct Wire {
    guard_engine: Engine,
    guard: EnergyGuardActor,
    box_driver: Lpc,
    /// What the household's driver reported upwards this run.
    reported: Vec<DriverEvent>,
    /// What the Energy Guard was told about its own writes.
    answers: Vec<GuardEvent>,
    attached: bool,
}

impl Wire {
    fn new() -> Self {
        let (guard_engine, guard) = steuerbox();
        Self {
            guard_engine,
            guard,
            box_driver: household(),
            reported: Vec::new(),
            answers: Vec::new(),
            attached: false,
        }
    }

    /// Both ends learn a session is up, and each asks the other who it is.
    fn open(&mut self, seconds: i64) {
        self.box_driver.on_link(LinkState::Up, at(seconds));
        let source = eebus::spine::node_management(self.guard_engine.device().address());
        let destination = eebus::spine::node_management_without_device();
        for function in [
            eebus::model::Function::NodeManagementDetailedDiscoveryData,
            eebus::model::Function::NodeManagementUseCaseData,
        ] {
            let _ = self
                .guard_engine
                .read(&destination, &source, function, elapsed(seconds));
        }
    }

    /// Move every datagram that is waiting, in both directions, until neither
    /// side has anything more to say.
    fn settle(&mut self, seconds: i64) {
        let now = at(seconds);
        let mono = elapsed(seconds);
        // Bounded: a protocol exchange that will not settle is a defect, and a
        // test that loops for ever reports it as a hang rather than a failure.
        for _ in 0..64 {
            let mut moved = false;

            while let Some(bytes) = self.box_driver.poll_transmit() {
                moved = true;
                let datagram = serde_json::from_slice(&bytes)
                    .expect("what the driver emits is a SPINE datagram");
                let _ = self.guard_engine.handle_datagram(&datagram, mono);
            }
            while let Some(datagram) = self.guard_engine.poll_transmit() {
                moved = true;
                let bytes = serde_json::to_vec(&datagram).expect("a datagram serialises");
                self.box_driver
                    .on_bytes(&bytes, now)
                    .expect("the driver understands its own protocol");
            }

            self.drain(seconds);
            if !moved {
                break;
            }
        }
    }

    /// Read what each side has made of what it received.
    fn drain(&mut self, seconds: i64) {
        let mono = elapsed(seconds);
        while let Some(event) = self.box_driver.poll_event() {
            self.reported.push(event);
        }
        while let Some(event) = self.guard_engine.poll_event() {
            self.answers.extend(
                self.guard
                    .handle_event(&mut self.guard_engine, &event, mono),
            );
        }
        // Discovery is what tells the Energy Guard where the household's
        // `LoadControl` feature lives; until it has been through, there is
        // nothing to bind to and nothing to write.
        if !self.attached {
            let peer = self
                .guard_engine
                .peers()
                .find_map(|remote| limitation::locate(remote, lpc::DIRECTION));
            if let Some(peer) = peer {
                self.guard.attach(&mut self.guard_engine, peer, mono);
                self.attached = true;
            }
        }
    }

    /// Time passes on both ends, which is where heartbeats go out.
    fn tick(&mut self, seconds: i64) {
        self.box_driver.on_timeout(at(seconds));
        let mono = elapsed(seconds);
        let answers = self.guard.handle_timeout(&mut self.guard_engine, mono);
        self.answers.extend(answers);
        self.settle(seconds);
    }

    /// The ceilings the driver reported to the rest of hems.
    fn ceilings(&self) -> Vec<(Option<Power>, LimitSource)> {
        self.reported
            .iter()
            .filter_map(|e| match e {
                DriverEvent::GridLimit(l) => Some((l.ceiling, l.source)),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn a_steuerbox_discovers_the_box_and_writes_a_limit_that_reaches_the_guard() {
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    assert!(
        wire.attached,
        "the Energy Guard has to find the household's LoadControl feature by \
         discovery — a hard-coded address is what makes an integration work on \
         one device and no other"
    );

    // The binding and the heartbeat that have to precede a limit are the actor's
    // business; the seconds here are what a real exchange takes.
    for second in [1_i64, 2, 3, 60, 61] {
        wire.tick(second);
    }

    let device = wire
        .guard
        .peers()
        .next()
        .expect("the household is attached")
        .device
        .clone();
    wire.guard
        .require(&device, Some(LimitWrite::active(4_200.0)), elapsed(62));
    wire.tick(62);
    wire.tick(63);

    let accepted = wire
        .answers
        .iter()
        .any(|e| matches!(e, GuardEvent::LimitAccepted { .. }));
    assert!(
        accepted,
        "the network operator gets an acknowledgement, and under § 14a it is the \
         evidence the reduction was received: {:?}",
        wire.answers
    );

    assert_eq!(
        wire.box_driver.ceiling(),
        Some(Power::from_kw(4.2)),
        "the limit is in force on the household's own state machine"
    );
    let reported = wire.ceilings();
    let last = reported
        .last()
        .expect("a limit that arrives over the wire reaches the guard");
    assert_eq!(last.0, Some(Power::from_kw(4.2)));
    assert_eq!(
        last.1,
        LimitSource::Operator,
        "the operator asked for this one — a Nachweis that called it a failsafe \
         would be describing a different event"
    );
}

#[test]
fn a_dropped_session_does_not_by_itself_restrain_the_household() {
    // A TCP reset is not a lost Energy Guard. `[LPC-911]` times the failsafe by
    // the heartbeat and by nothing else, so a WLAN glitch a reconnect repairs
    // inside two minutes must cost the household nothing — a driver that fell to
    // its failsafe on a socket error would restrain a house for a lost packet.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    for second in [1_i64, 2, 3, 60, 61] {
        wire.tick(second);
    }
    // Control has to be established before it can be lost: a guard that is
    // merely reachable is not yet a guard that is in charge, so the household
    // holds itself at its own failsafe until one writes.
    let device = wire
        .guard
        .peers()
        .next()
        .expect("the household is attached")
        .device
        .clone();
    wire.guard
        .require(&device, Some(LimitWrite::active(4_200.0)), elapsed(62));
    wire.tick(62);
    wire.tick(63);
    assert_eq!(
        wire.box_driver.ceiling(),
        Some(Power::from_kw(4.2)),
        "the operator is in charge and has asked for 4,2 kW"
    );

    // The socket drops.
    let before = wire.box_driver.state();
    wire.box_driver.on_link(LinkState::Down, at(64));
    wire.drain(64);
    assert_eq!(
        wire.box_driver.state(),
        before,
        "losing the socket is not losing the operator"
    );
    assert_eq!(
        wire.box_driver.ceiling(),
        Some(Power::from_kw(4.2)),
        "and the operator's own limit is what stays in force, not the failsafe"
    );

    // …but the heartbeat clock is not paused by the disconnection either, so a
    // session that stays down does end in the failsafe.
    wire.box_driver.on_timeout(at(64 + 121));
    wire.drain(64 + 121);
    assert_eq!(
        wire.box_driver.ceiling(),
        Some(Power::from_kw(10.5)),
        "two minutes without a heartbeat is the failsafe, whatever the socket did"
    );
    assert_eq!(
        wire.ceilings().last().map(|(_, source)| *source),
        Some(LimitSource::Failsafe),
        "and the record says the household restrained itself rather than that \
         the operator asked it to"
    );
}
