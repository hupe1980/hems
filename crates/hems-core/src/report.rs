//! What one site's day came to, in the form a fleet reads it.
//!
//! # Why the report is a type and not a JSON blob
//!
//! A box tells the fleet how its day went, and the fleet aggregates. Both halves
//! have to agree about what "the day cost" means, and the way that agreement
//! usually fails is silent: the box renames a field, the fleet's parser fills in
//! a default, and a dashboard reports a saving of zero for six weeks before
//! anybody asks.
//!
//! So the report is one type, in the domain crate both sides already depend on,
//! and it is deliberately **narrow**. A fleet service coupled to every field of
//! an edge daemon's internal record is a fleet service that cannot be deployed
//! independently of it; what crosses the wire is the dozen numbers a fleet has a
//! question about.
//!
//! # The three that must be zero
//!
//! [`DayKpis::respected_the_grid`], [`DayKpis::minutes_without_a_plan`] and
//! [`DayKpis::worst_overshoot_w`] are not statistics. A fleet operator asking
//! "how are my ten thousand households" is not asking for an average of those —
//! **any** breach of a network operator's instruction is a finding, and an
//! average is how one becomes invisible among nine thousand nine hundred and
//! ninety-nine.

use time::Date;

use crate::plan::CostBreakdown;

/// How one day's forecasts scored against what actually happened.
///
/// Reported rather than judged locally, because one day is close to **one
/// draw**: forecast error is correlated across a day, so ninety-six quarter
/// hours of one Tuesday are nearly one observation. Only a fleet's merge of many
/// days is a calibration figure, which is why these travel as a group that is
/// either present or absent — a day that was not scored contributes nothing
/// rather than contributing a zero.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct ForecastScores {
    /// The production band's coverage on this day, in `[0, 1]`.
    pub pv_coverage: f64,
    /// The production band's CRPS, watts.
    pub pv_crps: f64,
    /// The load band's coverage.
    pub load_coverage: f64,
    /// The load band's CRPS, watts.
    pub load_crps: f64,
}

/// What a day cost, and what it would have cost with no energy manager.
///
/// **Both or neither.** A cost with no baseline is a number with nothing to
/// compare it against, and a baseline is only comparable to a cost the same
/// model produced. Keeping them together makes the third state — a cost that
/// looks complete beside a missing counterfactual — unrepresentable.
///
/// Only a **simulator** has these. Five of `CostBreakdown`'s six terms are
/// modelled rather than metered — battery wear, curtailment, discomfort, energy
/// borrowed from the stores, charge delivered past what was asked for — and the
/// baseline is a counterfactual that has to be re-run. A box on a wall meters
/// its energies and its compliance and reports those; it does not publish a
/// figure about its own product that no meter saw (D116).
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct Economics {
    /// What the day cost, in the currency every term of the objective is in.
    pub cost: CostBreakdown,
    /// What the same day would have cost with no energy manager, under the same
    /// weather and the same grid rules.
    pub baseline: CostBreakdown,
}

/// The Autarkiegrad implied by three metered energies, in `[0, 1]`.
///
/// `(consumption − import) / consumption`, with consumption taken as
/// `import + max(0, production − export)` — everything drawn from the grid, plus
/// everything produced that did not leave. It lives here because there are two
/// callers and a fleet averages both: two arithmetics would be a mean over two
/// different questions (D125).
///
/// It counts a store's **round-trip losses** as consumption. Energy that went
/// into a battery and came back smaller is on both sides of the fraction,
/// because a connection point cannot tell a kilowatt-hour that heated water from
/// one that warmed a cell. Every commercial home energy manager reports it the
/// same way and the effect is under a percentage point — but it is an
/// approximation rather than an identity, and a figure quoted to a customer
/// should say so.
#[must_use]
pub fn self_sufficiency(imported_kwh: f64, produced_kwh: f64, exported_kwh: f64) -> f64 {
    let self_used = (produced_kwh - exported_kwh).max(0.0);
    let consumed = imported_kwh.max(0.0) + self_used;
    if consumed <= 0.0 {
        return 0.0;
    }
    (self_used / consumed).clamp(0.0, 1.0)
}

/// One site's day, as the fleet is told about it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct DayKpis {
    /// Which site.
    pub site: String,
    /// Which local day.
    #[cfg_attr(feature = "serde", serde(with = "crate::wire::iso_date"))]
    pub date: Date,

    // ── Energy ──────────────────────────────────────────────────────────────
    /// Drawn from the grid, kWh.
    pub imported_kwh: f64,
    /// Fed into the grid, kWh.
    pub exported_kwh: f64,
    /// Produced, kWh.
    pub produced_kwh: f64,
    /// The share of consumption the site covered from its own roof and store.
    ///
    /// Always [`self_sufficiency`], never a second arithmetic. A fleet averages
    /// these across boxes and simulations alike, and two definitions in one mean
    /// is a number about nothing.
    pub self_sufficiency: f64,
    /// Allocated by a § 42c community, kWh.
    #[cfg_attr(feature = "serde", serde(default))]
    pub shared_kwh: f64,

    // ── Money, where a model produced it ────────────────────────────────────
    /// What the day cost and what it would have cost unmanaged.
    ///
    /// `None` from a box: see [`Economics`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub economics: Option<Economics>,

    // ── Compliance: the three that must be zero ─────────────────────────────
    /// Whether every § 14a instruction was respected throughout.
    ///
    /// Aggregated by **counting the falses**, never by averaging. See the module
    /// note.
    pub respected_the_grid: bool,
    /// The furthest the netzwirksamer Leistungsbezug ever went over a commanded
    /// ceiling, watts. Zero on a compliant day.
    #[cfg_attr(feature = "serde", serde(default))]
    pub worst_overshoot_w: f64,
    /// Minutes the arbiter spent with no plan it was willing to follow.
    ///
    /// The seam number that found a defect costing €1,50 a day for the life of
    /// the project without violating a single property.
    #[cfg_attr(feature = "serde", serde(default))]
    pub minutes_without_a_plan: u32,
    /// How many § 14a control events the day recorded, `[A1 7.2]`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub control_events: usize,
    /// Whether a commanded ceiling went below the minimum the customer is owed,
    /// `[A1 4.5]`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub below_minimum_commanded: bool,
    /// Control ticks in which a device could not hold the command it was given.
    ///
    /// The seam between the arbiter and the hardware. A charge point is off or
    /// above 6 A with nothing in between, so the arbiter routinely decides a
    /// value a device cannot hold and `hems_device::realisable` resolves it
    /// correctly and **silently**. No property test can report that — every line
    /// of it behaves exactly as specified — so the day counts it instead.
    #[cfg_attr(feature = "serde", serde(default))]
    pub clipped_ticks: u32,
    /// The energy the hardware refused over those ticks, kWh.
    ///
    /// The count says how often; this says whether it mattered. A wallbox
    /// clipped for one tick at the tail of a slot whose energy is already
    /// delivered is a rounding error; one clipped for an hour is a plan built on
    /// a device that does not exist.
    #[cfg_attr(feature = "serde", serde(default))]
    pub clipped_kwh: f64,

    // ── Forecast ────────────────────────────────────────────────────────────
    /// How the day's own forecasts scored, where the box scored them.
    ///
    /// `None` is a day nobody scored, and it is **not** a day that scored zero.
    /// A fleet merges these as episodes, so a box that reported an unmeasured
    /// coverage as `0.0` would drag every calibration figure toward zero while
    /// looking like it had contributed a measurement (D116).
    #[cfg_attr(feature = "serde", serde(default))]
    pub forecast: Option<ForecastScores>,
    /// Whether the planner was shown the weather in advance.
    ///
    /// A day with perfect foresight is an upper bound rather than a result, and
    /// a fleet that mixed the two into one saving figure would be publishing a
    /// number no household can reach.
    #[cfg_attr(feature = "serde", serde(default))]
    pub foresight_was_perfect: bool,
}

impl Default for DayKpis {
    /// A day at the Unix epoch with nothing in it.
    ///
    /// `time::Date` has no `Default` and should not: there is no obvious zero
    /// date, and a type that invented one would let a report reach a fleet
    /// carrying a day nobody chose. The epoch is written here, once, where it is
    /// visible.
    fn default() -> Self {
        Self {
            site: String::new(),
            date: time::macros::date!(1970 - 01 - 01),
            imported_kwh: 0.0,
            exported_kwh: 0.0,
            produced_kwh: 0.0,
            self_sufficiency: 0.0,
            shared_kwh: 0.0,
            economics: None,
            respected_the_grid: true,
            worst_overshoot_w: 0.0,
            minutes_without_a_plan: 0,
            control_events: 0,
            below_minimum_commanded: false,
            clipped_ticks: 0,
            clipped_kwh: 0.0,
            forecast: None,
            foresight_was_perfect: false,
        }
    }
}

impl DayKpis {
    /// What the energy manager saved, in the currency every term is in.
    ///
    /// `None` on a day nobody modelled — which is every day from real hardware.
    #[must_use]
    pub fn saving_eur(&self) -> Option<f64> {
        self.economics
            .as_ref()
            .map(|e| e.baseline.total() - e.cost.total())
    }

    /// The same on the electricity bill alone — the flattering number.
    #[must_use]
    pub fn bill_saving_eur(&self) -> Option<f64> {
        self.economics
            .as_ref()
            .map(|e| e.baseline.billed_eur() - e.cost.billed_eur())
    }

    /// Whether this day is one a fleet may put in a saving figure.
    ///
    /// Two ways it is not. A day the planner was shown the answer to is an
    /// **upper bound**, and mixing it into an average produces a number no
    /// household can reach. A day with no economics has nothing to be a saving
    /// *of* — which is every day a real box reports, and is why the fleet counts
    /// those rather than averaging them in as days that saved nothing.
    #[must_use]
    pub const fn is_measurable(&self) -> bool {
        self.economics.is_some() && !self.foresight_was_perfect
    }

    /// Whether anything on this day is worth somebody looking at.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        !self.respected_the_grid || self.minutes_without_a_plan > 0 || self.below_minimum_commanded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn day() -> DayKpis {
        DayKpis {
            site: "site-1".into(),
            date: date!(2026 - 01 - 15),
            respected_the_grid: true,
            economics: Some(Economics {
                cost: CostBreakdown {
                    energy_eur: 21.08,
                    wear_eur: 0.62,
                    ..CostBreakdown::default()
                },
                baseline: CostBreakdown {
                    energy_eur: 24.12,
                    ..CostBreakdown::default()
                },
            }),
            ..DayKpis::default()
        }
    }

    #[test]
    fn the_saving_carries_what_the_plan_spent_and_the_bill_does_not() {
        let d = day();
        assert!((d.saving_eur().unwrap() - 2.42).abs() < 1e-9);
        assert!((d.bill_saving_eur().unwrap() - 3.04).abs() < 1e-9);
        assert!(d.saving_eur() < d.bill_saving_eur(), "wear is real");
    }

    #[test]
    fn a_day_shown_the_weather_is_not_one_a_fleet_may_average() {
        let mut d = day();
        assert!(d.is_measurable());
        d.foresight_was_perfect = true;
        assert!(!d.is_measurable());
    }

    #[test]
    fn a_breach_or_a_gap_in_planning_is_worth_looking_at() {
        let mut d = day();
        assert!(!d.needs_attention());
        d.minutes_without_a_plan = 3;
        assert!(d.needs_attention());

        let mut d = day();
        d.respected_the_grid = false;
        assert!(d.needs_attention());

        let mut d = day();
        d.below_minimum_commanded = true;
        assert!(d.needs_attention(), "the entitlement is the customer's");
    }
}

#[cfg(test)]
mod a_day_a_box_can_actually_report {
    use super::*;
    use time::macros::date;

    /// What a box on a wall has: what happened, and no counterfactual.
    fn measured() -> DayKpis {
        DayKpis {
            site: "haus-1".into(),
            date: date!(2026 - 01 - 15),
            imported_kwh: 14.2,
            ..DayKpis::default()
        }
    }

    #[test]
    fn a_day_with_no_baseline_has_no_saving_rather_than_a_saving_of_zero() {
        // The distinction the whole change exists for. A box cannot re-run its
        // own day as an unmanaged house, so it reports no baseline — and a
        // fleet that read that as "saved nothing" would publish an average
        // dragged toward zero by every real household in it.
        let day = measured();
        assert_eq!(day.saving_eur(), None);
        assert_eq!(day.bill_saving_eur(), None);
        assert!(
            !day.is_measurable(),
            "and it is not a day a saving figure may be computed from"
        );
    }

    #[test]
    fn an_unscored_forecast_is_absent_rather_than_zero() {
        // Same argument, on the other axis. A fleet merges these as episodes,
        // so a `0.0` coverage from a box that never scored anything is a
        // measurement that did not happen, counted as one that did.
        assert_eq!(measured().forecast, None);

        let scored = DayKpis {
            forecast: Some(ForecastScores {
                pv_coverage: 0.81,
                pv_crps: 192.3,
                load_coverage: 0.85,
                load_crps: 18.2,
            }),
            ..measured()
        };
        assert_eq!(scored.forecast.map(|f| f.pv_coverage), Some(0.81));
    }

    #[test]
    fn a_household_that_imported_anything_is_not_wholly_self_sufficient() {
        // The property the arithmetic this replaced could violate, and did. The
        // simulator's own version summed the loads it could see for the
        // denominator and put `production − export` over them — so on a June day
        // where the surplus went into a battery the numerator counted charging
        // as self-consumption and the denominator did not, the fraction passed
        // one, and it clamped. `just demo offline` reported **100 %
        // self-sufficiency and 2,6 kWh imported** in the same table.
        let ss = self_sufficiency(2.6, 51.4, 6.2);
        assert!(ss < 1.0, "2,6 kWh came off the grid: {ss}");
        assert!((ss - 45.2 / 47.8).abs() < 1e-9);
    }

    #[test]
    fn a_house_with_no_roof_covers_none_of_itself_and_one_with_no_meter_is_not_a_division() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        assert!(close(self_sufficiency(10.0, 0.0, 0.0), 0.0));
        assert!(
            close(self_sufficiency(0.0, 0.0, 0.0), 0.0),
            "and no NaN from an unplugged box"
        );
        assert!(
            close(self_sufficiency(0.0, 8.0, 0.0), 1.0),
            "a day entirely off its own roof"
        );
        assert!(
            close(self_sufficiency(0.0, 8.0, 9.0), 0.0),
            "and exporting more than it made is a meter fault, not a negative share"
        );
    }

    #[test]
    fn a_foresight_day_is_still_excluded_even_with_a_baseline() {
        // The older of the two exclusions, and it still holds: a day the
        // planner was shown the answer to is an upper bound whatever else is
        // attached to it.
        let day = DayKpis {
            economics: Some(Economics::default()),
            foresight_was_perfect: true,
            ..measured()
        };
        assert!(day.saving_eur().is_some(), "it has one");
        assert!(!day.is_measurable(), "and it still may not be averaged");
    }
}
