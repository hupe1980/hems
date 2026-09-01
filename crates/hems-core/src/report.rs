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
    pub self_sufficiency: f64,
    /// Allocated by a § 42c community, kWh.
    #[cfg_attr(feature = "serde", serde(default))]
    pub shared_kwh: f64,

    // ── Money ───────────────────────────────────────────────────────────────
    /// What the day cost, in the currency every term of the objective is in.
    pub cost: CostBreakdown,
    /// What the same day would have cost with no energy manager, under the same
    /// weather and the same grid rules.
    pub baseline: CostBreakdown,

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

    // ── Forecast ────────────────────────────────────────────────────────────
    /// The production band's coverage on this day, in `[0, 1]`.
    ///
    /// One day is close to **one draw** — forecast error is correlated across a
    /// day — so a single day's figure means almost nothing and only the fleet's
    /// merge of many does. That is the whole reason it is reported rather than
    /// judged locally.
    pub pv_coverage: f64,
    /// The production band's CRPS, watts.
    pub pv_crps: f64,
    /// The load band's coverage.
    pub load_coverage: f64,
    /// The load band's CRPS, watts.
    pub load_crps: f64,
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
            cost: CostBreakdown::default(),
            baseline: CostBreakdown::default(),
            respected_the_grid: true,
            worst_overshoot_w: 0.0,
            minutes_without_a_plan: 0,
            control_events: 0,
            below_minimum_commanded: false,
            pv_coverage: 0.0,
            pv_crps: 0.0,
            load_coverage: 0.0,
            load_crps: 0.0,
            foresight_was_perfect: false,
        }
    }
}

impl DayKpis {
    /// What the energy manager saved, in the currency every term is in.
    #[must_use]
    pub fn saving_eur(&self) -> f64 {
        self.baseline.total() - self.cost.total()
    }

    /// The same on the electricity bill alone — the flattering number.
    #[must_use]
    pub fn bill_saving_eur(&self) -> f64 {
        self.baseline.billed_eur() - self.cost.billed_eur()
    }

    /// Whether this day is one a fleet may put in a saving figure.
    ///
    /// A day the planner was shown the answer to is an upper bound, and mixing
    /// it into an average produces a number that is true of nothing.
    #[must_use]
    pub const fn is_measurable(&self) -> bool {
        !self.foresight_was_perfect
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
            cost: CostBreakdown {
                energy_eur: 21.08,
                wear_eur: 0.62,
                ..CostBreakdown::default()
            },
            baseline: CostBreakdown {
                energy_eur: 24.12,
                ..CostBreakdown::default()
            },
            ..DayKpis::default()
        }
    }

    #[test]
    fn the_saving_carries_what_the_plan_spent_and_the_bill_does_not() {
        let d = day();
        assert!((d.saving_eur() - 2.42).abs() < 1e-9);
        assert!((d.bill_saving_eur() - 3.04).abs() < 1e-9);
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
