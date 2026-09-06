//! The thing that owns the drivers.
//!
//! A driver knows a protocol and nothing else: it is handed bytes and a clock
//! and answers with events. Something has to hold a *set* of them, give each one
//! its bytes, collect what they say, and turn that into the two things the
//! control planes read — [`SiteState`], what the house is doing, and
//! [`GridLimits`], what the network operator is asking for.
//!
//! That is this. It is the last seam between a simulated day and a managed
//! house, and it lives in `hemsd` rather than in `hems-drv` for one reason: it
//! is the layer where a socket becomes legitimate. The drivers stay sans-I/O and
//! the purity gate keeps them that way; the registry is where a real box's
//! `tokio` loop hands bytes in and takes bytes out.
//!
//! # Registration is a check, not a formality
//!
//! A declaration nothing validates is a comment with a type, so
//! [`Registry::register`] is what gives [`hems_drv::DriverCapabilities`] its
//! meaning. It refuses four mismatches that would otherwise be discovered months
//! later by a limit that never arrived:
//!
//! * a driver for an asset the site does not have — a typo in configuration,
//!   which otherwise presents as a device that is simply never commanded;
//! * two drivers for one asset, which is two sources of truth about one meter;
//! * a **controllable** asset whose driver cannot take commands, which is a
//!   device the arbiter will spend the day talking to and never move;
//! * a site under § 14a with no driver that reports grid limits, which is a
//!   household that believes it is participating and would never hear a
//!   reduction.
//!
//! Each of those is silent at runtime and loud at startup, which is the right
//! way round.

use std::collections::{BTreeMap, BTreeSet};

use hems_core::prelude::{AssetId, Measurement, Power, Site};
use hems_core::setpoint::Setpoint;
use hems_drv::{
    CommandOutcome, Driver, DriverError, DriverEvent, LimitDirection, LimitSource, LinkState,
};
use hems_realtime::guard::{GridLimits, SiteState};
use time::{Duration, OffsetDateTime};

/// Which registered driver, as distinct from which asset.
///
/// An asset may have two: one that **commands** it and one that **measures**
/// it. The wallbox is the case the workspace already documented and could not
/// configure — commanded over Modbus, and read over EEBUS EVCC/EVSOC for
/// whether there is a car on the cable and how full it is — and the heat pump
/// is the second, commanded over OHPCF and measured wherever a room temperature
/// comes from. Both drivers have their own socket, their own link state and
/// their own deadline, so a transport keyed by asset would have run one of them
/// and silently starved the other.
///
/// Opaque, and handed back by [`Registry::register`] in registration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DriverId(usize);

/// A registered driver together with the asset it speaks for.
///
/// The two travel together everywhere a transport does — the identity says
/// which driver's socket this is, and the asset is what a log line has to name
/// for the fault to be actionable — and passing them separately is passing two
/// values that must agree and that nothing checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    /// Which registered driver.
    pub driver: DriverId,
    /// What it speaks for.
    pub asset: AssetId,
}

/// Why a set of drivers does not describe this site.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A driver names an asset the site does not have.
    #[error("a driver speaks for `{0}`, which this site does not have")]
    NoSuchAsset(String),
    /// Two drivers claim to command one asset.
    ///
    /// Two drivers that *measure* one asset is the same fault for the same
    /// reason — two sources of truth and nothing downstream that could tell
    /// which to believe — and they are one error because an installer fixes
    /// them the same way: delete one.
    #[error("two drivers {1} `{0}`, and nothing downstream could tell which to believe")]
    Duplicate(String, &'static str),
    /// A controllable asset has a driver that cannot command it.
    #[error(
        "`{0}` is controllable and its driver cannot take commands, so the arbiter \
         would spend the day talking to a device it can never move"
    )]
    CannotCommand(String),
    /// The site takes part in § 14a and nothing can hear a reduction.
    #[error(
        "this site takes part in the netzorientierte Steuerung and no driver reports \
         grid limits, so a reduction could never arrive — add a `kind = \"eebus-lpc\"` \
         driver, or describe a household with no steuerbare Verbrauchseinrichtung"
    )]
    NoGridDriver,
    /// Nothing at all is configured.
    ///
    /// Separated from [`RegistryError::NoGridDriver`] because they are different
    /// mistakes with the same symptom: one is a household that was commissioned
    /// wrongly, the other is a box nobody has commissioned yet, and telling an
    /// installer the first when they have done the second sends them looking in
    /// the wrong place.
    #[error(
        "no drivers are configured, so nothing would be measured and every \
         controllable device would be assumed to be drawing its nameplate power"
    )]
    Uncommissioned,
}

/// A set of drivers, and what they have told us.
pub struct Registry {
    entries: Vec<Entry>,
    /// The limits the grid drivers have reported, as the guard wants them.
    limits: GridLimits,
    /// The failsafe values the network operator has written, per direction.
    ///
    /// Kept apart from [`Registry::limits`] because it is not a limit: it is
    /// what the household holds when there is **no** limit and no session to
    /// carry one, and `[LPC-021]` makes it the operator's to change. The guard
    /// never reads it — the driver enforces it — so the only reason it is here
    /// is that something has to write it down before the next power cut.
    failsafe: BTreeMap<hems_drv::LimitDirection, hems_drv::Failsafe>,
}

struct Entry {
    /// Which driver this is, for the transport that owns its socket.
    id: DriverId,
    /// `Send`, because on a real box a driver lives in a task that owns its
    /// socket. The bound belongs **here** rather than on `hems_drv::Driver`: a
    /// driver is a state machine and a state machine has no business declaring
    /// which runtime will hold it, and a conformance harness that drove one on a
    /// single thread would be no less legitimate for it.
    driver: Box<dyn Driver + Send>,
    asset: AssetId,
    link: LinkState,
    /// The last measurement this driver produced, if any.
    latest: Option<Measurement>,
    /// Events this driver has produced and no caller has taken yet.
    ///
    /// Separate from the fold above, and the separation is the point: what a
    /// driver *said* is a stream one consumer drains (the § 14a evidence
    /// record), and what the registry *believes* is a view every consumer
    /// reads. Folding only on the drain would make the second depend on the
    /// first having happened — an ordering nothing declares and nothing checks,
    /// and a planner that reads a registry which has learned nothing plans a
    /// household with no battery in it.
    pending: Vec<DriverEvent>,
    /// The conductors it reports, for a switchable charge point.
    phases: Option<hems_core::prelude::PhaseMode>,
    /// Whether a car is plugged into this charge point, and what it has said.
    ///
    /// `None` on a driver that cannot tell — every charge point hems speaks to
    /// over Modbus, which reports power and has no idea what is on the end of
    /// the cable. Absence is therefore "nobody knows" and not "no car", and the
    /// planner has to treat the two differently: an unknown session is one to
    /// leave out, and an empty one is a socket to stop reserving energy for.
    vehicle: Option<hems_drv::VehiclePresence>,
    /// How the last command to this device turned out.
    ///
    /// A device that answered a setpoint and did not act on it is the one
    /// failure the layers above cannot see: the guard commanded, nothing
    /// errored, and the disagreement only surfaces when the meter contradicts
    /// the plan — hours later, on a household nobody is watching. It is kept
    /// per asset rather than folded into one flag because *which* device
    /// stopped obeying is the whole of the diagnosis.
    last_command: Option<CommandOutcome>,
    /// What this appliance last said it could do, where it can say so.
    ///
    /// `None` on every driver that has no such use case, and absence means
    /// "nobody knows" rather than "no flexibility" — the same distinction
    /// [`Entry::vehicle`] draws. A planner reads it to replace its configured
    /// minimum runtimes with the appliance's own.
    flex: Option<hems_drv::Flexibility>,
}

impl Entry {
    /// Whether this driver is in contact **and** has said something recently.
    ///
    /// Both halves are needed and neither implies the other. A driver reports
    /// its link on its own initiative — [`LinkState::Stale`] is a driver saying
    /// in so many words that it no longer knows, and its last reading is dropped
    /// rather than left to age out, because the driver knows something the
    /// timestamp does not. But a device can also stop updating a register while
    /// its socket stays open, and only the age says so.
    fn is_heard(&self, now: OffsetDateTime) -> bool {
        if !self.link.is_usable() {
            return false;
        }
        // A driver that does not measure cannot be judged by the age of a
        // measurement. The § 14a grid driver is the case: `[A1 4.6]` is an
        // *instruction*, so it reports limits and link state and nothing else,
        // and asking it for a recent reading counted a perfectly connected
        // Steuerbox as a device nobody could hear — for ever, on every box that
        // has one. The readiness probe then stays bad on a box that is working,
        // which is the state in which nobody looks at it again.
        //
        // A driver that *can* measure is still judged by the age: a device may
        // stop updating a register while its socket stays open, and only the
        // timestamp says so.
        let caps = self.driver.capabilities();
        if !caps.measures {
            return true;
        }
        // And a driver whose peer **notifies** it cannot be judged that way
        // either, for a reason one step further in. A subscription delivers a
        // value when it *changes*, so a hot-water tank holding 52 °C and a room
        // holding 21 °C are silent for hours — which is the protocol working.
        // Judging them by the age of the last reading dropped both from the
        // site's state seconds after every reading, so the tank and the building
        // were in the plan only in the moments just after they moved, which is
        // the opposite of when a plan needs them.
        //
        // What such a driver owes instead is a **link**, and it reports one on
        // its own initiative: a SHIP session that goes away takes the driver's
        // link with it, which the branch above already catches. Where the peer
        // stamps its readings the age is meaningful again, and the driver puts
        // the peer's own instant on the measurement — so this is a fallback for
        // the peers that send none rather than a blanket exemption.
        if caps.reports_on_change {
            return self.latest.is_some();
        }
        self.latest.is_some_and(|m| now - m.at <= SILENCE)
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("drivers", &self.entries.len())
            .field("limits", &self.limits)
            .field("failsafe", &self.failsafe)
            .finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            limits: GridLimits::default(),
            failsafe: BTreeMap::new(),
        }
    }

    /// How many drivers are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether anything is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add a driver, checking that it and the site agree about what it is for.
    ///
    /// # An asset may have two drivers, and only two
    ///
    /// One that **commands** it and one that **measures** it. What is refused is
    /// two of either: two commanders is a device nobody can predict, and two
    /// meters is two sources of truth with nothing downstream that could tell
    /// which to believe. A driver that does both is simply both.
    ///
    /// Whether a *controllable* asset has a commanding driver at all is not
    /// asked here, because it cannot be: the commanding driver may be the second
    /// one registered. [`Registry::validate`] asks it once, when the set is
    /// complete.
    ///
    /// # Errors
    /// [`RegistryError`] for any of the mismatches in the module note.
    pub fn register(
        &mut self,
        driver: Box<dyn Driver + Send>,
        site: &Site,
    ) -> Result<DriverId, RegistryError> {
        let asset = driver.asset().clone();
        let caps = driver.capabilities();

        // A grid driver speaks for the connection point, which is not one of the
        // site's assets — so only a device driver has to name one that exists.
        if !caps.reports_grid_limits && site.asset(&asset).is_none() {
            return Err(RegistryError::NoSuchAsset(asset.to_string()));
        }
        for (already, role) in [
            (caps.accepts_commands, "command"),
            (caps.measures, "measure"),
        ] {
            if already && self.role(&asset, role).is_some() {
                return Err(RegistryError::Duplicate(asset.to_string(), role));
            }
        }

        let id = DriverId(self.entries.len());
        self.entries.push(Entry {
            id,
            driver,
            asset,
            link: LinkState::Down,
            latest: None,
            pending: Vec::new(),
            phases: None,
            vehicle: None,
            last_command: None,
            flex: None,
        });
        Ok(id)
    }

    /// The driver that plays `role` for `asset`, where one does.
    fn role(&self, asset: &AssetId, role: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| {
            e.asset == *asset
                && match role {
                    "command" => e.driver.capabilities().accepts_commands,
                    _ => e.driver.capabilities().measures,
                }
        })
    }

    /// Check the registered set against what the site expects of it.
    ///
    /// Called once, after everything is registered — the § 14a question cannot
    /// be answered driver by driver, because it is about the *absence* of one.
    ///
    /// # Errors
    /// [`RegistryError::Uncommissioned`] where nothing is registered at all,
    /// [`RegistryError::CannotCommand`] where a controllable asset has drivers
    /// and none of them can move it, and [`RegistryError::NoGridDriver`] where
    /// the site takes part in the netzorientierte Steuerung and nothing can hear
    /// a reduction.
    pub fn validate(&self, site: &Site, now: OffsetDateTime) -> Result<(), RegistryError> {
        if self.entries.is_empty() {
            return Err(RegistryError::Uncommissioned);
        }
        // Asked here rather than at registration, because the commanding driver
        // may be the second one registered: a wallbox read over EEBUS and
        // commanded over Modbus is a household that is perfectly well managed,
        // and judging each driver as it arrived refused the pair on the strength
        // of the order they were listed in.
        //
        // An asset with **no** driver at all is a different fact and not this
        // one — `undriven` reports it, and a partially commissioned box is
        // allowed to run — so only an asset something already speaks for is
        // checked.
        for asset in &site.assets {
            let id = asset.meta().id.clone();
            if !hems_realtime::guard::is_controllable(asset) {
                continue;
            }
            let spoken_for = self.entries.iter().any(|e| e.asset == id);
            if spoken_for && self.role(&id, "command").is_none() {
                return Err(RegistryError::CannotCommand(id.to_string()));
            }
        }
        let participates = !hems_grid::classify_at(&site.assets, now).is_empty();
        let hears = self
            .entries
            .iter()
            .any(|e| e.driver.capabilities().reports_grid_limits);
        if participates && !hears {
            return Err(RegistryError::NoGridDriver);
        }
        Ok(())
    }

    /// The assets whose available power has to be guessed from a nameplate.
    ///
    /// Worth surfacing rather than leaving implicit: a curtailed inverter that
    /// cannot say what it *could* produce is one whose curtailment lifts on an
    /// assumption, and a household is entitled to know which of its devices are
    /// in that position.
    pub fn assumed_available_power(&self) -> impl Iterator<Item = &AssetId> {
        self.entries.iter().filter_map(|e| {
            let caps = e.driver.capabilities();
            (caps.measures && !caps.reports_available_power).then_some(&e.asset)
        })
    }

    /// The earliest moment any driver wants to be woken.
    ///
    /// `None` only where nothing is registered: a driver that offered no
    /// deadline could never notice its own silence.
    #[must_use]
    pub fn poll_deadline(&self) -> Option<OffsetDateTime> {
        self.entries
            .iter()
            .filter_map(|e| e.driver.poll_deadline())
            .min()
    }

    /// Bytes arrived for one driver.
    ///
    /// # Errors
    /// Whatever the driver made of them. The registry does not decide that a
    /// rate of malformed frames is an outage — that is a policy, and it belongs
    /// where the socket is.
    pub fn on_bytes(
        &mut self,
        driver: DriverId,
        bytes: &[u8],
        now: OffsetDateTime,
    ) -> Result<(), DriverError> {
        let Some(entry) = self.entry_mut(driver) else {
            return Ok(());
        };
        entry.driver.on_bytes(bytes, now)
    }

    /// The transport under one driver opened or closed.
    ///
    /// The one fact only the layer with the socket knows, and the one a stream
    /// of bytes cannot carry: a reconnect invalidates a half-frame, a request
    /// waiting for its answer and a discovered peer, and the first bytes of the
    /// new socket look exactly like the continuation of the old one.
    pub fn on_link(&mut self, driver: DriverId, state: LinkState, now: OffsetDateTime) {
        let Some(entry) = self.entry_mut(driver) else {
            return;
        };
        entry.driver.on_link(state, now);
        entry.link = state;
        if !state.is_usable() {
            // A reading whose session has gone is not a reading. Leaving it in
            // place would let the guard treat a device that has been unreachable
            // for a minute as one that is merely idle, which is the one
            // assumption a guard may never make about a controllable device.
            entry.latest = None;
            entry.phases = None;
        }
    }

    /// Time passed. Every driver is told, because each decides for itself
    /// whether its own deadline has gone by.
    pub fn on_timeout(&mut self, now: OffsetDateTime) {
        for entry in &mut self.entries {
            entry.driver.on_timeout(now);
        }
    }

    /// Time passed for one driver.
    ///
    /// What a per-driver transport task calls: each socket has its own deadline,
    /// and waking every driver because one of them had a timeout would make a
    /// slow inverter's cadence the cadence of the Steuerbox.
    pub fn on_timeout_of(&mut self, driver: DriverId, now: OffsetDateTime) {
        if let Some(entry) = self.entry_mut(driver) {
            entry.driver.on_timeout(now);
        }
    }

    /// The next bytes to put on a wire, and which driver's wire it is.
    pub fn poll_transmit(&mut self) -> Option<(DriverId, Vec<u8>)> {
        for entry in &mut self.entries {
            if let Some(bytes) = entry.driver.poll_transmit() {
                return Some((entry.id, bytes));
            }
        }
        None
    }

    /// One entry, by the identity registration handed back.
    fn entry_mut(&mut self, driver: DriverId) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == driver)
    }

    /// The next bytes for one driver's own wire.
    pub fn poll_transmit_of(&mut self, driver: DriverId) -> Option<Vec<u8>> {
        self.entry_mut(driver)?.driver.poll_transmit()
    }

    /// When one driver wants waking.
    #[must_use]
    pub fn deadline_of(&self, driver: DriverId) -> Option<OffsetDateTime> {
        self.entries
            .iter()
            .find(|e| e.id == driver)?
            .driver
            .poll_deadline()
    }

    /// Send one setpoint to whichever driver speaks for its asset.
    ///
    /// # Errors
    /// [`DriverError::NoDriver`] where nothing speaks for the asset — which is a
    /// command that would otherwise be dropped in silence, and the difference
    /// between a device that is idle and one that is unreachable.
    pub fn command(&mut self, setpoint: &Setpoint, now: OffsetDateTime) -> Result<(), DriverError> {
        // The **commanding** driver, which may not be the only one that speaks
        // for this asset: a wallbox read over EEBUS and commanded over Modbus
        // has two, and sending a setpoint to whichever was registered first
        // would deliver half of them to a driver that measures.
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.asset == setpoint.asset && e.driver.capabilities().accepts_commands)
        else {
            return Err(DriverError::NoDriver(setpoint.asset.to_string()));
        };
        entry.driver.command(&setpoint.command, now)
    }

    /// Take everything the drivers have said since the last call.
    ///
    /// The § 14a evidence record is built from exactly these, which is why they
    /// are handed over rather than merely folded: an event nobody journals is a
    /// control action nobody can prove was carried out.
    ///
    /// It does **not** decide what the registry believes. That folding happens
    /// on every call that could have produced an event, [`Registry::observe`]
    /// included — so a caller that never drains still reads a current view, and
    /// one that drains twice does not lose a limit.
    pub fn drain(&mut self) -> Vec<(AssetId, DriverEvent)> {
        self.absorb();
        let mut out = Vec::new();
        for entry in &mut self.entries {
            out.extend(
                core::mem::take(&mut entry.pending)
                    .into_iter()
                    .map(|e| (entry.asset.clone(), e)),
            );
        }
        out
    }

    /// Fold everything the drivers have produced into what the registry
    /// believes, and queue it for whoever drains next.
    ///
    /// Called by every method that could have made a driver say something, and
    /// by [`Registry::observe`] — so the view is never behind the drivers, and
    /// no consumer has to know that another one was supposed to run first.
    fn absorb(&mut self) {
        for entry in &mut self.entries {
            while let Some(event) = entry.driver.poll_event() {
                match &event {
                    DriverEvent::Measured(m) => entry.latest = Some(*m),
                    DriverEvent::Link(l) => entry.link = *l,
                    DriverEvent::GridLimit(limit) => match limit.direction {
                        LimitDirection::Consumption => {
                            self.limits.steuve_ceiling = limit.ceiling;
                            self.limits.steuve_since = limit.ceiling.map(|_| limit.at);
                            self.limits.in_failsafe = limit.source == LimitSource::Failsafe;
                        }
                        LimitDirection::Production => {
                            self.limits.feed_in_ceiling = limit.ceiling;
                        }
                    },
                    DriverEvent::Command(outcome) => entry.last_command = Some(outcome.clone()),
                    // `[MGCP-011]`, and it is a *factor* rather than a ceiling:
                    // the watts it means depend on the roof, which the guard
                    // knows and the driver does not.
                    DriverEvent::FeedInFactor(factor) => {
                        self.limits.mgcp_factor = Some(factor.percent / 100.0);
                    }
                    // Whether there is a car on the charge point, which is a
                    // fact about the *session* rather than a measurement: a
                    // socket with nothing in it is working perfectly and has no
                    // state of charge to report.
                    DriverEvent::Vehicle(presence) => entry.vehicle = Some(*presence),
                    // What the appliance says it *could* do, which is the one
                    // thing a ceiling can never carry. Kept rather than only
                    // journalled because the two numbers in it — how long the
                    // compressor must run once started and how long it must then
                    // rest — are the planner's minimum-runtime constraints, and
                    // a figure the machine states beats the same figure typed
                    // into a configuration file.
                    DriverEvent::Flexibility(offer) => entry.flex = Some(offer.clone()),
                    // Not a limit, and kept for one reason: it has to outlive
                    // the process. `[LPC-021]` lets the operator change what
                    // this household falls back to, and a box that held the new
                    // value only in memory would come back from a power cut on
                    // whatever its own file says.
                    DriverEvent::Failsafe(failsafe) => {
                        self.failsafe.insert(failsafe.direction, *failsafe);
                    }
                }
                entry.pending.push(event);
            }
        }
    }

    /// What the network operator is asking for.
    #[must_use]
    pub fn limits(&self) -> GridLimits {
        self.limits.clone()
    }

    /// What the network operator has said this household falls back to.
    ///
    /// Empty until an operator writes one, which is the ordinary case: the
    /// configured value stands until somebody changes it.
    #[must_use]
    pub fn failsafe(&self) -> &BTreeMap<hems_drv::LimitDirection, hems_drv::Failsafe> {
        &self.failsafe
    }

    /// What the house is doing, as far as the drivers can tell.
    ///
    /// # A stale driver contributes nothing rather than something old
    ///
    /// A measurement carries the instant it was *observed*, and the guard
    /// already refuses one older than its own tolerance — so the honest thing
    /// here is to pass the age through rather than to hide it. What the registry
    /// adds is the link: a driver that has reported itself
    /// [`LinkState::Stale`] has said, in so many words, that it no longer knows.
    /// Its last reading is dropped rather than left to age out, because the
    /// driver knows something the timestamp does not.
    ///
    /// The guard's response to an absent measurement is already the safe one: a
    /// controllable device nobody can hear is assumed to be running flat out.
    #[must_use]
    pub fn state(&self, grid_meter: Option<&AssetId>, now: OffsetDateTime) -> SiteState {
        let mut state = SiteState::default();
        for entry in &self.entries {
            let Some(measurement) = entry.latest else {
                continue;
            };
            if !entry.is_heard(now) {
                continue;
            }
            if Some(&entry.asset) == grid_meter {
                state.grid = Some(measurement);
            } else {
                let _ = state.assets.insert(entry.asset.clone(), measurement);
            }
            if let Some(mode) = entry.phases {
                let _ = state.phases.insert(entry.asset.clone(), mode);
            }
        }
        state
    }

    /// The controllable assets of `site` that no driver speaks for.
    ///
    /// Not an error, and that is a judgement rather than an oversight. The
    /// registry refuses a driver that *cannot command* a controllable asset,
    /// because that is a declaration contradicting itself. An asset with no
    /// driver at all is a different thing: a box part-way through commissioning,
    /// or a household that owns a device hems has no driver for yet. Refusing to
    /// start would make the site model a list of what is wired rather than a
    /// list of what is there.
    ///
    /// But it is not nothing either — the arbiter will decide a setpoint for
    /// each of these on every tick and have nowhere to send it — so it is named
    /// once at start-up and counted on the status surface, which is where a
    /// fact that is equally true every second belongs.
    pub fn undriven<'a>(&'a self, site: &'a Site) -> impl Iterator<Item = &'a AssetId> {
        site.assets
            .iter()
            .filter(|a| hems_realtime::guard::is_controllable(a))
            .map(hems_core::prelude::Asset::id)
            .filter(|id| !self.entries.iter().any(|e| e.asset == **id))
    }

    /// The assets no driver has been heard from.
    ///
    /// The number a box should put on a screen: a device nobody can hear is one
    /// the guard is being conservative about, and being conservative costs the
    /// household money.
    ///
    /// **Age counts as well as link state.** A driver reports [`LinkState`] on
    /// its own initiative, so a device that stops updating a register without
    /// dropping its socket would otherwise stay `Up` for ever: the guard drops
    /// its reading at `max_measurement_age` — nothing is unsafe — while the
    /// screen still calls the device healthy and the household is never told why
    /// its budget shrank. Two questions, and both have to be asked. See
    /// [`SILENCE`].
    pub fn silent(&self, now: OffsetDateTime) -> impl Iterator<Item = &AssetId> {
        self.entries
            .iter()
            .filter(move |e| !e.is_heard(now))
            .map(|e| &e.asset)
    }

    /// Devices whose last command was not carried out, and what they said.
    ///
    /// Distinct from [`Registry::silent`], and the distinction is the one worth
    /// having: a silent device is not being heard from at all, while one of
    /// these is answering perfectly well and not doing what it was told. The
    /// first is a network fault; the second is a device that has to be
    /// commanded some other way, and treating them as the same thing sends an
    /// installer to the wrong end of the house.
    pub fn disobedient(&self) -> impl Iterator<Item = (&AssetId, &CommandOutcome)> {
        self.entries.iter().filter_map(|e| {
            e.last_command
                .as_ref()
                .filter(|o| !o.accepted)
                .map(|o| (&e.asset, o))
        })
    }
}

/// How long a driver may be silent before the registry stops believing it.
///
/// Deliberately **shorter** than the guard's own `max_measurement_age` (30 s):
/// the two answer different questions and the order between them matters. The
/// guard asks "may I act on this number", and answers by falling back to a
/// conservative assumption. The registry asks "is this device being heard from",
/// and the answer is what a household is shown and what `obsd` counts. If this
/// were the *longer* of the two, a reading could age out of the guard — the
/// budget shrinking, the wallbox slowing down — while the screen still called
/// the link healthy, and nobody could tell the household why.
pub const SILENCE: Duration = Duration::seconds(10);

/// Everything the drivers said, in the shape the control planes read.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// What the house is doing.
    pub state: SiteState,
    /// What the network operator is asking for.
    pub limits: GridLimits,
    /// What could not be heard from.
    pub silent: BTreeSet<AssetId>,
    /// Devices whose available power is a nameplate rather than a reading.
    pub assumed_available: BTreeSet<AssetId>,
    /// Devices that answered their last setpoint and did not act on it, and why.
    pub disobedient: BTreeMap<AssetId, String>,
    /// Charge points that can say whether a car is on them, and what it said.
    pub vehicles: BTreeMap<AssetId, hems_drv::VehiclePresence>,
    /// Appliances that can announce what they could do, and what they announced.
    pub flexibility: BTreeMap<AssetId, hems_drv::Flexibility>,
}

impl Registry {
    /// The whole picture, in one call.
    ///
    /// Folds in whatever the drivers have said before answering, so it is never
    /// behind them — and leaves the events themselves queued for
    /// [`Registry::drain`], because the § 14a evidence record is a stream one
    /// consumer takes and this is a view every consumer reads.
    ///
    /// Taking `&mut self` is what makes that possible, and it is the right
    /// signature for the same reason: an `observe` that could only be current if
    /// somebody else had drained first is an ordering nothing declares and
    /// nothing checks.
    pub fn observe(&mut self, grid_meter: Option<&AssetId>, now: OffsetDateTime) -> Observed {
        self.absorb();
        Observed {
            state: self.state(grid_meter, now),
            limits: self.limits(),
            silent: self.silent(now).cloned().collect(),
            assumed_available: self.assumed_available_power().cloned().collect(),
            vehicles: self
                .entries
                .iter()
                .filter_map(|e| e.vehicle.map(|v| (e.asset.clone(), v)))
                .collect(),
            flexibility: self
                .entries
                .iter()
                .filter_map(|e| e.flex.clone().map(|f| (e.asset.clone(), f)))
                .collect(),
            disobedient: self
                .disobedient()
                .map(|(asset, outcome)| {
                    (
                        asset.clone(),
                        outcome
                            .detail
                            .clone()
                            .unwrap_or_else(|| "the device did not carry it out".into()),
                    )
                })
                .collect(),
        }
    }
}

/// The household's own load — everything the meter saw that no instrumented
/// asset accounts for.
///
/// Under the load convention the connection point equals the sum of the assets
/// behind it, so what is left over is the part of the house nobody metered: the
/// kettle, the lights, the fridge. It is what the load forecast is *about*, and
/// it is computed in one place because the control loop teaches the forecast
/// from it and the planner falls back on it — and two derivations of one
/// quantity are two chances to get the sign wrong.
///
/// `None` where the grid meter is not being heard from. There is no useful
/// guess: a house nobody is measuring did not use nothing.
#[must_use]
pub fn household_load(observed: &Observed) -> Option<Power> {
    let grid = observed.state.grid.and_then(|m| m.power)?;
    let assets: Power = observed.state.assets.values().filter_map(|m| m.power).sum();
    Some((grid - assets).max(Power::ZERO))
}

/// The per-asset powers a caller can read straight out of an [`Observed`].
#[must_use]
pub fn powers(observed: &Observed) -> BTreeMap<AssetId, Power> {
    observed
        .state
        .assets
        .iter()
        .filter_map(|(id, m)| m.power.map(|p| (id.clone(), p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::{Current, PhaseConnection};
    use hems_drv::DriverCapabilities;
    use hems_drv::eebus::{Lpc, Use};
    use hems_drv::modbus::{Cadence, SunSpec};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 12:00:00 UTC);

    fn id(s: &str) -> AssetId {
        AssetId::new(s).expect("a valid identifier")
    }

    /// The reference household, which has a wallbox and a heat pump under
    /// § 14a.
    fn site() -> Site {
        crate::site::Household::build(&crate::HouseholdConfig::default())
            .expect("the reference household")
            .site
    }

    /// A driver that reports nothing and takes nothing — the shape a
    /// misconfigured integration has.
    #[derive(Debug)]
    struct Mute {
        asset: AssetId,
        caps: DriverCapabilities,
    }

    impl Driver for Mute {
        fn asset(&self) -> &AssetId {
            &self.asset
        }
        fn capabilities(&self) -> DriverCapabilities {
            self.caps
        }
        fn on_bytes(&mut self, _: &[u8], _: OffsetDateTime) -> Result<(), DriverError> {
            Ok(())
        }
        fn on_timeout(&mut self, _: OffsetDateTime) {}
        fn command(
            &mut self,
            _: &hems_core::setpoint::Command,
            _: OffsetDateTime,
        ) -> Result<(), DriverError> {
            Ok(())
        }
        fn poll_event(&mut self) -> Option<DriverEvent> {
            None
        }
        fn poll_transmit(&mut self) -> Option<Vec<u8>> {
            None
        }
        fn poll_deadline(&self) -> Option<OffsetDateTime> {
            None
        }
    }

    /// A device that acknowledges every setpoint and acts on none of them.
    ///
    /// The failure mode that has no error in it anywhere: the write is
    /// accepted, the driver reports it, and the device holds where it was.
    #[derive(Debug)]
    struct Nodding {
        asset: AssetId,
        pending: Option<hems_drv::CommandOutcome>,
    }

    impl Driver for Nodding {
        fn asset(&self) -> &AssetId {
            &self.asset
        }
        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities::device()
        }
        fn on_bytes(&mut self, _: &[u8], _: OffsetDateTime) -> Result<(), DriverError> {
            Ok(())
        }
        fn on_timeout(&mut self, _: OffsetDateTime) {}
        fn command(
            &mut self,
            _: &hems_core::setpoint::Command,
            at: OffsetDateTime,
        ) -> Result<(), DriverError> {
            self.pending = Some(hems_drv::CommandOutcome {
                accepted: false,
                confirmed: None,
                at,
                detail: Some("the setpoint was stored and never switched on".into()),
            });
            Ok(())
        }
        fn poll_event(&mut self) -> Option<DriverEvent> {
            self.pending.take().map(DriverEvent::Command)
        }
        fn poll_transmit(&mut self) -> Option<Vec<u8>> {
            None
        }
        fn poll_deadline(&self) -> Option<OffsetDateTime> {
            None
        }
    }

    /// A grid driver that publishes nothing but the § 9 curtailment factor.
    #[derive(Debug)]
    struct Announcing {
        asset: AssetId,
        pending: Vec<DriverEvent>,
    }

    impl Driver for Announcing {
        fn asset(&self) -> &AssetId {
            &self.asset
        }
        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities::grid()
        }
        fn on_bytes(&mut self, _: &[u8], _: OffsetDateTime) -> Result<(), DriverError> {
            Ok(())
        }
        fn on_timeout(&mut self, _: OffsetDateTime) {}
        fn command(
            &mut self,
            c: &hems_core::setpoint::Command,
            _: OffsetDateTime,
        ) -> Result<(), DriverError> {
            Err(DriverError::Unsupported(format!("{c:?}")))
        }
        fn poll_event(&mut self) -> Option<DriverEvent> {
            (!self.pending.is_empty()).then(|| self.pending.remove(0))
        }
        fn poll_transmit(&mut self) -> Option<Vec<u8>> {
            None
        }
        fn poll_deadline(&self) -> Option<OffsetDateTime> {
            None
        }
    }

    #[test]
    fn a_grid_driver_that_measures_nothing_is_not_therefore_unheard() {
        // A § 14a driver reports limits, not measurements — `[A1 4.6]` is an
        // instruction and not a reading. Judging it by the age of a measurement
        // it never sends counts a perfectly connected Steuerbox as a device
        // nobody can hear, for ever: the readiness probe stays bad on a box that
        // is working, which is the state in which nobody looks at it again.
        let s = site();
        let mut r = Registry::new();
        r.register(
            Box::new(Announcing {
                asset: id("netzanschluss"),
                pending: vec![DriverEvent::Link(LinkState::Up)],
            }),
            &s,
        )
        .expect("the connection point");

        let observed = r.observe(None, NOW);

        assert!(
            observed.silent.is_empty(),
            "it said its link is up and it has nothing else to say: {:?}",
            observed.silent
        );
    }

    #[test]
    fn the_curtailment_factor_reaches_the_guard_as_a_fraction() {
        // `[MGCP-011]` crosses the wire as a *percentage* and `hems-grid` reads
        // a *fraction*, because that is what multiplies an inverter rating.
        // A factor of 70 arriving where 0,7 was meant is a roof allowed
        // seventy times what the connection point permits, and both numbers
        // look like a plausible configuration.
        let s = site();
        let mut r = Registry::new();
        r.register(
            Box::new(Announcing {
                asset: id("netzanschluss"),
                pending: vec![DriverEvent::FeedInFactor(hems_drv::FeedInFactor {
                    percent: 70.0,
                    at: NOW,
                })],
            }),
            &s,
        )
        .expect("the connection point");

        let observed = r.observe(None, NOW);

        assert_eq!(observed.limits.mgcp_factor, Some(0.7));
    }

    #[test]
    fn a_device_that_nods_and_does_nothing_is_named_rather_than_believed() {
        // `Ok(())` from `command` means the driver got the setpoint out, and
        // nothing more. The device then answers it, stores it and holds where
        // it was — which reaches every layer above as a house that was
        // commanded and is not doing it, hours before the meter says so. It is
        // *not* silent, and that distinction is the diagnosis: this device is
        // answering perfectly well.
        let s = site();
        let mut r = Registry::new();
        r.register(
            Box::new(Nodding {
                asset: id("wallbox"),
                pending: None,
            }),
            &s,
        )
        .expect("the wallbox");

        assert!(
            r.observe(None, NOW).disobedient.is_empty(),
            "nothing has been commanded yet, and a device nobody asked is not \
             one that refused"
        );

        r.command(
            &Setpoint {
                asset: id("wallbox"),
                command: hems_core::setpoint::Command::ChargingCurrent(Current::new(10.0)),
                reason: hems_core::setpoint::Reason::Fallback(
                    hems_core::setpoint::FallbackCause::NoPlan,
                ),
                at: NOW,
            },
            NOW,
        )
        .expect("the driver took it");

        let observed = r.observe(None, NOW);
        assert!(
            observed.silent.contains(&id("wallbox")),
            "it reports no measurement, so it is silent too — but that is the \
             other fault"
        );
        assert_eq!(
            observed.disobedient.get(&id("wallbox")).map(String::as_str),
            Some("the setpoint was stored and never switched on"),
            "and what the device said about it is the whole of the diagnosis"
        );
    }

    fn mute(asset: &str, caps: DriverCapabilities) -> Box<dyn Driver + Send> {
        Box::new(Mute {
            asset: id(asset),
            caps,
        })
    }

    #[test]
    fn a_driver_for_an_asset_the_site_does_not_have_is_refused() {
        // A typo in configuration. Without this it presents as a device that is
        // simply never commanded, which looks exactly like a device that had
        // nothing to do.
        let mut r = Registry::new();
        let err = r
            .register(mute("waermepumpe-2", DriverCapabilities::device()), &site())
            .expect_err("no such asset");
        assert!(matches!(err, RegistryError::NoSuchAsset(_)), "{err}");
    }

    #[test]
    fn two_drivers_that_both_command_one_asset_are_refused() {
        // One wallbox, two managers, and nothing downstream that could tell
        // which of them the contactor is obeying.
        let s = site();
        let mut r = Registry::new();
        r.register(mute("wallbox", DriverCapabilities::device()), &s)
            .expect("the first");
        let err = r
            .register(mute("wallbox", DriverCapabilities::commanding()), &s)
            .expect_err("the second");
        assert!(
            matches!(err, RegistryError::Duplicate(_, "command")),
            "{err}"
        );
    }

    #[test]
    fn two_drivers_that_both_measure_one_asset_are_refused() {
        // Two sources of truth about one meter, which is the same fault with
        // the other sign: the registry would keep whichever spoke last.
        let s = site();
        let mut r = Registry::new();
        r.register(mute("wallbox", DriverCapabilities::device()), &s)
            .expect("the first");
        let err = r
            .register(mute("wallbox", DriverCapabilities::meter()), &s)
            .expect_err("the second");
        assert!(
            matches!(err, RegistryError::Duplicate(_, "measure")),
            "{err}"
        );
    }

    #[test]
    fn one_driver_may_command_an_asset_while_another_reads_it() {
        // The configuration this workspace documented and could not run. The
        // charge point is commanded over Modbus and read over EEBUS EVCC/EVSOC —
        // whether there is a car on the cable and how full it is — and until the
        // rule was two-per-role rather than one-per-asset, `eebus-ev` could not
        // be registered on any household at all: alone it failed
        // `CannotCommand`, and beside its Modbus driver it failed `Duplicate`.
        let s = site();
        let mut r = Registry::new();
        r.register(mute("wallbox", DriverCapabilities::commanding()), &s)
            .expect("the one that drives it");
        r.register(mute("wallbox", DriverCapabilities::meter()), &s)
            .expect("and the one that watches it");
        r.register(mute("netzanschluss", DriverCapabilities::grid()), &s)
            .expect("a § 14a household needs something that hears the operator");
        r.validate(&s, NOW)
            .expect("a commanded and watched wallbox is a well-driven wallbox");
        assert_eq!(r.len(), 3, "and both of its drivers are kept, not merged");
    }

    #[test]
    fn a_controllable_asset_no_driver_can_command_is_refused() {
        // The check that is worth the most: the arbiter would spend every tick
        // computing a setpoint for this device, the driver would drop it, and
        // nothing anywhere would say so. It is exactly the shape of the defects
        // this workspace keeps finding in itself.
        //
        // Asked once the set is complete rather than driver by driver, because
        // the commanding driver may be the second one registered — judging each
        // as it arrived refused a perfectly good pair on the strength of the
        // order somebody listed them in.
        let s = site();
        let mut r = Registry::new();
        r.register(mute("wallbox", DriverCapabilities::meter()), &s)
            .expect("a meter alone is not yet a fault");
        r.register(mute("netzanschluss", DriverCapabilities::grid()), &s)
            .expect("and something hears the operator");
        let err = r
            .validate(&s, NOW)
            .expect_err("nothing can drive the wallbox");
        assert!(matches!(err, RegistryError::CannotCommand(_)), "{err}");
    }

    #[test]
    fn a_paragraph_14a_household_with_nothing_to_hear_a_reduction_is_refused() {
        // A household that believes it is participating and would never hear a
        // reduction. It cannot be caught driver by driver, because it is about
        // the *absence* of one — so it is a separate pass after registration.
        let s = site();
        let mut r = Registry::new();
        r.register(mute("wallbox", DriverCapabilities::device()), &s)
            .expect("a device driver");
        let err = r.validate(&s, NOW).expect_err("nothing hears the operator");
        assert!(matches!(err, RegistryError::NoGridDriver), "{err}");

        // …and with one, it is fine.
        r.register(
            Box::new(Lpc::new(
                id("netzanschluss"),
                Use::Lpc,
                Power::from_kw(10.5),
                core::time::Duration::from_secs(2 * 3600),
                NOW,
            )),
            &s,
        )
        .expect("a grid driver names the connection point, not a site asset");
        r.validate(&s, NOW).expect("now something can hear");
    }

    #[test]
    fn a_reading_that_stops_arriving_stops_being_offered_to_the_guard() {
        // Two questions, and both have to be asked. The guard has its own
        // freshness rule and falls back to a nameplate; the registry's job is to
        // say *which device* it is being conservative about, and that is what a
        // household is shown. `SILENCE` has to be the shorter of the two, or the
        // screen goes red after the guard has already stopped believing rather
        // than before.
        assert!(
            SILENCE < hems_realtime::GuardConfig::default().max_measurement_age,
            "the registry has to stop believing a device before the guard does"
        );

        let s = site();
        let mut r = Registry::new();
        let mut driver = Chatty::new(id("wallbox"));
        driver.say(NOW, Power::from_kw(7.0));
        r.register(Box::new(driver), &s).expect("a device driver");
        let _ = r.drain();

        let fresh = r.observe(None, NOW);
        assert!(fresh.silent.is_empty(), "it has just spoken");
        assert!(fresh.state.asset(&id("wallbox")).is_some());

        let later = NOW + SILENCE + Duration::seconds(1);
        let quiet = r.observe(None, later);
        assert!(
            quiet.silent.contains(&id("wallbox")),
            "and once it stops, the household is told which device it is"
        );
        assert!(
            quiet.state.asset(&id("wallbox")).is_none(),
            "and the stale reading is not handed to the guard as though it were current"
        );
    }

    #[test]
    fn a_device_that_notifies_on_change_is_not_silent_while_nothing_changes() {
        // The defect this capability exists for, and it was live on every
        // household with a hot-water tank. A subscription delivers a value when
        // it *changes*; a tank holding 52 °C and a room holding 21 °C change
        // nothing for hours. Judging them by the age of the last reading dropped
        // both from `SiteState` ten seconds after every reading — so
        // `dhw_model` saw no tank and `heat_pump_model` no house, and the two
        // stores the box had just learned to read were in the plan only in the
        // moments just after they moved.
        let s = site();
        let mut r = Registry::new();
        let mut driver = Chatty::new(id("warmwasser"));
        driver.caps = DriverCapabilities::device().on_change();
        driver.say_temperature(NOW, 52.0);
        r.register(Box::new(driver), &s).expect("a device driver");
        let _ = r.drain();

        let hours_later = NOW + Duration::hours(3);
        let quiet = r.observe(None, hours_later);
        assert!(
            quiet.silent.is_empty(),
            "a tank that has not changed is not a tank nobody can hear"
        );
        assert_eq!(
            quiet
                .state
                .asset(&id("warmwasser"))
                .and_then(|m| m.temperature_c),
            Some(52.0),
            "and the plan still has a tank to move"
        );
    }

    #[test]
    fn a_device_that_notifies_on_change_and_has_never_spoken_is_still_silent() {
        // The other half, and the one the exemption must not swallow: a peer
        // that connected and said nothing at all is a peer nobody has heard
        // from. Only a reading that *arrived* is one that can still be true.
        let s = site();
        let mut r = Registry::new();
        let mut driver = Chatty::new(id("warmwasser"));
        driver.caps = DriverCapabilities::device().on_change();
        r.register(Box::new(driver), &s).expect("a device driver");
        let _ = r.drain();

        let quiet = r.observe(None, NOW + Duration::hours(3));
        assert!(quiet.silent.contains(&id("warmwasser")));
    }

    #[test]
    fn a_polled_device_is_still_judged_by_the_age_of_its_reading() {
        // The exemption is for the drivers whose peers notify them, and for no
        // others: a SunSpec inverter reads every second, so a reading older than
        // a few of those means the device stopped answering while its socket
        // stayed open — which only the timestamp can say.
        let s = site();
        let mut r = Registry::new();
        let mut driver = Chatty::new(id("pv"));
        driver.say(NOW, Power::from_kw(3.0));
        r.register(Box::new(driver), &s).expect("a device driver");
        let _ = r.drain();
        assert!(
            r.observe(None, NOW + SILENCE + Duration::seconds(1))
                .silent
                .contains(&id("pv"))
        );
    }

    /// A driver that reports whatever it is told to, so the registry's own
    /// bookkeeping can be tested without a protocol.
    #[derive(Debug)]
    struct Chatty {
        asset: AssetId,
        caps: DriverCapabilities,
        events: Vec<hems_drv::DriverEvent>,
    }

    impl Chatty {
        fn new(asset: AssetId) -> Self {
            Self {
                asset,
                caps: DriverCapabilities::device(),
                events: vec![hems_drv::DriverEvent::Link(LinkState::Up)],
            }
        }

        /// A temperature rather than a power — what a tank and a room report.
        fn say_temperature(&mut self, at: OffsetDateTime, degrees: f64) {
            let mut m = Measurement::at(at);
            m.temperature_c = Some(degrees);
            self.events.push(hems_drv::DriverEvent::Measured(m));
        }

        /// Report `power`, observed at `at`.
        fn say(&mut self, at: OffsetDateTime, power: Power) {
            let mut m = Measurement::at(at);
            m.power = Some(power);
            self.events.push(hems_drv::DriverEvent::Measured(m));
        }
    }

    impl Driver for Chatty {
        fn asset(&self) -> &AssetId {
            &self.asset
        }
        fn capabilities(&self) -> DriverCapabilities {
            self.caps
        }
        fn on_bytes(&mut self, _: &[u8], _: OffsetDateTime) -> Result<(), DriverError> {
            Ok(())
        }
        fn on_timeout(&mut self, _: OffsetDateTime) {}
        fn command(
            &mut self,
            _: &hems_core::setpoint::Command,
            _: OffsetDateTime,
        ) -> Result<(), DriverError> {
            Ok(())
        }
        fn poll_event(&mut self) -> Option<hems_drv::DriverEvent> {
            if self.events.is_empty() {
                None
            } else {
                Some(self.events.remove(0))
            }
        }
        fn poll_transmit(&mut self) -> Option<Vec<u8>> {
            None
        }
        fn poll_deadline(&self) -> Option<OffsetDateTime> {
            None
        }
    }

    #[test]
    fn a_reduction_reaches_the_guard_as_a_grid_limit() {
        // The whole point of the registry: what a driver heard on a wire becomes
        // the number the guard enforces, with the failsafe distinguished from a
        // command because they are different things in the evidence record.
        let s = site();
        let mut r = Registry::new();
        let mut lpc = Lpc::new(
            id("netzanschluss"),
            Use::Lpc,
            Power::from_kw(10.5),
            core::time::Duration::from_secs(2 * 3600),
            NOW,
        );
        // Contact, then a reduction to 4,2 kW.
        // The heartbeat has to be *recent* when the write lands: outside the
        // controlled states a limit is only evaluated if one arrived in the last
        // sixty seconds, so a write a full minute after the last beat is refused.
        for beat in 0..4 {
            lpc.on_heartbeat(NOW + Duration::seconds(beat * 30));
        }
        let outcome = lpc.on_limit(
            &hems_drv::eebus::LimitWrite::active(4_200.0),
            NOW + Duration::seconds(110),
        );
        assert!(outcome.is_accepted(), "{outcome:?}");
        r.register(Box::new(lpc), &s).expect("a grid driver");

        let drained = r.drain();
        assert!(!drained.is_empty(), "the driver had something to say");
        let limits = r.limits();
        assert_eq!(limits.steuve_ceiling, Some(Power::from_kw(4.2)));
        assert!(!limits.in_failsafe, "the operator asked for this one");
        assert!(limits.steuve_since.is_some(), "and the record needs when");
    }

    #[test]
    fn a_driver_that_says_its_link_is_stale_contributes_nothing() {
        // A stale reading is worse than none: the guard's answer to an absent
        // measurement is already the safe one — a controllable device nobody can
        // hear is assumed to be running flat out — and a number that merely
        // *looks* fresh defeats it.
        let s = site();
        let mut r = Registry::new();
        let mut d = SunSpec::new(id("pv"), 1, Cadence::default());
        // Nothing ever answers it, so it gives up and says so.
        d.on_timeout(NOW);
        d.on_timeout(NOW + Duration::seconds(30));
        r.register(Box::new(d), &s).expect("an inverter driver");
        let _ = r.drain();

        let observed = r.observe(None, NOW + Duration::seconds(30));
        assert!(
            observed.state.assets.is_empty(),
            "a driver that has said it no longer knows contributes nothing"
        );
        assert!(observed.silent.contains(&id("pv")));
    }

    #[test]
    fn a_command_for_an_asset_no_driver_speaks_for_is_an_error() {
        // The difference between a device that is idle and one that is
        // unreachable, which is the difference between a saving and a surprise.
        let mut r = Registry::new();
        let setpoint = Setpoint::new(
            id("wallbox"),
            hems_core::setpoint::Command::ChargingCurrent(Current::new(16.0)),
            hems_core::setpoint::Reason::Fallback(hems_core::setpoint::FallbackCause::NoPlan),
            NOW,
        )
        .expect("a valid setpoint");
        let err = r
            .command(&setpoint, NOW)
            .expect_err("nothing speaks for it");
        assert!(
            matches!(err, DriverError::NoDriver(_)),
            "and it is its own error rather than a general refusal, because \
             `this device cannot do that` is a fault to look into and `this \
             device has no driver` is a configuration fact that is equally true \
             on every tick: {err:?}"
        );
    }

    #[test]
    fn the_controllable_devices_nothing_speaks_for_are_named() {
        // Not refused, and that is the judgement. A driver that *cannot command*
        // a controllable asset is a declaration contradicting itself and is
        // refused; an asset with no driver at all is a box part-way through
        // commissioning, or a household that owns a device hems has no driver
        // for yet. Refusing would make the site model a list of what is wired
        // rather than a list of what is there.
        //
        // But the arbiter decides a setpoint for each of them every tick and has
        // nowhere to send it, so they are counted rather than left implicit.
        let s = site();
        let mut r = Registry::new();
        assert!(
            r.undriven(&s).count() > 0,
            "an empty registry speaks for none of the household's devices"
        );

        r.register(mute("wallbox", DriverCapabilities::device()), &s)
            .expect("a device driver");
        let named: Vec<String> = r.undriven(&s).map(ToString::to_string).collect();
        assert!(
            !named.iter().any(|a| a == "wallbox"),
            "the one with a driver drops off the list: {named:?}"
        );
        assert!(
            named.iter().any(|a| a == "battery"),
            "and the ones without stay on it: {named:?}"
        );
        assert!(
            !named.iter().any(|a| a == "haushalt"),
            "the household's own base load is not controllable and was never \
             going to be commanded: {named:?}"
        );
    }

    #[test]
    fn the_devices_running_on_a_nameplate_are_named() {
        // A curtailed inverter that cannot say what it *could* produce is one
        // whose curtailment lifts on an assumption. A household is entitled to
        // know which of its devices are in that position, so the registry says
        // rather than leaving it implicit.
        let s = site();
        let mut r = Registry::new();
        r.register(Box::new(SunSpec::new(id("pv"), 1, Cadence::default())), &s)
            .expect("an inverter driver");
        let assumed: Vec<&AssetId> = r.assumed_available_power().collect();
        assert_eq!(
            assumed,
            vec![&id("pv")],
            "no model 701, so its available power is a nameplate"
        );
        let _ = PhaseConnection::Three;
    }
}
