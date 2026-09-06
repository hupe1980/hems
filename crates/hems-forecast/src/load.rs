//! Household load: what the house does that nobody controls.
//!
//! The base load of a household is strongly periodic — a weekday looks like
//! other weekdays at the same time of day — and the residual around that
//! periodicity is what a forecast has to quantify. Recent work on single-household
//! series finds that simple seasonal models with empirical residual quantiles
//! stay competitive with much larger ones (`specs/arxiv/arxiv-2512.00856.pdf`),
//! which is fortunate: this has to run on a gateway box with no GPU and no
//! internet connection.
//!
//! So: a profile indexed by day type and quarter hour, and quantiles taken from
//! the observed spread in each cell. Nothing is fitted that cannot be recomputed
//! on the box in milliseconds, and a cell with too little history says so rather
//! than inventing confidence.

use std::collections::BTreeMap;

use hems_core::prelude::{DayType, Horizon, Power, Slot};
use metering::holiday::Bundesland;

use crate::quantile::{Band, Forecast};

/// One cell of the profile: everything observed at this day type and time.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Cell {
    samples: Vec<f64>,
}

impl Cell {
    fn quantile(&self, q: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        // Nearest-rank: no interpolation between samples, so a cell with three
        // observations reports one of those three rather than a number nobody
        // ever measured.
        let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[rank - 1]
    }
}

/// The smallest number of observations a cell needs before its spread is
/// treated as informative.
pub const MIN_SAMPLES: usize = 3;

/// How much wider a band gets when it is answered from a **different day type**.
///
/// A household's Saturday is like its Monday in shape and not in level — later
/// mornings, more cooking, somebody at home — so borrowing the quarter hour
/// across day types is a good guess and a worse one than the cell it replaces.
/// Half again, which takes the 40 % default to 60 %.
const CROSS_DAY_WIDENING: f64 = 1.5;

/// And when it is answered from a quarter hour the household has never been
/// metered through at all.
///
/// Twice, which takes the default to 80 % — wide enough that a planner handed
/// one does not bet a battery on it, which is the whole job of a band nobody has
/// evidence for.
const UNSEEN_WIDENING: f64 = 2.0;

/// The cells, as a **sequence** rather than a map.
///
/// The key is a `(DayType, u32)` pair, and a map with a non-string key is
/// something JSON cannot express at all: `serde_json` refuses it at
/// serialisation time with "key must be a string". The derive is therefore not
/// enough on its own, and the failure is the worst shape a failure can have —
/// the type compiles, every other format accepts it, and the one a box actually
/// stores its learning in returns an error at run time, once, in a code path
/// that was warning rather than failing. A whole household's fortnight of
/// history went missing and the only symptom was a forecast that never got
/// better.
///
/// A sequence of triples has none of that: every format can carry it, the
/// ordering is the `BTreeMap`'s own so the bytes are stable, and a round trip is
/// a test rather than a hope (P3 — a serialisable type states how it travels).
#[cfg(feature = "serde")]
mod cells_as_a_sequence {
    use super::{Cell, DayType};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        cells: &BTreeMap<(DayType, u32), Cell>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let flat: Vec<(DayType, u32, &Cell)> = cells
            .iter()
            .map(|((day, index), cell)| (*day, *index, cell))
            .collect();
        flat.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<(DayType, u32), Cell>, D::Error> {
        let flat: Vec<(DayType, u32, Cell)> = Vec::deserialize(d)?;
        Ok(flat
            .into_iter()
            .map(|(day, index, cell)| ((day, index), cell))
            .collect())
    }
}

/// A household's load profile, learned from its own history.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoadProfile {
    #[cfg_attr(feature = "serde", serde(with = "cells_as_a_sequence"))]
    cells: BTreeMap<(DayType, u32), Cell>,
    /// The state whose holiday calendar applies.
    pub land: Bundesland,
    /// The fallback spread for a cell with too little history.
    pub default_spread: f64,
}

impl Default for LoadProfile {
    fn default() -> Self {
        Self::new(Bundesland::Be)
    }
}

impl LoadProfile {
    /// An empty profile for a state.
    #[must_use]
    pub fn new(land: Bundesland) -> Self {
        Self {
            cells: BTreeMap::new(),
            land,
            default_spread: 0.4,
        }
    }

    /// Add one observation.
    pub fn observe(&mut self, slot: Slot, power: Power) {
        let key = (DayType::of(slot, self.land), slot.index_in_local_day());
        self.cells.entry(key).or_default().samples.push(power.get());
    }

    /// Add a whole series.
    pub fn observe_all(&mut self, samples: impl IntoIterator<Item = (Slot, Power)>) {
        for (slot, power) in samples {
            self.observe(slot, power);
        }
    }

    /// How many observations back this slot's cell.
    #[must_use]
    pub fn support(&self, slot: Slot) -> usize {
        self.cells
            .get(&(DayType::of(slot, self.land), slot.index_in_local_day()))
            .map_or(0, |c| c.samples.len())
    }

    /// The band for one slot.
    ///
    /// With enough history the quantiles are the observed ones. With too
    /// little, the median is whatever was seen and the spread is the configured
    /// default — a forecast that admits it is guessing, rather than a narrow
    /// band the optimiser would trust.
    ///
    /// # Why a small cell is widened rather than trusted
    ///
    /// An empirical quantile from a handful of samples is **systematically too
    /// tight**, and the direction is the dangerous one: the nearest-rank 10th
    /// percentile of five observations is the smallest of the five, which is on
    /// average well inside the true tenth percentile. A planner handed that band
    /// is being told the household is more predictable than it is, and it spends
    /// a battery on the difference.
    ///
    /// This showed up the first time the days were scored rather than asserted:
    /// a Sunday backed by three observed Sundays produced a band the outcome fell
    /// inside **41 %** of the time, against the 80 % a 10–90 band promises.
    /// [`Calibration::is_well_calibrated`] says so, which is what the metric is
    /// for.
    ///
    /// So the observed half-width is inflated by `√((n+1)/(n−1))` — the usual
    /// small-sample correction for a quantile's own sampling error — which is
    /// large where the history is thin and vanishes as it grows. It never
    /// narrows a band.
    ///
    /// [`Calibration::is_well_calibrated`]: crate::metrics::Calibration::is_well_calibrated
    #[must_use]
    pub fn band_at(&self, slot: Slot) -> Band {
        let key = (DayType::of(slot, self.land), slot.index_in_local_day());
        let Some(cell) = self.cells.get(&key) else {
            return self.seasonal_naive_at(slot);
        };
        let n = cell.samples.len();
        if n < MIN_SAMPLES {
            let median = cell.quantile(0.5);
            return Band::relative(median, self.default_spread);
        }
        let median = cell.quantile(0.5);
        let inflate = (((n + 1) as f64) / ((n - 1) as f64)).sqrt();
        Band {
            p10: (median - (median - cell.quantile(0.1)) * inflate).max(0.0),
            p50: median,
            p90: median + (cell.quantile(0.9) - median) * inflate,
        }
        .sorted()
    }

    /// The same quarter hour of the day, under whatever day type this household
    /// has been seen on — the seasonal-naive fallback, for a cell with nothing
    /// in it.
    ///
    /// A cell is empty on the first Saturday of a box's life, on the first public
    /// holiday, and on any quarter hour the household has not been metered
    /// through — so this is the ordinary case rather than an edge. Answering
    /// `p10 = p50 = p90 = 0` would say *the house will use nothing, and I am
    /// sure*, which is the confident lie `ForecastTooShort` refuses one slot at a
    /// time; a plan given it defers every flexible kilowatt-hour into hours it
    /// believes are free and overstates the § 14a surplus by the load it did not
    /// expect.
    ///
    /// The household's own Monday 07:15 is a far better guess at its Saturday
    /// 07:15, and the profile already holds it. A quarter hour seen on no day at
    /// all falls back to the household's overall median, widened further. Only a
    /// profile with **no history whatsoever** returns zero, which its caller is
    /// supposed to have excluded with [`LoadProfile::is_empty`] (D145).
    #[must_use]
    fn seasonal_naive_at(&self, slot: Slot) -> Band {
        let quarter = slot.index_in_local_day();
        let across: Vec<f64> = self
            .cells
            .iter()
            .filter(|((_, q), _)| *q == quarter)
            .map(|(_, cell)| cell.quantile(0.5))
            .collect();
        if !across.is_empty() {
            let median = across.iter().sum::<f64>() / across.len() as f64;
            return Band::relative(median, self.default_spread * CROSS_DAY_WIDENING);
        }
        // Not this quarter hour on any day. The household's own level is still
        // a better answer than nothing, and the band says how much better.
        let all: Vec<f64> = self.cells.values().map(|c| c.quantile(0.5)).collect();
        if all.is_empty() {
            return Band::certain(0.0);
        }
        let median = all.iter().sum::<f64>() / all.len() as f64;
        Band::relative(median, self.default_spread * UNSEEN_WIDENING)
    }

    /// Whether this profile has learned anything at all.
    ///
    /// The gate a caller needs instead of [`LoadProfile::support`] on one slot:
    /// with the seasonal-naive fallback above, a profile that has seen *any* of
    /// this household can answer for every slot, and one that has seen none of
    /// it can answer for none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.values().all(|c| c.samples.is_empty())
    }

    /// A forecast over a horizon.
    #[must_use]
    pub fn forecast(&self, horizon: Horizon) -> Forecast {
        Forecast {
            slots: horizon.slots().map(|s| (s, self.band_at(s))).collect(),
        }
    }
}

#[cfg(test)]
mod tests {

    /// A quarter hour with no history is never forecast as an **empty house**.
    ///
    /// The defect this pins, and it was a whole day wide. A box gates on the
    /// support of the horizon's *first* slot, so a Friday-evening re-plan over
    /// a two-day horizon reached a Saturday whose cells were all empty and was
    /// handed `p10 = p50 = p90 = 0` for every one of them — a household that
    /// uses nothing all weekend, stated with certainty. A plan given that defers
    /// every flexible kilowatt-hour into hours it believes are free and
    /// overstates the § 14a surplus by the whole of the load it did not expect.
    ///
    /// It is the same confident lie `hems_optimizer::SolveError::ForecastTooShort`
    /// refuses one slot at a time, arriving through a forecast that was long
    /// enough.
    #[test]
    fn a_day_type_the_household_has_never_had_is_not_forecast_as_an_empty_house() {
        let mut profile = LoadProfile::new(Bundesland::Be);
        // A week of workdays, and nothing else. 2026-06-01 is a Monday.
        for day in 0..5 {
            for q in 0..96 {
                let slot = Slot::containing(
                    datetime!(2026-06-01 00:00:00 UTC)
                        + time::Duration::days(day)
                        + time::Duration::minutes(15 * q),
                );
                profile.observe(slot, Power::from_kw(0.6));
            }
        }
        // The Saturday. No cell for it, on any quarter hour.
        let saturday = slot_at(5, 7);
        assert_eq!(profile.support(saturday), 0, "no Saturday has been seen");
        let band = profile.band_at(saturday);
        assert!(
            band.p50 > 0.0,
            "a household that has been metered all week does not use nothing on \
             Saturday: {band:?}"
        );
        assert!(
            band.p90 > band.p10,
            "and a band with no evidence behind it has to be wide, not certain: {band:?}"
        );
        assert!(
            band.width() > profile.band_at(slot_at(1, 7)).width(),
            "wider than the cell it stands in for"
        );
    }

    /// …and a quarter hour seen on no day at all still answers from the
    /// household's own level.
    #[test]
    fn an_hour_never_metered_falls_back_to_the_households_own_level() {
        let mut profile = LoadProfile::new(Bundesland::Be);
        // Only the mornings, for a week.
        for day in 0..5 {
            for q in 28..40 {
                let slot = Slot::containing(
                    datetime!(2026-06-01 00:00:00 UTC)
                        + time::Duration::days(day)
                        + time::Duration::minutes(15 * q),
                );
                profile.observe(slot, Power::from_kw(0.8));
            }
        }
        let midnight = slot_at(1, 0);
        let band = profile.band_at(midnight);
        assert!(band.p50 > 0.0, "{band:?}");
        assert!(band.p90 > band.p50, "{band:?}");
    }

    /// A profile that has learned nothing says so, rather than answering zero.
    #[test]
    fn an_empty_profile_is_empty_rather_than_confidently_nothing() {
        let profile = LoadProfile::new(Bundesland::Be);
        assert!(profile.is_empty());
        // It still answers — the type has no "unknown" — and the caller is the
        // one that must not ask.
        assert_eq!(profile.band_at(slot_at(0, 12)), Band::certain(0.0));

        let mut one = LoadProfile::new(Bundesland::Be);
        one.observe(slot_at(0, 12), Power::from_kw(0.5));
        assert!(!one.is_empty(), "one reading is history");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn a_profile_survives_a_round_trip_through_json() {
        // A `BTreeMap` with a `(DayType, u32)` key is something JSON cannot
        // express: `serde_json` refuses a non-string map key at serialisation
        // time. The derive compiles, every other format accepts it, and the one
        // a box actually keeps its learning in fails at run time — which is how
        // a household's fortnight of history went missing with no symptom but a
        // forecast that never got better.
        let mut profile = LoadProfile::new(Bundesland::Be);
        let start = Slot::containing(time::macros::datetime!(2026-01-15 00:00:00 UTC));
        for i in 0..96 {
            profile.observe(start.offset(i), Power::from_kw(0.6));
        }

        let json = serde_json::to_string(&profile).expect("a profile has to be storable as JSON");
        let back: LoadProfile = serde_json::from_str(&json).expect("and readable again");
        assert_eq!(back, profile);
        assert!(back.support(start) > 0, "with its history intact");
    }
    use super::*;
    use time::macros::datetime;

    fn slot_at(day: i32, hour: u8) -> Slot {
        // 2026-06-01 is a Monday.
        let base = datetime!(2026-06-01 00:00:00 UTC);
        Slot::containing(
            base + time::Duration::days(i64::from(day)) + time::Duration::hours(i64::from(hour)),
        )
    }

    #[test]
    fn a_profile_learns_the_shape_of_a_weekday() {
        let mut p = LoadProfile::new(Bundesland::Be);
        // Four Mondays with a 500 W evening.
        for week in 0..4 {
            p.observe(slot_at(week * 7, 18), Power::new(500.0));
        }
        let band = p.band_at(slot_at(28, 18));
        assert_eq!(band.p50, 500.0);
        assert_eq!(band.width(), 0.0, "a perfectly repeatable household");
    }

    #[test]
    fn the_spread_comes_from_what_was_actually_observed() {
        let mut p = LoadProfile::new(Bundesland::Be);
        for (week, watts) in [
            (0, 200.0),
            (7, 400.0),
            (14, 600.0),
            (21, 800.0),
            (28, 1000.0),
        ] {
            p.observe(slot_at(week, 18), Power::new(watts));
        }
        let band = p.band_at(slot_at(35, 18));
        assert!(band.is_ordered());
        assert_eq!(band.p50, 600.0);
        assert!(band.p10 <= 400.0 && band.p90 >= 800.0, "{band:?}");
    }

    #[test]
    fn a_cell_with_too_little_history_widens_instead_of_pretending() {
        let mut p = LoadProfile::new(Bundesland::Be);
        p.observe(slot_at(0, 18), Power::new(500.0));
        let band = p.band_at(slot_at(7, 18));
        assert_eq!(band.p50, 500.0);
        assert!(band.width() > 0.0, "one observation is not certainty");
        assert_eq!(p.support(slot_at(7, 18)), 1);
    }

    #[test]
    fn weekdays_and_weekends_are_kept_apart() {
        let mut p = LoadProfile::new(Bundesland::Be);
        for week in 0..4 {
            p.observe(slot_at(week * 7, 12), Power::new(300.0)); // Mondays
            p.observe(slot_at(week * 7 + 6, 12), Power::new(900.0)); // Sundays
        }
        assert_eq!(p.band_at(slot_at(28, 12)).p50, 300.0);
        assert_eq!(p.band_at(slot_at(34, 12)).p50, 900.0);
    }

    #[test]
    fn an_unseen_slot_forecasts_nothing_rather_than_something_invented() {
        let p = LoadProfile::new(Bundesland::Be);
        assert_eq!(p.band_at(slot_at(0, 3)), Band::certain(0.0));
    }

    #[test]
    fn a_forecast_covers_every_slot_of_its_horizon_in_order() {
        let mut p = LoadProfile::new(Bundesland::Be);
        for week in 0..4 {
            for hour in 0..24 {
                p.observe(slot_at(week * 7, hour), Power::new(f64::from(hour) * 20.0));
            }
        }
        let f = p.forecast(Horizon::new(slot_at(28, 0).start(), 96));
        assert_eq!(f.slots.len(), 96);
        assert!(f.is_ordered());
    }
}
