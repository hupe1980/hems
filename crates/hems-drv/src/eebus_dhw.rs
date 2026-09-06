//! The hot-water tank, over EEBUS.
//!
//! `hems-drv/eebus` is the § 14a side, where the box **listens** and a network
//! operator writes limits to it. This is the other direction entirely: a
//! domestic hot-water circuit is a device on the household's own network, hems
//! is the *Monitoring Appliance* that reads it, and the box is the side that
//! dials.
//!
//! # Why a tank temperature is worth a driver of its own
//!
//! The planner has modelled a hot-water tank since the optimiser was written —
//! a linear store with a heater, a coefficient of performance, a standing loss
//! and a price on a cold shower. It has never been *given* one on a running
//! box, and the reason is one number: `DhwModel::stored_now`, the heat in the
//! tank right now, which nothing reported. A store whose state of charge is
//! unknown cannot be planned; a plan that guessed it would decide when to heat
//! from a number nobody measured.
//!
//! MDT is that number. One scenario, mandatory for both actors, and a
//! measurement rather than a setpoint — which is the right half to build first:
//! reading a tank is safe, and a household whose tank is *modelled* is already
//! better off than one whose heater runs on a timer.
//!
//! # What it deliberately does not do
//!
//! It does not write a setpoint. That is CDT, and CDT has a trap in it that
//! makes it a separate piece of work: a setpoint written into an operation mode
//! the circuit is not in is applied, acknowledged, and changes nothing — so a
//! box that only wrote setpoints would report success and heat no water. MDT is
//! what makes that visible at all, by saying what the tank actually reached, so
//! it comes first.

use core::time::Duration as StdDuration;

use eebus::model::{DeviceType, EntityType, Function};
use eebus::spine::{Engine, LocalDevice, LocalEntity, SpineEvent};
use eebus::usecases::hvac::system_function::{OverrunReport, SystemFunction};
use eebus::usecases::hvac::{cdsf, mdt};
use eebus::usecases::limitation;
use eebus::usecases::monitoring::{MonitoringApplianceActor, MonitoringEvent, UnitId};
use hems_core::prelude::{AssetId, Measurement};
use time::OffsetDateTime;

use crate::{
    Driver, DriverCapabilities, DriverError, DriverEvent, LinkState, eebus::SpineIdentity,
};

/// The entity and feature this appliance reads from.
const ENTITY: [u32; 1] = [1];
/// One `Generic` client feature for all client functionality, as the LPC
/// implementation guide § 3.3 asks — not one mirroring each server it reads.
const CLIENT_FEATURE: u32 = 1;

/// A domestic hot-water circuit, read over MDT.
#[derive(Debug)]
pub struct DhwTank {
    asset: AssetId,
    engine: Engine,
    /// The tank, once discovery has named it.
    ///
    /// Read through the same actor the heat pump reads its rooms with rather
    /// than by hand: the actor issues the reads and the subscription its own use
    /// case declares, so a descriptor that gains a feature is followed here
    /// without anybody noticing it had to be.
    circuit: Option<UnitId>,
    /// The circuit's `HVAC` feature, which carries its system function.
    ///
    /// The other half of the tank: `mdt` says how warm it is, and this is how
    /// the box asks for it to be warmer.
    hvac: Option<eebus::model::FeatureAddress>,
    /// What the circuit has published about its hot-water function — the modes
    /// it relates, whether they may be changed, and whether a one-time loading
    /// is running.
    function: SystemFunction,
    /// The tank's readings, resolved against the descriptions that give them
    /// meaning.
    ///
    /// Not optional bookkeeping: a `measurementListData` is a `measurementId`
    /// and a number, and only the description says that the number is a hot
    /// water temperature in degrees Celsius rather than a flow rate. MDT Table 7
    /// permits `degC`, `degF` and `K`, and a circuit reporting Fahrenheit to an
    /// appliance assuming Celsius disagrees by forty degrees exactly where it
    /// matters.
    monitor: MonitoringApplianceActor,
    /// The last temperature reported upwards, so an unchanged reading is not
    /// re-emitted on every notification.
    last: Option<f64>,
    started_at: OffsetDateTime,
    link: LinkState,
    events: Vec<DriverEvent>,
}

impl DhwTank {
    /// A Monitoring Appliance reading the tank on `asset`.
    ///
    /// # Errors
    /// [`DriverError::Unsupported`] where `identity` does not make a valid SPINE
    /// device address.
    pub fn new(
        asset: AssetId,
        started_at: OffsetDateTime,
        identity: &SpineIdentity,
    ) -> Result<Self, DriverError> {
        let mut device = LocalDevice::new(
            &identity.vendor,
            &identity.unique,
            DeviceType::EnergyManagementSystem,
        )
        .map_err(|e| {
            DriverError::Unsupported(format!(
                "`{}` and `{}` do not make a SPINE device address: {e}",
                identity.vendor, identity.unique
            ))
        })?;
        device
            .add_entity(
                LocalEntity::new(ENTITY, EntityType::CEM)
                    .with_feature(limitation::client_feature(CLIENT_FEATURE)),
            )
            .expect("entity [1] is the first one added");
        let client = device.address_of(&ENTITY, CLIENT_FEATURE);
        let mut engine = Engine::new(device);
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &mdt::MONITORING_APPLIANCE);
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &cdsf::CONFIGURATION_APPLIANCE);
        Ok(Self {
            asset,
            engine,
            circuit: None,
            hvac: None,
            function: cdsf::reader(),
            monitor: MonitoringApplianceActor::new(client),
            last: None,
            started_at,
            link: LinkState::Down,
            events: Vec::new(),
        })
    }

    /// `eebus`'s monotonic clock, from a wall-clock instant.
    fn since_start(&self, now: OffsetDateTime) -> StdDuration {
        StdDuration::try_from(now - self.started_at).unwrap_or(StdDuration::ZERO)
    }

    /// This device's one client feature.
    fn client(&self) -> eebus::model::FeatureAddress {
        self.engine.device().address_of(&ENTITY, CLIENT_FEATURE)
    }

    /// Ask whoever is on the other end who they are.
    fn discover(&mut self, elapsed: StdDuration) {
        let source = eebus::spine::node_management(self.engine.device().address());
        let destination = eebus::spine::node_management_without_device();
        for function in [
            Function::NodeManagementDetailedDiscoveryData,
            Function::NodeManagementUseCaseData,
        ] {
            let _ = self.engine.read(&destination, &source, function, elapsed);
        }
    }

    /// Fold everything the engine produced, and read the tank out of it.
    fn consume(&mut self, elapsed: StdDuration, now: OffsetDateTime) {
        while let Some(event) = self.engine.poll_event() {
            self.follow(&event, elapsed, now);
        }
    }

    fn follow(&mut self, event: &SpineEvent, elapsed: StdDuration, now: OffsetDateTime) {
        match event {
            // Retried until it succeeds, because a circuit may gain the use case
            // later — a heat pump whose firmware is updated, or one that only
            // publishes the tank once its own commissioning is done.
            SpineEvent::UseCasesUpdated { device } | SpineEvent::DiscoveryUpdated { device } => {
                self.follow_function(device, elapsed);
                if self.circuit.is_some() {
                    return;
                }
                let Some(remote) = self.engine.peer(device) else {
                    return;
                };
                // `mdt::locate` rather than the use-case lookup and the
                // `address_of` by hand: the feature is on the entity that
                // announced the actor, which is the implementation guide's §3.3
                // rule and the thing a lookup keyed on the appliance above it
                // would get wrong.
                let Some(tank) = mdt::locate(remote) else {
                    return;
                };
                self.circuit = Some(tank.id());
                // The actor issues the reads and the subscription MDT's own
                // descriptor declares — descriptions before values, because a
                // subscription delivers only the *next* change and a tank
                // holding its temperature notifies nothing for hours, so the
                // read is what makes the driver useful in its first minute.
                //
                // By the actor rather than by hand so that a descriptor which
                // gains a feature is followed here without anybody noticing it
                // had to be: `MonitoringApplianceActor` was missing the
                // `ElectricalConnection` subscription until `eebus` 0.9, and a
                // driver that had written its own list would still be missing
                // it.
                self.monitor.attach(&mut self.engine, tank, elapsed);
            }
            // `resolved`, not `data`: a notification may be partial, and a
            // temperature notified as a bare `number` with its `scale` omitted
            // keeps the scale already sent — so reading the fragment alone is
            // off by a power of ten, which at these temperatures is 5,8 °C or
            // 580 °C and neither is a tank.
            SpineEvent::DataNotified {
                feature, resolved, ..
            }
            | SpineEvent::ReplyReceived {
                feature, resolved, ..
            } => {
                if self.hvac.as_ref() == Some(feature) {
                    // Every description and every state the circuit publishes
                    // about its hot-water function, folded into one reader: what
                    // `start_overrun` needs is the circuit's *own* overrun
                    // identifier, and inventing one writes into nothing.
                    self.function.learn(resolved);
                    return;
                }
                let Some(MonitoringEvent::Measured { unit, .. }) = self.monitor.handle_event(event)
                else {
                    return;
                };
                if self.circuit.as_ref() != Some(&unit) {
                    return;
                }
                // `read_at` withholds a reading the circuit flagged `outOfRange`
                // or `error`, which [MDT-005] says an appliance SHALL ignore —
                // so a failed sensor reaches the planner as an absent tank
                // rather than as a number it will heat against.
                let Some((degrees, stamp)) = self
                    .monitor
                    .readings(&unit)
                    .and_then(|readings| readings.read_at(&mdt::MEASURAND))
                else {
                    return;
                };
                if self.last == Some(degrees) {
                    return;
                }
                self.last = Some(degrees);
                // Stamped with the instant the **circuit** says it measured,
                // where it said one. `Measurement::at` has always meant "when
                // the value was observed at the device", and until MDT carried a
                // timestamp every driver here could only fill it with the moment
                // the value arrived.
                let mut measurement = Measurement::at(crate::eebus::taken_at(stamp, now));
                measurement.temperature_c = Some(degrees);
                self.events.push(DriverEvent::Measured(measurement));
            }
            _ => {}
        }
    }
}

impl DhwTank {
    /// Find the circuit's `HVAC` feature and read what it says about hot water.
    ///
    /// Retried until it succeeds, for the same reason the measurement lookup is:
    /// a circuit may gain the use case with a firmware update.
    ///
    /// **No binding.** Every HVAC use case says "Binding SHOULD NOT be used for
    /// this Scenario" — §3.4.1.1 — which is the opposite of OHPCF scenario 2 and
    /// of § 14a. SPINE puts the requirement on the *feature* rather than on the
    /// protocol, and a client that asked for one here would be asking for a
    /// privilege the specification tells it not to want.
    fn follow_function(&mut self, device: &eebus::model::AddressDevice, elapsed: StdDuration) {
        if self.hvac.is_some() {
            return;
        }
        let Some(remote) = self.engine.peer(device) else {
            return;
        };
        // §3.2.2.2.1 gives an entity **one** `HVAC` feature, so a heat pump that
        // heats water and two rooms has three of them and a lookup by feature
        // type alone reports a living room's operation mode from the tank's.
        // `locate` knows the feature is on the entity that announced the actor,
        // which is the implementation guide's §3.3 rule.
        let Some(peer) = cdsf::locate(remote) else {
            return;
        };
        self.hvac = Some(peer.hvac.clone());
        let client = self.client();
        // Subscribe and read in one call, and the read list is the use case's
        // own scenario table intersected with what the peer announced — so a
        // circuit that serves the operation mode and not the one-time loading,
        // which is conformant, is asked for four functions rather than six
        // instead of earning two refusals to questions discovery had already
        // answered.
        //
        // The subscription goes out **before** the reads: a mode changed between
        // the reply and a later subscription request is a change nothing ever
        // hears about. And no binding — all nine HVAC use cases say not to.
        let _ = peer.follow(&mut self.engine, &client, elapsed);
    }

    /// Ask the circuit to start or stop its one-time hot-water loading.
    fn overrun(&mut self, on: bool, now: OffsetDateTime) -> Result<(), DriverError> {
        let Some(server) = self.hvac.clone() else {
            return Err(DriverError::Unsupported(
                "this circuit has not said it can be asked to heat".into(),
            ));
        };
        // Already where it was asked to be. The arbiter decides afresh every
        // control period and this asset's decision is a boolean, so an unguarded
        // write would restate it every few seconds — and a circuit that
        // published no overrun state at all is one this driver has nothing to
        // compare against, so it writes and lets the circuit answer.
        // `overrun`, not `overrun_active`: the second is the *system function's*
        // flag for whether an overrun is overriding its mode, and the first is
        // what the overrun itself says it is doing. A circuit that has been
        // asked to load and has not started heating yet reports `active` on the
        // one and may report nothing on the other.
        //
        // `None` means the circuit has published no overrun state at all, so
        // there is nothing to compare against: the driver writes, and lets the
        // circuit answer.
        let running = self
            .function
            .overrun()
            .map(|state| !matches!(state, OverrunReport::Inactive));
        if running == Some(on) {
            return Ok(());
        }
        let data = if on {
            self.function.start_overrun()
        } else {
            self.function.stop_overrun()
        }
        .map_err(|refused| {
            DriverError::Unsupported(format!(
                "this circuit refuses a one-time loading: {refused:?}"
            ))
        })?;
        let client = self.client();
        let elapsed = self.since_start(now);
        // Partial, so the write names the overrun it addresses and leaves every
        // other entry of a list function alone.
        self.engine.write(&server, &client, data, true, elapsed);
        Ok(())
    }
}

impl Driver for DhwTank {
    fn asset(&self) -> &AssetId {
        &self.asset
    }

    fn capabilities(&self) -> DriverCapabilities {
        // Both, since CDSF: it reports how warm the tank is and can ask for it
        // to be warmer. Before that it was a meter, and a hot-water tank is a
        // **controllable** asset — so a household whose only tank driver was
        // this one was refused at start-up, and the plan moved a store nothing
        // could carry the decision to.
        DriverCapabilities::device().on_change()
    }

    fn on_bytes(&mut self, bytes: &[u8], now: OffsetDateTime) -> Result<(), DriverError> {
        let datagram: eebus::model::Datagram = serde_json::from_slice(bytes)
            .map_err(|e| DriverError::Malformed(format!("not a SPINE datagram: {e}")))?;
        let elapsed = self.since_start(now);
        self.engine.handle_datagram(&datagram, elapsed);
        self.consume(elapsed, now);
        Ok(())
    }

    fn on_link(&mut self, state: LinkState, now: OffsetDateTime) {
        // A session's facts go with the session: the circuit's own identifiers
        // are what a write is addressed by, and a reconnection may be to a
        // device that publishes different ones.
        self.hvac = None;
        self.function = cdsf::reader();
        // Everything a session owns goes with it, discovery included: a circuit
        // that reconnects may be a different device, and a description kept
        // across the gap would resolve the new one's values against the old
        // one's meaning.
        self.circuit = None;
        self.monitor = MonitoringApplianceActor::new(self.client());
        self.last = None;
        // The peer goes with them. Discovery is a *session* fact: the
        // subscription did not survive the gap, and a peer left in the engine
        // would leave this driver believing it had already found a circuit it
        // is no longer subscribed to — so the reads that make the first minute
        // useful would never go out again. Walking it again costs two messages.
        let known: Vec<_> = self
            .engine
            .peers()
            .filter_map(|peer| peer.address.clone())
            .collect();
        for peer in &known {
            self.engine.remove_peer(peer);
        }
        if self.link != state {
            self.link = state;
            self.events.push(DriverEvent::Link(state));
        }
        if state == LinkState::Up {
            let elapsed = self.since_start(now);
            self.discover(elapsed);
            self.consume(elapsed, now);
        }
    }

    fn on_timeout(&mut self, now: OffsetDateTime) {
        let elapsed = self.since_start(now);
        self.engine.handle_timeout(elapsed);
        self.consume(elapsed, now);
    }

    /// `on` starts a one-time hot-water loading; `off` stops it.
    ///
    /// Scenario 2 of CDSF, and it is the shortest path there is from "the roof
    /// is exporting" to "the tank is absorbing it": the button in the bathroom,
    /// pressed over the wire. Not a **setpoint** — that is `cdt`, which the
    /// circuit's own controller may decline to act on, and which changes nothing
    /// at all when written into an operation mode the circuit is not in.
    ///
    /// A ceiling is refused. Curtailing a tank is the § 14a envelope's business
    /// and reaches the heater some other way; answering one here by stopping a
    /// loading would turn a limit the circuit could have respected underneath
    /// into a shower nobody gets.
    fn command(
        &mut self,
        command: &hems_core::setpoint::Command,
        now: OffsetDateTime,
    ) -> Result<(), DriverError> {
        match command {
            hems_core::setpoint::Command::OnOff(on) => self.overrun(*on, now),
            other => Err(DriverError::Unsupported(format!("{other:?}"))),
        }
    }

    fn poll_event(&mut self) -> Option<DriverEvent> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }

    fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        let datagram = self.engine.poll_transmit()?;
        serde_json::to_vec(&datagram).ok()
    }

    fn poll_deadline(&self) -> Option<OffsetDateTime> {
        let soonest = self.engine.poll_timeout()?;
        time::Duration::try_from(soonest)
            .ok()
            .map(|d| self.started_at + d)
    }
}
