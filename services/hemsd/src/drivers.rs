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
use hems_drv::{Driver, DriverError, DriverEvent, LimitDirection, LimitSource, LinkState};
use hems_realtime::guard::{GridLimits, SiteState};
use time::{Duration, OffsetDateTime};

/// Why a set of drivers does not describe this site.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A driver names an asset the site does not have.
    #[error("a driver speaks for `{0}`, which this site does not have")]
    NoSuchAsset(String),
    /// Two drivers claim the same asset.
    #[error("two drivers speak for `{0}`")]
    Duplicate(String),
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
}

struct Entry {
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
        self.latest.is_some_and(|m| now - m.at <= SILENCE)
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("drivers", &self.entries.len())
            .field("limits", &self.limits)
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
    /// # Errors
    /// [`RegistryError`] for any of the four mismatches in the module note.
    pub fn register(
        &mut self,
        driver: Box<dyn Driver + Send>,
        site: &Site,
    ) -> Result<(), RegistryError> {
        let asset = driver.asset().clone();
        let caps = driver.capabilities();

        // A grid driver speaks for the connection point, which is not one of the
        // site's assets — so only a device driver has to name one that exists.
        if !caps.reports_grid_limits {
            let Some(found) = site.asset(&asset) else {
                return Err(RegistryError::NoSuchAsset(asset.to_string()));
            };
            if hems_realtime::guard::is_controllable(found) && !caps.accepts_commands {
                return Err(RegistryError::CannotCommand(asset.to_string()));
            }
        }
        if self.entries.iter().any(|e| e.asset == asset) {
            return Err(RegistryError::Duplicate(asset.to_string()));
        }

        self.entries.push(Entry {
            driver,
            asset,
            link: LinkState::Down,
            latest: None,
            pending: Vec::new(),
            phases: None,
        });
        Ok(())
    }

    /// Check the registered set against what the site expects of it.
    ///
    /// Called once, after everything is registered — the § 14a question cannot
    /// be answered driver by driver, because it is about the *absence* of one.
    ///
    /// # Errors
    /// [`RegistryError::Uncommissioned`] where nothing is registered at all, and
    /// [`RegistryError::NoGridDriver`] where the site takes part in the
    /// netzorientierte Steuerung and nothing can hear a reduction.
    pub fn validate(&self, site: &Site, now: OffsetDateTime) -> Result<(), RegistryError> {
        if self.entries.is_empty() {
            return Err(RegistryError::Uncommissioned);
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
        asset: &AssetId,
        bytes: &[u8],
        now: OffsetDateTime,
    ) -> Result<(), DriverError> {
        let Some(entry) = self.entries.iter_mut().find(|e| e.asset == *asset) else {
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
    pub fn on_link(&mut self, asset: &AssetId, state: LinkState, now: OffsetDateTime) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.asset == *asset) else {
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
    pub fn on_timeout_of(&mut self, asset: &AssetId, now: OffsetDateTime) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.asset == *asset) {
            entry.driver.on_timeout(now);
        }
    }

    /// The next bytes to put on a wire, and which driver's wire it is.
    pub fn poll_transmit(&mut self) -> Option<(AssetId, Vec<u8>)> {
        for entry in &mut self.entries {
            if let Some(bytes) = entry.driver.poll_transmit() {
                return Some((entry.asset.clone(), bytes));
            }
        }
        None
    }

    /// The next bytes for one driver's own wire.
    pub fn poll_transmit_of(&mut self, asset: &AssetId) -> Option<Vec<u8>> {
        self.entries
            .iter_mut()
            .find(|e| e.asset == *asset)?
            .driver
            .poll_transmit()
    }

    /// When one driver wants waking.
    #[must_use]
    pub fn deadline_of(&self, asset: &AssetId) -> Option<OffsetDateTime> {
        self.entries
            .iter()
            .find(|e| e.asset == *asset)?
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
        let Some(entry) = self.entries.iter_mut().find(|e| e.asset == setpoint.asset) else {
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
                    DriverEvent::Command(_) => {}
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
#[derive(Debug, Clone)]
pub struct Observed {
    /// What the house is doing.
    pub state: SiteState,
    /// What the network operator is asking for.
    pub limits: GridLimits,
    /// What could not be heard from.
    pub silent: BTreeSet<AssetId>,
    /// Devices whose available power is a nameplate rather than a reading.
    pub assumed_available: BTreeSet<AssetId>,
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
    fn two_drivers_for_one_asset_are_refused() {
        // Two sources of truth about one meter, and nothing downstream that
        // could tell which of them to believe.
        let s = site();
        let mut r = Registry::new();
        r.register(mute("wallbox", DriverCapabilities::device()), &s)
            .expect("the first");
        let err = r
            .register(mute("wallbox", DriverCapabilities::device()), &s)
            .expect_err("the second");
        assert!(matches!(err, RegistryError::Duplicate(_)), "{err}");
    }

    #[test]
    fn a_controllable_asset_whose_driver_cannot_command_it_is_refused() {
        // The check that is worth the most: the arbiter would spend every tick
        // computing a setpoint for this device, the driver would drop it, and
        // nothing anywhere would say so. It is exactly the shape of the defects
        // this workspace keeps finding in itself.
        let mut r = Registry::new();
        let err = r
            .register(mute("wallbox", DriverCapabilities::meter()), &site())
            .expect_err("a meter cannot drive a wallbox");
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

    /// A driver that reports whatever it is told to, so the registry's own
    /// bookkeeping can be tested without a protocol.
    #[derive(Debug)]
    struct Chatty {
        asset: AssetId,
        events: Vec<hems_drv::DriverEvent>,
    }

    impl Chatty {
        fn new(asset: AssetId) -> Self {
            Self {
                asset,
                events: vec![hems_drv::DriverEvent::Link(LinkState::Up)],
            }
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
            DriverCapabilities::device()
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
