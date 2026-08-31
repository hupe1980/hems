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
         grid limits, so a reduction could never arrive"
    )]
    NoGridDriver,
}

/// A set of drivers, and what they have told us.
pub struct Registry {
    entries: Vec<Entry>,
    /// The limits the grid drivers have reported, as the guard wants them.
    limits: GridLimits,
}

struct Entry {
    driver: Box<dyn Driver>,
    asset: AssetId,
    link: LinkState,
    /// The last measurement this driver produced, if any.
    latest: Option<Measurement>,
    /// The conductors it reports, for a switchable charge point.
    phases: Option<hems_core::prelude::PhaseMode>,
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
    pub fn register(&mut self, driver: Box<dyn Driver>, site: &Site) -> Result<(), RegistryError> {
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
    /// [`RegistryError::NoGridDriver`] where the site takes part in the
    /// netzorientierte Steuerung and nothing can hear a reduction.
    pub fn validate(&self, site: &Site, now: OffsetDateTime) -> Result<(), RegistryError> {
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

    /// Time passed. Every driver is told, because each decides for itself
    /// whether its own deadline has gone by.
    pub fn on_timeout(&mut self, now: OffsetDateTime) {
        for entry in &mut self.entries {
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

    /// Send one setpoint to whichever driver speaks for its asset.
    ///
    /// # Errors
    /// [`DriverError::Unsupported`] where no driver speaks for the asset — which
    /// is a command that would otherwise be dropped in silence, and the
    /// difference between a device that is idle and one that is unreachable.
    pub fn command(&mut self, setpoint: &Setpoint, now: OffsetDateTime) -> Result<(), DriverError> {
        let Some(entry) = self.entries.iter_mut().find(|e| e.asset == setpoint.asset) else {
            return Err(DriverError::Unsupported(format!(
                "no driver speaks for {}",
                setpoint.asset
            )));
        };
        entry.driver.command(&setpoint.command, now)
    }

    /// Drain every driver's events into the registry's own view.
    ///
    /// Returns what was drained, so a caller that wants to log or journal them
    /// can — the § 14a evidence record is built from exactly these.
    pub fn drain(&mut self) -> Vec<(AssetId, DriverEvent)> {
        let mut out = Vec::new();
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
                out.push((entry.asset.clone(), event));
            }
        }
        out
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
    pub fn state(&self, grid_meter: Option<&AssetId>, _now: OffsetDateTime) -> SiteState {
        let mut state = SiteState::default();
        for entry in &self.entries {
            let Some(measurement) = entry.latest else {
                continue;
            };
            if entry.link == LinkState::Stale || entry.link == LinkState::Down {
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

    /// The assets no driver has been heard from.
    ///
    /// The number a box should put on a screen: a device nobody can hear is one
    /// the guard is being conservative about, and being conservative costs the
    /// household money.
    pub fn silent(&self) -> impl Iterator<Item = &AssetId> {
        self.entries
            .iter()
            .filter(|e| e.link != LinkState::Up)
            .map(|e| &e.asset)
    }
}

/// How long a driver may be silent before the registry stops believing it.
///
/// Deliberately **shorter** than the guard's own `max_measurement_age`: the two
/// answer different questions, and if this were the longer of the two a reading
/// could age out of the guard while the registry still called the link healthy.
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
    #[must_use]
    pub fn observe(&self, grid_meter: Option<&AssetId>, now: OffsetDateTime) -> Observed {
        Observed {
            state: self.state(grid_meter, now),
            limits: self.limits(),
            silent: self.silent().cloned().collect(),
            assumed_available: self.assumed_available_power().cloned().collect(),
        }
    }
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

    fn mute(asset: &str, caps: DriverCapabilities) -> Box<dyn Driver> {
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
        assert!(matches!(err, DriverError::Unsupported(_)), "{err:?}");
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
