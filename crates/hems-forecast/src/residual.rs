//! What the physical model got wrong last time, remembered.
//!
//! [`crate::solar`] computes what an array *would* produce under a clear sky
//! from geometry alone. That is a model, not a forecast: it knows nothing about
//! the tree that shades the east string until ten, the chimney, the fact that
//! the installer wired one string in reverse, or that this roof has not been
//! cleaned since 2023. Every one of those is a **systematic** error — the same
//! sign, at the same time of day, every day — and a planner that never learns
//! them plans the same battery too small every morning.
//!
//! So the model's output is multiplied by a ratio learned from the site's own
//! history, and the *spread* of that ratio becomes the width of the band. The
//! form is deliberately the humblest thing that works:
//!
//! * **multiplicative**, because the errors scale with the irradiance — a 10 %
//!   shading loss is 400 W at noon and 40 W at eight in the morning, and an
//!   additive correction learned at noon would invent 400 W of production at
//!   midnight;
//! * **bucketed by hour of the local day**, because a residual at eight in the
//!   morning is a different fact from a residual at noon — that is where the
//!   shade is;
//! * **exponentially weighted**, so a roof that has just been cleaned, or a
//!   winter that has just started, is not held to the last two years;
//! * and it **widens rather than narrows** where there is no history, because a
//!   forecast with no evidence behind it must not look confident.
//!
//! This is the "online residual correction on the ratio actual/model" of the
//! design, and it is the whole difference between a photovoltaic *model* and a
//! photovoltaic *forecast*. The same object works for any quantity a physical
//! model predicts and a meter later measures.
//!
//! # The band is calibrated against its own outcomes, not against a normal
//!
//! The median is one question and the **width** is another, and the second is
//! where a corrector like this normally goes wrong. The obvious construction —
//! take the mean absolute deviation of the ratio, multiply by the factor that
//! turns a MAD into a 10–90 half-width *for a normal distribution* (1,606), and
//! floor it so a quiet week cannot claim certainty — is wrong on two counts.
//!
//! The residual of a roof is **not normal**: it has a hard ceiling at the clear
//! sky and a long tail downwards, so a Gaussian conversion of its dispersion
//! over-states the central quantiles. And the floor is a **constant**: measured
//! on this crate's own June reference day, sixty-six of the sixty-seven daylight
//! quarter hours came out sitting exactly on it. A band that is the same ±12 %
//! in every slot of a clear summer day is not an uncertainty estimate, it is a
//! house number wearing one — and the scenario planner pays for its width in
//! every hedge it takes against a pessimistic future that never happens.
//!
//! So the two tails are **calibrated online against the outcomes they are meant
//! to bound**. Each bucket carries a multiplier per tail, seeded at the Gaussian
//! guess and then moved by
//!
//! ```text
//! lo ← lo · exp(η · (1{actual < p10} − 0,10))
//! hi ← hi · exp(η · (1{actual > p90} − 0,10))
//! ```
//!
//! which has its fixed point exactly where a tenth of outcomes fall outside each
//! side — that is, where the band covers 80 % — whatever shape the residual
//! distribution has. It is the update of **adaptive conformal inference** (Gibbs
//! and Candès, *Adaptive Conformal Inference Under Distribution Shift*, 2021)
//! applied to one scalar per tail rather than to a miscoverage level, and it
//! costs two multiplications per observation. The paper is not in `specs/`,
//! because that index carries the regulatory and domain sources this workspace
//! cites *normatively*; this is a method, and the fixed point above is its whole
//! content.
//!
//! Three consequences worth naming. The band comes out **asymmetric**, which is
//! right: a roof can fall a long way below the model and cannot rise far above
//! it. A bucket whose dispersion has collapsed can still be calibrated, because
//! the multiplier rides on a base that is floored rather than on the dispersion
//! alone. And the floor now applies where it belongs — to a bucket with too
//! little history to have been calibrated at all — rather than to every slot of
//! every clear day.
//!
//! [`crate::metrics::Calibration::coverage`] is then a genuine check on this
//! estimator rather than a check on an assumption, which is the property that
//! makes it worth reporting beside the money.
//!
//! # What it deliberately does not do
//!
//! It does not learn from slots where the model predicted nothing. A ratio of
//! `actual / 0` is not a large residual, it is an undefined one, and feeding it
//! in is how a corrector learns to produce electricity at night.

use std::collections::BTreeMap;

use hems_core::prelude::Slot;

use crate::quantile::Band;

/// The share of a new observation that enters the running estimate.
///
/// The memory is about `2/α − 1` **observations**, not days, and getting that
/// distinction wrong is how a corrector ends up chasing last Tuesday: a bucket
/// an hour wide is fed four times a day, so α = 0,1 remembers two and a half
/// days rather than the ten it looks like. At 0,03 the window is about
/// fifty-five observations — a fortnight — which is long enough for the weather
/// to average out and short enough to follow a season, a cleaning or a new
/// shading obstacle.
pub const DEFAULT_ALPHA: f64 = 0.03;

/// How small a modelled value has to be before a ratio against it is noise.
///
/// Twenty watts. Below that the denominator dominates the quotient and the
/// "residual" being learned is the arithmetic of dividing by nearly nothing.
const MIN_MODELLED: f64 = 20.0;

/// Ratios this far from one are not corrections, they are broken inputs — a
/// meter reading the wrong CT, a driver reporting kilowatts as watts.
const RATIO_BOUNDS: (f64, f64) = (0.1, 3.0);

/// Mean absolute deviation to a 10-90 half-width, for a normal.
///
/// `sigma = 1,2533 x MAD` and the 10-90 half-width is `1,2816 x sigma`, so the
/// two together are 1,606.
///
/// It is the **seed** for the calibrated tail multipliers below and no longer
/// the answer: a roof's residual is not normal, and where the two disagree the
/// outcomes win. Starting there rather than at one means a bucket's first few
/// bands are the ones the Gaussian assumption would have given, which is a
/// better guess than nothing while the calibration has no evidence.
const MAD_TO_HALF_WIDTH: f64 = 1.606;

/// The share of outcomes a 10–90 band is meant to leave outside on **each**
/// side.
const TAIL: f64 = 0.10;

/// How hard the **first** outcomes move the tail they fell outside of.
///
/// The gain decays with the bucket's own history — see [`calibration_gain`] —
/// because the two directions of this update are not symmetric and a constant
/// rate has to choose which one to serve. A miss moves a tail by `η · (1 − 0,1)`
/// and a hit by `η · 0,1`, so a band that is too **narrow** corrects nine times
/// faster than one that is too **wide**; at a rate small enough to be stable
/// after a year, a bucket seeded at the Gaussian guess needs a season to work
/// its width down. Starting at half and decaying is the ordinary Robbins–Monro
/// answer: converge in days, settle in weeks, and never stop adapting.
const CALIBRATION_RATE: f64 = 0.5;

/// How many observations the gain decays over.
///
/// Thirty is about a week of an hour-wide bucket at quarter-hour resolution.
const GAIN_HORIZON: f64 = 30.0;

/// The gain a bucket keeps for ever, however long its history.
///
/// A decaying gain that reaches zero is a corrector that has stopped listening,
/// and a roof acquires a new tree, a dirty winter or a replaced string. This is
/// the floor that keeps it slow rather than deaf.
const MIN_CALIBRATION_RATE: f64 = 0.02;

/// The step one outcome takes, for a bucket that has seen `samples` of them.
fn calibration_gain(samples: usize) -> f64 {
    let n = samples as f64;
    (CALIBRATION_RATE * GAIN_HORIZON / (GAIN_HORIZON + n)).max(MIN_CALIBRATION_RATE)
}

/// What the tail multipliers ride on where the dispersion has collapsed.
///
/// A multiplier on a base of exactly zero is still zero, so without this a
/// bucket that has seen a fortnight of identical days could never be calibrated
/// at all. One per cent of the ratio is small enough not to widen an honest band
/// and large enough to be multiplied.
const MIN_BASE: f64 = 0.01;

/// How far a tail multiplier may travel from its Gaussian seed.
///
/// Wide enough for any residual distribution a roof produces, bounded so a
/// single freak month cannot leave a bucket with a band of nothing or a band of
/// everything.
const MULTIPLIER_BOUNDS: (f64, f64) = (0.25, 12.0);

/// The narrowest relative half-width the calibration may ever produce.
///
/// Not a substitute for the old constant floor — this is two orders of
/// magnitude below it. It exists so that a band is never literally a point,
/// which would make the planner's pessimistic and optimistic futures the same
/// number and quietly turn a scenario solve into a deterministic one.
const HARD_MIN_SPREAD: f64 = 0.01;

/// How many observations a bucket needs before its own calibration is trusted
/// over [`ResidualModel::prior_spread`].
///
/// Eight is two days of an hour-wide bucket at quarter-hour resolution. Below
/// it the tail multipliers have been moved a handful of times and say more
/// about the last two mornings than about the roof.
pub const SETTLED_SAMPLES: usize = 8;

/// One bucket's running estimate of the ratio, its dispersion and the width its
/// own outcomes have earned.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Bucket {
    /// Exponentially weighted mean of `actual / modelled`.
    mean: f64,
    /// Exponentially weighted mean absolute deviation from it.
    dispersion: f64,
    /// How far below the mean the 10th percentile sits, as a multiple of
    /// [`Bucket::base`]. Calibrated so a tenth of outcomes fall below it.
    lo: f64,
    /// The same for the 90th percentile, above.
    hi: f64,
    /// How many observations have entered it.
    samples: usize,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            mean: 1.0,
            dispersion: 0.0,
            lo: MAD_TO_HALF_WIDTH,
            hi: MAD_TO_HALF_WIDTH,
            samples: 0,
        }
    }
}

impl Bucket {
    /// What the tail multipliers are multiples **of**, in ratio units.
    fn base(&self) -> f64 {
        self.dispersion.max(MIN_BASE)
    }

    /// The relative half-widths this bucket currently claims, `(below, above)`.
    fn half_widths(&self) -> (f64, f64) {
        let base = self.base();
        (self.lo * base, self.hi * base)
    }

    /// Record one outcome.
    ///
    /// The tails are calibrated **before** the mean and the dispersion move, so
    /// each multiplier is scored against the band that would actually have been
    /// published for that slot rather than against one built with hindsight.
    /// That ordering is the whole of what makes the fixed point mean anything.
    fn observe(&mut self, ratio: f64, alpha: f64) {
        if self.samples == 0 {
            self.mean = ratio;
            self.samples = 1;
            return;
        }

        let (below, above) = self.half_widths();
        let missed_low = f64::from(u8::from(ratio < self.mean - below));
        let missed_high = f64::from(u8::from(ratio > self.mean + above));
        let gain = calibration_gain(self.samples);
        let step = |m: f64, missed: f64| {
            (m * (gain * (missed - TAIL)).exp()).clamp(MULTIPLIER_BOUNDS.0, MULTIPLIER_BOUNDS.1)
        };
        self.lo = step(self.lo, missed_low);
        self.hi = step(self.hi, missed_high);

        let deviation = (ratio - self.mean).abs();
        self.dispersion = alpha.mul_add(deviation, (1.0 - alpha) * self.dispersion);
        self.mean = alpha.mul_add(ratio, (1.0 - alpha) * self.mean);
        self.samples += 1;
    }
}

/// The correction a physical model has earned from its own history.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ResidualModel {
    buckets: BTreeMap<u8, Bucket>,
    /// The weight a new observation carries.
    pub alpha: f64,
    /// The band half-width used where a bucket has no history yet, as a
    /// fraction of the median.
    ///
    /// Deliberately wide. A forecast that has never been checked against a
    /// meter should look like one.
    pub prior_spread: f64,
    /// The narrowest band a bucket that is still settling may report, as a
    /// fraction of the median.
    ///
    /// It bounds the first [`SETTLED_SAMPLES`] observations of a bucket and
    /// nothing after them. A corrector that has seen two identical mornings
    /// would otherwise report a dispersion of nearly zero, and a planner handed
    /// a certainty will spend the battery on it — but a bucket that has been
    /// checked against a hundred days and calibrated against their outcomes has
    /// earned whatever width those outcomes say, and holding it to a constant is
    /// what made every clear June slot report the same ±12 %.
    pub floor_spread: f64,
}

impl Default for ResidualModel {
    fn default() -> Self {
        Self::new(DEFAULT_ALPHA)
    }
}

impl ResidualModel {
    /// A corrector that has learned nothing: ratio one, band [`prior_spread`]
    /// wide.
    ///
    /// [`prior_spread`]: ResidualModel::prior_spread
    #[must_use]
    pub fn new(alpha: f64) -> Self {
        Self {
            buckets: BTreeMap::new(),
            alpha: alpha.clamp(0.001, 1.0),
            prior_spread: 0.45,
            floor_spread: 0.12,
        }
    }

    /// Which bucket a slot falls in: the hour of the *local* day.
    ///
    /// Local, because the shade is cast by the sun and the sun keeps local time.
    /// A model bucketed by UTC hour learns two different corrections for the
    /// same physical morning and swaps between them at the end of March.
    fn bucket_of(slot: Slot) -> u8 {
        u8::try_from(slot.local_minute_of_day() / 60).unwrap_or(0)
    }

    /// Record what the model said and what actually happened.
    ///
    /// Values are in whatever unit the model works in; only their ratio is
    /// kept. Observations where the model predicted essentially nothing are
    /// ignored — see the module note.
    pub fn observe(&mut self, slot: Slot, modelled: f64, actual: f64) {
        if !modelled.is_finite() || !actual.is_finite() || modelled < MIN_MODELLED || actual < 0.0 {
            return;
        }
        let ratio = actual / modelled;
        if !(RATIO_BOUNDS.0..=RATIO_BOUNDS.1).contains(&ratio) {
            return;
        }
        self.buckets
            .entry(Self::bucket_of(slot))
            .or_default()
            .observe(ratio, self.alpha);
    }

    /// How many observations back the bucket `slot` falls in.
    #[must_use]
    pub fn support(&self, slot: Slot) -> usize {
        self.buckets
            .get(&Self::bucket_of(slot))
            .map_or(0, |b| b.samples)
    }

    /// The multiplicative correction learned for this time of day.
    ///
    /// One where nothing has been learned, which makes an untrained corrector
    /// the identity rather than a source of error.
    #[must_use]
    pub fn ratio_at(&self, slot: Slot) -> f64 {
        self.buckets
            .get(&Self::bucket_of(slot))
            .map_or(1.0, |b| b.mean)
    }

    /// The relative half-widths of the band for this time of day,
    /// `(below the median, above it)`.
    ///
    /// Asymmetric, because a roof's residual is: it can fall a long way below
    /// the clear-sky model and cannot rise far above it, and a band forced to be
    /// symmetric buys the impossible side at the price of the one that matters.
    ///
    /// Both are the bucket's dispersion times the tail multiplier its own
    /// outcomes have earned — see the module note. Capped at one on the way
    /// down, because a 10th percentile below zero is not a forecast, and at
    /// three on the way up, which is the widest thing that is still a statement.
    #[must_use]
    pub fn spreads_at(&self, slot: Slot) -> (f64, f64) {
        match self.buckets.get(&Self::bucket_of(slot)) {
            Some(b) if b.samples >= SETTLED_SAMPLES => {
                let (below, above) = b.half_widths();
                let relative = |half: f64| half / b.mean.max(0.05);
                (
                    relative(below).clamp(HARD_MIN_SPREAD, 1.0),
                    relative(above).clamp(HARD_MIN_SPREAD, 3.0),
                )
            }
            // A bucket that is still settling reports the width of a bucket that
            // is still settling: the prior, floored so two identical mornings
            // cannot look like a certainty.
            Some(b) if b.samples >= 2 => {
                let (below, above) = b.half_widths();
                let relative = |half: f64| {
                    (half / b.mean.max(0.05)).clamp(self.floor_spread, self.prior_spread)
                };
                (relative(below), relative(above))
            }
            _ => (self.prior_spread, self.prior_spread),
        }
    }

    /// The band's relative half-width, averaged over its two sides.
    ///
    /// A single number for a band that no longer has one — kept because a
    /// report wants one figure per slot, and because "how uncertain is this
    /// hour" is a fair question even when the answer is lopsided. Anything
    /// deciding something reads [`ResidualModel::spreads_at`] or the band
    /// itself.
    #[must_use]
    pub fn spread_at(&self, slot: Slot) -> f64 {
        let (below, above) = self.spreads_at(slot);
        f64::midpoint(below, above)
    }

    /// Turn a modelled value into a forecast band.
    #[must_use]
    pub fn correct(&self, slot: Slot, modelled: f64) -> Band {
        let median = modelled * self.ratio_at(slot);
        let (below, above) = self.spreads_at(slot);
        Band {
            p10: (median * (1.0 - below)).max(0.0),
            p50: median,
            p90: median * (1.0 + above),
        }
    }

    /// Whether any bucket has enough history to be worth trusting.
    #[must_use]
    pub fn is_trained(&self) -> bool {
        self.buckets.values().any(|b| b.samples >= 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::Slot;
    use time::macros::datetime;

    fn noon(day: i64) -> Slot {
        Slot::containing(datetime!(2026-06-01 10:00:00 UTC) + time::Duration::days(day))
    }

    #[test]
    fn an_untrained_corrector_is_the_identity_with_a_wide_band() {
        let m = ResidualModel::default();
        let band = m.correct(noon(0), 5000.0);
        assert_eq!(band.p50, 5000.0);
        assert!(band.width() > 3000.0, "{band}");
        assert!(!m.is_trained());
    }

    #[test]
    fn a_roof_that_always_underperforms_is_corrected_towards_the_truth() {
        // A shaded array delivering 70 % of what the geometry predicts.
        let mut m = ResidualModel::new(0.3);
        for day in 0..40 {
            m.observe(noon(day), 5000.0, 3500.0);
        }
        let ratio = m.ratio_at(noon(41));
        assert!(
            (ratio - 0.7).abs() < 0.01,
            "learned {ratio}, expected about 0,7"
        );
        assert!((m.correct(noon(41), 5000.0).p50 - 3500.0).abs() < 60.0);
    }

    #[test]
    fn a_steady_roof_narrows_its_band_but_never_to_a_certainty() {
        // The case the old constant floor got wrong. A roof that has delivered
        // the same thing sixty times running is genuinely predictable, and
        // holding it to a fixed ±12 % is what made every clear June slot report
        // an uncertainty it did not have.
        let mut m = ResidualModel::new(0.3);
        for day in 0..60 {
            m.observe(noon(day), 5000.0, 5000.0);
        }
        let band = m.correct(noon(61), 5000.0);
        assert!(band.width() > 0.0, "a forecast is never certain");
        assert!(
            band.width() < 5000.0 * 2.0 * m.floor_spread,
            "{band} should have narrowed past the settling floor"
        );
    }

    #[test]
    fn a_variable_roof_keeps_a_wide_band() {
        let mut m = ResidualModel::new(0.3);
        for day in 0..60 {
            let actual = if day % 2 == 0 { 5000.0 } else { 1500.0 };
            m.observe(noon(day), 5000.0, actual);
        }
        assert!(m.spread_at(noon(61)) > m.floor_spread * 2.0);
    }

    #[test]
    fn the_band_calibrates_itself_to_eighty_per_cent_on_a_skewed_roof() {
        // The property the whole construction exists for, tested against the
        // distribution a Gaussian conversion of the dispersion is worst on: a
        // roof that is at the model most days and a long way below it when a
        // front comes through. Nine outcomes in ten should land inside, and the
        // estimator is told nothing about the shape.
        let mut m = ResidualModel::new(0.05);
        let mut state = 0x5EED_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 1000) as f64 / 1000.0
        };
        let draw = |u: f64| {
            // 80 % clear-ish, 20 % a deep cloud event.
            if u < 0.8 {
                0.92 + u * 0.1
            } else {
                0.35 + (u - 0.8) * 1.5
            }
        };

        // Warm up, then score the next four hundred against the band the model
        // would have published for each.
        for _ in 0..400 {
            let u = next();
            m.observe(noon(0), 5000.0, 5000.0 * draw(u));
        }
        let (mut inside, mut total) = (0usize, 0usize);
        for _ in 0..800 {
            let u = next();
            let actual = 5000.0 * draw(u);
            let band = m.correct(noon(0), 5000.0);
            if actual >= band.p10 && actual <= band.p90 {
                inside += 1;
            }
            total += 1;
            m.observe(noon(0), 5000.0, actual);
        }
        let coverage = inside as f64 / total as f64;
        assert!(
            (0.72..=0.88).contains(&coverage),
            "a 10-90 band should cover about 80 %, covered {coverage:.2}"
        );
    }

    #[test]
    fn the_band_is_asymmetric_where_the_roof_is() {
        // A roof that is at the model or well below it, never above. The
        // downward tail has to be the wide one; a symmetric band buys the
        // impossible side at the price of the one that happens.
        let mut m = ResidualModel::new(0.05);
        for day in 0..300 {
            let actual = if day % 5 == 0 { 2000.0 } else { 5000.0 };
            m.observe(noon(day), 5000.0, actual);
        }
        let (below, above) = m.spreads_at(noon(301));
        assert!(
            below > above * 1.5,
            "the downward tail should dominate: below {below:.3}, above {above:.3}"
        );
        let band = m.correct(noon(301), 5000.0);
        assert!(band.is_ordered(), "{band}");
    }

    #[test]
    fn a_band_that_is_far_too_narrow_widens_within_a_few_days() {
        // The other direction, and the one that matters for safety: a bucket
        // calibrated on a settled fortnight meets a week of weather. It must
        // not take a season to admit it.
        let mut m = ResidualModel::new(0.05);
        for day in 0..200 {
            m.observe(noon(day), 5000.0, 5000.0);
        }
        let settled = m.spread_at(noon(201));
        for day in 200..240 {
            let actual = if day % 2 == 0 { 5000.0 } else { 2500.0 };
            m.observe(noon(day), 5000.0, actual);
        }
        let widened = m.spread_at(noon(241));
        assert!(
            widened > settled * 3.0,
            "settled at {settled:.3}, widened only to {widened:.3}"
        );
    }

    #[test]
    fn the_hours_are_learned_apart() {
        let mut m = ResidualModel::new(0.5);
        let morning = Slot::containing(datetime!(2026-06-01 06:00:00 UTC));
        for _ in 0..20 {
            m.observe(morning, 2000.0, 1000.0);
            m.observe(noon(0), 5000.0, 5000.0);
        }
        assert!((m.ratio_at(morning) - 0.5).abs() < 0.01, "the shaded hour");
        assert!((m.ratio_at(noon(1)) - 1.0).abs() < 0.01, "the clear hour");
    }

    #[test]
    fn a_model_predicting_nothing_teaches_nothing() {
        let mut m = ResidualModel::default();
        // Midnight: the model says zero, the meter says zero. A ratio of 0/0
        // is not a residual.
        m.observe(
            Slot::containing(datetime!(2026-06-01 00:00:00 UTC)),
            0.0,
            0.0,
        );
        // And a meter reporting kilowatts where watts were expected is a bug,
        // not a correction.
        m.observe(noon(0), 5000.0, 500_000.0);
        assert!(!m.is_trained());
        assert_eq!(m.ratio_at(noon(0)), 1.0);
    }
}
