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
//!
//! # The scope of the statutory cap is three conditions, and two of them are not
//! sizes
//!
//! It is tempting to read § 9 Abs. 2 as *"between 2 and 100 kWp, from
//! 25.02.2025"* and be done. Every part of that is slightly wrong, and the
//! errors point in both directions:
//!
//! * There is **no lower bound**. § 9 Abs. 2 S. 1 Nr. 3 reaches every system
//!   below 25 kW that is zugeordnet der Einspeisevergütung; the 2 kW in the
//!   statute belongs to S. 4, which exempts **Steckersolargeräte** — and only
//!   those, and only with at most 800 VA of inverter, and only behind a
//!   Letztverbraucher's Entnahmestelle. Read as a size class it exempts every
//!   small roof array from a cap the statute puts on it, which is a household
//!   feeding in above a statutory limit and nothing noticing.
//! * The **upper bound is exclusive**: Nr. 2 is *"ab 25 Kilowatt und von weniger
//!   als 100 Kilowatt"*, and Nr. 1 — from 100 kW up — carries no percentage at
//!   all. Between 25 and 100 kW the percentage is the same 60 %, so the two
//!   Nummern collapse into one range here.
//! * The **date is a window, not a threshold**. § 100 Abs. 3b disapplies both
//!   Nummern to systems commissioned between 01.01.2023 and 24.02.2025, so
//!   "before the Solarspitzengesetz" does not mean "uncapped": a system from
//!   2019 still carries the obligation of the EEG version applicable to it,
//!   which it met either with equipment the network operator can reduce it with
//!   or by limiting itself to **70 %** (§ 100 Abs. 3 S. 2 Nr. 2). Which of the
//!   two is a fact about the installation and is declared, not guessed
//!   ([`Para9Status::legacy_70_percent`]).
//!
//! [`StatutoryLimit`] is the answer to all three at once, and
//! [`GenerationProfile::statutory_limit`] is where the decision tree lives.

use hems_core::asset::{Asset, PvArray};
use hems_core::prelude::{Power, Site};
use time::Date;

pub use hems_core::asset::{CapRelief, Para9Status};

/// The day the Solarspitzengesetz took effect.
///
/// § 100 Abs. 3b EEG disapplies § 9 Abs. 2 S. 1 Nr. 2 lit. b and Nr. 3 to
/// systems commissioned between 01.01.2023 and this day, so a system newer than
/// this is in the 60 % regime and one from the window in between is in none.
pub const SOLARSPITZEN_START: Date = time::macros::date!(2025 - 02 - 25);

/// The first day of the window § 100 Abs. 3b EEG exempts.
///
/// A system commissioned **before** this carries the obligation of the EEG
/// version applicable to it — see [`Para9Status::legacy_70_percent`].
pub const EXEMPT_WINDOW_START: Date = time::macros::date!(2023 - 01 - 01);

/// The share of installed capacity a capped system may feed in,
/// § 9 Abs. 2 S. 1 Nr. 2 lit. b and Nr. 3 EEG.
pub const CAP_FRACTION: f64 = 0.60;

/// The share a pre-2023 system that chose the Einspeisebegrenzung instead of
/// remote-control equipment holds itself to, § 100 Abs. 3 S. 2 Nr. 2 EEG.
pub const LEGACY_CAP_FRACTION: f64 = 0.70;

/// The size at which § 9 Abs. 2 S. 1 Nr. 1 EEG takes over, **exclusive**.
///
/// Nr. 2 is *"ab 25 Kilowatt und von **weniger als** 100 Kilowatt"* and Nr. 1 —
/// everything from 100 kW up — carries no percentage cap at all, only the duty
/// to be remotely reducible. So a system of exactly 100 kW is not capped, and
/// the comparison has to be strict. The same care as
/// [`crate::para14a::FALLGRUPPE_THRESHOLD`]'s *"mehr als 4,2 kW"*.
///
/// There is deliberately **no lower bound**. The 2 kW belongs to the
/// Steckersolargerät exemption of § 9 Abs. 2 S. 4 (see [`STECKERSOLAR_MAX_DC`]),
/// which is a statement about a kind of installation and not a size class —
/// read as a floor it exempts every small roof array from a cap the statute puts
/// on it.
pub const CAP_MAX_SIZE_EXCLUSIVE: Power = Power::new_const(100_000.0);

/// The installed power a Steckersolargerät may have and stay outside
/// § 9 Abs. 2 S. 1 Nr. 3, § 9 Abs. 2 S. 4 EEG.
pub const STECKERSOLAR_MAX_DC: Power = Power::new_const(2_000.0);

/// The inverter power the same exemption allows, § 9 Abs. 2 S. 4 EEG.
///
/// The statute says 800 **Voltampere**. A household micro-inverter is rated at
/// unity power factor, so its nameplate watts and its voltamperes are the same
/// number, and [`GenerationProfile::installed_ac_nominal`] is compared against
/// it directly. An installation where they differ is one where the declaration
/// on [`Para9Status::steckersolargeraet`] is the thing to get right.
pub const STECKERSOLAR_MAX_AC: Power = Power::new_const(800.0);

/// Which statutory limitation a system's own feed-in is under.
///
/// Not a boolean, because there are two of them and they are different
/// percentages of the same reference — which is exactly the sort of thing a
/// `bool` named `capped` loses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum StatutoryLimit {
    /// § 9 Abs. 2 S. 1 Nr. 2 lit. b / Nr. 3 EEG — 60 % of the installed power,
    /// for a system commissioned from 25.02.2025 and below 100 kW, until an
    /// intelligent metering system with a control device is in operation.
    Cap60,
    /// § 100 Abs. 3 S. 2 Nr. 2 EEG — the 70 % Einspeisebegrenzung a system
    /// commissioned before 2023 chose instead of equipment the network operator
    /// could reduce it with.
    Cap70Legacy,
}

impl StatutoryLimit {
    /// The share of the installed power this limitation allows.
    #[must_use]
    pub const fn fraction(self) -> f64 {
        match self {
            StatutoryLimit::Cap60 => CAP_FRACTION,
            StatutoryLimit::Cap70Legacy => LEGACY_CAP_FRACTION,
        }
    }
}

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
    #[cfg_attr(
        feature = "serde",
        serde(default, with = "hems_core::wire::iso_date::option")
    )]
    pub commissioned_at: Option<Date>,
    /// The § 9 EEG facts the installer declared about this system.
    #[cfg_attr(feature = "serde", serde(default))]
    pub para9: Para9Status,
}

impl GenerationProfile {
    /// Whether this is a Steckersolargerät the statute leaves alone.
    ///
    /// All three conditions of § 9 Abs. 2 S. 4 EEG, and the declaration is the
    /// one that carries the weight: *"Steckersolargeräte mit einer installierten
    /// Leistung von insgesamt bis zu 2 Kilowatt und mit einer
    /// Wechselrichterleistung von insgesamt bis zu 800 Voltampere, die hinter
    /// der Entnahmestelle eines Letztverbrauchers betrieben werden"*. The two
    /// numbers bound a Steckersolargerät; they do not turn a small roof array
    /// into one.
    #[must_use]
    pub fn is_exempt_steckersolargeraet(&self) -> bool {
        self.para9.steckersolargeraet
            && self.installed_dc <= STECKERSOLAR_MAX_DC
            && self.installed_ac_nominal <= STECKERSOLAR_MAX_AC
    }

    /// Which statutory limitation this system's feed-in is under, if any.
    ///
    /// The decision tree of § 9 Abs. 2 S. 1 EEG together with the transitional
    /// rules of § 100 Abs. 3 and 3b, read in the order the statute is written:
    ///
    /// 1. an intelligent metering system with a control device in operation, or
    ///    a market contract with a working control path, lifts the cap
    ///    ([`CapRelief`]);
    /// 2. a Steckersolargerät within its two size tests is outside Nr. 3
    ///    altogether (§ 9 Abs. 2 S. 4);
    /// 3. commissioned from **25.02.2025** and below 100 kW → 60 % (Nr. 2 lit. b
    ///    for 25 kW and up, Nr. 3 below it — the same percentage either way);
    /// 4. commissioned in the window **01.01.2023 – 24.02.2025** → nothing, by
    ///    § 100 Abs. 3b;
    /// 5. commissioned **before 2023** → 70 % if, and only if, that is the way
    ///    the system met its obligation ([`Para9Status::legacy_70_percent`]);
    ///    otherwise the network operator holds a Rundsteuerempfänger and there
    ///    is no static ceiling to apply.
    ///
    /// An **unknown** commissioning date leaves the feed-in unlimited, because
    /// every limitation above is tied to one and applying a cap to a system the
    /// statute may never have reached would curtail a household for nothing.
    /// That is the opposite default from § 14a participation, and for the
    /// opposite reason: there, silence risks exceeding a network operator's
    /// limit; here it only risks throwing away a household's own energy.
    #[must_use]
    pub fn statutory_limit(&self) -> Option<StatutoryLimit> {
        if self.para9.relief.lifts_cap() || self.is_exempt_steckersolargeraet() {
            return None;
        }
        match self.commissioned_at? {
            d if d >= SOLARSPITZEN_START => {
                (self.installed_dc < CAP_MAX_SIZE_EXCLUSIVE).then_some(StatutoryLimit::Cap60)
            }
            d if d >= EXEMPT_WINDOW_START => None,
            _ => self
                .para9
                .legacy_70_percent
                .then_some(StatutoryLimit::Cap70Legacy),
        }
    }

    /// The ceiling the statutory limitation puts on feed-in, if one applies.
    #[must_use]
    pub fn statutory_cap(&self) -> Option<Power> {
        self.statutory_limit()
            .map(|limit| self.installed_dc * limit.fraction())
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
    /// over the wire, and the statutory cap is the law applying by itself.
    /// Reporting the factor as an LPP session would tell the household the
    /// operator had intervened when it had not.
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
            para9: pv.para9,
        }
    }

    /// The § 9 EEG facts of a whole site, summed over its arrays.
    ///
    /// `None` when the site has no photovoltaic system at all — there is nothing
    /// for the cap to apply to, and returning a zero-sized profile would answer
    /// "capped at 0 kW", which is the wrong kind of wrong.
    ///
    /// Four readings are deliberate, and every one of them is about the fact
    /// that all three limitations are measured at the **Verknüpfungspunkt**
    /// rather than at an inverter.
    ///
    /// The **capacities add up**, because § 9 Abs. 3 measures the installierte
    /// Leistung at one connection point and a household that splits a roof
    /// across two inverters has not thereby left the size class. The **relief**
    /// counts as lifted only when it is lifted for *every* array: an intelligent
    /// metering system with a control device reaching one of two inverters does
    /// not lift the cap on the other. A **Steckersolargerät stops being one**
    /// the moment it shares a connection point with a second array — § 9 Abs. 2
    /// S. 4 exempts a device operated behind a Letztverbraucher's Entnahmestelle,
    /// not one inverter of a roof installation. And **one legacy array on the
    /// 70 % limitation holds the whole point to it**, because that is where
    /// § 100 Abs. 3 S. 2 Nr. 2 asks the question.
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
                    // The relief is lifted only where it is lifted for
                    // **every** array; a Steckersolargerät stops being one the
                    // moment a second array shares its connection point; and
                    // one legacy array on the 70 % limitation holds the whole
                    // Verknüpfungspunkt to it, because that is where § 100
                    // Abs. 3 S. 2 Nr. 2 measures.
                    para9: Para9Status {
                        relief: if acc.para9.relief.lifts_cap() && next.para9.relief.lifts_cap() {
                            acc.para9.relief
                        } else {
                            CapRelief::None
                        },
                        // The earlier fitting, because § 51 Abs. 2 Nr. 1 asks
                        // when *the plant* got an intelligent metering system
                        // and a Verknüpfungspunkt has one meter.
                        imsys_since: match (acc.para9.imsys_since, next.para9.imsys_since) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        },
                        steckersolargeraet: false,
                        legacy_70_percent: acc.para9.legacy_70_percent
                            || next.para9.legacy_70_percent,
                    },
                },
            });
        }
        profile
    }
}

/// The day § 51 EEG starts to reduce this plant's anzulegender Wert to zero in a
/// quarter hour with a negative spot price, if it ever does.
///
/// `None` is "not yet, and possibly never" — and for a German household roof
/// that is the ordinary answer, which is why the rule is worth stating carefully
/// rather than as a boolean somebody sets by hand.
///
/// § 51 Abs. 1 zeroes the anzulegender Wert whenever the spot price is negative,
/// and § 51 Abs. 2 then takes most household systems back out of it:
///
/// * **Nr. 1** — a plant below 100 kW is exempt for every period *before the end
///   of the calendar year in which it is fitted with an intelligent metering
///   system*. So the trigger is not the day the meter went in: it is the first
///   of January after it, and a household that gets an iMSys in March keeps the
///   remuneration for the negative hours of that whole year. It asks about the
///   **fitting** and nothing about the Ansteuerbarkeit test § 9 Abs. 2 wants,
///   which is why [`Para9Status::imsys_since`] is a separate fact from
///   [`CapRelief::ImsysWithControl`].
/// * **Nr. 2** — a plant below 2 kW is exempt until the end of the year the
///   Bundesnetzagentur makes its § 85 Abs. 2 Nr. 12 Festlegung in. It has not,
///   so such a plant is exempt indefinitely and this returns `None` for it
///   whatever else is true. The day the Festlegung lands, that is one constant
///   here.
///
/// At 100 kW and above neither exemption applies and the rule has always been in
/// force, so the answer is the plant's own commissioning date.
///
/// The value goes on [`hems_tariff::FeedIn::para51_from`], where it reaches both
/// remuneration schemes — because § 51 reduces the *anzulegender Wert*, which is
/// what the Einspeisevergütung (§ 53 Abs. 1) and the Marktprämie (Anlage 1 zu
/// § 23a Nr. 1) are both computed from.
///
/// [`hems_tariff::FeedIn::para51_from`]: https://docs.rs/hems-tariff
#[must_use]
pub fn para51_applies_from(profile: &GenerationProfile) -> Option<Date> {
    if profile.installed_dc < PARA51_FESTLEGUNG_SIZE {
        return None;
    }
    if profile.installed_dc >= PARA51_IMSYS_SIZE {
        return profile.commissioned_at;
    }
    let since = profile.para9.imsys_since?;
    // "vor dem Ablauf des Kalenderjahres, in dem die Anlage mit einem
    // intelligenten Messsystem ausgestattet wird" — so the first period the rule
    // reaches is the first of January following.
    Date::from_calendar_date(since.year() + 1, time::Month::January, 1).ok()
}

/// The size at or above which § 51 Abs. 2 Nr. 1 EEG's iMSys exemption stops
/// applying.
pub const PARA51_IMSYS_SIZE: Power = Power::new_const(100_000.0);

/// The size below which § 51 Abs. 2 Nr. 2 EEG exempts a plant until the
/// Bundesnetzagentur's § 85 Abs. 2 Nr. 12 Festlegung, which has not been made.
pub const PARA51_FESTLEGUNG_SIZE: Power = Power::new_const(2_000.0);

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
            para9: Para9Status::default(),
        }
    }

    #[test]
    fn a_new_small_system_without_a_control_device_is_capped_at_sixty_percent() {
        let p = profile(9.8, Some(time::macros::date!(2025 - 06 - 01)));
        assert_eq!(p.statutory_limit(), Some(StatutoryLimit::Cap60));
        assert!((p.statutory_cap().unwrap().kw() - 5.88).abs() < 1e-9);
    }

    #[test]
    fn the_window_paragraph_100_exempts_carries_no_cap_and_the_day_the_law_starts_does() {
        // § 100 Abs. 3b: nothing applies to a system from the window between
        // the EEG 2023 and the Solarspitzengesetz.
        assert_eq!(
            profile(9.8, Some(time::macros::date!(2025 - 02 - 24))).statutory_limit(),
            None
        );
        assert_eq!(
            profile(9.8, Some(EXEMPT_WINDOW_START)).statutory_limit(),
            None
        );
        assert_eq!(
            profile(9.8, Some(SOLARSPITZEN_START)).statutory_limit(),
            Some(StatutoryLimit::Cap60),
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
            p.para9.relief = relief;
            assert_eq!(p.statutory_limit(), None, "{relief:?} should lift the cap");
        }
        // And the default is that nothing has.
        assert_eq!(
            profile(9.8, Some(time::macros::date!(2025 - 06 - 01))).statutory_limit(),
            Some(StatutoryLimit::Cap60)
        );
    }

    #[test]
    fn a_small_roof_array_is_capped_and_only_a_declared_steckersolargeraet_is_not() {
        // The defect this closes. § 9 Abs. 2 S. 4 exempts *Steckersolargeräte*
        // within two size tests; it does not put a 2 kW floor under the cap. A
        // 1,5 kWp roof array read as "below the size class" fed in above its
        // statutory limit all summer and nothing said so.
        let roof = profile(1.5, Some(time::macros::date!(2025 - 06 - 01)));
        assert_eq!(
            roof.statutory_limit(),
            Some(StatutoryLimit::Cap60),
            "a small roof array is not a balcony device"
        );
        assert!((roof.statutory_cap().unwrap().kw() - 0.9).abs() < 1e-9);

        // Declared, and inside both tests: outside Nr. 3 altogether.
        let balcony = GenerationProfile {
            installed_dc: Power::from_kw(2.0),
            installed_ac_nominal: Power::new(800.0),
            para9: Para9Status::steckersolargeraet(),
            ..roof
        };
        assert_eq!(balcony.statutory_limit(), None);

        // …and the size tests are conditions on the exemption, not on the cap:
        // an 1 000 VA inverter is a Steckersolargerät the statute does not
        // exempt.
        let oversized = GenerationProfile {
            installed_ac_nominal: Power::new(1_000.0),
            ..balcony
        };
        assert_eq!(oversized.statutory_limit(), Some(StatutoryLimit::Cap60));
    }

    #[test]
    fn a_hundred_kilowatt_system_is_paragraph_9_nummer_1s_and_carries_no_percentage() {
        // *"weniger als 100 Kilowatt"*: exactly 100 kW is Nr. 1's, and Nr. 1
        // demands remote reducibility rather than a percentage.
        let commissioned = Some(time::macros::date!(2025 - 06 - 01));
        assert_eq!(
            profile(99.999, commissioned).statutory_limit(),
            Some(StatutoryLimit::Cap60)
        );
        assert_eq!(profile(100.0, commissioned).statutory_limit(), None);
        assert_eq!(profile(150.0, commissioned).statutory_limit(), None);
    }

    #[test]
    fn a_legacy_system_on_the_old_limitation_is_held_to_seventy_percent() {
        // § 100 Abs. 3 S. 2 Nr. 2: a system from before 2023 met its obligation
        // either with equipment the operator can reduce it with — no static
        // ceiling — or by limiting itself to 70 %. Which one is a fact about
        // the installation, so it is declared rather than guessed.
        let old = Some(time::macros::date!(2019 - 05 - 20));
        assert_eq!(
            profile(9.8, old).statutory_limit(),
            None,
            "a Rundsteuerempfänger leaves no static ceiling"
        );

        let limited = GenerationProfile {
            para9: Para9Status::legacy_70_percent(),
            ..profile(9.8, old)
        };
        assert_eq!(limited.statutory_limit(), Some(StatutoryLimit::Cap70Legacy));
        assert!((limited.statutory_cap().unwrap().kw() - 6.86).abs() < 1e-9);
    }

    #[test]
    fn an_unknown_commissioning_date_leaves_the_feed_in_alone() {
        assert_eq!(profile(9.8, None).statutory_limit(), None);
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
