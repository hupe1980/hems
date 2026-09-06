//! A car is plugged in, says how full it is, and is unplugged again.
//!
//! `EvSession` has been in the optimiser from the beginning and has never been
//! built on a running box, because nothing reported an arrival. EVCC scenario 1
//! is the one place in EEBUS where the *absence* of a payload is the payload —
//! an `EV` entity appearing under the `EVSE` entity is how a car says it is
//! plugged in — so this test is about the entity tree as much as about any
//! message, and both ends are real engines throughout.

#![cfg(feature = "eebus")]

use core::time::Duration as StdDuration;

use eebus::model::{DeviceType, EntityType};
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::usecases::emobility::{evcc, evsoc};
use eebus::usecases::monitoring::{Measurand, MonitoredUnit, Quantity};
use hems_core::prelude::AssetId;
use hems_drv::eebus::SpineIdentity;
use hems_drv::eebus_ev::EvCharger;
use hems_drv::{Driver, DriverEvent, LinkState, VehiclePresence};
use time::OffsetDateTime;
use time::macros::datetime;

const START: OffsetDateTime = datetime!(2026-01-15 18:00:00 UTC);

fn at(seconds: i64) -> OffsetDateTime {
    START + time::Duration::seconds(seconds)
}

fn elapsed(seconds: i64) -> StdDuration {
    StdDuration::from_secs(seconds.unsigned_abs())
}

/// A wallbox, with or without a car in it.
struct Wallbox {
    engine: Engine,
    car: Option<CarFeatures>,
}

struct CarFeatures {
    unit: MonitoredUnit,
    measurement: eebus::model::FeatureAddress,
    electrical: eebus::model::FeatureAddress,
}

impl Wallbox {
    /// A charge point with nothing plugged into it: an `EVSE` entity and no
    /// `EV` beneath it, which is exactly what an empty socket looks like.
    fn empty() -> Self {
        let mut device = LocalDevice::new("n:acme", "Wallbox-1", DeviceType::ChargingStation)
            .expect("a valid device address");
        device
            .add_entity(LocalEntity::new([1], EntityType::EVSE))
            .expect("a fresh entity");
        Self {
            engine: Engine::new(device),
            car: None,
        }
    }

    /// A cable goes in. The `EV` entity appears under the `EVSE`, which is
    /// EVCC scenario 1 in its entirety — there is no message for it.
    fn car_arrives(&mut self, soc_percent: f64, capacity_wh: f64, seconds: i64) {
        let unit = evsoc::monitored_unit(1).with(Measurand::unphased(Quantity::StateOfCharge));
        let device = self.engine.device_mut();
        device
            .add_entity(
                LocalEntity::new([1, 1], EntityType::EV)
                    .with_feature(unit.measurement_feature(1))
                    .with_feature(evsoc::characteristic_feature(2)),
            )
            .expect("a car on the wallbox");
        let measurement = device.address_of(&[1, 1], 1);
        let electrical = device.address_of(&[1, 1], 2);
        self.engine.add_use_case([1, 1], 1, &evcc::EV);
        self.engine.add_use_case([1, 1], 1, &evsoc::EV);
        let mut car = CarFeatures {
            unit,
            measurement,
            electrical,
        };
        car.unit
            .set(&Measurand::unphased(Quantity::StateOfCharge), soc_percent);
        car.unit
            .publish(&mut self.engine, &car.electrical, &car.measurement);
        if let Some(feature) = self.engine.device_mut().resolve_mut(&car.electrical) {
            feature
                .set_data(evsoc::nominal_capacity(capacity_wh))
                .expect("a battery has a size");
        }
        self.car = Some(car);
        self.announce(seconds);
    }

    /// Tell whoever is subscribed that the entity tree changed.
    ///
    /// This is how scenario 1 and scenario 8 actually reach a manager: SPINE
    /// notifies `NodeManagementDetailedDiscoveryData`, and the *content* of that
    /// notification — an `EV` entity that is there, or is not — is the whole
    /// message. Nothing else is sent.
    fn announce(&mut self, seconds: i64) {
        let address = eebus::spine::node_management(self.engine.device().address());
        self.engine.notify(
            &address,
            &eebus::model::Function::NodeManagementDetailedDiscoveryData,
            elapsed(seconds),
        );
    }

    /// The cable comes out and the charge point **says so**: EVCC scenario 8, as
    /// a `cmdClassifier: delete` on the `EV` entity.
    ///
    /// `Engine::remove_entity` is what sends it. That is the only message an
    /// arrival is not — a merged discovery document cannot shrink on its own, so
    /// a peer learns of a departure from a device that deletes and from nothing
    /// else.
    fn car_drives_away(&mut self, seconds: i64) {
        self.engine
            .remove_entity(&[1, 1], elapsed(seconds))
            .expect("the car entity is there to remove");
        self.car = None;
    }

    /// The cable comes out and the charge point says **nothing**, which is the
    /// commoner case: it simply re-answers discovery without the `EV`.
    ///
    /// A shorter reply cannot remove what an earlier one added (§ 7.1.5), so the
    /// peer's merged tree still has the car in it. This is the case a box must
    /// not read as a departure.
    fn car_leaves_quietly(&mut self, seconds: i64) {
        let mut empty = Self::empty();
        core::mem::swap(self, &mut empty);
        self.announce(seconds);
    }

    fn reports_soc(&mut self, soc_percent: f64, seconds: i64) {
        let Some(car) = self.car.as_mut() else { return };
        car.unit
            .set(&Measurand::unphased(Quantity::StateOfCharge), soc_percent);
        let (electrical, measurement) = (car.electrical.clone(), car.measurement.clone());
        let unit = car.unit.clone();
        unit.publish(&mut self.engine, &electrical, &measurement);
        self.engine.notify(
            &measurement,
            &eebus::model::Function::MeasurementListData,
            elapsed(seconds),
        );
    }
}

struct Wire {
    wallbox: Wallbox,
    box_driver: EvCharger,
    reported: Vec<DriverEvent>,
}

impl Wire {
    fn new() -> Self {
        Self {
            wallbox: Wallbox::empty(),
            box_driver: EvCharger::new(
                AssetId::new("wallbox").expect("a valid identifier"),
                START,
                &SpineIdentity::default(),
            )
            .expect("the default SPINE identity is a valid device address"),
            reported: Vec::new(),
        }
    }

    fn open(&mut self, seconds: i64) {
        self.box_driver.on_link(LinkState::Up, at(seconds));
    }

    /// Time passes, which is when the box next looks at the entity tree.
    fn tick(&mut self, seconds: i64) {
        self.box_driver.on_timeout(at(seconds));
        self.settle(seconds);
    }

    fn settle(&mut self, seconds: i64) {
        let now = at(seconds);
        let mono = elapsed(seconds);
        for _ in 0..64 {
            let mut moved = false;
            while let Some(bytes) = self.box_driver.poll_transmit() {
                moved = true;
                let datagram =
                    serde_json::from_slice(&bytes).expect("what the driver emits is a datagram");
                let _ = self.wallbox.engine.handle_datagram(&datagram, mono);
            }
            while let Some(datagram) = self.wallbox.engine.poll_transmit() {
                moved = true;
                let bytes = serde_json::to_vec(&datagram).expect("a datagram serialises");
                self.box_driver
                    .on_bytes(&bytes, now)
                    .expect("the driver understands its own protocol");
            }
            while let Some(event) = self.box_driver.poll_event() {
                self.reported.push(event);
            }
            if !moved {
                break;
            }
        }
    }

    fn sessions(&self) -> Vec<VehiclePresence> {
        self.reported
            .iter()
            .filter_map(|e| match e {
                DriverEvent::Vehicle(v) => Some(*v),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn an_empty_socket_is_not_a_car() {
    // The distinction the planner needs before it needs any number: a charge
    // point with nothing plugged into it is working perfectly and has no state
    // of charge to report, so an absent measurement cannot be the signal.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    assert!(
        wire.sessions().iter().all(|s| !s.connected),
        "nothing has been plugged in: {:?}",
        wire.sessions()
    );
}

#[test]
fn a_cable_going_in_is_the_message() {
    // EVCC scenario 1 has no payload at all — the `EV` entity appearing under
    // the `EVSE` is how a car says it is there. So the arrival is read off the
    // peer's own entity tree, and the state of charge follows it.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.wallbox.car_arrives(35.0, 58_000.0, 1);
    wire.tick(31);

    let last = wire.sessions().pop().expect("a session");
    assert!(last.connected, "a car is on the end of it");
    assert_eq!(
        last.soc,
        Some(0.35),
        "thirty-five per cent as a fraction — a 0,35 that should have been 35 \
         is a car the plan believes is nearly empty, and the other way round is \
         one it never charges"
    );
    assert_eq!(last.capacity_wh, Some(58_000.0));
}

#[test]
fn a_charge_point_that_deletes_the_entity_ends_the_session() {
    // EVCC scenario 8, and the one message an arrival is not: a merged
    // discovery document cannot shrink on its own, so a departure reaches a
    // manager only from a device that sends `cmdClassifier: delete`.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.wallbox.car_arrives(35.0, 58_000.0, 1);
    wire.tick(31);
    assert!(wire.sessions().last().expect("a session").connected);

    wire.wallbox.car_drives_away(60);
    wire.tick(61);

    let last = wire.sessions().pop().expect("a session");
    assert!(
        !last.connected,
        "a deleted `EV` entity is a car that has gone, and the box may act on it"
    );
    assert_eq!(last.soc, None, "an absent car has no state of charge");
    assert_eq!(last.capacity_wh, None);
}

#[test]
fn a_cable_coming_out_quietly_is_not_a_departure_anybody_may_act_on() {
    // The commoner case, and the one a box must not read as a departure. A
    // charge point that simply re-answers discovery without the `EV` has said
    // nothing: §7.1.5 lets a re-send be partial, so a shorter reply cannot
    // remove what an earlier one added and the entity is still in the tree.
    //
    // Inventing a departure here would end a charging session the household is
    // in the middle of.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.wallbox.car_arrives(35.0, 58_000.0, 1);
    wire.tick(31);
    assert!(wire.sessions().last().expect("a session").connected);

    wire.wallbox.car_leaves_quietly(60);
    wire.tick(61);

    assert!(
        wire.sessions().last().expect("a session").connected,
        "the merged tree still has the car in it, and nothing has said otherwise"
    );

    // What *is* reliable: the session restarting. A reconnect drops the peer, so
    // the tree is built again from nothing — which is how a box that was away
    // for an hour finds out the car left while it was.
    wire.box_driver.on_link(LinkState::Down, at(70));
    wire.open(71);
    wire.settle(71);

    let last = wire.sessions().pop().expect("a session");
    assert!(!last.connected, "and now the socket is empty");
    assert_eq!(last.soc, None, "an absent car has no state of charge");
    assert_eq!(last.capacity_wh, None);
}

#[test]
fn a_car_filling_up_is_reported_and_a_car_holding_still_is_not() {
    // The registry is edge-driven and a re-plan is not free. A car reporting the
    // same percentage every four seconds — which is what a subscription refresh
    // looks like — must not be news.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.wallbox.car_arrives(35.0, 58_000.0, 1);
    wire.tick(31);

    // Everything the arrival itself produced. It is several events on purpose:
    // "a car is there", then "and it is at 35 %", then "and its battery is
    // 58 kWh" are three different things the box knows, and each arrives in its
    // own reply.
    let settled = wire.sessions().len();
    assert_eq!(wire.sessions().last().expect("a session").soc, Some(0.35));

    // The same percentage again, which is what a subscription refresh looks
    // like.
    wire.wallbox.reports_soc(35.0, 40);
    wire.settle(40);
    assert_eq!(
        wire.sessions().len(),
        settled,
        "a car holding still is not news: {:?}",
        wire.sessions()
    );

    wire.wallbox.reports_soc(41.0, 50);
    wire.settle(50);
    assert_eq!(
        wire.sessions().last().expect("a session").soc,
        Some(0.41),
        "and a car that has taken some charge is"
    );
}

#[test]
fn a_car_that_cannot_say_how_full_it_is_is_still_a_car() {
    // A car on IEC 61851 has a pilot wire and nothing else: it cannot be asked
    // its state of charge. It is still plugged in, and the plan still has to
    // know that — a box that required a percentage would refuse to work with
    // most of the cars on the road.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    let device = wire.wallbox.engine.device_mut();
    device
        .add_entity(LocalEntity::new([1, 1], EntityType::EV))
        .expect("a car with nothing to say");
    wire.wallbox.announce(1);
    wire.tick(31);

    // No `Measurement` feature, so nothing is located and nothing is claimed —
    // which is the honest answer rather than a percentage nobody published.
    assert!(
        wire.sessions().iter().all(|s| s.soc.is_none()),
        "no percentage was invented: {:?}",
        wire.sessions()
    );
}

#[test]
fn a_charge_point_going_quiet_is_not_a_car_leaving() {
    // The difference matters to a plan. An unplugged car is a session that
    // ended; an unreachable charge point is one nobody can see, and inventing a
    // departure would end a session the household is in the middle of.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.wallbox.car_arrives(35.0, 58_000.0, 1);
    wire.tick(31);
    wire.reported.clear();

    wire.box_driver.on_link(LinkState::Down, at(40));
    while let Some(event) = wire.box_driver.poll_event() {
        wire.reported.push(event);
    }

    assert!(
        wire.sessions().is_empty(),
        "the link went, and that is a link event and not a departure: {:?}",
        wire.sessions()
    );
    assert!(
        wire.reported
            .iter()
            .any(|e| matches!(e, DriverEvent::Link(LinkState::Down))),
        "and it is reported as what it is"
    );
}
