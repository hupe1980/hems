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

use hems_core::prelude::{Horizon, Power, Slot};
use metering::holiday::Bundesland;

use crate::quantile::{Band, Forecast};

/// The kind of day a slot falls on.
///
/// Three classes, because that is what the data supports: a household's Saturday
/// differs from its Tuesday, and a public holiday behaves like a Sunday. Using
/// the metering layer's holiday calendar means hems and the settlement layer
/// never disagree about which days those are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum DayType {
    /// Monday to Friday, excluding public holidays.
    Workday,
    /// Saturday.
    Saturday,
    /// Sunday and public holidays.
    Sunday,
}

impl DayType {
    /// The day type of `slot` in `land`.
    #[must_use]
    pub fn of(slot: Slot, land: Bundesland) -> Self {
        match metering::holiday::slp_day_type(slot.local_date(), land) {
            metering::load_profile::SlpDayType::Samstag => DayType::Saturday,
            metering::load_profile::SlpDayType::SonnFeiertag => DayType::Sunday,
            metering::load_profile::SlpDayType::Werktag => DayType::Workday,
        }
    }
}

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
            return Band::certain(0.0);
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
