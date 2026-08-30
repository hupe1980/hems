//! § 9 EEG — feed-in limitation, and what the Solarspitzengesetz changed.
//!
//! Two limits apply to what a photovoltaic system may push into the grid, and
//! they have different references, which is the detail that makes this worth a
//! module of its own:
//!
//! * the **60 % cap** of the Solarspitzengesetz (in force 25.02.2025) is a
//!   fraction of the *installed* capacity of a system, and applies until an
//!   intelligent metering system with a control device is **in operation** — a
//!   technical fact, not a commercial one (see [`CapRelief`]);
//! * the **EEBUS `MGCP` feed-in factor** (`[MGCP-011]`) is a fraction of the
//!   *cumulated nominal AC power* of the inverters in the building:
//!   `P_feed-in ≤ factor × Σ P_PV,AC,nom`.
//!
//! Those two references differ on almost every real installation, because
//! inverters are routinely undersized against the modules. Taking the smaller of
//! the two results — which is what [`FeedInLimits::ceiling`] does — is the only
//! reading that satisfies both.
//!
//! The third mechanism, § 51 EEG, does not limit anything: in quarter hours with
//! a negative day-ahead price a supported system simply earns nothing. That is a
//! price fact, so it lives in `hems-tariff`; what belongs here is only the
//! observation that it makes *self-consumption or curtailment* the rational
//! choice, not feeding in.

use hems_core::asset::{Asset, PvArray};
use hems_core::prelude::{Power, Site};
use time::Date;

pub use hems_core::asset::CapRelief;

/// The day the Solarspitzengesetz took effect.
pub const SOLARSPITZEN_START: Date = time::macros::date!(2025 - 02 - 25);

/// The share of installed capacity a capped system may feed in.
pub const CAP_FRACTION: f64 = 0.60;

/// The lower bound of the capped size class, in installed DC power.
pub const CAP_MIN_SIZE: Power = Power::new_const(2_000.0);

/// The upper bound of the capped size class.
pub const CAP_MAX_SIZE: Power = Power::new_const(100_000.0);

/// What the site's generation looks like to § 9 EEG.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GenerationProfile {
    /// Installed DC power — the reference of the 60 % cap.
    pub installed_dc: Power,
    /// Cumulated nominal AC power of the inverters — the reference of the
    /// EEBUS `MGCP` feed-in factor.
    pub installed_ac_nominal: Power,
    /// When the system went into operation.
    #[cfg_attr(feature = "serde", serde(default))]
    pub commissioned_at: Option<Date>,
    /// What, if anything, has lifted the 60 % cap for this system.
    #[cfg_attr(feature = "serde", serde(default))]
    pub relief: CapRelief,
}

impl GenerationProfile {
    /// Whether the 60 % cap applies.
    ///
    /// It applies to systems commissioned from 25.02.2025 with an installed
    /// capacity between 2 and 100 kWp, until a [`CapRelief`] lifts it.
    ///
    /// An **unknown** commissioning date leaves the cap off, because the cap only
    /// ever applied to systems commissioned after 25.02.2025 and applying it to
    /// an unknown one would curtail a system the statute never reached. That is
    /// the opposite default from § 14a participation, and for the opposite
    /// reason: there, silence risks exceeding a network operator's limit; here it
    /// only risks throwing away a household's own energy.
    #[must_use]
    pub fn cap_applies(&self) -> bool {
        let in_size_class = self.installed_dc >= CAP_MIN_SIZE && self.installed_dc <= CAP_MAX_SIZE;
        let new_enough = self
            .commissioned_at
            .is_some_and(|d| d >= SOLARSPITZEN_START);
        in_size_class && new_enough && !self.relief.lifts_cap()
    }

    /// The ceiling the 60 % cap puts on feed-in, if it applies.
    #[must_use]
    pub fn statutory_cap(&self) -> Option<Power> {
        self.cap_applies().then(|| self.installed_dc * CAP_FRACTION)
    }
}

/// Everything currently limiting feed-in at the connection point.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FeedInLimits {
    /// The statutory 60 % cap, when it applies.
    pub statutory_cap: Option<Power>,
    /// The EEBUS `MGCP` feed-in power limitation factor, a fraction in `[0, 1]`
    /// of the cumulated nominal AC power (`[MGCP-011]`).
    pub mgcp_factor: Option<f64>,
    /// An absolute limit from an active LPP session — the same write as
    /// `[LPC-011]` with the direction flipped, which is why one machine serves
    /// both (see [`crate::lpc`]).
    pub lpp_limit: Option<Power>,
}

impl FeedInLimits {
    /// Collect the limits that apply to a generation profile.
    #[must_use]
    pub fn for_profile(
        profile: &GenerationProfile,
        mgcp_factor: Option<f64>,
        lpp_limit: Option<Power>,
    ) -> Self {
        Self {
            statutory_cap: profile.statutory_cap(),
            mgcp_factor,
            lpp_limit,
        }
    }

    /// The largest feed-in permitted, as a non-negative magnitude.
    ///
    /// `None` means nothing is limiting feed-in. The three sources have
    /// different references, so they are resolved into watts first and the
    /// smallest wins.
    #[must_use]
    pub fn ceiling(&self, profile: &GenerationProfile) -> Option<Power> {
        let from_factor = self.factor_ceiling(profile);
        [self.statutory_cap, from_factor, self.lpp_limit]
            .into_iter()
            .flatten()
            .reduce(Power::min)
    }

    /// The ceiling the EEBUS `MGCP` feed-in factor implies, if one was received.
    #[must_use]
    pub fn factor_ceiling(&self, profile: &GenerationProfile) -> Option<Power> {
        self.mgcp_factor
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| profile.installed_ac_nominal * f.min(1.0))
    }

    /// Why the ceiling has the value it has — for the reason chain a user sees.
    ///
    /// The three sources are named separately because they are three different
    /// conversations: an LPP session is the network operator reducing this
    /// house right now, the `MGCP` factor is the § 9 EEG limitation announced
    /// over the wire, and the statutory cap is the law applying by itself. An
    /// earlier version reported the factor as an LPP session, which told the
    /// household the operator had intervened when it had not.
    #[must_use]
    pub fn binding_rule(
        &self,
        profile: &GenerationProfile,
    ) -> Option<hems_core::prelude::GuardRule> {
        use hems_core::prelude::GuardRule;
        let ceiling = self.ceiling(profile)?;
        // A live session first, because a network operator asking for something
        // right now is the most specific thing that can be happening. Everything
        // else — the wire's announcement of the statutory limit, and the statute
        // applying by itself — is § 9 EEG, and saying so is the whole point:
        // reporting the feed-in factor as an LPP session tells a household the
        // operator intervened when it did not.
        if self.lpp_limit == Some(ceiling) {
            Some(GuardRule::Lpp)
        } else {
            Some(GuardRule::Para9Cap)
        }
    }
}

impl GenerationProfile {
    /// The § 9 EEG facts of one array.
    #[must_use]
    pub fn of_array(pv: &PvArray) -> Self {
        Self {
            installed_dc: pv.kwp_dc,
            installed_ac_nominal: pv.ac_nominal,
            commissioned_at: pv.meta.commissioned_at,
            relief: pv.cap_relief,
        }
    }

    /// The § 9 EEG facts of a whole site, summed over its arrays.
    ///
    /// `None` when the site has no photovoltaic system at all — there is nothing
    /// for the cap to apply to, and returning a zero-sized profile would answer
    /// "capped at 0 kW", which is the wrong kind of wrong.
    ///
    /// Two readings are deliberate. The **capacities add up**, because § 9 Abs. 1
    /// applies to the installierte Leistung at one connection point and a
    /// household that splits a roof across two inverters has not thereby left the
    /// size class. And the relief is taken as lifted only when it is lifted for
    /// **every** array: an intelligent metering system with a control device that
    /// reaches one of two inverters does not lift the cap on the other, and the
    /// cap is measured at the connection point where both of them feed.
    #[must_use]
    pub fn of_site(site: &Site) -> Option<Self> {
        let mut profile: Option<Self> = None;
        for asset in &site.assets {
            let Asset::Pv(pv) = asset else { continue };
            let next = Self::of_array(pv);
            profile = Some(match profile {
                None => next,
                Some(acc) => Self {
                    installed_dc: acc.installed_dc + next.installed_dc,
                    installed_ac_nominal: acc.installed_ac_nominal + next.installed_ac_nominal,
                    // The oldest date, because the cap turns on the *system*
                    // reaching the size class, and a later extension does not
                    // make the original array new.
                    commissioned_at: match (acc.commissioned_at, next.commissioned_at) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    },
                    relief: if acc.relief.lifts_cap() && next.relief.lifts_cap() {
                        acc.relief
                    } else {
                        CapRelief::None
                    },
                },
            });
        }
        profile
    }
}

/// The ceiling § 9 EEG, the EEBUS `MGCP` factor and an LPP session together put
/// on this site's feed-in, and which of them set it.
///
/// The one call a daemon needs: three types and one entry point, so that
/// nothing has to assemble a [`GenerationProfile`] by hand to find out what a
/// site may feed in.
///
/// `None` means nothing is limiting feed-in.
#[must_use]
pub fn site_feed_in_ceiling(
    site: &Site,
    mgcp_factor: Option<f64>,
    lpp_limit: Option<Power>,
) -> Option<(Power, hems_core::prelude::GuardRule)> {
    let profile = GenerationProfile::of_site(site)?;
    let limits = FeedInLimits::for_profile(&profile, mgcp_factor, lpp_limit);
    let ceiling = limits.ceiling(&profile)?;
    Some((ceiling, limits.binding_rule(&profile)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(kwp: f64, commissioned: Option<Date>) -> GenerationProfile {
        GenerationProfile {
            installed_dc: Power::from_kw(kwp),
            installed_ac_nominal: Power::from_kw(kwp * 0.8),
            commissioned_at: commissioned,
            relief: CapRelief::None,
        }
    }

    #[test]
    fn a_new_small_system_without_a_control_device_is_capped_at_sixty_percent() {
        let p = profile(9.8, Some(time::macros::date!(2025 - 06 - 01)));
        assert!(p.cap_applies());
        assert!((p.statutory_cap().unwrap().kw() - 5.88).abs() < 1e-9);
    }

    #[test]
    fn a_system_commissioned_before_the_law_is_not_capped() {
        assert!(!profile(9.8, Some(time::macros::date!(2025 - 02 - 24))).cap_applies());
        assert!(
            profile(9.8, Some(SOLARSPITZEN_START)).cap_applies(),
            "the day itself counts"
        );
    }

    #[test]
    fn only_a_working_control_path_lifts_the_cap() {
        for relief in [
            CapRelief::ImsysWithControl,
            CapRelief::DirektvermarktungFernsteuerbar,
        ] {
            let mut p = profile(9.8, Some(time::macros::date!(2025 - 06 - 01)));
            p.relief = relief;
            assert!(!p.cap_applies(), "{relief:?} should lift the cap");
        }
        // And the default is that nothing has.
        assert!(profile(9.8, Some(time::macros::date!(2025 - 06 - 01))).cap_applies());
    }

    #[test]
    fn a_balcony_system_and_a_large_roof_are_both_outside_the_size_class() {
        assert!(!profile(0.8, Some(time::macros::date!(2025 - 06 - 01))).cap_applies());
        assert!(!profile(150.0, Some(time::macros::date!(2025 - 06 - 01))).cap_applies());
    }

    #[test]
    fn the_mgcp_factor_is_a_fraction_of_inverter_power_not_of_module_power() {
        // 10 kWp of modules behind an 8 kW inverter, factor 0,7:
        // [MGCP-011] gives 0,7 × 8 kW = 5,6 kW — not 0,7 × 10 kW.
        let p = profile(10.0, None);
        let limits = FeedInLimits::for_profile(&p, Some(0.7), None);
        assert!((limits.ceiling(&p).unwrap().kw() - 5.6).abs() < 1e-9);
    }

    #[test]
    fn the_strictest_of_the_three_limits_wins() {
        let p = profile(10.0, Some(time::macros::date!(2025 - 06 - 01)));
        // Statutory: 6 kW. MGCP: 0,9 × 8 = 7,2 kW. LPP: 4 kW.
        let limits = FeedInLimits::for_profile(&p, Some(0.9), Some(Power::from_kw(4.0)));
        assert_eq!(limits.ceiling(&p), Some(Power::from_kw(4.0)));
        assert_eq!(
            limits.binding_rule(&p),
            Some(hems_core::prelude::GuardRule::Lpp)
        );

        // Without the LPP session the statutory cap binds.
        let limits = FeedInLimits::for_profile(&p, Some(0.9), None);
        assert_eq!(limits.ceiling(&p), Some(Power::from_kw(6.0)));
        assert_eq!(
            limits.binding_rule(&p),
            Some(hems_core::prelude::GuardRule::Para9Cap)
        );
    }

    #[test]
    fn the_feed_in_factor_is_reported_as_the_paragraph_9_cap_not_as_an_lpp_session() {
        // [MGCP-011] carries the § 9 EEG limitation as a percentage of the
        // inverters' nominal power. Naming it "LPP" would tell the household the
        // network operator had intervened when nothing of the sort happened.
        let p = profile(10.0, None);
        let limits = FeedInLimits::for_profile(&p, Some(0.5), None);
        assert_eq!(limits.ceiling(&p), Some(Power::from_kw(4.0)));
        assert_eq!(
            limits.binding_rule(&p),
            Some(hems_core::prelude::GuardRule::Para9Cap)
        );
    }

    #[test]
    fn no_limits_means_no_ceiling() {
        let p = profile(10.0, Some(time::macros::date!(2024 - 01 - 01)));
        assert_eq!(FeedInLimits::for_profile(&p, None, None).ceiling(&p), None);
    }

    #[test]
    fn a_nonsense_factor_is_ignored_rather_than_producing_a_nonsense_ceiling() {
        let p = profile(10.0, None);
        for bad in [f64::NAN, -0.5] {
            let limits = FeedInLimits::for_profile(&p, Some(bad), None);
            assert_eq!(limits.ceiling(&p), None, "factor {bad}");
        }
        // A factor above 1 is clamped rather than dropped: it is not a limit at
        // all, and clamping keeps the arithmetic total.
        let limits = FeedInLimits::for_profile(&p, Some(1.5), None);
        assert_eq!(limits.ceiling(&p), Some(p.installed_ac_nominal));
    }
}
