//! A heat pump's compressor tells the box it *could* run, and the box starts it.
//!
//! Every other limit in this workspace asks an appliance to do less, and a
//! ceiling an appliance is already under changes nothing — so a plan that has
//! worked out the building will be cheaper if the compressor runs *now*, while
//! the roof is exporting, has had no way to say so. This is the seam that closes
//! that, and it is exercised the way the § 14a one is: both ends are real
//! engines, every message crosses as the JSON a SHIP data frame carries, and a
//! message either side refuses to encode simply does not arrive.

#![cfg(feature = "eebus")]

use core::time::Duration as StdDuration;

use eebus::model::{DeviceType, EntityType, PowerSequenceState};
use eebus::spine::{Engine, LocalDevice, LocalEntity, SpineEvent};
use eebus::usecases::hvac::{mot, mrt};
use eebus::usecases::ohpcf::{self, Durations, Flexibility as Offer, Interrupt, Request};
use hems_core::prelude::{AssetId, Power};
use hems_core::setpoint::Command;
use hems_drv::eebus::SpineIdentity;
use hems_drv::eebus_heat_pump::HeatPump;
use hems_drv::{Driver, DriverEvent, Flexibility, LinkState};
use time::OffsetDateTime;
use time::macros::datetime;

const START: OffsetDateTime = datetime!(2026-05-15 09:00:00 UTC);

fn at(seconds: i64) -> OffsetDateTime {
    START + time::Duration::seconds(seconds)
}

fn elapsed(seconds: i64) -> StdDuration {
    StdDuration::from_secs(seconds.unsigned_abs())
}

/// The unit on the other end: OHPCF on a `Compressor` entity, and its rooms.
///
/// The entity nests — `[1, 1]` under the appliance's `[1]` — because § 3.2.2.1
/// puts the compressor *inside* the heat-pump appliance, and a lookup that
/// searched the appliance would find the appliance's own features instead.
struct Unit {
    engine: Engine,
    feature: eebus::model::FeatureAddress,
    /// The two rooms' `Measurement` features.
    rooms: [eebus::model::FeatureAddress; 2],
    /// The outdoor sensor's.
    outdoors: eebus::model::FeatureAddress,
    offer: Offer,
    /// What the CEM asked for, in the order it asked.
    asked: Vec<Request>,
}

impl Unit {
    fn new() -> Self {
        let mut device =
            LocalDevice::new("n:acme", "Waermepumpe-1", DeviceType::HeatGenerationSystem)
                .expect("a valid device address");
        device
            .add_entity(LocalEntity::new([1], EntityType::HeatPumpAppliance))
            .expect("a fresh entity");
        device
            .add_entity(
                LocalEntity::new([1, 1], EntityType::Compressor)
                    .with_feature(ohpcf::flexibility_feature(1)),
            )
            .expect("the compressor under it");
        // Two rooms under the appliance, each its own `HVACRoom` entity with its
        // own `Measurement` feature — which is how §7.5 says a device announces
        // more than one.
        for (entity, address) in [([1, 2], 1_u32), ([1, 3], 1)] {
            device
                .add_entity(
                    LocalEntity::new(entity, EntityType::HVACRoom)
                        .with_feature(mrt::measurement_feature(address)),
                )
                .expect("a room under the appliance");
        }
        // …and the outdoor unit's own sensor, which every heat pump has: its
        // defrost logic runs on nothing else.
        device
            .add_entity(
                LocalEntity::new([1, 4], EntityType::HVACRoom)
                    .with_feature(mot::measurement_feature(1)),
            )
            .expect("the outdoor sensor");
        let feature = device.address_of(&[1, 1], 1);
        let rooms = [device.address_of(&[1, 2], 1), device.address_of(&[1, 3], 1)];
        let outdoors = device.address_of(&[1, 4], 1);
        let mut engine = Engine::new(device);
        engine.add_use_case([1, 1], 1, &ohpcf::COMPRESSOR);
        for entity in [[1, 2], [1, 3]] {
            engine.add_use_case(entity, 1, &mrt::HVAC_ROOM);
        }
        engine.add_use_case([1, 4], 1, &mot::OUTDOOR_TEMPERATURE_SENSOR);
        if let Some(f) = engine.device_mut().resolve_mut(&outdoors) {
            let _ = f.set_data(mot::temperature_description());
        }
        for room in &rooms {
            if let Some(f) = engine.device_mut().resolve_mut(room) {
                let _ = f.set_data(mrt::temperature_description());
            }
        }
        // 1,8 kW, half an hour minimum on, half an hour minimum off — the same
        // two numbers `CompressorState` carries, arriving from the machine
        // rather than from a configuration file.
        let offer = Offer::offered(1_800.0)
            .interruptible(Interrupt::Either)
            .lasting(
                Durations::new()
                    .at_least(StdDuration::from_secs(1_800))
                    .resting(StdDuration::from_secs(1_800)),
            );
        offer.publish(&mut engine, &feature);
        Self {
            engine,
            feature,
            rooms,
            outdoors,
            offer,
            asked: Vec::new(),
        }
    }

    /// One room reports how warm it is.
    fn room_says(&mut self, room: usize, degrees: f64, seconds: i64) {
        let Some(feature) = self.rooms.get(room).cloned() else {
            return;
        };
        if let Some(f) = self.engine.device_mut().resolve_mut(&feature) {
            let _ = f.set_data(mrt::temperature(degrees));
        }
        self.engine.notify(
            &feature,
            &eebus::model::Function::MeasurementListData,
            elapsed(seconds),
        );
    }

    /// The outdoor sensor reports the weather at this building.
    fn outside_is(&mut self, degrees: f64, seconds: i64) {
        let feature = self.outdoors.clone();
        if let Some(f) = self.engine.device_mut().resolve_mut(&feature) {
            let _ = f.set_data(mot::temperature(degrees));
        }
        self.engine.notify(
            &feature,
            &eebus::model::Function::MeasurementListData,
            elapsed(seconds),
        );
    }

    /// Withdraw the offer: the compressor has nothing to run.
    fn nothing_to_offer(&mut self, seconds: i64) {
        self.offer.withdraw();
        let (offer, feature) = (self.offer.clone(), self.feature.clone());
        offer.notify(&mut self.engine, &feature, elapsed(seconds));
    }

    /// Answer whatever the CEM wrote, the way a compressor does.
    fn answer_writes(&mut self, seconds: i64) {
        let now = elapsed(seconds);
        let mut pending = Vec::new();
        while let Some(event) = self.engine.poll_event() {
            if let SpineEvent::WriteRequested(write) = event {
                pending.push(write);
            }
        }
        for write in pending {
            match self.offer.apply(&write.resolved) {
                Ok(request) => {
                    self.asked.push(request);
                    let data = self.offer.data();
                    let _ = self.engine.accept_write_with(write.token, data, now);
                }
                Err(refused) => {
                    let _ = self
                        .engine
                        .reject_write(write.token, refused.error_number(), now);
                }
            }
        }
    }
}

/// One turn of the loop `hemsd` would run, with the socket replaced by a `Vec`.
struct Wire {
    pump: Unit,
    box_driver: HeatPump,
    reported: Vec<DriverEvent>,
}

impl Wire {
    fn new() -> Self {
        Self {
            pump: Unit::new(),
            box_driver: HeatPump::new(
                AssetId::new("waermepumpe").expect("a valid identifier"),
                START,
                &SpineIdentity::default(),
            )
            .expect("the default SPINE identity is a valid device address"),
            reported: Vec::new(),
        }
    }

    /// The box dials, so it is the box that opens the exchange.
    fn open(&mut self, seconds: i64) {
        self.box_driver.on_link(LinkState::Up, at(seconds));
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
                self.pump.engine.handle_datagram(&datagram, mono);
            }
            self.pump.answer_writes(seconds);
            while let Some(datagram) = self.pump.engine.poll_transmit() {
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

    /// The offers the driver reported to the rest of hems.
    fn offers(&self) -> Vec<Flexibility> {
        self.reported
            .iter()
            .filter_map(|e| match e {
                DriverEvent::Flexibility(f) => Some(f.clone()),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn a_compressor_that_could_run_says_so_and_the_terms_come_with_it() {
    // The lever the planner has never had. A ceiling can only ask a heat pump to
    // use less; this is an appliance announcing that it *could* use more, and on
    // what terms.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    let offer = wire.offers().pop().expect("the compressor made an offer");
    assert_eq!(offer.asset, AssetId::new("waermepumpe").unwrap());
    assert_eq!(offer.power, Some(Power::from_kw(1.8)));
    assert_eq!(offer.min_run, Some(time::Duration::minutes(30)));
    assert_eq!(offer.min_rest, Some(time::Duration::minutes(30)));
    assert!(!offer.running, "nothing is scheduled yet");
    assert!(offer.stoppable && offer.pausable);
}

#[test]
fn the_box_starts_the_process_and_the_compressor_runs_it() {
    // The whole point: a household on a sunny afternoon, and a manager that can
    // say *now* rather than only *no more than*.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    assert!(!wire.offers().is_empty(), "there is something to schedule");

    wire.box_driver
        .command(&Command::OnOff(true), at(1))
        .expect("a compressor that offered a controllable sequence takes a start");
    wire.settle(1);

    assert_eq!(
        wire.pump.asked,
        vec![Request::Schedule {
            // A span, and not by preference: `SmartEnergyManagementPs` restricts
            // `schedule.startTime` to `xs:duration`, so an instant is not
            // expressible there at all — and a compressor with no clock of its
            // own could not have acted on one.
            start_time: StdDuration::ZERO
        }],
        "the compressor was asked to run, and asked in the form it can obey"
    );

    // …and the compressor starting is a state change the box hears about,
    // because scenario 1 is a subscription rather than a poll.
    wire.pump.offer.start();
    let (offer, feature) = (wire.pump.offer.clone(), wire.pump.feature.clone());
    offer.notify(&mut wire.pump.engine, &feature, elapsed(2));
    wire.settle(2);

    let latest = wire.offers().pop().expect("an offer");
    assert!(
        latest.running,
        "the process is under way and the box knows it"
    );
}

#[test]
fn a_stop_is_only_sent_where_the_compressor_said_it_may_be() {
    // A stop written to a process that is not stoppable is acknowledged and
    // ignored, which reads to every layer above as a heat pump that was turned
    // off and did not stop. The driver refuses it instead.
    let mut wire = Wire::new();
    wire.pump.offer = Offer::offered(1_800.0).interruptible(Interrupt::Pausable);
    let (offer, feature) = (wire.pump.offer.clone(), wire.pump.feature.clone());
    offer.publish(&mut wire.pump.engine, &feature);
    wire.open(0);
    wire.settle(0);

    let seen = wire.offers().pop().expect("an offer");
    assert!(!seen.stoppable, "this one may be paused and not aborted");

    let refused = wire.box_driver.command(&Command::OnOff(false), at(1));
    assert!(
        refused.is_err(),
        "and the driver says so rather than writing"
    );
    wire.settle(1);
    assert!(wire.pump.asked.is_empty(), "nothing reached the compressor");
}

#[test]
fn a_compressor_with_nothing_to_offer_is_not_scheduled() {
    // [OHPCF-003] "there is no process" is a *fact* rather than a parse failure,
    // and it is the one that has to reach the planner: a compressor with nothing
    // to offer is one no plan may schedule.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    assert!(!wire.offers().is_empty());

    wire.pump.nothing_to_offer(10);
    wire.settle(10);

    let refused = wire.box_driver.command(&Command::OnOff(true), at(11));
    assert!(
        refused.is_err(),
        "there is no sequence to address, and inventing one writes into nothing"
    );
}

#[test]
fn a_ceiling_is_not_this_use_cases_business() {
    // LPC is where a heat pump is told to use less. A driver that answered a
    // ceiling by aborting the compressor's process would turn a limit the unit
    // could have respected underneath into a stop nobody asked for.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    let refused = wire
        .box_driver
        .command(&Command::ConsumptionCeiling(Power::from_kw(1.0)), at(1));
    assert!(refused.is_err());
    wire.settle(1);
    assert!(wire.pump.asked.is_empty());
}

#[test]
fn a_sequence_the_compressor_will_not_take_instructions_on_is_not_flexibility() {
    // `sequenceRemoteControllable: false` is a compressor describing itself to a
    // manager that may not drive it. Reporting it as flexibility would have the
    // planner schedule a start that is refused every time, and blame the
    // household's heat pump for it.
    let mut wire = Wire::new();
    wire.pump.offer = Offer::offered(1_800.0).as_maximum();
    let mut data = wire.pump.offer.data();
    strip_remote_control(&mut data);
    let feature = wire.pump.feature.clone();
    if let Some(f) = wire.pump.engine.device_mut().resolve_mut(&feature) {
        f.set_data(data).expect("a sequence");
    }
    wire.open(0);
    wire.settle(0);

    assert!(
        wire.offers().is_empty(),
        "a sequence nobody may drive is not an offer"
    );
}

/// Say `sequenceRemoteControllable: false` on the one sequence.
fn strip_remote_control(data: &mut eebus::model::CmdData) {
    let eebus::model::CmdData::SmartEnergyManagementPsData(payload) = data else {
        return;
    };
    for alternative in payload.alternatives.iter_mut().flatten() {
        for sequence in alternative.power_sequence.iter_mut().flatten() {
            if let Some(state) = sequence.state.as_mut() {
                state.sequence_remote_controllable = Some(false);
            }
        }
    }
}

/// A state the driver reports as "running" covers paused too: a paused process
/// is one the household is in the middle of, not one that has ended.
#[test]
fn a_paused_process_is_still_a_process() {
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.box_driver
        .command(&Command::OnOff(true), at(1))
        .expect("a start");
    wire.settle(1);
    wire.pump.offer.start();
    let (offer, feature) = (wire.pump.offer.clone(), wire.pump.feature.clone());
    offer.notify(&mut wire.pump.engine, &feature, elapsed(2));
    wire.settle(2);
    assert!(wire.offers().pop().expect("an offer").running);

    wire.pump.offer = {
        let mut paused = wire.pump.offer.clone();
        let _ = paused.apply(&ohpcf::pause(ohpcf::SEQUENCE_ID));
        paused
    };
    assert_eq!(wire.pump.offer.state(), PowerSequenceState::Paused);
    let (offer, feature) = (wire.pump.offer.clone(), wire.pump.feature.clone());
    offer.notify(&mut wire.pump.engine, &feature, elapsed(3));
    wire.settle(3);

    assert!(
        wire.offers().pop().expect("an offer").running,
        "paused is a process the household is in the middle of"
    );
}

#[test]
fn a_start_repeated_every_control_period_is_sent_once() {
    // The arbiter decides afresh every few seconds and this asset's decision is
    // a boolean, so the same `on` arrives over and over. [OHPCF-013] permits one
    // process at a time, which makes the second write an error the compressor is
    // right to raise — and a driver that produced one would have the box report
    // its own heat pump as refusing instructions.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.box_driver
        .command(&Command::OnOff(true), at(1))
        .expect("a start");
    wire.settle(1);
    wire.pump.offer.start();
    let (offer, feature) = (wire.pump.offer.clone(), wire.pump.feature.clone());
    offer.notify(&mut wire.pump.engine, &feature, elapsed(2));
    wire.settle(2);

    for second in 3..10 {
        wire.box_driver
            .command(&Command::OnOff(true), at(second))
            .expect("a repeat is not an error, it is a no-op");
        wire.settle(second);
    }

    assert_eq!(
        wire.pump.asked.len(),
        1,
        "the compressor was asked to run once, and it is still running"
    );
}

#[test]
fn a_stop_for_a_process_that_never_started_is_not_sent_either() {
    // The symmetric case, and the common one: the plan does not want the heat
    // pump for the next four hours, so `off` arrives every control period
    // against a compressor that is already idle.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    for second in 1..5 {
        wire.box_driver
            .command(&Command::OnOff(false), at(second))
            .expect("an idle compressor takes an off without complaint");
        wire.settle(second);
    }
    assert!(wire.pump.asked.is_empty());
}

/// Every temperature the driver has reported to the rest of hems.
fn temperatures(wire: &Wire) -> Vec<f64> {
    wire.reported
        .iter()
        .filter_map(|e| match e {
            DriverEvent::Measured(m) => m.temperature_c,
            _ => None,
        })
        .collect()
}

#[test]
fn the_room_the_compressor_heats_arrives_over_the_same_session() {
    // The measurement the whole thermal plan is gated on. Until `eebus` 0.7 no
    // use case carried it: the planner could model the building, learn which
    // house it is and start the compressor, and the one state it integrates from
    // reached it from nothing. The unit had been measuring it the entire time —
    // its own heating curve runs on it.
    //
    // Same session as OHPCF, because SHIP grants one connection per peer pair: a
    // second driver dialling this unit would spend the day being refused.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.pump.room_says(0, 21.0, 1);
    wire.settle(1);
    assert_eq!(
        temperatures(&wire),
        vec![21.0],
        "one room has spoken, so the house is what it says"
    );
}

#[test]
fn a_house_is_the_mean_of_the_rooms_that_have_spoken() {
    // `Rc2` has a single air node — it describes a *dwelling* — so several rooms
    // have to become one number, and the mean is it. Picking the first would
    // have the planner heat the house to keep one bedroom in band.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.pump.room_says(0, 22.0, 1);
    wire.settle(1);
    wire.pump.room_says(1, 20.0, 2);
    wire.settle(2);

    let reported = temperatures(&wire);
    assert_eq!(
        reported.last().copied(),
        Some(21.0),
        "22 °C and 20 °C is a house at 21 °C"
    );
    // And the first reading was *not* averaged against a room that had not
    // spoken: a four-room unit whose second sensor is late has three readings,
    // and folding in a zero for the fourth is a house the planner thinks is
    // freezing.
    assert_eq!(reported.first().copied(), Some(22.0));
}

#[test]
fn a_temperature_does_not_survive_the_session_that_carried_it() {
    // A reading whose session has gone is not a reading. Kept across the gap it
    // would let the planner integrate a thermal model from a room measured
    // before the device on the other end was replaced.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.pump.room_says(0, 21.0, 1);
    wire.settle(1);
    assert!(!temperatures(&wire).is_empty());

    wire.reported.clear();
    wire.box_driver.on_link(LinkState::Down, at(2));
    wire.box_driver.on_link(LinkState::Up, at(3));
    while let Some(event) = wire.box_driver.poll_event() {
        wire.reported.push(event);
    }
    assert!(
        temperatures(&wire).is_empty(),
        "nothing is reported until a room speaks on the new session"
    );
}

#[test]
fn a_heat_pump_both_drives_its_compressor_and_reports_its_rooms() {
    // Which is why they are one driver: the registry allows one commanding and
    // one measuring driver per asset, and this is both — so a household needs no
    // second EEBUS session, and SHIP would not grant one anyway.
    let wire = Wire::new();
    assert!(wire.box_driver.capabilities().accepts_commands);
    assert!(wire.box_driver.capabilities().measures);
}

#[test]
fn the_weather_at_this_building_is_read_from_its_own_sensor() {
    // The third of the three signals an RC model is fitted from, and the one a
    // household already owns twice over: the planner has a *forecast* for the
    // grid square, and the heat pump has a thermometer on the wall of this
    // building, in its own shade and its own wind.
    //
    // Planning still needs the forecast — the future cannot be measured — but
    // the **fit** is better off with the thermometer, and that is the difference
    // between identifying a house and identifying a house plus the weather
    // service's error.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.pump.outside_is(-3.5, 1);
    wire.settle(1);

    let outdoor: Vec<f64> = wire
        .reported
        .iter()
        .filter_map(|e| match e {
            DriverEvent::Measured(m) => m.outdoor_c,
            _ => None,
        })
        .collect();
    assert_eq!(outdoor, vec![-3.5]);
    assert!(
        temperatures(&wire).is_empty(),
        "and it is not mistaken for a room: the two are different quantities, \
         and averaging the outside into the house is a plan that heats a \
         building it thinks is at −3 °C"
    );
}
