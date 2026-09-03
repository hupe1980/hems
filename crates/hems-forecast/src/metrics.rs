//! Was the forecast any good?
//!
//! A forecast nobody scores drifts, and the first sign is usually a household
//! wondering why its battery is empty every evening. These are the three numbers
//! worth tracking, computed the same way every time so they can be compared
//! across releases:
//!
//! * **pinball loss** — the proper score for a quantile. Lower is better, and
//!   unlike an absolute error it punishes over- and under-forecasting
//!   differently, which is right: running out of battery is not the same kind of
//!   mistake as leaving some unused.
//! * **coverage** — how often the truth actually landed inside the band. A 10-90
//!   band should contain the outcome 80 % of the time; a band that contains it
//!   99 % of the time is uselessly wide, and one that manages 40 % is lying.
//! * **bias** — the mean signed error, which says whether the model is
//!   systematically optimistic.
//! * **CRPS** — the continuous ranked probability score, which is the number
//!   the forecasting literature compares models on, so a claim about this
//!   crate's forecasts can be put beside a published one. It is the pinball
//!   loss integrated over every quantile level; with three levels the integral
//!   is approximated by their mean, doubled — the standard quantile
//!   approximation, and it is stated here rather than hidden because with three
//!   levels it is an approximation and not the score itself.
//!
//! # What is not scored, and why leaving it in was worse than a bug
//!
//! A photovoltaic forecast of nothing, against an outcome of nothing, is not a
//! forecast that came true. It is midnight.
//!
//! Two thirds of a January day are dark. Scoring all ninety-six quarter hours
//! puts sixty-four pairs of `0 [0 … 0]` against `0` into the denominator, every
//! one of them trivially *inside* its own band and trivially zero-loss, and the
//! result is a coverage figure that cannot fall below 67 % however wrong the
//! forecast is, and a CRPS diluted by a factor of three. It reads like a
//! measurement and it is arithmetic about the night.
//!
//! That is not hypothetical: this crate's own reference days reported a
//! production band covering 93 % of outcomes against the 80 % it promises, a
//! backlog item was opened to make the band narrower, and a design document
//! recorded it as a blocker for the scenario planner. Once the dark slots come
//! out, the same day covers **80 %** — the band was right all along, and the
//! number that said otherwise was measuring how long the night is.
//!
//! So [`Calibration::score`] skips a pair where the band's own top and the
//! outcome are both zero, and counts what it skipped
//! ([`Calibration::skipped`]). No threshold and no special case for solar: the
//! test is "did either side of this comparison contain a quantity", and for a
//! quantity that is never zero — a household's load — it removes nothing at
//! all, which is the property that makes it safe to apply to every forecast.

use crate::quantile::Band;

/// The pinball loss of one quantile forecast against one outcome.
#[must_use]
pub fn pinball(quantile: f64, forecast: f64, actual: f64) -> f64 {
    let error = actual - forecast;
    if error >= 0.0 {
        quantile * error
    } else {
        (quantile - 1.0) * error
    }
}

/// The mean pinball loss of a band, averaged over its three quantiles.
#[must_use]
pub fn band_pinball(band: Band, actual: f64) -> f64 {
    (pinball(0.1, band.p10, actual)
        + pinball(0.5, band.p50, actual)
        + pinball(0.9, band.p90, actual))
        / 3.0
}

/// The continuous ranked probability score of a band against one outcome.
///
/// `CRPS = 2 ∫₀¹ QL_q dq`, approximated by the mean of the quantile losses
/// actually held. With three levels that approximation is coarse — it is exact
/// only in the limit of a dense grid — but it is the one the M-competitions and
/// most published benchmarks use, so a CRPS from this crate is comparable with
/// a CRPS from a paper. It is in the unit of the quantity, and a deterministic
/// forecast's CRPS is its absolute error, which is the property that makes it
/// the right score to compare a band against a point forecast.
#[must_use]
pub fn band_crps(band: Band, actual: f64) -> f64 {
    2.0 * band_pinball(band, actual)
}

/// Whether a `(forecast, outcome)` pair says anything about the forecast.
///
/// It does not when the band's own top and the outcome are both zero: nothing
/// was forecast, nothing happened, and no band could have been wrong. See the
/// module note for what counting those instead did to this project's own
/// numbers.
#[must_use]
pub fn is_informative(band: Band, actual: f64) -> bool {
    band.p90 > 0.0 || actual > 0.0
}

/// The fewest independent days a coverage figure may be called calibration on.
///
/// Twenty **days**, not twenty quarter hours. At an 80 % nominal band the
/// binomial standard error on twenty draws is still nine percentage points, so
/// this is a floor rather than a comfortable one — but it is the difference
/// between a statement and a coin toss, and the number that was there before
/// this was spelled in days was satisfied by a fifth of one afternoon.
pub const CALIBRATION_DAYS: usize = 20;

/// How a forecast performed over a set of outcomes.
///
/// # Two counts, and only one of them is a sample size
///
/// [`Calibration::samples`] is how many quarter hours were scored.
/// [`Calibration::episodes`] is how many **days** they came from, and it is the
/// one [`Calibration::is_well_calibrated`] reads.
///
/// Forecast error is correlated across a day — a front arrives three hours late
/// and every slot from lunchtime on is wrong in the same direction — so ninety-
/// six slots of one Tuesday are not ninety-six samples. They are close to *one*.
/// A day therefore lands mostly inside its own band or mostly outside it, and a
/// coverage figure computed from one day says almost nothing: it is a coin toss
/// reported to three significant figures.
///
/// Counting quarter hours is the mistake that hides itself, because it makes the
/// number look precise exactly when it is least trustworthy. So the type carries
/// both, and the calibration verdict is spelled in days.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Calibration {
    /// How many pairs were scored.
    pub samples: usize,
    /// How many pairs held nothing to forecast and were left out — see the
    /// module note.
    ///
    /// Reported rather than dropped silently, because it is the number that
    /// says how much of a day the score is actually about. A production score
    /// over a January day skips about two thirds of it; a load score skips
    /// none, and one that suddenly does is a meter that has stopped.
    pub skipped: usize,
    /// How many independent **days** those pairs came from.
    ///
    /// One for a single day scored with [`Calibration::score`]; the sum for a
    /// back-test assembled with [`Calibration::merge`].
    pub episodes: usize,
    /// Mean pinball loss across the band. Lower is better.
    pub pinball: f64,
    /// The share of outcomes that fell inside the 10–90 band. Should be near 0,8.
    pub coverage: f64,
    /// Mean signed error of the median, `actual − forecast`. Positive means the
    /// model is systematically forecasting too little.
    pub bias: f64,
    /// Mean absolute error of the median.
    pub mae: f64,
    /// Mean continuous ranked probability score, in the unit of the quantity.
    ///
    /// The one number to compare a probabilistic forecast against a published
    /// benchmark, and the one to watch across releases: it moves when either
    /// the median or the width gets worse, where [`Calibration::mae`] only sees
    /// the median and [`Calibration::coverage`] only sees the width.
    pub crps: f64,
}

impl Calibration {
    /// Score **one day** of `(forecast, actual)` pairs.
    ///
    /// One episode, however many quarter hours it holds — see the note on the
    /// type. A back-test assembles days with [`Calibration::merge`].
    #[must_use]
    pub fn score(pairs: impl IntoIterator<Item = (Band, f64)>) -> Self {
        let mut c = Self::default();
        let mut inside = 0usize;
        for (band, actual) in pairs {
            if !is_informative(band, actual) {
                c.skipped += 1;
                continue;
            }
            c.samples += 1;
            c.pinball += band_pinball(band, actual);
            c.crps += band_crps(band, actual);
            c.bias += actual - band.p50;
            c.mae += (actual - band.p50).abs();
            if actual >= band.p10 && actual <= band.p90 {
                inside += 1;
            }
        }
        if c.samples > 0 || c.skipped > 0 {
            c.episodes = 1;
        }
        if c.samples > 0 {
            let n = c.samples as f64;
            c.pinball /= n;
            c.crps /= n;
            c.bias /= n;
            c.mae /= n;
            c.coverage = inside as f64 / n;
        }
        c
    }

    /// Combine this day's score with another's.
    ///
    /// Every mean is re-weighted by the number of pairs behind it, so merging
    /// twenty days gives the same numbers as scoring their pairs together —
    /// except for [`Calibration::episodes`], which is the whole point: the
    /// merged figure knows it rests on twenty independent draws rather than on
    /// nineteen hundred correlated ones.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        let total = self.samples + other.samples;
        if total == 0 {
            return Self {
                skipped: self.skipped + other.skipped,
                episodes: self.episodes + other.episodes,
                ..Self::default()
            };
        }
        let (a, b) = (self.samples as f64, other.samples as f64);
        let n = total as f64;
        let mean = |x: f64, y: f64| x.mul_add(a, y * b) / n;
        Self {
            samples: total,
            skipped: self.skipped + other.skipped,
            episodes: self.episodes + other.episodes,
            pinball: mean(self.pinball, other.pinball),
            coverage: mean(self.coverage, other.coverage),
            bias: mean(self.bias, other.bias),
            mae: mean(self.mae, other.mae),
            crps: mean(self.crps, other.crps),
        }
    }

    /// Whether the band is about as wide as it should be.
    ///
    /// A 10–90 band ought to contain four outcomes in five. Outside `[0,6, 0,95]`
    /// the forecast is either overconfident — and the optimiser is planning
    /// against a certainty that does not exist — or so wide that it carries no
    /// information.
    ///
    /// **Twenty days**, not twenty quarter hours ([`CALIBRATION_DAYS`]). A single
    /// day's ninety-six slots are one draw wearing ninety-six hats, so a day that
    /// happened to land inside its band would otherwise report itself calibrated
    /// on the strength of one afternoon.
    #[must_use]
    pub fn is_well_calibrated(&self) -> bool {
        self.episodes >= CALIBRATION_DAYS && (0.6..=0.95).contains(&self.coverage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Score `days` and merge them, one [`Calibration::episodes`] each.
    ///
    /// A **test** helper rather than API. It was public, and its only callers
    /// were the four tests below: a fold over `score` and `merge` is two lines
    /// at any call site that wants one, and `hemsd backtest` — the one thing in
    /// the workspace that produces independent days — cannot use it anyway,
    /// because it merges as it goes rather than holding a sweep in memory. A
    /// second way to do what the daemon does by hand is a second thing to keep
    /// right (R20).
    fn back_test<I>(days: impl IntoIterator<Item = I>) -> Calibration
    where
        I: IntoIterator<Item = (Band, f64)>,
    {
        days.into_iter()
            .map(Calibration::score)
            .fold(Calibration::default(), Calibration::merge)
    }

    #[test]
    fn pinball_punishes_the_two_directions_differently() {
        // For the 10th percentile, over-forecasting is the expensive mistake.
        let over = pinball(0.1, 100.0, 50.0);
        let under = pinball(0.1, 50.0, 100.0);
        assert!(over > under, "{over} should exceed {under}");
        // For the 90th, it is the other way round.
        assert!(pinball(0.9, 50.0, 100.0) > pinball(0.9, 100.0, 50.0));
    }

    #[test]
    fn a_perfect_forecast_scores_zero() {
        assert_eq!(band_pinball(Band::certain(42.0), 42.0), 0.0);
    }

    #[test]
    fn coverage_catches_an_overconfident_model() {
        // A narrow band that is nearly always wrong.
        let pairs: Vec<(Band, f64)> = (0..100)
            .map(|i| (Band::relative(100.0, 0.01), 100.0 + f64::from(i % 40) * 5.0))
            .collect();
        let c = Calibration::score(pairs);
        assert!(c.coverage < 0.3, "coverage {}", c.coverage);
        assert!(!c.is_well_calibrated());
    }

    #[test]
    fn coverage_accepts_an_honest_model() {
        // Nine in ten outcomes inside a wide band — over enough *days* for the
        // figure to mean anything, which one hundred quarter hours of a single
        // Tuesday would not (see `one_day_is_one_episode…`).
        let day = || {
            (0..100).map(|i| {
                let actual = if i % 10 == 0 { 500.0 } else { 100.0 };
                (
                    Band {
                        p10: 50.0,
                        p50: 100.0,
                        p90: 200.0,
                    },
                    actual,
                )
            })
        };
        let c = back_test((0..CALIBRATION_DAYS).map(|_| day()));
        assert!((c.coverage - 0.9).abs() < 1e-9);
        assert!(c.is_well_calibrated());
    }

    #[test]
    fn bias_shows_a_model_that_is_systematically_low() {
        let pairs: Vec<(Band, f64)> = (0..30).map(|_| (Band::certain(100.0), 150.0)).collect();
        let c = Calibration::score(pairs);
        assert!((c.bias - 50.0).abs() < 1e-9);
        assert!((c.mae - 50.0).abs() < 1e-9);
    }

    #[test]
    fn the_night_is_not_a_forecast_that_came_true() {
        // The defect this closes, in the shape it had. A January day: sixty-four
        // dark quarter hours where the model says nothing and nothing happens,
        // and thirty lit ones where the band is honestly wrong a fifth of the
        // time. Counting the dark ones reports 94 % coverage against a nominal
        // 80 % and sends somebody off to narrow a band that was right.
        let day = || {
            (0..96).map(|i| {
                if i < 66 {
                    (Band::certain(0.0), 0.0)
                } else if (i - 66) % 5 == 0 {
                    // Outside the band: the forecast really was wrong here.
                    (Band::relative(1000.0, 0.3), 2000.0)
                } else {
                    (Band::relative(1000.0, 0.3), 1000.0)
                }
            })
        };
        let c = Calibration::score(day());
        assert_eq!(c.samples, 30, "only the lit slots are a forecast");
        assert_eq!(c.skipped, 66, "and the night is counted, not hidden");
        assert!(
            (c.coverage - 0.8).abs() < 1e-9,
            "coverage {} should be the daylight figure, not the 0,94 the \
             night would have reported",
            c.coverage
        );
        // Twenty such days are a calibration, and they say the band is right.
        let year = back_test((0..CALIBRATION_DAYS).map(|_| day()));
        assert!(year.is_well_calibrated());
        assert_eq!(year.skipped, 66 * CALIBRATION_DAYS);
    }

    #[test]
    fn a_quantity_that_is_never_zero_loses_nothing() {
        // The property that makes the rule safe to apply to every forecast
        // rather than to solar alone: a household's load is never nothing, so
        // nothing is skipped and the score is exactly what it was.
        let pairs: Vec<(Band, f64)> = (0..96)
            .map(|i| (Band::relative(400.0, 0.2), 350.0 + f64::from(i % 7) * 20.0))
            .collect();
        let c = Calibration::score(pairs);
        assert_eq!(c.samples, 96);
        assert_eq!(c.skipped, 0);
    }

    #[test]
    fn a_day_that_was_entirely_dark_is_an_episode_that_scored_nothing() {
        // A December day under a week of fog still happened, and a back-test
        // that silently dropped it would count nineteen days as twenty.
        let c = Calibration::score((0..96).map(|_| (Band::certain(0.0), 0.0)));
        assert_eq!(c.samples, 0);
        assert_eq!(c.skipped, 96);
        assert_eq!(c.episodes, 1);
        assert!(!c.is_well_calibrated());
    }

    #[test]
    fn scoring_nothing_is_not_a_division_by_zero() {
        let c = Calibration::score([]);
        assert_eq!(c.samples, 0);
        assert_eq!(c.pinball, 0.0);
        assert!(!c.is_well_calibrated());
    }

    #[test]
    fn one_day_is_one_episode_however_many_quarter_hours_it_holds() {
        // The bug this closes: ninety-six slots of one Tuesday reporting
        // themselves as ninety-six independent samples, so a single afternoon
        // that happened to land inside its band called itself calibrated.
        let day: Vec<(Band, f64)> = (0..96)
            .map(|_| {
                (
                    Band {
                        p10: 50.0,
                        p50: 100.0,
                        p90: 200.0,
                    },
                    100.0,
                )
            })
            .collect();
        let c = Calibration::score(day);
        assert_eq!(c.samples, 96);
        assert_eq!(c.episodes, 1);
        assert_eq!(c.coverage, 1.0);
        assert!(
            !c.is_well_calibrated(),
            "one day is not a calibration, whatever its coverage"
        );
    }

    #[test]
    fn twenty_days_are_a_calibration_and_nineteen_are_not() {
        let day = || {
            (0..96).map(|i| {
                let actual = if i % 10 == 0 { 500.0 } else { 100.0 };
                (
                    Band {
                        p10: 50.0,
                        p50: 100.0,
                        p90: 200.0,
                    },
                    actual,
                )
            })
        };
        let nineteen = back_test((0..19).map(|_| day()));
        assert_eq!(nineteen.episodes, 19);
        assert!(!nineteen.is_well_calibrated());

        let twenty = back_test((0..20).map(|_| day()));
        assert_eq!(twenty.episodes, 20);
        assert_eq!(twenty.samples, 96 * 20);
        assert!(twenty.is_well_calibrated());
    }

    #[test]
    fn merging_days_gives_the_same_means_as_scoring_them_together() {
        let a: Vec<(Band, f64)> = (0..40)
            .map(|i| (Band::relative(100.0, 0.3), 90.0 + f64::from(i)))
            .collect();
        let b: Vec<(Band, f64)> = (0..17)
            .map(|i| (Band::relative(200.0, 0.2), 150.0 + f64::from(i) * 3.0))
            .collect();

        let merged = Calibration::score(a.clone()).merge(Calibration::score(b.clone()));
        let together = Calibration::score(a.into_iter().chain(b));

        assert_eq!(merged.samples, together.samples);
        for (x, y) in [
            (merged.pinball, together.pinball),
            (merged.crps, together.crps),
            (merged.bias, together.bias),
            (merged.mae, together.mae),
            (merged.coverage, together.coverage),
        ] {
            assert!((x - y).abs() < 1e-9, "{x} against {y}");
        }
        // …and only the episode count knows the difference, which is the point.
        assert_eq!(merged.episodes, 2);
        assert_eq!(together.episodes, 1);
    }

    #[test]
    fn merging_carries_the_skipped_count_through() {
        let dark = Calibration::score((0..64).map(|_| (Band::certain(0.0), 0.0)));
        let lit = Calibration::score((0..32).map(|_| (Band::relative(1000.0, 0.3), 1000.0)));
        let both = dark.merge(lit);
        assert_eq!(both.samples, 32);
        assert_eq!(both.skipped, 64);
        assert_eq!(both.episodes, 2);
    }

    #[test]
    fn merging_nothing_into_something_changes_nothing() {
        let c = Calibration::score((0..5).map(|_| (Band::certain(1.0), 1.0)));
        assert_eq!(c.merge(Calibration::default()), c);
        assert_eq!(Calibration::default().merge(c), c);
    }
}
