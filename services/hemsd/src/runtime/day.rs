//! What the box tells the fleet about a day that has finished.
//!
//! # A box meters; it does not model
//!
//! `hemsd simulate` can answer "what would this day have cost with no energy
//! manager" because it re-runs the day as an unmanaged house. A box on a wall
//! cannot: it has what happened, and a counterfactual is not a measurement. Five
//! of `CostBreakdown`'s six terms are modelled too — battery wear, curtailment,
//! discomfort, energy borrowed from the stores, charge past what was asked for.
//!
//! So this builds the half a box actually knows, and leaves
//! [`DayKpis::economics`] and [`DayKpis::forecast`] absent rather than filling
//! them with zeroes that would read as measurements (D116). What is left is not
//! small — it is every energy the connection point saw and the whole § 14a
//! compliance record, which is what `obsd` **counts** rather than averages and
//! is the reason the fleet view exists.
//!
//! # Built from the store, not from a counter in memory
//!
//! Every figure here is read back out of the rows the control loop already
//! wrote: the quarter-hour registers and the `[A1 7.2]` control events. A day
//! accumulated in memory would be lost to a restart at 23:50 — and a box that
//! silently reported four hours as a day would be worse than one that reported
//! nothing.
//!
//! The one exception is [`Unplanned`], which counts minutes the arbiter spent
//! with no plan. Nothing writes that to a row, because a row per minute of
//! ordinary operation is a lot of rows for a number that matters once a day.

use std::collections::BTreeMap;

use time::Date;

use hems_core::prelude::Slot;
use hems_core::report::{DayKpis, ForecastScores};
use hems_forecast::quantile::Band;

use crate::store::{Store, StoreError};

/// How much of a day must be scored before its coverage is a day's worth.
///
/// Half. A coverage figure is a fraction of the slots that fell inside their
/// band, and `obsd` merges each day as **one episode** — one draw — regardless
/// of how many slots stand behind it. So a box that saw eight slots after a
/// restart and reported them as a day would give a handful of quarter hours the
/// same weight in a fleet's calibration as a household that ran all day.
///
/// Half rather than all, because insisting on ninety-six would throw away every
/// day a box was restarted on, and a box under a rollout is restarted often.
const ENOUGH_OF_A_DAY: usize = 48;

/// What the box forecast for a slot, kept until the slot has happened.
///
/// The **bands the plan was made against**, not fresh ones: a score is only a
/// score of the forecast that was actually acted on. The planner republishes
/// these every five minutes, and a slot is scored against whatever band was
/// standing when the slot began.
pub type PublishedBands = BTreeMap<Slot, (Band, Band)>;

/// A day's forecast scores, accumulated one quarter hour at a time.
///
/// The box makes the forecast and later meters the truth, so it can score
/// itself — and it is the only thing that can, because the fleet never sees the
/// bands. Without this a fleet's `forecast_is_calibrated` could only ever be
/// true of simulations, since twenty independent *real* days would never arrive
/// (R22, R23).
#[derive(Debug, Clone, Default)]
pub struct Scored {
    pv: Vec<(Band, f64)>,
    load: Vec<(Band, f64)>,
}

impl Scored {
    /// Record what was forecast for a finished slot and what actually happened.
    pub fn observe(&mut self, bands: Option<&(Band, Band)>, pv_w: f64, load_w: f64) {
        if let Some((pv, load)) = bands {
            self.pv.push((*pv, pv_w));
            self.load.push((*load, load_w));
        }
    }

    /// The day's scores, or `None` where too little of the day was scored.
    #[must_use]
    pub fn scores(&self) -> Option<ForecastScores> {
        if self.pv.len() < ENOUGH_OF_A_DAY {
            return None;
        }
        let pv = hems_forecast::Calibration::score(self.pv.iter().copied());
        let load = hems_forecast::Calibration::score(self.load.iter().copied());
        Some(ForecastScores {
            pv_coverage: pv.coverage,
            pv_crps: pv.crps,
            load_coverage: load.coverage,
            load_crps: load.crps,
        })
    }

    /// Begin a new day.
    pub fn roll(&mut self) {
        self.pv.clear();
        self.load.clear();
    }
}

/// Minutes the arbiter spent on the fallback, accumulated across a day.
///
/// Kept in memory rather than in the store: it is one number a day, and a row
/// per minute of ordinary operation would be a lot of rows to answer it.
/// A restart loses it, and that is stated in the report rather than hidden —
/// see [`Unplanned::minutes`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Unplanned {
    /// Whole minutes on the fallback since this counter was last rolled.
    minutes: u32,
    /// Seconds not yet whole.
    seconds: u32,
    /// Whether the counter has run for the whole of the day it is reporting.
    ///
    /// False after a restart, and it makes the difference between "this box
    /// spent no time without a plan" and "this box was not watching for part of
    /// the day" — which a fleet counting unplanned minutes must not confuse.
    complete: bool,
}

impl Unplanned {
    /// A counter that has watched the whole of the day so far.
    #[must_use]
    pub const fn watching() -> Self {
        Self {
            minutes: 0,
            seconds: 0,
            complete: true,
        }
    }

    /// A counter that started part-way through a day, after a restart.
    #[must_use]
    pub const fn resumed() -> Self {
        Self {
            minutes: 0,
            seconds: 0,
            complete: false,
        }
    }

    /// Add one control period, which the arbiter spent with or without a plan.
    pub const fn tick(&mut self, had_a_plan: bool, period_s: u32) {
        if !had_a_plan {
            self.seconds += period_s;
            self.minutes += self.seconds / 60;
            self.seconds %= 60;
        }
    }

    /// The minutes to report, or `None` where the box was not watching for all
    /// of the day.
    #[must_use]
    pub const fn minutes(&self) -> Option<u32> {
        if self.complete {
            Some(self.minutes)
        } else {
            None
        }
    }

    /// Begin a new day, watching all of it.
    pub const fn roll(&mut self) {
        *self = Self::watching();
    }
}

/// Build the KPIs for the Berlin calendar day `date`, from what the box wrote
/// down.
///
/// The day boundary comes from `metering::calendar`, which is the same one
/// `[A1 10.1]` and the Zählzeitdefinitionen are written in — and the only one
/// that tiles a DST transition without a gap or an overlap. A day computed from
/// a fixed `+01:00` would lose an hour every March and double one every October,
/// on the two days of the year a household is most likely to look.
///
/// `None` where the box has no register for that day at all — a box that was off,
/// or one whose prices never arrived. Reporting a day of zeroes would put a
/// household on the fleet's roll with no imports, no exports and perfect
/// compliance, which is what a box that was unplugged looks like.
///
/// # Errors
/// [`StoreError`] where the box's own store cannot be read.
pub fn kpis(
    store: &Store,
    site: &str,
    date: Date,
    unplanned: Unplanned,
    scored: &Scored,
) -> Result<Option<DayKpis>, StoreError> {
    let from = metering::calendar::day_start_utc(date);
    let to = metering::calendar::day_end_utc(date);

    let registers = store.quarter_hours_between(from, to)?;
    if registers.is_empty() {
        return Ok(None);
    }

    let kwh = |d: rust_decimal::Decimal| -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        d.to_f64().unwrap_or(0.0)
    };
    let imported_kwh: f64 = registers.iter().map(|q| kwh(q.grid_draw)).sum();
    let exported_kwh: f64 = registers.iter().map(|q| kwh(q.grid_feed_in)).sum();
    let produced_kwh: f64 = registers.iter().map(|q| kwh(q.device_generation)).sum();

    // What the household consumed = what it drew from the grid, plus what it
    // produced and did not export. The registers are the only place this can be
    // computed from, and it is the same arithmetic the settlement uses.
    let consumed_kwh = imported_kwh + (produced_kwh - exported_kwh).max(0.0);
    let self_sufficiency = if consumed_kwh > 0.0 {
        ((consumed_kwh - imported_kwh) / consumed_kwh).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let events = store.control_events_between(from, to)?;
    let worst_overshoot_w = events
        .iter()
        .filter_map(|s| s.event.worst_overshoot())
        .map(hems_core::units::Power::get)
        .fold(0.0_f64, f64::max);

    Ok(Some(DayKpis {
        site: site.to_owned(),
        date,
        imported_kwh,
        exported_kwh,
        produced_kwh,
        self_sufficiency,
        // § 42c allocation is the community's arithmetic and not this box's.
        shared_kwh: 0.0,
        // Modelled, so absent. See the module note.
        economics: None,
        // Scored, though: the box makes the forecast and later meters the
        // truth, so this half needs no model it does not have.
        forecast: scored.scores(),
        // A day is compliant unless an event on it says otherwise. `[A1 7.2]`
        // events are the record, so the absence of one is the absence of a
        // reduction rather than the absence of evidence.
        respected_the_grid: worst_overshoot_w <= 0.0,
        worst_overshoot_w,
        // Zero where the box was not watching for the whole day. A restart is
        // not a day without a plan, and a fleet that added them would count a
        // reboot as a fault.
        minutes_without_a_plan: unplanned.minutes().unwrap_or(0),
        control_events: events.len(),
        below_minimum_commanded: events.iter().any(|s| s.event.below_minimum()),
        foresight_was_perfect: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use time::macros::date;

    fn register(
        slot_from: time::OffsetDateTime,
        i: i64,
        draw: i64,
        feed: i64,
        produced: i64,
    ) -> hems_grid::mispel::QuarterHour {
        hems_grid::mispel::QuarterHour {
            slot: hems_core::prelude::Slot::containing(slot_from + time::Duration::minutes(15 * i)),
            grid_draw: Decimal::new(draw, 2),
            grid_feed_in: Decimal::new(feed, 2),
            device_consumption: Decimal::ZERO,
            device_generation: Decimal::new(produced, 2),
            storage_consumption: None,
            storage_generation: None,
            anzulegender_wert: Decimal::new(786, 2),
            spot_price: Decimal::new(1250, 2),
        }
    }

    #[test]
    fn a_day_the_box_never_metered_is_no_day_at_all() {
        // Rather than a day of zeroes, which is what an unplugged box looks
        // like: no imports, no exports, and perfect compliance.
        let store = Store::in_memory().unwrap();
        let none = kpis(
            &store,
            "haus-1",
            date!(2026 - 01 - 15),
            Unplanned::watching(),
            &Scored::default(),
        )
        .unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn the_energies_come_from_the_registers_and_the_money_does_not() {
        let store = Store::in_memory().unwrap();
        let midnight = metering::calendar::day_start_utc(date!(2026 - 01 - 15));
        for i in 0..4 {
            store
                .put_quarter_hour(&register(midnight, i, 100, 25, 50), midnight)
                .unwrap();
        }

        let day = kpis(
            &store,
            "haus-1",
            date!(2026 - 01 - 15),
            Unplanned::watching(),
            &Scored::default(),
        )
        .unwrap()
        .expect("four registers is a day");

        assert!((day.imported_kwh - 4.0).abs() < 1e-9, "4 × 1,00 kWh");
        assert!((day.exported_kwh - 1.0).abs() < 1e-9);
        assert!((day.produced_kwh - 2.0).abs() < 1e-9);
        assert_eq!(
            day.economics, None,
            "a box meters; the money is modelled and it does not model it"
        );
        assert_eq!(day.forecast, None, "and it did not score its own bands");
        assert!(!day.is_measurable(), "so no saving may be computed from it");
    }

    #[test]
    fn a_register_from_the_next_day_belongs_to_the_next_day() {
        // The window is half-open, so the boundary quarter hour is counted once.
        let store = Store::in_memory().unwrap();
        let midnight = metering::calendar::day_start_utc(date!(2026 - 01 - 15));
        // 23:45 on the 15th, and 00:00 on the 16th.
        store
            .put_quarter_hour(&register(midnight, 95, 100, 0, 0), midnight)
            .unwrap();
        store
            .put_quarter_hour(&register(midnight, 96, 400, 0, 0), midnight)
            .unwrap();

        let day = kpis(
            &store,
            "haus-1",
            date!(2026 - 01 - 15),
            Unplanned::watching(),
            &Scored::default(),
        )
        .unwrap()
        .unwrap();
        assert!(
            (day.imported_kwh - 1.0).abs() < 1e-9,
            "only the 23:45 register: {}",
            day.imported_kwh
        );
    }

    #[test]
    fn a_restart_reports_no_unplanned_minutes_rather_than_none_observed() {
        // "Nothing went wrong" and "nobody was watching" are different facts,
        // and a fleet that adds unplanned minutes across a rollout must not read
        // a reboot as a clean day *or* as a fault.
        let mut counter = Unplanned::resumed();
        counter.tick(false, 60);
        counter.tick(false, 60);
        assert_eq!(counter.minutes(), None, "it did not watch the whole day");

        let mut whole = Unplanned::watching();
        for _ in 0..90 {
            whole.tick(false, 60);
        }
        for _ in 0..10 {
            whole.tick(true, 60);
        }
        assert_eq!(whole.minutes(), Some(90));

        whole.roll();
        assert_eq!(whole.minutes(), Some(0), "a new day starts clean");
    }

    #[test]
    fn a_box_scores_the_band_it_actually_planned_against() {
        // The half of the fleet's forecast picture a box can produce honestly:
        // it makes the forecast and later meters the truth. Without it twenty
        // independent *real* days never reach `obsd`, and
        // `forecast_is_calibrated` could only ever be true of simulations.
        let mut scored = Scored::default();
        let band = Band {
            p10: 900.0,
            p50: 1000.0,
            p90: 1100.0,
        };
        for i in 0..ENOUGH_OF_A_DAY {
            // Inside the band for all but a tenth of them, which is what an
            // honest 80 % band looks like.
            let actual = if i % 10 == 0 { 1400.0 } else { 1000.0 };
            scored.observe(Some(&(band, band)), actual, actual);
        }

        let s = scored.scores().expect("half a day is a day's worth");
        assert!(
            s.pv_coverage > 0.85 && s.pv_coverage < 0.95,
            "about nine in ten inside: {}",
            s.pv_coverage
        );
        assert!(s.pv_crps > 0.0, "and a cost for the ones that were not");
    }

    #[test]
    fn a_slot_nobody_forecast_is_not_scored_as_a_miss() {
        // A box between plans has no band for the slot that just ended. Scoring
        // it against nothing would report a forecast that was never made.
        let mut scored = Scored::default();
        for _ in 0..ENOUGH_OF_A_DAY {
            scored.observe(None, 1000.0, 500.0);
        }
        assert_eq!(scored.scores(), None, "nothing was scored, so no score");
    }

    #[test]
    fn a_handful_of_slots_is_not_a_days_coverage() {
        // `obsd` merges each day as **one episode** — one draw — however many
        // slots stand behind it. So eight quarter hours after a restart would
        // carry the same weight in a fleet's calibration as a household that ran
        // all day.
        let mut scored = Scored::default();
        let band = Band {
            p10: 0.0,
            p50: 1000.0,
            p90: 2000.0,
        };
        for _ in 0..(ENOUGH_OF_A_DAY - 1) {
            scored.observe(Some(&(band, band)), 1000.0, 1000.0);
        }
        assert_eq!(scored.scores(), None, "one slot short of half a day");

        scored.observe(Some(&(band, band)), 1000.0, 1000.0);
        assert!(scored.scores().is_some(), "and exactly half is enough");

        scored.roll();
        assert_eq!(scored.scores(), None, "a new day starts with nothing");
    }

    #[test]
    fn seconds_accumulate_into_minutes_without_drifting() {
        // The arbiter's period is a second, so this is 3600 additions a day and
        // the obvious `seconds / 60` on each one would floor away most of it.
        let mut counter = Unplanned::watching();
        for _ in 0..150 {
            counter.tick(false, 1);
        }
        assert_eq!(counter.minutes(), Some(2), "150 s is two whole minutes");
    }
}
