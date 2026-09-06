//! A hot-water circuit tells the box what the tank reached, over SPINE.
//!
//! The planner has modelled a hot-water tank since the optimiser was written and
//! has never been given one on a running box, for want of a single number:
//! `DhwModel::stored_now`, the heat in the tank right now. This is the seam that
//! produces it, and it is exercised the same way the § 14a one is — both ends
//! are real engines, every message crosses as the JSON a SHIP data frame
//! carries, and a message either side refuses to encode simply does not arrive.

#![cfg(feature = "eebus")]

use core::time::Duration as StdDuration;

use eebus::model::HvacOperationModeType;
use eebus::model::{DeviceType, EntityType, MeasurementValueSource, MeasurementValueState};
use eebus::spine::SpineEvent;
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::usecases::hvac::system_function::{Request, SystemFunction};
use eebus::usecases::hvac::{cdsf, mdt};
use hems_core::prelude::AssetId;
use hems_drv::eebus::SpineIdentity;
use hems_drv::eebus_dhw::DhwTank;
use hems_drv::{Driver, DriverEvent, LinkState};
use time::OffsetDateTime;
use time::macros::datetime;

const START: OffsetDateTime = datetime!(2026-01-15 06:00:00 UTC);

fn at(seconds: i64) -> OffsetDateTime {
    START + time::Duration::seconds(seconds)
}

fn elapsed(seconds: i64) -> StdDuration {
    StdDuration::from_secs(seconds.unsigned_abs())
}

/// A hot-water heat pump publishing MDT on a `DHWCircuit` entity.
struct Circuit {
    engine: Engine,
    measurement: eebus::model::FeatureAddress,
    /// The circuit's own view of it, which is what answers a write.
    function: SystemFunction,
    /// What the box asked for, in the order it asked.
    asked: Vec<Request>,
}

impl Circuit {
    fn new() -> Self {
        let mut device =
            LocalDevice::new("n:acme", "Warmwasser-1", DeviceType::HeatGenerationSystem)
                .expect("a valid device address");
        device
            .add_entity(
                LocalEntity::new([1], EntityType::DHWCircuit)
                    .with_feature(mdt::measurement_feature(1))
                    // The one `HVAC` feature §3.2.2.2.1 gives an entity, carrying
                    // the hot-water system function and its one-time loading.
                    .with_feature(cdsf::hvac_feature(2)),
            )
            .expect("a fresh entity");
        let measurement = device.address_of(&[1], 1);
        let hvac = device.address_of(&[1], 2);
        let mut engine = Engine::new(device);
        engine.add_use_case([1], 1, &mdt::DHW_CIRCUIT);
        engine.add_use_case([1], 1, &cdsf::DHW_CIRCUIT);
        // The description is published at start-up, as a real circuit publishes
        // it: it is what says the number is a hot-water temperature in degrees
        // Celsius, and MDT Table 7 permits degF and K as well.
        if let Some(feature) = engine.device_mut().resolve_mut(&measurement) {
            feature
                .set_data(mdt::temperature_description())
                .expect("the description Table 7 fixes");
            feature
                .set_data(mdt::temperature_constraints(10.0, 75.0, Some(0.5)))
                .expect("what the circuit can report");
        }
        // What a circuit publishes about its hot water: the function, the modes
        // it relates, and the overrun a manager may start.
        let modes = [
            HvacOperationModeType::Auto,
            HvacOperationModeType::On,
            HvacOperationModeType::Eco,
        ];
        let published = [
            cdsf::system_function_description(),
            cdsf::operation_mode_descriptions(&modes).expect("three modes"),
            cdsf::operation_mode_relations(&modes).expect("three modes"),
            cdsf::overrun_description(),
            cdsf::system_function_state(
                cdsf::operation_mode_id(&HvacOperationModeType::Auto).expect("a known mode"),
                false,
                Some(true),
            ),
            cdsf::overrun_state(eebus::model::HvacOverrunStatus::Inactive),
        ];
        let mut function = cdsf::reader();
        for data in published {
            function.learn(&data);
            if let Some(feature) = engine.device_mut().resolve_mut(&hvac) {
                let _ = feature.set_data(data);
            }
        }
        Self {
            engine,
            measurement,
            function,
            asked: Vec::new(),
        }
    }

    fn reports(&mut self, degrees: f64, seconds: i64) {
        self.publish(mdt::temperature(degrees), seconds);
    }

    fn reports_with(&mut self, degrees: f64, state: MeasurementValueState, seconds: i64) {
        self.publish(
            mdt::temperature_from(
                degrees,
                MeasurementValueSource::MeasuredValue,
                Some(state),
                None,
            ),
            seconds,
        );
    }

    /// Answer whatever the box wrote, the way a circuit does.
    fn answer_writes(&mut self, seconds: i64) {
        let now = elapsed(seconds);
        let mut pending = Vec::new();
        while let Some(event) = self.engine.poll_event() {
            if let SpineEvent::WriteRequested(write) = event {
                pending.push(write);
            }
        }
        for write in pending {
            // `write.data`, the fragment — not `resolved`. These are list
            // functions with more than one entry, and the resolved state cannot
            // say which one the peer addressed.
            match self.function.apply(&write.data) {
                Ok(request) => {
                    self.asked.push(request);
                    let status = match request {
                        Request::StartOverrun(_) => eebus::model::HvacOverrunStatus::Running,
                        _ => eebus::model::HvacOverrunStatus::Inactive,
                    };
                    let data = cdsf::overrun_state(status);
                    self.function.learn(&data);
                    // `accept_write_with`, not `accept_write`: the second stores
                    // the peer's own *fragment* on the feature and notifies
                    // that — so a manager subscribed to the overrun would be
                    // told back exactly what it asked for, whatever the circuit
                    // decided. This stores what the circuit is now doing, and
                    // notifies that, in one step.
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

    fn publish(&mut self, data: eebus::model::CmdData, seconds: i64) {
        let address = self.measurement.clone();
        if let Some(feature) = self.engine.device_mut().resolve_mut(&address) {
            feature.set_data(data).expect("a temperature");
        }
        self.engine.notify(
            &address,
            &eebus::model::Function::MeasurementListData,
            elapsed(seconds),
        );
    }
}

/// One turn of the loop `hemsd` would run, with the socket replaced by a `Vec`.
struct Wire {
    circuit: Circuit,
    box_driver: DhwTank,
    reported: Vec<DriverEvent>,
}

impl Wire {
    fn new() -> Self {
        Self {
            circuit: Circuit::new(),
            box_driver: DhwTank::new(
                AssetId::new("warmwasser").expect("a valid identifier"),
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
                let _ = self.circuit.engine.handle_datagram(&datagram, mono);
            }
            self.circuit.answer_writes(seconds);
            while let Some(datagram) = self.circuit.engine.poll_transmit() {
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

    /// The temperatures the driver reported to the rest of hems.
    fn temperatures(&self) -> Vec<f64> {
        self.reported
            .iter()
            .filter_map(|e| match e {
                DriverEvent::Measured(m) => m.temperature_c,
                _ => None,
            })
            .collect()
    }
}

#[test]
fn the_tank_temperature_reaches_the_box() {
    // The number the plan was missing. A store whose state of charge is unknown
    // cannot be planned, and a plan that guessed it would decide when to heat
    // from something nobody measured.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    assert!(
        wire.temperatures().is_empty(),
        "nothing has been published, and an unread tank is not a cold one"
    );

    wire.circuit.reports(52.5, 1);
    wire.settle(1);

    assert_eq!(wire.temperatures(), vec![52.5]);
}

#[test]
fn a_tank_holding_its_temperature_is_reported_once() {
    // A subscription refresh on a tank that has not moved is not news, and the
    // registry is edge-driven.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.circuit.reports(52.5, 1);
    wire.settle(1);
    wire.circuit.reports(52.5, 2);
    wire.settle(2);
    wire.circuit.reports(48.0, 3);
    wire.settle(3);

    assert_eq!(
        wire.temperatures(),
        vec![52.5, 48.0],
        "three published, two reported — and the shower is the one that shows"
    );
}

#[test]
fn a_sensor_the_circuit_has_flagged_is_not_a_temperature() {
    // [MDT-005]: a value the circuit marks `error` or `outOfRange` **SHALL be
    // ignored**. The dangerous reading is not a wild one — it is a plausible
    // one: a failed sensor stuck at 5 °C would have the plan heat a full tank
    // all night at the day's worst price.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.circuit.reports(52.5, 1);
    wire.settle(1);
    wire.circuit
        .reports_with(5.0, MeasurementValueState::Error, 2);
    wire.settle(2);

    assert_eq!(
        wire.temperatures(),
        vec![52.5],
        "the flagged reading reaches the planner as an absent tank rather than \
         as a number it will heat against"
    );
}

#[test]
fn a_reconnect_does_not_resolve_a_new_circuits_values_against_an_old_ones_meaning() {
    // An address that reconnects may be a different device — a replaced heat
    // pump, a gateway that renumbered. A description kept across the gap would
    // read the new circuit's `measurementId` with the old one's unit, and
    // Fahrenheit against Celsius is forty degrees exactly where it matters.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    wire.circuit.reports(52.5, 1);
    wire.settle(1);

    wire.box_driver.on_link(LinkState::Down, at(2));
    wire.circuit = Circuit::new();
    wire.reported.clear();
    wire.open(3);
    wire.settle(3);
    wire.circuit.reports(52.5, 4);
    wire.settle(4);

    assert_eq!(
        wire.temperatures(),
        vec![52.5],
        "the same reading is news again, because it is a different circuit"
    );
}

#[test]
fn the_box_asks_the_tank_to_heat_and_the_circuit_starts_a_loading() {
    // The lever this driver never had. It has reported the tank's temperature
    // since it was written — which is what put a `DhwModel` in the running
    // plan — and a hot-water tank is a **controllable** asset, so a household
    // whose only tank driver was this one was refused at start-up with
    // `CannotCommand`. The plan moved a store nothing could carry the decision
    // to.
    //
    // CDSF scenario 2 is the shortest path there is from "the roof is
    // exporting" to "the tank is absorbing it": the button in the bathroom,
    // pressed over the wire.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    wire.box_driver
        .command(&hems_core::setpoint::Command::OnOff(true), at(1))
        .expect("a circuit that published an overrun takes a start");
    wire.settle(1);

    assert!(
        matches!(wire.circuit.asked.as_slice(), [Request::StartOverrun(_)]),
        "the circuit was asked to load, once: {:?}",
        wire.circuit.asked
    );

    // …and it gives it back when a cloud arrives.
    wire.box_driver
        .command(&hems_core::setpoint::Command::OnOff(false), at(2))
        .expect("and a stop");
    wire.settle(2);
    assert!(
        matches!(
            wire.circuit.asked.as_slice(),
            [Request::StartOverrun(_), Request::StopOverrun(_)]
        ),
        "{:?}",
        wire.circuit.asked
    );
}

#[test]
fn a_loading_already_running_is_not_restarted_every_control_period() {
    // The arbiter decides afresh every few seconds and this asset's decision is
    // a boolean, so the same `on` arrives over and over. Restating it would put
    // a write on the wire every control period for as long as the sun is out.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);

    for second in 1..8 {
        wire.box_driver
            .command(&hems_core::setpoint::Command::OnOff(true), at(second))
            .expect("a repeat is a no-op, not an error");
        wire.settle(second);
    }
    assert_eq!(
        wire.circuit.asked.len(),
        1,
        "asked once, and it is still loading: {:?}",
        wire.circuit.asked
    );
}

#[test]
fn a_ceiling_is_not_this_use_cases_business() {
    // Curtailing a tank is the § 14a envelope's business and reaches the heater
    // some other way. Answering a ceiling here by stopping a loading would turn
    // a limit the circuit could have respected underneath into a shower nobody
    // gets.
    let mut wire = Wire::new();
    wire.open(0);
    wire.settle(0);
    let refused = wire.box_driver.command(
        &hems_core::setpoint::Command::ConsumptionCeiling(hems_core::prelude::Power::from_kw(1.0)),
        at(1),
    );
    assert!(refused.is_err());
    wire.settle(1);
    assert!(wire.circuit.asked.is_empty());
}

#[test]
fn a_tank_both_reports_its_temperature_and_takes_a_loading() {
    // Which is why they are one driver: SHIP grants one session per peer pair,
    // and the registry allows one commanding and one measuring driver per asset.
    let wire = Wire::new();
    assert!(wire.box_driver.capabilities().measures);
    assert!(wire.box_driver.capabilities().accepts_commands);
}
