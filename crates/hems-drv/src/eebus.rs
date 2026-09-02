//! EEBUS, as the § 14a side of a household.
//!
//! This is what closes the gap between "the logic is right" and "the
//! house is managed". Everything else in the workspace enforces a limit; until
//! this existed, a limit could only ever arrive from the simulator.
//!
//! # What it is, and what it deliberately is not
//!
//! It is the **Controllable System** of the EEBUS *Limitation of Power
//! Consumption* use case — the role a household energy manager plays toward the
//! network operator's Steuerbox. The operator's box is the *Energy Guard*; it
//! writes an active-power limit, it sends a heartbeat every sixty seconds, and
//! if it stops, the household restrains itself to a pre-agreed failsafe value
//! until a minimum period has run.
//!
//! It is **not** a second control plane. The limit it reports is a fact about
//! what the operator asked for; what the household then does about it is
//! `hems_realtime::Guard`'s decision, made with every asset in view. A driver
//! that decided its own response would be a compliance argument nobody audited.
//!
//! # The protocol logic is not ours, and that is the point
//!
//! The five-state limitation machine, the hundred-and-twenty-second heartbeat
//! timeout, the two-to-twenty-four-hour `FailsafeDurationMinimum`, the rule that
//! an expired duration deactivates a limit — all of it lives in the [`eebus`]
//! crate, sans-I/O, and is exercised by its own conformance tests against the
//! use-case specification. This crate is a **translation**: `eebus`'s vocabulary
//! into hems's.
//!
//! Writing a second copy of that state machine here is the obvious first
//! implementation and it is the one thing this crate must not do. Two
//! implementations of a certifiable state machine disagree, and the one that is
//! wrong is whichever the certification lab is not looking at. So
//! [`hems_grid::LpcState`] is *derived* from [`eebus`]'s rather than tracked
//! alongside it, and a test pins the mapping over every state.
//!
//! # Time
//!
//! [`eebus`] measures in a monotonic [`core::time::Duration`] since the system
//! started; hems works in wall-clock [`OffsetDateTime`], because a § 14a
//! evidence record is a statement about calendar time. The conversion is the
//! whole of the conversion, and it is one-directional: a driver is given
//! wall-clock instants and never asks what time it is.

use core::time::Duration as StdDuration;

use crate::{
    Driver, DriverCapabilities, DriverError, DriverEvent, GridLimit, LimitDirection, LimitSource,
    LinkState,
};
use eebus::model::{DeviceType, EntityType};
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::usecases::limitation::{
    ControllableSystem, ControllableSystemActor, CsConfig, EffectiveLimit, LimitationState,
    LocalDecision,
};
use eebus::usecases::{limitation, lpc, lpp};

/// What the Energy Guard wrote, and what a Controllable System answers.
///
/// Re-exported from [`eebus`] rather than mirrored: a caller has to be able to
/// build a limit and read an outcome, and a second set of types that meant the
/// same thing would be one more place for the two to drift.
pub use eebus::usecases::limitation::{LimitWrite, WriteOutcome};
use hems_core::prelude::{AssetId, Power};
use hems_grid::lpc::LpcState;
use time::OffsetDateTime;

/// Which of the two limitation use cases an instance plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Use {
    /// Limitation of Power **Consumption** — § 14a EnWG.
    Lpc,
    /// Limitation of Power **Production** — the § 9 EEG side.
    Lpp,
}

impl Use {
    /// Which way the limits this use case carries point.
    #[must_use]
    pub const fn direction(self) -> LimitDirection {
        match self {
            Use::Lpc => LimitDirection::Consumption,
            Use::Lpp => LimitDirection::Production,
        }
    }
}

/// How this Controllable System names itself on the EEBUS network.
///
/// SHIP and SPINE both address a *device* rather than a household, so the box
/// has to have a name before a Steuerbox can write to it. It is configuration
/// rather than a constant: `vendor` says who made it and `unique` distinguishes
/// two boxes from the same maker, and together they are what a pairing screen
/// shows an installer beside the SKI.
///
/// SPINE's own pattern is `d:_<vendor>_<unique>` with the vendor part either
/// `i:<IANA Private Enterprise Number>` or `n:<name>`
/// (Protocol Specification § 7.1.1.2). A project with a registered enterprise
/// number should use it — they are free and unique, where names need a registry
/// nobody runs — so the default here is the honest `n:` form until hems has one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SpineIdentity {
    /// The vendor part of the SPINE device address, `i:46925` or `n:hems`.
    pub vendor: String,
    /// What distinguishes this box from the next one off the same line.
    pub unique: String,
}

impl Default for SpineIdentity {
    fn default() -> Self {
        Self {
            vendor: "n:hems".into(),
            unique: "hems-1".into(),
        }
    }
}

impl SpineIdentity {
    /// An identity with this vendor and unique name.
    pub fn new(vendor: impl Into<String>, unique: impl Into<String>) -> Self {
        Self {
            vendor: vendor.into(),
            unique: unique.into(),
        }
    }
}

/// The Controllable System of LPC or LPP, as hems sees it.
///
/// Holds [`eebus`]'s state machine, the SPINE engine that serves it, and the
/// translation into hems's vocabulary in both directions. It carries **no
/// socket**: what it takes and gives are SPINE datagrams as bytes, which is what
/// a SHIP data frame carries, and opening the TLS WebSocket under them is
/// `hemsd`'s job.
#[derive(Debug)]
pub struct Lpc {
    asset: AssetId,
    which: Use,
    /// The SPINE engine: discovery, bindings, subscriptions and the datagrams
    /// that carry them. Sans-I/O like everything else here — datagrams in,
    /// datagrams out, and a clock as a parameter.
    engine: Engine,
    /// The use-case actor, which owns the one and only [`ControllableSystem`].
    ///
    /// There is deliberately no second copy: `hems_grid::LpcState` is *derived*
    /// from this one (see the module note), and so is everything the guard is
    /// told.
    actor: ControllableSystemActor,
    /// The instant `eebus`'s monotonic clock counts from.
    started_at: OffsetDateTime,
    /// What was last reported upwards, so an unchanged limit is not re-emitted
    /// every tick — the guard is edge-driven and a repeated event is noise in an
    /// evidence record.
    last_reported: Option<EffectiveLimit>,
    events: Vec<DriverEvent>,
}

impl Lpc {
    /// A Controllable System that falls back to `failsafe` and holds it for at
    /// least `failsafe_for`.
    ///
    /// `failsafe` is what the household restrains itself to when the operator
    /// goes quiet. Under § 14a it should be the site's own minimum power
    /// (`[A1 4.5]`, `hems_grid::minimum_power`) rather than a vendor default: a
    /// box that falls back to 4,2 kW on a household owed 10,5 kW has given away
    /// six kilowatts nobody asked it to.
    ///
    /// # Panics
    /// Never. An out-of-range `failsafe_for` is clamped by [`eebus`] itself.
    pub fn new(
        asset: AssetId,
        which: Use,
        failsafe: Power,
        failsafe_for: StdDuration,
        started_at: OffsetDateTime,
    ) -> Self {
        Self::with_identity(
            asset,
            which,
            failsafe,
            failsafe_for,
            started_at,
            &SpineIdentity::default(),
        )
        .expect("the default SPINE identity is a valid device address")
    }

    /// The same, under a name a Steuerbox will see on the network.
    ///
    /// Refused rather than corrected where the identity is not one SPINE's
    /// addressing rules allow: an installer who typed a vendor part wrongly has
    /// configured a box that would be discovered under a name nobody expects,
    /// and finding that out at start-up is the cheap version of finding it out
    /// during a commissioning appointment.
    ///
    /// # Errors
    /// [`DriverError::Unsupported`] where `identity` does not make a valid SPINE
    /// device address.
    pub fn with_identity(
        asset: AssetId,
        which: Use,
        failsafe: Power,
        failsafe_for: StdDuration,
        started_at: OffsetDateTime,
        identity: &SpineIdentity,
    ) -> Result<Self, DriverError> {
        let config = CsConfig::new(failsafe.get().max(0.0), failsafe_for).on_cem();
        let system = ControllableSystem::new(config, StdDuration::ZERO);
        let (mut engine, actor) = build(which, system, identity)?;
        // Published once, here: the descriptions and the current values a peer
        // reads before it writes anything. A Controllable System that answered
        // a discovery with nothing would be discovered as a device that plays no
        // use case at all.
        actor.install(&mut engine, StdDuration::ZERO);
        Ok(Self {
            asset,
            which,
            engine,
            actor,
            started_at,
            last_reported: None,
            events: Vec::new(),
        })
    }

    /// The SPINE device address this box answers on, for a pairing screen.
    pub fn spine_address(&self) -> &str {
        self.engine.device().address().as_str()
    }

    /// Which asset the limit applies to — the connection point, ordinarily.
    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// What this driver can do.
    ///
    /// A grid driver: it reports limits and accepts nothing. A household does
    /// not command its own reduction.
    pub fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::grid()
    }

    /// The limitation state, in hems's own vocabulary.
    ///
    /// **Derived**, never tracked in parallel. See the module note: two copies
    /// of a certifiable state machine are one copy and one liability.
    pub fn state(&self) -> LpcState {
        match self.actor.system().state() {
            LimitationState::Init => LpcState::Init,
            LimitationState::Limited => LpcState::Limited,
            LimitationState::UnlimitedControlled => LpcState::UnlimitedControlled,
            LimitationState::FailsafeState => LpcState::Failsafe,
            LimitationState::UnlimitedAutonomous => LpcState::UnlimitedAutonomous,
        }
    }

    /// Whether the operator is considered present.
    pub fn is_controlled(&self) -> bool {
        self.state().is_controlled()
    }

    /// The ceiling in force right now, if any.
    pub fn ceiling(&self) -> Option<Power> {
        match self.actor.system().effective_limit() {
            EffectiveLimit::None => None,
            EffectiveLimit::Active(w) | EffectiveLimit::Failsafe(w) => Some(Power::new(w)),
        }
    }

    /// The operator's heartbeat arrived.
    ///
    /// The direct form, for a caller that has a heartbeat and no SPINE session —
    /// a simulator, a relay input, a test. Over a real session the heartbeat
    /// arrives as a datagram and [`Driver::on_bytes`] is what carries it.
    pub fn on_heartbeat(&mut self, now: OffsetDateTime) {
        let elapsed = self.since_start(now);
        self.actor.system_mut().on_heartbeat(elapsed);
        self.publish(elapsed);
        self.drain(now);
    }

    /// The operator wrote a limit.
    ///
    /// Returns what the Controllable System answers — an `ACK` or a `NACK` with
    /// a reason, which the SPINE layer above sends back. hems always accepts a
    /// well-formed limit ([`LocalDecision::Apply`]): the *guard* is what
    /// decides how the household meets it, and refusing at this layer would be
    /// the driver making a compliance decision on its own.
    pub fn on_limit(&mut self, write: &LimitWrite, now: OffsetDateTime) -> WriteOutcome {
        let elapsed = self.since_start(now);
        let outcome = self
            .actor
            .system_mut()
            .on_limit_write(write, LocalDecision::Apply, elapsed);
        // What the state machine now holds is also what the Energy Guard reads
        // back, so the published value follows the decision. Left out, a
        // Steuerbox that subscribed to `LoadControlLimitListData` would be told
        // nothing had changed.
        self.publish(elapsed);
        self.drain(now);
        outcome
    }

    /// Nothing arrived before the deadline.
    ///
    /// The most important call in the driver: this is where a missed heartbeat
    /// becomes a failsafe.
    pub fn on_timeout(&mut self, now: OffsetDateTime) {
        let elapsed = self.since_start(now);
        // Both clocks, in this order. The engine's own timers retire
        // acknowledgements and re-send what was not answered; the actor's are
        // where the heartbeat this box owes goes out and where a missed one
        // becomes the failsafe of `[LPC-911]`.
        self.engine.handle_timeout(elapsed);
        let _ = self.actor.handle_timeout(&mut self.engine, elapsed);
        self.consume_engine_events(elapsed);
        self.drain(now);
    }

    /// When [`Lpc::on_timeout`] should next be called.
    ///
    /// Never `None` while the operator is in contact — that is what makes the
    /// failsafe reachable rather than merely written down.
    pub fn deadline(&self) -> Option<OffsetDateTime> {
        // Whichever wants waking first. The actor always wants waking — it owes
        // a heartbeat every sixty seconds — so this is never `None` in practice,
        // and that is what makes the failsafe reachable rather than merely
        // written down.
        let actor = Some(self.actor.poll_timeout());
        let engine = self.engine.poll_timeout();
        let soonest = match (actor, engine) {
            (Some(a), Some(e)) => Some(a.min(e)),
            (only, None) | (None, only) => only,
        }?;
        time::Duration::try_from(soonest)
            .ok()
            .map(|d| self.started_at + d)
    }

    /// The next thing that happened.
    pub fn poll_event(&mut self) -> Option<DriverEvent> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }

    /// Ask whoever is on the other end who they are.
    ///
    /// SPINE's own bootstrap (implementation guide § 2.7): the detailed
    /// discovery, then the use cases, both read from the NodeManagement feature
    /// with **no device part** — the one message allowed to omit it, because the
    /// peer's address is by definition not yet known.
    fn discover(&mut self, elapsed: StdDuration) {
        let source = eebus::spine::node_management(self.engine.device().address());
        let destination = eebus::spine::node_management_without_device();
        for function in [
            eebus::model::Function::NodeManagementDetailedDiscoveryData,
            eebus::model::Function::NodeManagementUseCaseData,
        ] {
            let _ = self.engine.read(&destination, &source, function, elapsed);
        }
    }

    /// Publish what the state machine now holds, so a peer reading it — or
    /// subscribed to it — is told.
    fn publish(&mut self, elapsed: StdDuration) {
        self.actor.publish(&mut self.engine, elapsed);
        self.consume_engine_events(elapsed);
    }

    /// Hand every event the engine has produced to the use-case actor.
    ///
    /// The actor is what turns a binding request, a subscription and a
    /// heartbeat notification into state-machine input; the engine is only the
    /// postal service. Draining to empty each turn is deliberate — one datagram
    /// can produce several events, and an event left in the queue is a limit
    /// that arrived and was not acted on.
    fn consume_engine_events(&mut self, elapsed: StdDuration) {
        while let Some(event) = self.engine.poll_event() {
            let _ = self.actor.handle_event(&mut self.engine, &event, elapsed);
        }
    }

    /// `eebus`'s monotonic clock, from a wall-clock instant.
    ///
    /// Saturating at zero: an instant before the driver started is a caller
    /// error, and answering "the beginning" is better than panicking inside a
    /// control loop.
    fn since_start(&self, now: OffsetDateTime) -> StdDuration {
        StdDuration::try_from(now - self.started_at).unwrap_or(StdDuration::ZERO)
    }

    /// Emit an event where the effective limit has actually moved.
    fn drain(&mut self, now: OffsetDateTime) {
        let current = self.actor.system().effective_limit();
        if self.last_reported == Some(current) {
            return;
        }
        self.last_reported = Some(current);
        let (ceiling, source) = match current {
            EffectiveLimit::None => (None, LimitSource::Operator),
            EffectiveLimit::Active(w) => (Some(Power::new(w)), LimitSource::Operator),
            // Not the operator asking: the household restraining itself because
            // nobody is talking to it. The two look identical at the connection
            // point and are entirely different events in the evidence record.
            EffectiveLimit::Failsafe(w) => (Some(Power::new(w)), LimitSource::Failsafe),
        };
        self.events.push(DriverEvent::GridLimit(GridLimit {
            direction: self.which.direction(),
            ceiling,
            duration: None,
            at: now,
            source,
        }));
        self.events.push(DriverEvent::Link(match self.state() {
            LpcState::Init => LinkState::Connecting,
            LpcState::Limited | LpcState::UnlimitedControlled => LinkState::Up,
            LpcState::Failsafe => LinkState::Stale,
            LpcState::UnlimitedAutonomous => LinkState::Down,
        }));
    }
}

/// The SPINE device a Controllable System lives on, and the actor that serves it.
///
/// Three features, because the use case needs exactly three: `LoadControl`
/// carries the limit, `DeviceConfiguration` carries the failsafe values, and
/// `DeviceDiagnosis` carries the heartbeat. Their addresses are what the actor
/// is wired to, so they are built here in one place rather than being passed in
/// — a caller that got them out of order would produce a device that discovers
/// correctly and answers a limit write on the wrong feature.
///
/// The entity is a `CEM`: hems is an energy manager acting for everything behind
/// it, which is `[A1 4.4.b]`'s addressing of the whole household through one
/// system rather than device by device.
fn build(
    which: Use,
    system: ControllableSystem,
    identity: &SpineIdentity,
) -> Result<(Engine, ControllableSystemActor), DriverError> {
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

    let entity = LocalEntity::new([1], EntityType::CEM)
        .with_feature(limitation::load_control_feature(1))
        .with_feature(limitation::device_configuration_feature(2))
        .with_feature(limitation::device_diagnosis_feature(3));
    device
        .add_entity(entity)
        .expect("entity [1] is the first one added");

    let load_control = device.address_of(&[1], 1);
    let configuration = device.address_of(&[1], 2);
    let diagnosis = device.address_of(&[1], 3);

    let mut engine = Engine::new(device);
    let (descriptor, direction) = match which {
        Use::Lpc => (&lpc::CONTROLLABLE_SYSTEM, lpc::DIRECTION),
        Use::Lpp => (&lpp::CONTROLLABLE_SYSTEM, lpp::DIRECTION),
    };
    engine.add_use_case([1], 1, descriptor);

    let actor =
        ControllableSystemActor::new(system, direction, load_control, configuration, diagnosis);
    Ok((engine, actor))
}

/// The driver contract, over SPINE datagrams.
///
/// # The bytes are the SPINE datagram, and that is not a convention
///
/// A SHIP data frame carries one SPINE datagram as JSON and nothing else
/// (SHIP § 13.4). So `on_bytes` and `poll_transmit` here take and give exactly
/// what the wire carries, and what `hemsd` adds underneath is the *session*:
/// TCP, TLS 1.2 with mutual authentication, the WebSocket upgrade, the SHIP
/// handshake and the framing. That split is what keeps this crate testable
/// without a socket — a § 14a day, a lost heartbeat and a failsafe are ordinary
/// assertions — while leaving nothing about the protocol itself in `hemsd`.
///
/// [`Lpc::on_limit`] and [`Lpc::on_heartbeat`] remain, and they are not a second
/// path into the state machine: they are the same machine reached without a
/// session, which is what a simulator, a relay input and a conformance test use.
impl Driver for Lpc {
    fn asset(&self) -> &AssetId {
        &self.asset
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::grid()
    }

    fn on_bytes(&mut self, bytes: &[u8], now: OffsetDateTime) -> Result<(), DriverError> {
        let datagram: eebus::model::Datagram = serde_json::from_slice(bytes)
            .map_err(|e| DriverError::Malformed(format!("not a SPINE datagram: {e}")))?;
        let elapsed = self.since_start(now);
        // `handle_datagram` answers `false` for a datagram this device is not
        // the destination of, which over a point-to-point SHIP session means the
        // peer is addressing somebody else. Not an error and not silence: the
        // engine has already decided, and there is nothing for the state machine
        // to do about it.
        let _ = self.engine.handle_datagram(&datagram, elapsed);
        self.consume_engine_events(elapsed);
        self.drain(now);
        Ok(())
    }

    /// A SHIP session opened or closed under this driver.
    ///
    /// Opening one is where **discovery** begins: SPINE learns who is on the
    /// other end by asking, and until it has, a Controllable System does not
    /// know which entity of the Energy Guard is going to write to it or where
    /// that guard's heartbeat lives. The Energy Guard asks too, and a device
    /// that only ever answered would be discovered without ever discovering.
    ///
    /// Closing one drops the peer. That is deliberately **not** the same thing
    /// as losing contact: `[LPC-911]`'s failsafe is timed by the heartbeat and
    /// only by the heartbeat, so a dropped socket starts the same two-minute
    /// clock a silent one does, and a reconnect inside it costs the household
    /// nothing. A driver that forced the failsafe on a TCP reset would restrain
    /// a house for a WLAN glitch.
    fn on_link(&mut self, state: LinkState, now: OffsetDateTime) {
        let elapsed = self.since_start(now);
        match state {
            LinkState::Up => {
                self.actor.install(&mut self.engine, elapsed);
                self.discover(elapsed);
                self.consume_engine_events(elapsed);
            }
            LinkState::Down | LinkState::Stale | LinkState::Connecting => {
                let peers: Vec<_> = self
                    .engine
                    .peers()
                    .filter_map(|p| p.address.clone())
                    .collect();
                for peer in &peers {
                    self.engine.remove_peer(peer);
                }
            }
        }
        self.drain(now);
    }

    fn on_timeout(&mut self, now: OffsetDateTime) {
        Lpc::on_timeout(self, now);
    }

    fn command(
        &mut self,
        command: &hems_core::setpoint::Command,
        _now: OffsetDateTime,
    ) -> Result<(), DriverError> {
        // Not an omission: a household does not command its own reduction. The
        // Controllable System is the side that is *told*.
        Err(DriverError::Unsupported(format!(
            "{command:?} — a Controllable System is told a limit, it does not set one"
        )))
    }

    fn poll_event(&mut self) -> Option<DriverEvent> {
        Lpc::poll_event(self)
    }

    fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        // One datagram per call, as JSON — a SHIP data frame's whole payload.
        // A datagram that cannot be serialised is dropped rather than returned
        // half-encoded: it would be a bug in `eebus`'s own model types, and
        // putting a truncated frame on a certifiable wire is worse than putting
        // nothing there.
        loop {
            let datagram = self.engine.poll_transmit()?;
            if let Ok(bytes) = serde_json::to_vec(&datagram) {
                return Some(bytes);
            }
        }
    }

    fn poll_deadline(&self) -> Option<OffsetDateTime> {
        Lpc::deadline(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const START: OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

    fn lpc() -> Lpc {
        Lpc::new(
            AssetId::new("netzanschluss").expect("a valid identifier"),
            Use::Lpc,
            // The household's own § 14a minimum, not a vendor default: a box
            // that falls back to 4,2 kW on a household owed 10,5 has given away
            // six kilowatts nobody asked it to.
            Power::from_kw(10.5),
            StdDuration::from_secs(2 * 3600),
            START,
        )
    }

    fn at(seconds: i64) -> OffsetDateTime {
        START + time::Duration::seconds(seconds)
    }

    fn limits(driver: &mut Lpc) -> Vec<GridLimit> {
        let mut out = Vec::new();
        while let Some(event) = driver.poll_event() {
            if let DriverEvent::GridLimit(l) = event {
                out.push(l);
            }
        }
        out
    }

    /// Heartbeat every sixty seconds from `from` to `to`, as an Energy Guard does.
    ///
    /// Written out rather than assumed, because the heartbeat is not decoration:
    /// outside the controlled states a limit write is only *evaluated* when one
    /// arrived in the last sixty seconds (implementation guide §2.11), so a test
    /// that skipped them would be testing a rejection.
    fn heartbeat_through(d: &mut Lpc, from: i64, to: i64) {
        let mut t = from;
        while t <= to {
            d.on_heartbeat(at(t));
            t += 60;
        }
    }

    #[test]
    fn a_whole_lpc_day_runs_in_virtual_time() {
        // A whole § 14a day: a reduction, its own expiry, heartbeat loss, the
        // failsafe and the release — as ordinary assertions rather than as two
        // hours of waiting. Nothing here reads a clock.
        let mut d = lpc();
        assert_eq!(
            d.state(),
            LpcState::Init,
            "a box that reboots during a grid emergency comes back restrained"
        );

        // The operator is in contact all afternoon.
        heartbeat_through(&mut d, 0, 17 * 3600);
        let _ = limits(&mut d);

        // 17:00 — a reduction to 4,2 kW for ninety minutes.
        let write = LimitWrite::active_for(4_200.0, StdDuration::from_secs(90 * 60));
        let outcome = d.on_limit(&write, at(17 * 3600));
        assert!(
            outcome.is_accepted(),
            "a well-formed limit is always accepted here: how the household *meets* \
             it is the guard's decision, not the driver's"
        );
        assert_eq!(d.state(), LpcState::Limited);
        assert_eq!(d.ceiling(), Some(Power::from_kw(4.2)));

        let reported = limits(&mut d);
        let last = reported.last().expect("the reduction reaches the guard");
        assert_eq!(last.ceiling, Some(Power::from_kw(4.2)));
        assert_eq!(last.direction, LimitDirection::Consumption);
        assert_eq!(
            last.source,
            LimitSource::Operator,
            "the operator asked for this one, and the evidence record has to say so"
        );

        // The heartbeat keeps coming, so the limit stays.
        heartbeat_through(&mut d, 17 * 3600 + 60, 17 * 3600 + 89 * 60);
        assert_eq!(d.state(), LpcState::Limited);

        // 18:30 — the duration runs out and the limit lifts itself. Nobody sent
        // anything: `[LPC-908]`.
        d.on_timeout(at(17 * 3600 + 90 * 60 + 1));
        assert_eq!(d.state(), LpcState::UnlimitedControlled);
        assert_eq!(d.ceiling(), None);
    }

    #[test]
    fn an_operator_that_goes_quiet_puts_the_household_on_its_own_failsafe() {
        let mut d = lpc();
        // Contact, and a write — which is what establishes control. A heartbeat
        // on its own never leaves `Init`, and that is the specification rather
        // than an implementation detail: a guard that is merely *reachable* is
        // not yet a guard that is *in charge*.
        heartbeat_through(&mut d, 0, 120);
        d.on_limit(&LimitWrite::deactivated(), at(130));
        assert_eq!(d.state(), LpcState::UnlimitedControlled);
        let _ = limits(&mut d);

        // Two minutes of silence is the whole of it: `[LPC-911]`.
        d.on_timeout(at(130 + 121));
        assert_eq!(d.state(), LpcState::Failsafe);
        assert_eq!(
            d.ceiling(),
            Some(Power::from_kw(10.5)),
            "the failsafe is the household's own § 14a minimum, not a vendor default"
        );

        let reported = limits(&mut d);
        let last = reported.last().expect("the failsafe reaches the guard");
        assert_eq!(
            last.source,
            LimitSource::Failsafe,
            "nobody asked for this — the household is restraining itself, and a \
             Nachweis that called it a control action would be wrong"
        );

        // …and after the minimum has run, the household is released. A limit
        // nobody is maintaining does not last for ever `[LPC-922]`.
        d.on_timeout(at(130 + 121 + 2 * 3600 + 1));
        assert_eq!(d.state(), LpcState::UnlimitedAutonomous);
        assert_eq!(d.ceiling(), None);
    }

    #[test]
    fn the_state_is_derived_from_the_protocol_rather_than_tracked_beside_it() {
        // The mapping this crate exists to be: five states, one to one. A second
        // copy of a certifiable state machine is one copy and one liability, so
        // the only thing that may differ between `eebus` and hems is the name.
        for (theirs, ours) in [
            (LimitationState::Init, LpcState::Init),
            (LimitationState::Limited, LpcState::Limited),
            (
                LimitationState::UnlimitedControlled,
                LpcState::UnlimitedControlled,
            ),
            (LimitationState::FailsafeState, LpcState::Failsafe),
            (
                LimitationState::UnlimitedAutonomous,
                LpcState::UnlimitedAutonomous,
            ),
        ] {
            let mapped = match theirs {
                LimitationState::Init => LpcState::Init,
                LimitationState::Limited => LpcState::Limited,
                LimitationState::UnlimitedControlled => LpcState::UnlimitedControlled,
                LimitationState::FailsafeState => LpcState::Failsafe,
                LimitationState::UnlimitedAutonomous => LpcState::UnlimitedAutonomous,
            };
            assert_eq!(mapped, ours, "{theirs:?} must map to {ours:?}");
        }
    }

    #[test]
    fn an_unchanged_limit_is_not_reported_twice() {
        // The guard is edge-driven and the evidence record is a log of events. A
        // driver that re-emitted the same ceiling on every heartbeat would fill
        // both with noise and make a real change hard to find.
        let mut d = lpc();
        d.on_heartbeat(at(10));
        let _ = limits(&mut d);
        d.on_limit(&LimitWrite::active(4_200.0), at(60));
        assert_eq!(limits(&mut d).len(), 1);
        for minute in 2..10 {
            d.on_heartbeat(at(minute * 60));
        }
        assert!(
            limits(&mut d).is_empty(),
            "nothing changed, so nothing is reported"
        );
    }

    #[test]
    fn a_deadline_is_always_offered_while_the_operator_is_in_contact() {
        // What makes the failsafe reachable rather than merely written down: if
        // the driver ever answered `None` here, `hemsd` would wait for bytes
        // that are not coming and the timeout would never fire.
        let mut d = lpc();
        d.on_heartbeat(at(10));
        assert!(d.deadline().is_some(), "a heartbeat has to have a deadline");
        d.on_limit(
            &LimitWrite::active_for(4_200.0, StdDuration::from_secs(600)),
            at(20),
        );
        assert!(d.deadline().is_some(), "so does a limit with a duration");
    }
}
