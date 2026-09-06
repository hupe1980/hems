//! The car, over EEBUS.
//!
//! What the planner has always been missing about a charge point is not power —
//! the arbiter has commanded amperes since the first day — but *whether there is
//! a car on the end of it, and how full it is*. `EvSession` has been in the
//! optimiser from the beginning and has never been built on a running box,
//! because nothing reported an arrival.
//!
//! # An arrival has no message
//!
//! EVCC scenario 1 is the one place in EEBUS where the absence of a payload *is*
//! the payload: an `EV` entity **appearing** underneath the `EVSE` entity is how
//! a car says it is plugged in, and scenario 8 is the entity going away again.
//! So this driver watches the peer's own entity tree rather than waiting for a
//! message that is never sent.
//!
//! That is also why it is a driver rather than a reader: the fact lives in
//! SPINE's discovery, which only a device that is *talking to* the charge point
//! has.
//!
//! # What it does not decide
//!
//! It reports an arrival, a state of charge and a capacity. It does not report a
//! **departure time** or an **energy target**, and no EEBUS use case carries
//! either: both are things the household wants rather than things the car knows.
//! They are configuration, and the planner takes them from there — which is the
//! right split, because a car that published a departure time would be
//! publishing a guess about its driver.

use core::time::Duration as StdDuration;

use eebus::model::{DeviceType, EntityType, FeatureType, Function, Role};
use eebus::spine::{Engine, LocalDevice, LocalEntity, SpineEvent};
use eebus::usecases::emobility::{evcc, evsoc};
use eebus::usecases::limitation;
use eebus::usecases::monitoring::Readings;
use hems_core::prelude::AssetId;
use time::OffsetDateTime;

use crate::{
    Driver, DriverCapabilities, DriverError, DriverEvent, LinkState, VehiclePresence,
    eebus::SpineIdentity,
};

/// The entity and feature this appliance reads from.
const ENTITY: [u32; 1] = [1];
/// One `Generic` client feature for all client functionality (LPC IG § 3.3).
const CLIENT_FEATURE: u32 = 1;

/// How often the entity tree is re-read while a charge point is reachable.
///
/// A compromise, and the two sides of it are worth naming: shorter is a plug-in
/// noticed sooner, and the plan only moves on the next re-plan anyway; longer is
/// two SPINE reads less per interval on a device that may be a wallbox with a
/// small processor. Half a minute means a car that arrives is in the next plan.
///
/// # A departure is harder to see than an arrival, and that is the protocol
///
/// SPINE keeps a peer's discovery as a **merged document** — §7.1.5 allows a
/// re-send to be partial, so every consumer would otherwise reimplement the
/// merge. The consequence is that a shorter reply cannot remove what an earlier
/// one added: an `EV` entity that has gone away is still in the merged tree, and
/// re-reading discovery does not take it out. Removing it needs a datagram with
/// `cmdClassifier: delete`, which a charge point has to *choose* to send.
///
/// So an arrival is visible on every peer and a departure only on one that
/// deletes. This driver acts on a delete where it gets one and treats a quiet
/// re-answer as no news, which is why both are tests. The **plan's** deadline is
/// the household's own departure time either way: a car that published one would
/// be publishing a guess about its driver, and a charge point that never deletes
/// would otherwise leave a session open for ever.
const DISCOVERY_PERIOD: time::Duration = time::Duration::seconds(30);

/// A charge point and whatever is plugged into it, read over EVCC and EVSOC.
#[derive(Debug)]
pub struct EvCharger {
    asset: AssetId,
    engine: Engine,
    /// The car's `Measurement` feature, once an `EV` entity has appeared.
    measurement: Option<eebus::model::FeatureAddress>,
    /// Its `ElectricalConnection` feature, which is where the nominal capacity
    /// lives — a *characteristic* rather than a measurement, because it does not
    /// change while the car is plugged in.
    electrical: Option<eebus::model::FeatureAddress>,
    /// The descriptions the measurements are resolved against.
    readings: Readings,
    /// What the car has said about its battery.
    battery: evsoc::Battery,
    /// What was last reported upwards, so an unchanged session is not re-emitted.
    last: Option<VehiclePresence>,
    /// When the entity tree is next re-read.
    ///
    /// A cable going in is an entity *appearing*, and there is no message for
    /// it: EVCC scenario 1 has no functions of its own. A charge point that
    /// notified its detailed discovery would reach a subscriber, but
    /// NodeManagement subscriptions are not something every peer offers — so
    /// this box asks, on its own cadence, which is what makes an arrival visible
    /// on hardware that says nothing.
    next_discovery: Option<OffsetDateTime>,
    started_at: OffsetDateTime,
    link: LinkState,
    events: Vec<DriverEvent>,
}

impl EvCharger {
    /// A Monitoring Appliance reading the charge point on `asset`.
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
        let mut engine = Engine::new(device);
        // Scenario 1 and 8 are announced with the rest even though they carry no
        // functions: `useCaseScenarioSupport` is what a peer plans against, and a
        // manager that did not claim "EV connected" is one a car has no reason to
        // tell about itself.
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &evcc::CEM);
        engine.add_use_case(ENTITY, CLIENT_FEATURE, &evsoc::MONITORING_APPLIANCE);
        Ok(Self {
            asset,
            engine,
            measurement: None,
            electrical: None,
            readings: Readings::new(),
            battery: evsoc::Battery::new(),
            last: None,
            next_discovery: None,
            started_at,
            link: LinkState::Down,
            events: Vec::new(),
        })
    }

    fn since_start(&self, now: OffsetDateTime) -> StdDuration {
        StdDuration::try_from(now - self.started_at).unwrap_or(StdDuration::ZERO)
    }

    fn client(&self) -> eebus::model::FeatureAddress {
        self.engine.device().address_of(&ENTITY, CLIENT_FEATURE)
    }

    fn discover(&mut self, elapsed: StdDuration) {
        self.next_discovery = Some(self.started_at + elapsed + DISCOVERY_PERIOD);
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
            // The entity tree *is* the message. Re-read on every update rather
            // than once, because that is exactly what changes when a cable goes
            // in and when it comes out again.
            SpineEvent::DiscoveryUpdated { device } | SpineEvent::UseCasesUpdated { device } => {
                let found = self.engine.peer(device).and_then(ev_features);
                match (found, self.measurement.is_some()) {
                    (Some((measurement, electrical)), false) => {
                        self.measurement = Some(measurement.clone());
                        self.electrical = Some(electrical.clone());
                        let client = self.client();
                        for function in [
                            Function::MeasurementDescriptionListData,
                            Function::MeasurementConstraintsListData,
                            Function::MeasurementListData,
                        ] {
                            let _ = self.engine.read(&measurement, &client, function, elapsed);
                        }
                        let _ = self.engine.read(
                            &electrical,
                            &client,
                            Function::ElectricalConnectionCharacteristicListData,
                            elapsed,
                        );
                        self.engine
                            .request_subscription(&client, &measurement, elapsed);
                        self.report(true, now);
                    }
                    // Scenario 8, where it can be seen at all: the entity is
                    // gone, so the car is. See the note on `DISCOVERY_PERIOD`
                    // for why "where it can be seen" is doing work there.
                    (None, true) => {
                        self.forget_car();
                        self.report(false, now);
                    }
                    // An empty socket, said out loud. A charge point with
                    // nothing plugged into it is working perfectly, and the
                    // planner needs that told to it rather than inferred from a
                    // measurement that never comes.
                    (None, false) => self.report(false, now),
                    (Some(_), true) => {}
                }
            }
            SpineEvent::DataNotified {
                feature, resolved, ..
            }
            | SpineEvent::ReplyReceived {
                feature, resolved, ..
            } => {
                let ours = self.measurement.as_ref() == Some(feature)
                    || self.electrical.as_ref() == Some(feature);
                if !ours {
                    return;
                }
                // The descriptions are what turn a `measurementId` back into a
                // state of charge rather than a state of health or a travel
                // range in metres — three unphased percentages and a distance on
                // one feature, and only the description tells them apart.
                if self.readings.describe(resolved) {
                    return;
                }
                if self.battery.apply(resolved, &self.readings) {
                    self.report(true, now);
                }
            }
            _ => {}
        }
    }

    /// Everything that was true of the car that has just left.
    fn forget_car(&mut self) {
        self.measurement = None;
        self.electrical = None;
        self.readings = Readings::new();
        self.battery = evsoc::Battery::new();
    }

    /// Say what the session is, where it has changed.
    fn report(&mut self, connected: bool, at: OffsetDateTime) {
        let presence = VehiclePresence {
            connected,
            // A percentage on the wire, a fraction here — `Soc` is `0..=1`
            // everywhere in this workspace, and a 0,85 that should have been 85
            // is a car the plan believes is nearly empty.
            soc: connected
                .then_some(self.battery.state_of_charge.map(|pct| pct / 100.0))
                .flatten(),
            capacity_wh: connected.then_some(self.battery.nominal_capacity).flatten(),
            at,
        };
        // Compared without the timestamp: a notification that repeats what is
        // already known is not news, and the registry is edge-driven.
        let unchanged = self.last.is_some_and(|last| {
            last.connected == presence.connected
                && last.soc == presence.soc
                && last.capacity_wh == presence.capacity_wh
        });
        if unchanged {
            return;
        }
        self.last = Some(presence);
        self.events.push(DriverEvent::Vehicle(presence));
    }
}

/// The car's two features, where a peer has an `EV` entity with them.
///
/// EVCC scenario 1 has no payload: the entity appearing *is* the message. So the
/// question this answers is "is there an `EV` entity on this peer", and the
/// features come with it.
fn ev_features(
    remote: &eebus::spine::RemoteDevice,
) -> Option<(eebus::model::FeatureAddress, eebus::model::FeatureAddress)> {
    remote.entities.iter().find_map(|entity| {
        if entity.entity_type != Some(EntityType::EV) {
            return None;
        }
        let measurement = entity.feature(&FeatureType::Measurement, Role::Server)?;
        let electrical = entity.feature(&FeatureType::ElectricalConnection, Role::Server)?;
        Some((measurement.address.clone(), electrical.address.clone()))
    })
}

impl Driver for EvCharger {
    fn asset(&self) -> &AssetId {
        &self.asset
    }

    fn capabilities(&self) -> DriverCapabilities {
        // Neither, and both halves are deliberate. It does not **command**: the
        // charge point is already commanded, over Modbus or by the § 14a
        // envelope the arbiter shares out, and two drivers claiming one
        // contactor is one wallbox nobody can predict.
        //
        // And it does not **measure**, which it used to claim. What it reports
        // is a session fact — a car is on the cable, and this full — never a
        // quantity; the wallbox's own driver reports the watts. `measures` is
        // what silence is judged by, so claiming it had this driver counted as a
        // device nobody could hear for the whole of every day the car was away.
        DriverCapabilities::observing()
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
        self.forget_car();
        // The peer goes too. Discovery is a session fact, and a charge point
        // that reconnects may have a different car on it — or none.
        let known: Vec<_> = self
            .engine
            .peers()
            .filter_map(|peer| peer.address.clone())
            .collect();
        for peer in &known {
            self.engine.remove_peer(peer);
        }
        // A link that has gone is not a car that has gone, and the difference
        // matters to a plan: an unplugged car is a charging session that ended,
        // and an unreachable charge point is one nobody can see. Reported as a
        // link change so the registry ages the asset out, and *not* as a
        // departure — inventing one would end a session the household is still
        // in the middle of. `last` is kept for the same reason: what the box
        // knew about the session is still the best thing it knows until
        // discovery says otherwise.
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
        if self.link == LinkState::Up && self.next_discovery.is_none_or(|due| now >= due) {
            self.discover(elapsed);
        }
        self.consume(elapsed, now);
    }

    fn command(
        &mut self,
        command: &hems_core::setpoint::Command,
        _: OffsetDateTime,
    ) -> Result<(), DriverError> {
        Err(DriverError::Unsupported(format!("{command:?}")))
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
        // Whichever comes first: the engine's own timers, or the next look at
        // the entity tree. Never `None` while the link is up, because a driver
        // that offered no deadline would be woken only by bytes — and a cable
        // going in sends none.
        let engine = self
            .engine
            .poll_timeout()
            .and_then(|d| time::Duration::try_from(d).ok())
            .map(|d| self.started_at + d);
        match (engine, self.next_discovery) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (only, None) | (None, only) => only,
        }
    }
}
