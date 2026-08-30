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

/// How a forecast performed over a set of outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Calibration {
    /// How many pairs were scored.
    pub samples: usize,
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
    /// Score a sequence of `(forecast, actual)` pairs.
    #[must_use]
    pub fn score(pairs: impl IntoIterator<Item = (Band, f64)>) -> Self {
        let mut c = Self::default();
        let mut inside = 0usize;
        for (band, actual) in pairs {
            c.samples += 1;
            c.pinball += band_pinball(band, actual);
            c.crps += band_crps(band, actual);
            c.bias += actual - band.p50;
            c.mae += (actual - band.p50).abs();
            if actual >= band.p10 && actual <= band.p90 {
                inside += 1;
            }
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

    /// Whether the band is about as wide as it should be.
    ///
    /// A 10–90 band ought to contain four outcomes in five. Outside `[0,6, 0,95]`
    /// the forecast is either overconfident — and the optimiser is planning
    /// against a certainty that does not exist — or so wide that it carries no
    /// information.
    #[must_use]
    pub fn is_well_calibrated(&self) -> bool {
        self.samples >= 20 && (0.6..=0.95).contains(&self.coverage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Nine in ten outcomes inside a wide band.
        let pairs: Vec<(Band, f64)> = (0..100)
            .map(|i| {
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
            .collect();
        let c = Calibration::score(pairs);
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
    fn scoring_nothing_is_not_a_division_by_zero() {
        let c = Calibration::score([]);
        assert_eq!(c.samples, 0);
        assert_eq!(c.pinball, 0.0);
        assert!(!c.is_well_calibrated());
    }
}
