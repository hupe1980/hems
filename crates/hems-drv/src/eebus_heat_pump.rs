//! The heat pump, over EEBUS: the compressor it can run, and the room it heats.
//!
//! Two use cases on one session, because they are two halves of one decision and
//! a household has one heat pump. **OHPCF** is the lever — the one use case in
//! the whole set that can ask an appliance to consume *more*. **MRT** is the
//! state that lever is aimed at: the air temperature of the space, which is what
//! the planner's thermal model is integrated from and what identifies which
//! house this is.
//!
//! They are in one driver rather than two because SHIP has one session per peer
//! pair. Two drivers dialling the same unit would be two connections to a device
//! that grants one, and the second would spend the day being refused.
//!
//! # The lever
//!
//! Everything else on the grid side asks a device to do less: LPC a consumption
//! ceiling, § 9 EEG a production one, `opev` a current not to exceed. A ceiling
//! is a bound, and a bound an appliance is already under changes nothing. So a
//! planner that has worked out the building will be cheaper if the compressor
//! runs **now**, while the roof is exporting, has had no way to say so — and
//! pre-heating into a cheap hour is the whole reason a thermal model is in the
//! optimiser at all.
//!
//! OHPCF is that lever. The compressor announces that it *could* run, what it
//! would draw, how long it must run once started and how long it must then rest;
//! the CEM schedules it, and may stop, pause or resume it afterwards.
//!
//! # It is not a setpoint, and the difference decides which one to use
//!
//! [`hvac::cdt`](eebus::usecases::hvac::cdt) raises a hot-water **setpoint** and
//! leaves the circuit's own controller to decide what to do about it — so a tank
//! already at temperature does nothing. This **starts a process** at a time the
//! CEM names. A household that wants the compressor to run on today's surplus
//! wants this one.
//!
//! # What the driver reports, and what it refuses
//!
//! An offer is a *capability*, not a measurement, so it goes up as a
//! [`DriverEvent::Flexibility`] rather than as a reading. Two things in it are
//! load-bearing for a planner that already has minimum-runtime constraints:
//! `active_duration_min` is how long the compressor must run once started, and
//! `pause_duration_min` how long it must then rest — the same two numbers
//! `CompressorState` carries, arriving from the machine rather than from a
//! configuration file.
//!
//! # The room, and what is done with several of them
//!
//! MRT reports the air temperature of an **HVAC Room**, which the specification
//! defines as "a logical or physical indoor space" — one room, or a whole floor.
//! A device that monitors four announces the use case four times, and this
//! driver reports the **mean** of them.
//!
//! The mean rather than one of them, because the model it feeds has a single air
//! node: `Rc2` describes a dwelling, and picking the first room would have the
//! planner heat the house to keep one bedroom in band. It is unweighted, because
//! nothing on the wire carries a room's volume — an approximation, and the
//! honest one available.
//!
//! What the driver does **not** report is power. What the unit draws is the site
//! meter's business, and the registry allows one measuring driver per asset.
//!
//! A sequence whose `sequenceRemoteControllable` is false is **not** reported as
//! flexibility. It is a compressor describing itself to a manager that may not
//! drive it, and a plan built on one would schedule a start that is refused every
//! time.

use core::time::Duration as StdDuration;

use eebus::model::PowerSequenceId;
use eebus::model::{DeviceType, EntityType, Function};
use eebus::spine::{Engine, LocalDevice, LocalEntity, SpineEvent};
use eebus::usecases::hvac::{mot, mrt};
use eebus::usecases::limitation;
use eebus::usecases::monitoring::{MonitoringApplianceActor, MonitoringEvent, UnitId};
use eebus::usecases::ohpcf::{self, CompressorOffer};
use hems_core::prelude::{AssetId, Measurement, Power};
use hems_core::setpoint::Command;
use time::OffsetDateTime;

use crate::{
    Driver, DriverCapabilities, DriverError, DriverEvent, Flexibility, LinkState,
    eebus::SpineIdentity,
};

/// The entity and feature this appliance drives from.
const ENTITY: [u32; 1] = [1];
/// One `Generic` client feature for all client functionality, as the LPC
/// implementation guide § 3.3 asks.
const CLIENT_FEATURE: u32 = 1;

/// "Now", as OHPCF spells it: no delay at all.
///
/// A **span**, and not because a relative time is the safer of two choices —
/// because it is the only one. `SmartEnergyManagementPs` restricts
/// `schedule.startTime` to `xs:duration` by an `xs:restriction` on the
/// `PowerSequences` type, so a wall-clock instant is not expressible there and a
/// strict peer refuses one. Which is just as well: a compressor with no clock of
/// its own could not have acted on it.
const START_NOW: StdDuration = StdDuration::ZERO;

/// A heat pump: its compressor over OHPCF, and its rooms over MRT.
#[derive(Debug)]
pub struct HeatPump {
    asset: AssetId,
    engine: Engine,
    /// The rooms this unit reports, read the same way the § 14a driver reads a
    /// Grid Connection Point: the actor does the exchange and resolves every
    /// notification against the descriptions that give it meaning.
    monitor: MonitoringApplianceActor,
    /// The outdoor sensor this unit announced, if it has one.
    ///
    /// A heat pump nearly always does — its defrost logic runs on it — and the
    /// reading is worth having even though the planner has a forecast: a
    /// forecast is for a grid square, and this is the wall of *this* building.
    /// The **fit** wants the thermometer; only the plan needs the forecast.
    outdoors: Option<UnitId>,
    /// The rooms this unit announced, in discovery order, each with whatever it
    /// last said.
    ///
    /// One collection rather than a list of rooms beside a map of temperatures:
    /// two that must agree about which rooms exist are two that can disagree,
    /// and `UnitId` is not `Ord` anyway. A household has a handful of rooms, so
    /// the linear scan is the cheaper half of the trade.
    rooms: Vec<(UnitId, Option<f64>)>,
    /// The peer's `SmartEnergyManagementPs` feature, once discovery names one.
    flexibility: Option<eebus::model::FeatureAddress>,
    /// The sequence every write addresses, from the offer that announced it.
    ///
    /// Kept rather than assumed: [`ohpcf::SEQUENCE_ID`] is what a compressor
    /// usually publishes and nothing obliges it to, and a write against the
    /// wrong sequence is acknowledged and does nothing.
    sequence: Option<PowerSequenceId>,
    /// The last offer reported upwards, so an unchanged one is not re-emitted.
    last: Option<Flexibility>,
    started_at: OffsetDateTime,
    link: LinkState,
    events: Vec<DriverEvent>,
}

impl HeatPump {
    /// A CEM driving the compressor on `asset`.
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
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &ohpcf::CEM);
        // The same one `Generic` client feature carries both: the LPC
        // implementation guide §3.3 asks an actor to use one for all its client
        // functionality rather than mirroring each server feature it reads.
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &mrt::MONITORING_APPLIANCE);
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &mot::MONITORING_APPLIANCE);
        Ok(Self {
            asset,
            engine,
            monitor: MonitoringApplianceActor::new(client),
            outdoors: None,
            rooms: Vec::new(),
            flexibility: None,
            sequence: None,
            last: None,
            started_at,
            link: LinkState::Down,
            events: Vec::new(),
        })
    }

    /// Whether the compressor is in the middle of a process, as far as the last
    /// thing it said goes.
    fn running(&self) -> bool {
        self.last.as_ref().is_some_and(|offer| offer.running)
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

    fn consume(&mut self, elapsed: StdDuration, now: OffsetDateTime) {
        while let Some(event) = self.engine.poll_event() {
            self.follow(&event, elapsed, now);
        }
    }

    fn follow(&mut self, event: &SpineEvent, elapsed: StdDuration, now: OffsetDateTime) {
        match event {
            SpineEvent::UseCasesUpdated { device } | SpineEvent::DiscoveryUpdated { device } => {
                self.follow_rooms(device, elapsed);
                if self.flexibility.is_some() {
                    return;
                }
                let Some(remote) = self.engine.peer(device) else {
                    return;
                };
                // The compressor is a sub-entity of the heat-pump appliance, so
                // its feature is on the entity that announced the actor rather
                // than on the appliance above it. `locate` is what knows that.
                let Some(peer) = ohpcf::locate(remote) else {
                    return;
                };
                self.flexibility = Some(peer.flexibility.clone());
                let client = self.client();
                // Bind, subscribe and read, in the order §3.4.2 puts them, and
                // in one call since `eebus` 0.7. The binding is the step that is
                // easy to leave out and the expensive one to leave out: a
                // subscription buys the right to be *told*, and only a binding
                // buys the right to **write** — without it the compressor
                // answers every start `BindingRequired`, which is an offer this
                // driver can see, report and never take up.
                let _ = peer.follow(&mut self.engine, &client, elapsed);
            }
            SpineEvent::DataNotified { feature, .. }
            | SpineEvent::ReplyReceived { feature, .. }
                if self.flexibility.as_ref() != Some(feature) =>
            {
                // Not the compressor's feature, so it is a room's — or nothing
                // this driver asked for, which the actor ignores.
                self.watch_rooms(event, now);
            }
            // `resolved`, never `data`: a partial notification carrying only the
            // new state has no power value and no interrupt flags in it, and
            // reading that as a whole offer is a compressor that has just
            // withdrawn everything it said.
            SpineEvent::DataNotified {
                feature, resolved, ..
            }
            | SpineEvent::ReplyReceived {
                feature, resolved, ..
            } => {
                if self.flexibility.as_ref() != Some(feature) {
                    return;
                }
                self.offered(resolved, now);
            }
            _ => {}
        }
    }

    /// Attach every room this peer monitors, once.
    ///
    /// Retried on every discovery update until it finds something, because a
    /// peer may gain the use case later — a unit whose firmware is updated, or
    /// one whose room sensor is paired after the box was installed.
    fn follow_rooms(&mut self, device: &eebus::model::AddressDevice, elapsed: StdDuration) {
        // Both lookups first, then both attaches: `peer` borrows the engine and
        // `attach` needs it back.
        let Some(remote) = self.engine.peer(device) else {
            return;
        };
        // `locate_all`, not `locate`: §7.5 lets a device announce `HVACRoom`
        // once per entity, and a building is rarely one room.
        let rooms = if self.rooms.is_empty() {
            mrt::locate_all(remote)
        } else {
            Vec::new()
        };
        // `locate`, not `locate_all`: a building has one outside, and a unit
        // announcing two sensors would be describing two readings of the same
        // air with no way to say which is in the shade.
        let outdoors = if self.outdoors.is_none() {
            mot::locate(remote)
        } else {
            None
        };
        for room in rooms {
            self.rooms.push((room.id(), None));
            self.monitor.attach(&mut self.engine, room, elapsed);
        }
        if let Some(sensor) = outdoors {
            self.outdoors = Some(sensor.id());
            self.monitor.attach(&mut self.engine, sensor, elapsed);
        }
    }

    /// Fold a room's reading in and report the house's air temperature.
    fn watch_rooms(&mut self, event: &SpineEvent, now: OffsetDateTime) {
        let Some(MonitoringEvent::Measured { unit, .. }) = self.monitor.handle_event(event) else {
            return;
        };
        if self.outdoors.as_ref() == Some(&unit) {
            if let Some((degrees, stamp)) = self
                .monitor
                .readings(&unit)
                .and_then(|readings| readings.read_at(&mot::MEASURAND))
            {
                let mut measurement = Measurement::at(crate::eebus::taken_at(stamp, now));
                measurement.outdoor_c = Some(degrees);
                self.events.push(DriverEvent::Measured(measurement));
            }
            return;
        }
        // The rooms are averaged, so the measurement carries the instant the
        // **latest** of them was taken rather than one room's: the number is a
        // house, and a house is only as current as its freshest reading.
        let Some((degrees, stamp)) = self
            .monitor
            .readings(&unit)
            .and_then(|readings| readings.read_at(&mrt::MEASURAND))
        else {
            return;
        };
        let at = crate::eebus::taken_at(stamp, now);
        if let Some(room) = self.rooms.iter_mut().find(|(id, _)| *id == unit) {
            room.1 = Some(degrees);
        }
        // The mean over the rooms that have **spoken**, not over the ones that
        // exist: a four-room unit whose second sensor has not reported yet has
        // three readings, and averaging in a zero for the fourth would have the
        // planner heat a house it thinks is freezing.
        let heard: Vec<f64> = self.rooms.iter().filter_map(|(_, d)| *d).collect();
        if heard.is_empty() {
            return;
        }
        #[allow(clippy::cast_precision_loss)]
        let mean = heard.iter().sum::<f64>() / heard.len() as f64;
        let mut measurement = Measurement::at(at);
        measurement.temperature_c = Some(mean);
        self.events.push(DriverEvent::Measured(measurement));
    }

    /// Read one payload and report what changed.
    fn offered(&mut self, data: &eebus::model::CmdData, now: OffsetDateTime) {
        // Phase D — "there is no process" — is a *fact* rather than a parse
        // failure, and it is the one that has to reach the planner: a compressor
        // with nothing to offer is one no plan may schedule.
        if ohpcf::is_absent(data) {
            self.sequence = None;
            self.report(None, now);
            return;
        }
        let Some(offer) = CompressorOffer::read(data) else {
            return;
        };
        // A sequence the peer says it will not take instructions on is not
        // flexibility. Reporting it would have the planner schedule a start that
        // is refused every time, and blame the household's heat pump for it.
        if !offer.remote_controllable {
            self.sequence = None;
            self.report(None, now);
            return;
        }
        self.sequence = Some(offer.sequence);
        self.report(
            Some(Flexibility {
                asset: self.asset.clone(),
                power: offer.power_watts.map(Power::new),
                min_run: offer.active_duration_min.and_then(as_time),
                min_rest: offer.pause_duration_min.and_then(as_time),
                running: matches!(
                    offer.state,
                    eebus::model::PowerSequenceState::Running
                        | eebus::model::PowerSequenceState::Paused
                ),
                stoppable: offer.is_stoppable,
                pausable: offer.is_pausable,
                at: now,
            }),
            now,
        );
    }

    /// Emit an offer, or its absence, where it is news.
    fn report(&mut self, offer: Option<Flexibility>, _now: OffsetDateTime) {
        // Compared without the timestamp: an offer restated unchanged is not
        // news, and the registry is edge-driven.
        let same = match (&self.last, &offer) {
            (Some(a), Some(b)) => a.same_as(b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.last.clone_from(&offer);
        if let Some(offer) = offer {
            self.events.push(DriverEvent::Flexibility(offer));
        }
    }

    /// Write one of OHPCF's four commands against the announced sequence.
    fn instruct(
        &mut self,
        data: eebus::model::CmdData,
        now: OffsetDateTime,
    ) -> Result<(), DriverError> {
        let (Some(server), Some(_)) = (self.flexibility.clone(), self.sequence) else {
            return Err(DriverError::Unsupported(
                "this compressor has not offered a controllable sequence".into(),
            ));
        };
        let client = self.client();
        let elapsed = self.since_start(now);
        // Partial: Table 10 makes partial support mandatory for exactly this, so
        // everything the compressor announced stays as it announced it and only
        // the instruction arrives. A full write would restate the whole offer
        // back at the device as though the CEM owned it.
        self.engine.write(&server, &client, data, true, elapsed);
        Ok(())
    }
}

/// An OHPCF duration, as `time` counts one.
fn as_time(duration: StdDuration) -> Option<time::Duration> {
    time::Duration::try_from(duration).ok()
}

impl Driver for HeatPump {
    fn asset(&self) -> &AssetId {
        &self.asset
    }

    fn capabilities(&self) -> DriverCapabilities {
        // Both, since MRT: it drives the compressor and reports the air
        // temperature of the rooms. What it still does not report is *power* —
        // what the unit draws is the site meter's business.
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
        // A session's facts go with the session. An offer kept across the gap
        // would have the planner schedule a process on a device that may have
        // been replaced, against a sequence id it no longer publishes.
        self.flexibility = None;
        self.sequence = None;
        self.last = None;
        // A temperature whose session has gone is not a temperature. Kept across
        // the gap it would let the planner integrate a thermal model from a
        // reading taken before a device was swapped.
        self.rooms.clear();
        self.outdoors = None;
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

    /// `on` starts the announced process; `off` aborts it.
    ///
    /// A ceiling is deliberately **not** accepted. LPC is where a heat pump is
    /// told to use less, and a driver that answered a `ConsumptionCeiling` by
    /// aborting the compressor's process would turn a limit the unit could have
    /// respected underneath into a stop it did not ask for.
    fn command(&mut self, command: &Command, now: OffsetDateTime) -> Result<(), DriverError> {
        let sequence = self.sequence;
        match command {
            Command::OnOff(true) => {
                let Some(sequence) = sequence else {
                    return Err(DriverError::Unsupported(
                        "this compressor has not offered a controllable sequence".into(),
                    ));
                };
                // Already under way, so there is nothing to start. The arbiter
                // decides afresh every control period and this asset's decision
                // is a boolean, so an unguarded write here would re-schedule the
                // running process every few seconds — and [OHPCF-013] permits
                // one process at a time, which makes the second one an error the
                // compressor is right to raise.
                if self.running() {
                    return Ok(());
                }
                self.instruct(ohpcf::activate(sequence, START_NOW), now)
            }
            Command::OnOff(false) => {
                let Some(sequence) = sequence else {
                    return Err(DriverError::Unsupported(
                        "this compressor has not offered a controllable sequence".into(),
                    ));
                };
                // Only where the compressor said the CEM may, and this is
                // checked before the idle case below rather than after it. A
                // stop written to a process that is not stoppable is
                // acknowledged and ignored, which reads to every layer above as
                // a heat pump that was turned off and did not stop — and that is
                // a fact about the *device*, so a household finds it out now
                // rather than on the one afternoon the process is running.
                if !self.last.as_ref().is_some_and(|offer| offer.stoppable) {
                    return Err(DriverError::Unsupported(
                        "this compressor's process cannot be stopped once started".into(),
                    ));
                }
                // Nothing under way, so there is nothing to stop.
                if !self.running() {
                    return Ok(());
                }
                self.instruct(ohpcf::stop(sequence), now)
            }
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
