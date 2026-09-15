//! Whether the roof is still the roof it was.
//!
//! [`crate::residual::ResidualModel`] makes the forecast accurate by learning
//! what this array actually delivers against what its geometry says. That is the
//! right thing for a *planner* and the wrong thing for a *household*, and the
//! difference is the reason this module exists.
//!
//! # The corrector is built to absorb exactly what a household wants told
//!
//! The corrector's own documentation names what it follows: α = 0,03 is about a
//! fortnight of observations, "long enough for the weather to average out and
//! short enough to follow a season, **a cleaning or a new shading obstacle**".
//! So when a string fails, a tree grows, snow lies for a week or the array is
//! never cleaned again, the corrector does its job — it learns the lower
//! output, the forecast stays accurate, the planner keeps planning well — and
//! **nobody is ever told the roof got worse**. The yield is gone and the only
//! artefact is a number that drifted.
//!
//! That is not a defect in the corrector. It is a second question the same
//! measurements answer, and it needs the opposite time constant: the corrector's
//! job is to *follow* the roof, and this one's is to *notice it moving*.
//!
//! # What it measures
//!
//! The **performance ratio** in the sense of IEC 61724-1 — delivered energy over
//! the energy the plane-of-array irradiance and the nameplate say was available.
//! A residential array between 0,75 and 0,85 is ordinary; below about 0,70 the
//! losses are large enough to look for.
//!
//! hems gets it for nothing: the corrector already forms `actual / modelled`
//! every time a slot is scored, and `modelled` is a physical model rather than a
//! fitted one, so the denominator does not drift with the roof. What the monitor
//! adds is a **reference**: 0,90 could be an ordinary German roof in its third
//! year or one string of three stopped, and the ratio alone cannot say which.
//!
//! # The denominator is a forecast, not a measurement
//!
//! IEC 61724-1 puts **measured** in-plane irradiance under the ratio, from a
//! pyranometer in the array's own plane. This box has none, and divides by the
//! plane-of-array figure the *forecast* used — so the ratio carries the weather
//! model's error as well as the roof's condition, and one meter cannot separate
//! them.
//!
//! Day-to-day noise is absorbed, because it is most of what `spread` measures. A
//! **persistent** bias is not: a fortnight of a model promising more sun than
//! arrives reads like an array that has stopped delivering, and this module
//! reports it as one. A false alarm rather than a missed fault, which is the
//! safe way round for something whose only action is to send somebody to look —
//! but it is why this figure must not be set beside a commercial monitoring
//! product's PR as though they were the same measurement. Separating the two
//! needs a neighbouring array, which is a **fleet** question (R38).
//!
//! # How it tells a fault from weather
//!
//! Two time constants over the same daily figure, which is the standard
//! two-EWMA construction and is the cheapest thing that works:
//!
//! * **`baseline`** — a season. What this roof has earned over months, which is
//!   slow enough that a fault cannot become the new normal before it is
//!   reported.
//! * **`recent`** — a week. What it is delivering now.
//! * **`spread`** — the mean absolute day-to-day deviation from the baseline,
//!   learned on the same slow clock, so the threshold is *this roof's* own
//!   variability rather than a constant. A roof under broken cloud on a coast
//!   earns a wider one than a roof under a settled continental sky, and neither
//!   is told it is faulty for being itself.
//!
//! A verdict of [`Health::Degraded`] needs the day's own ratio to sit more than
//! [`SIGMAS`] spreads below `baseline` on [`PERSISTENCE_DAYS`] consecutive days
//! — the ordinary control-chart rule. One bad day is weather; three in a row is
//! something to go and look at.
//!
//! It reports **underperformance**, not a diagnosis. Snow, fog, leaves,
//! soiling, a new shadow and a failed string are indistinguishable from one
//! array's own meter, and pretending otherwise would be inventing a confidence
//! the measurement does not carry. What the household is told is how far down
//! the roof is and for how long, which is what sends somebody to look — and the
//! counter resets the day it recovers, so a spell of weather closes itself.
//!
//! Over a whole season the baseline does eventually follow — and that is
//! correct. A roof shaded by a tree that is not coming down really has a new
//! normal; the point is that the household was told once, while it was still
//! news.
//!
//! # Why this is not `chronix`'s anomaly detector
//!
//! The box stores its measurements in `chronix`, which ships a detector suite.
//! It is not the right tool here and the reason is the **signal** rather than
//! the algorithm: what has to be watched is `actual / modelled`, a ratio of a
//! measurement to a physical model evaluated per slot, and the model is not a
//! series in the store. Detecting on the raw production series instead would
//! flag every cloudy day, because a series that is *supposed* to vary by an
//! order of magnitude with the sky carries no anomaly signal of its own.
//!
//! The ratio also arrives already scaled: the corrector maintains a calibrated
//! dispersion per hour-of-day bucket, so the question "is this far?" has an
//! answer here that a generic z-score on a stored column cannot reach.
//! `CHRONIX_FEEDBACK.md` says what would change that.

/// How many spreads below its own baseline a roof has to sit.
///
/// Three, which is the ordinary control-chart figure and is deliberately not
/// tuned: this is a *notification* to a household, and the cost of crying wolf
/// is that the next one is ignored.
pub const SIGMAS: f64 = 3.0;

/// How many consecutive days it has to stay there.
///
/// Three. One bad day is weather, and a week is too slow to be worth telling
/// somebody about a string that failed on Monday.
pub const PERSISTENCE_DAYS: u32 = 3;

/// Days of history before a verdict means anything.
///
/// A season's baseline cannot be built in a fortnight, and a monitor that
/// announced a fault in its second week would be reporting its own warm-up.
pub const SETTLED_DAYS: u32 = 30;

/// The weight one day carries in the fast estimate — about a week.
const RECENT_ALPHA: f64 = 1.0 / 7.0;

/// The weight one day carries in the slow one — about a season.
const BASELINE_ALPHA: f64 = 1.0 / 90.0;

/// The smallest spread the test will use, as a fraction of the baseline.
///
/// Four per cent. Without a floor a roof that has had a fortnight of identical
/// settled days claims a spread near zero and then reports the first ordinary
/// cloudy week as a fault — the same failure the corrector's own band floor
/// exists to prevent, one level up.
const MIN_SPREAD: f64 = 0.04;

/// What the monitor makes of the roof.
///
/// [`Health::Learning`] with no days is the default, because that is what a
/// monitor that has seen nothing honestly says — and a default of `Healthy`
/// would be a box asserting a roof is fine before it has looked at it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "verdict"))]
pub enum Health {
    /// Not enough history to say. Carries the days it has.
    Learning {
        /// Days observed so far, of [`SETTLED_DAYS`].
        days: u32,
    },
    /// Delivering what this roof has earned the right to be expected to.
    Healthy {
        /// The performance ratio over the recent window.
        recent: f64,
        /// The ratio this roof has held over a season.
        baseline: f64,
    },
    /// Persistently below its own baseline — **underperforming**, which is not
    /// the same as *broken*.
    ///
    /// Snow, a fortnight of fog, leaves in autumn, soiling, a shading obstacle
    /// and a failed string all look alike from one array's own meter, and
    /// telling them apart needs something this box does not have — a
    /// neighbouring array, a per-string measurement, or a person on a ladder.
    /// What it can say honestly is *how far down, and for how long*, which is
    /// what a household needs in order to go and look.
    Degraded {
        /// The performance ratio over the recent window.
        recent: f64,
        /// The ratio this roof used to hold.
        baseline: f64,
        /// How many consecutive days it has been below.
        days: u32,
    },
}

impl Default for Health {
    fn default() -> Self {
        Self::Learning { days: 0 }
    }
}

impl Health {
    /// The share of its own baseline the roof is currently delivering.
    ///
    /// `None` while still learning. One is "exactly as it always was"; a
    /// household reads this more easily than two ratios.
    #[must_use]
    pub fn of_baseline(&self) -> Option<f64> {
        match self {
            Health::Learning { .. } => None,
            Health::Healthy { recent, baseline }
            | Health::Degraded {
                recent, baseline, ..
            } => (*baseline > 0.0).then(|| recent / baseline),
        }
    }

    /// Whether this verdict is one worth putting in front of somebody.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        matches!(self, Health::Degraded { .. })
    }
}

/// Watches one array's performance ratio against its own history.
///
/// Fed one **day** at a time, because the performance ratio is a daily figure
/// and a quarter hour of it is mostly geometry. It reads no clock and does no
/// I/O: the caller decides what a day is, which is what lets a whole winter run
/// as a unit test.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlantHealth {
    recent: f64,
    baseline: f64,
    spread: f64,
    days: u32,
    below: u32,
    /// What the model has said so far today, kWh.
    pending_modelled: f64,
    /// What the meter has seen so far today, kWh.
    pending_delivered: f64,
}

impl Default for PlantHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl PlantHealth {
    /// A monitor that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            recent: 0.0,
            baseline: 0.0,
            spread: 0.0,
            days: 0,
            below: 0,
            pending_modelled: 0.0,
            pending_delivered: 0.0,
        }
    }

    /// Add one completed slot to the day being accumulated.
    ///
    /// The running day lives **here** rather than in the caller so that it is
    /// serialised with the rest of the record: a box restarted at four in the
    /// afternoon keeps the morning it has already measured, where an accumulator
    /// held in the control loop would silently start the day again and report a
    /// half-day as a whole one — which is a fault-shaped thing to invent.
    pub fn observe_slot(&mut self, modelled_kwh: f64, delivered_kwh: f64) {
        if modelled_kwh.is_finite() && modelled_kwh > 0.0 {
            self.pending_modelled += modelled_kwh;
        }
        if delivered_kwh.is_finite() && delivered_kwh > 0.0 {
            self.pending_delivered += delivered_kwh;
        }
    }

    /// Close the accumulated day and judge it.
    ///
    /// Called on the local-day boundary. A day with nothing in the denominator
    /// — the box was off, or it is December and the array made nothing the model
    /// expected either — is discarded rather than recorded, and the accumulator
    /// is reset either way.
    pub fn close_day(&mut self) -> Health {
        let (modelled, delivered) = (self.pending_modelled, self.pending_delivered);
        self.pending_modelled = 0.0;
        self.pending_delivered = 0.0;
        self.observe_day(modelled, delivered)
    }

    /// Record one day, and say what the roof looks like after it.
    ///
    /// `modelled_kwh` is what the physical model said the array would make and
    /// `delivered_kwh` what the meter saw. A day with no sun in it — the
    /// denominator at or below zero — is **not** an observation: dividing one
    /// darkness by another is not a performance ratio, and in December it would
    /// otherwise be most of the record.
    pub fn observe_day(&mut self, modelled_kwh: f64, delivered_kwh: f64) -> Health {
        if !(modelled_kwh > 0.0 && delivered_kwh.is_finite() && delivered_kwh >= 0.0) {
            return self.verdict();
        }
        let ratio = delivered_kwh / modelled_kwh;
        if self.days == 0 {
            self.recent = ratio;
            self.baseline = ratio;
            self.spread = 0.0;
            self.days = 1;
            return self.verdict();
        }

        // Scored **before** the estimates move, so the day is judged against the
        // baseline that would actually have been published for it rather than
        // against one built with hindsight — the ordering the corrector's own
        // calibration turns on, for the same reason.
        let was_below = ratio < self.baseline - SIGMAS * self.floor_spread();
        self.below = if was_below { self.below + 1 } else { 0 };

        let deviation = (ratio - self.baseline).abs();
        self.spread = BASELINE_ALPHA.mul_add(deviation, (1.0 - BASELINE_ALPHA) * self.spread);
        self.recent = RECENT_ALPHA.mul_add(ratio, (1.0 - RECENT_ALPHA) * self.recent);
        self.baseline = BASELINE_ALPHA.mul_add(ratio, (1.0 - BASELINE_ALPHA) * self.baseline);
        self.days = self.days.saturating_add(1);
        self.verdict()
    }

    /// The spread the test uses: **debiased**, then floored at [`MIN_SPREAD`]
    /// of the baseline.
    ///
    /// An exponentially weighted mean started at zero estimates
    /// `μ·(1 − (1−α)ⁿ)` rather than `μ`, and at α = 1/90 that factor is **0,28
    /// after twenty-nine updates** — exactly where [`SETTLED_DAYS`] lets this
    /// monitor speak. Uncorrected, "three spreads below" is 0,83 spreads in
    /// month two and three in month twelve: a test whose sensitivity moves by a
    /// factor of three and a half while nobody touches it. A false alarm in
    /// month two teaches a household to ignore the true one in month eleven,
    /// which is what [`SIGMAS`] exists to avoid.
    ///
    /// Divide by the weight actually accumulated. One `powi` a day.
    fn floor_spread(&self) -> f64 {
        let updates = self.days.saturating_sub(1);
        let accumulated =
            1.0 - (1.0 - BASELINE_ALPHA).powi(i32::try_from(updates).unwrap_or(i32::MAX));
        let spread = if accumulated > f64::EPSILON {
            self.spread / accumulated
        } else {
            self.spread
        };
        spread.max(MIN_SPREAD * self.baseline.abs())
    }

    /// What the monitor makes of the roof right now.
    #[must_use]
    pub fn verdict(&self) -> Health {
        if self.days < SETTLED_DAYS {
            return Health::Learning { days: self.days };
        }
        // The persistence counter alone, and deliberately. Requiring the *weekly*
        // mean to have fallen too is the same test twice with a lag on it: an
        // EWMA over seven days needs about seven to reflect a step, so a string
        // that failed on Monday would not be mentioned until the following week
        // — by which time the corrector has started calling it normal, which is
        // the one thing this module exists to beat. `N consecutive days beyond
        // three sigma` is the ordinary control-chart rule and it is enough.
        if self.below >= PERSISTENCE_DAYS {
            return Health::Degraded {
                recent: self.recent,
                baseline: self.baseline,
                days: self.below,
            };
        }
        Health::Healthy {
            recent: self.recent,
            baseline: self.baseline,
        }
    }

    /// The performance ratio over the recent window, whatever the verdict.
    #[must_use]
    pub fn performance_ratio(&self) -> f64 {
        self.recent
    }

    /// Days of history behind it.
    #[must_use]
    pub fn days(&self) -> u32 {
        self.days
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `days` of a roof delivering `ratio` of its model, with `noise`
    /// alternating either side so the spread is something rather than nothing.
    fn run(m: &mut PlantHealth, days: u32, ratio: f64, noise: f64) -> Health {
        let mut last = m.verdict();
        for d in 0..days {
            let wobble = if d % 2 == 0 { noise } else { -noise };
            last = m.observe_day(10.0, 10.0 * (ratio + wobble));
        }
        last
    }

    #[test]
    fn a_monitor_with_no_history_refuses_to_judge() {
        let mut m = PlantHealth::new();
        assert_eq!(m.verdict(), Health::Learning { days: 0 });
        let v = run(&mut m, 10, 0.85, 0.02);
        assert!(
            matches!(v, Health::Learning { .. }),
            "ten days is not a season: {v:?}"
        );
        assert!(v.of_baseline().is_none());
    }

    #[test]
    fn an_ordinary_roof_settles_healthy() {
        let mut m = PlantHealth::new();
        let v = run(&mut m, 120, 0.85, 0.03);
        assert!(matches!(v, Health::Healthy { .. }), "{v:?}");
        assert!(
            (m.performance_ratio() - 0.85).abs() < 0.02,
            "the ratio it was fed: {}",
            m.performance_ratio()
        );
        assert!(v.of_baseline().is_some_and(|r| (r - 1.0).abs() < 0.05));
    }

    /// The case the corrector cannot report: a step down that stays down.
    #[test]
    fn a_string_that_fails_is_reported_rather_than_learned() {
        let mut m = PlantHealth::new();
        run(&mut m, 120, 0.85, 0.03);
        assert!(!m.verdict().is_degraded(), "healthy before the fault");

        // One of three strings stops: a third of the output, overnight.
        let v = run(&mut m, PERSISTENCE_DAYS, 0.85 * 2.0 / 3.0, 0.03);
        assert!(
            v.is_degraded(),
            "a third of the array stopping has to be reported: {v:?}"
        );
        let Health::Degraded {
            recent,
            baseline,
            days,
        } = v
        else {
            unreachable!("asserted above")
        };
        assert!(days >= PERSISTENCE_DAYS);
        assert!(recent < baseline, "{recent} against {baseline}");
        assert!(
            v.of_baseline().is_some_and(|r| r < 0.95),
            "and the household is told how much of its roof is left"
        );
    }

    /// …and the thing that makes it worth having: it fires **before** the
    /// corrector has absorbed the fault.
    ///
    /// The corrector's memory is about a fortnight of observations; this has to
    /// speak inside that window or it is reporting history. Three days is well
    /// inside it, which is the whole design.
    #[test]
    fn it_speaks_while_the_fault_is_still_news() {
        let mut m = PlantHealth::new();
        run(&mut m, 120, 0.85, 0.03);
        let mut spoke_on = None;
        for d in 1..=14 {
            let v = m.observe_day(10.0, 10.0 * 0.85 * 2.0 / 3.0);
            if v.is_degraded() && spoke_on.is_none() {
                spoke_on = Some(d);
            }
        }
        let day = spoke_on.expect("a third of the array is gone and nothing said so");
        assert!(
            day <= PERSISTENCE_DAYS + 1,
            "it took {day} days, by which time the corrector has started calling it normal"
        );
    }

    /// A fortnight of bad weather is not a fault.
    ///
    /// The distinction is *recovery*: an overcast spell moves `recent` down and
    /// then lets it back up, and the baseline it is judged against is rebuilt
    /// from the same days.
    #[test]
    fn a_bad_fortnight_is_weather_and_recovers() {
        let mut m = PlantHealth::new();
        run(&mut m, 150, 0.85, 0.04);
        // A dull spell: a fifth off, which is weather rather than hardware.
        run(&mut m, 10, 0.85 * 0.8, 0.04);
        // …and then it clears.
        let v = run(&mut m, 20, 0.85, 0.04);
        assert!(
            !v.is_degraded(),
            "a spell of cloud that recovered is not a broken roof: {v:?}"
        );
    }

    /// A dark day is not an observation.
    #[test]
    fn a_december_night_does_not_enter_the_record() {
        let mut m = PlantHealth::new();
        run(&mut m, 60, 0.85, 0.02);
        let before = (m.days(), m.performance_ratio());
        for _ in 0..20 {
            m.observe_day(0.0, 0.0);
        }
        assert_eq!(
            (m.days(), m.performance_ratio()),
            before,
            "dividing one darkness by another is not a performance ratio"
        );
    }

    /// A settled roof does not claim a spread of zero and then panic.
    #[test]
    fn a_roof_with_no_variation_still_has_a_floor_under_its_threshold() {
        let mut m = PlantHealth::new();
        // Identical days: the measured spread collapses toward nothing.
        for _ in 0..120 {
            m.observe_day(10.0, 8.5);
        }
        assert!(!m.verdict().is_degraded());
        // An ordinary 3 % cloudy week must not read as a fault.
        let v = run(&mut m, 7, 0.85 * 0.97, 0.0);
        assert!(
            !v.is_degraded(),
            "three per cent off a settled roof is weather, not hardware: {v:?}"
        );
    }

    /// The threshold must mean the same thing in month two as in month twelve.
    ///
    /// An exponentially weighted spread started at zero reaches only 28 % of the
    /// quantity it estimates by the time [`SETTLED_DAYS`] lets the monitor
    /// speak, so an uncorrected test is three and a half times more
    /// trigger-happy on its first day than on its four-hundredth. The defect is
    /// invisible in every test that runs a roof for one length of time, which is
    /// why this one runs two.
    #[test]
    fn a_young_monitor_is_no_more_suspicious_than_an_old_one() {
        // A roof that wobbles by five per cent either side, for a month and for
        // a year. Both have exactly the same day-to-day spread.
        let mut young = PlantHealth::new();
        run(&mut young, SETTLED_DAYS, 0.85, 0.05);
        let mut old = PlantHealth::new();
        run(&mut old, 400, 0.85, 0.05);

        let (a, b) = (young.floor_spread(), old.floor_spread());
        assert!(
            (a - b).abs() / b < 0.15,
            "the same roof is judged against {a:.4} after a month and {b:.4} after a year"
        );

        // …and neither calls an ordinary wobble a fault.
        for m in [&mut young, &mut old] {
            let v = run(m, PERSISTENCE_DAYS, 0.85, 0.05);
            assert!(!v.is_degraded(), "an ordinary roof was reported: {v:?}");
        }
    }
}
