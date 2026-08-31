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
/// two together are 1,606. Applying them separately as 1,25 and 1,6 gives 2,0 —
/// a quarter too wide, which produces a band the outcome falls inside 98 % of
/// the time against the 80 % it promises.
const MAD_TO_HALF_WIDTH: f64 = 1.606;

/// One bucket's running estimate of the ratio and its dispersion.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Bucket {
    /// Exponentially weighted mean of `actual / modelled`.
    mean: f64,
    /// Exponentially weighted mean absolute deviation from it.
    dispersion: f64,
    /// How many observations have entered it.
    samples: usize,
}

impl Default for Bucket {
    fn default() -> Self {
        Self {
            mean: 1.0,
            dispersion: 0.0,
            samples: 0,
        }
    }
}

impl Bucket {
    fn observe(&mut self, ratio: f64, alpha: f64) {
        if self.samples == 0 {
            self.mean = ratio;
        } else {
            let deviation = (ratio - self.mean).abs();
            self.dispersion = alpha.mul_add(deviation, (1.0 - alpha) * self.dispersion);
            self.mean = alpha.mul_add(ratio, (1.0 - alpha) * self.mean);
        }
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
    /// The narrowest the band may ever get, as a fraction of the median.
    ///
    /// A corrector that has seen ten identical days will report a dispersion of
    /// nearly zero, and a planner handed a certainty will spend the battery on
    /// it. The weather does not become certain because it has been the same for
    /// a week.
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

    /// The relative half-width of the band for this time of day.
    ///
    /// The observed mean absolute deviation of the ratio, converted to a 10–90
    /// half-width (`σ ≈ 1,2533 · MAD`, half-width `≈ 1,2816 · σ`, so 1,606
    /// together) and expressed relative to the ratio itself. It is capped at one — a band running from nothing to twice the
    /// median is the widest thing that is still a statement — and floored at
    /// [`ResidualModel::floor_spread`].
    ///
    /// The cap is deliberately **not** [`ResidualModel::prior_spread`], which is
    /// what an *untrained* bucket says. Using the prior as a ceiling would let a
    /// roof that has genuinely been unpredictable report itself as no more
    /// uncertain than one nobody has ever measured, which is the wrong way round.
    #[must_use]
    pub fn spread_at(&self, slot: Slot) -> f64 {
        match self.buckets.get(&Self::bucket_of(slot)) {
            // One observation says what the ratio is and nothing about how much
            // it moves, so the prior spread stands until there are two.
            Some(b) if b.samples >= 2 => {
                (MAD_TO_HALF_WIDTH * b.dispersion / b.mean.max(0.05)).clamp(self.floor_spread, 1.0)
            }
            _ => self.prior_spread,
        }
    }

    /// Turn a modelled value into a forecast band.
    #[must_use]
    pub fn correct(&self, slot: Slot, modelled: f64) -> Band {
        let median = modelled * self.ratio_at(slot);
        Band::relative(median, self.spread_at(slot))
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
        let mut m = ResidualModel::new(0.3);
        for day in 0..60 {
            m.observe(noon(day), 5000.0, 5000.0);
        }
        let band = m.correct(noon(61), 5000.0);
        assert!(band.width() > 0.0, "a forecast is never certain");
        assert!(
            band.width() <= 5000.0 * 2.0 * m.floor_spread + 1e-9,
            "{band} should have narrowed to the floor"
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
