//! Forecasts that admit they are uncertain.
//!
//! A point forecast is a lie a planner then optimises against. Three quantiles
//! are enough to plan honestly: the optimiser sizes a battery reserve against
//! the pessimistic one, prices the expected case against the median, and knows
//! from the spread how much it should hedge at all.
//!
//! P10 is the value the true outcome falls **below** 10 % of the time — for
//! photovoltaic production, the pessimistic case; for load, the optimistic one.

use core::fmt;

use hems_core::prelude::{Power, Slot};

/// Three quantiles of one quantity in one slot.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Band {
    /// The 10th percentile.
    pub p10: f64,
    /// The median.
    pub p50: f64,
    /// The 90th percentile.
    pub p90: f64,
}

impl Band {
    /// A band with no uncertainty at all — a measured value, or a model that
    /// does not know what it does not know.
    #[must_use]
    pub const fn certain(value: f64) -> Self {
        Self {
            p10: value,
            p50: value,
            p90: value,
        }
    }

    /// A band around `median` with a relative spread, clamped at zero below.
    ///
    /// The usual shape for a quantity that cannot be negative: a photovoltaic
    /// forecast that is 40 % uncertain is `−40 %/+40 %` around the median, but
    /// never below nothing.
    #[must_use]
    pub fn relative(median: f64, spread: f64) -> Self {
        Self {
            p10: (median * (1.0 - spread)).max(0.0),
            p50: median,
            p90: median * (1.0 + spread),
        }
    }

    /// The width of the band — how much the forecast is admitting it does not
    /// know.
    #[must_use]
    pub fn width(&self) -> f64 {
        self.p90 - self.p10
    }

    /// Whether the quantiles are in order. A band that fails this is a bug in
    /// whatever produced it.
    #[must_use]
    pub fn is_ordered(&self) -> bool {
        self.p10 <= self.p50 && self.p50 <= self.p90
    }

    /// Sort the quantiles into order, so a merged or interpolated band is
    /// always usable.
    #[must_use]
    pub fn sorted(self) -> Self {
        let mut v = [self.p10, self.p50, self.p90];
        v.sort_by(f64::total_cmp);
        Self {
            p10: v[0],
            p50: v[1],
            p90: v[2],
        }
    }

    /// Scale every quantile.
    #[must_use]
    pub fn scaled(self, factor: f64) -> Self {
        Self {
            p10: self.p10 * factor,
            p50: self.p50 * factor,
            p90: self.p90 * factor,
        }
        .sorted()
    }

    /// The band as powers, load convention.
    #[must_use]
    pub fn as_power(self) -> PowerBand {
        PowerBand {
            p10: Power::new(self.p10),
            p50: Power::new(self.p50),
            p90: Power::new(self.p90),
        }
    }
}

impl fmt::Display for Band {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.0} [{:.0} … {:.0}]", self.p50, self.p10, self.p90)
    }
}

/// A [`Band`] in watts.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PowerBand {
    /// The 10th percentile.
    pub p10: Power,
    /// The median.
    pub p50: Power,
    /// The 90th percentile.
    pub p90: Power,
}

/// A forecast over a horizon.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Forecast {
    /// One band per slot, in order.
    pub slots: Vec<(Slot, Band)>,
}

impl Forecast {
    /// The band for one slot.
    #[must_use]
    pub fn at(&self, slot: Slot) -> Option<Band> {
        self.slots.iter().find(|(s, _)| *s == slot).map(|(_, b)| *b)
    }

    /// The medians, in order.
    pub fn medians(&self) -> impl Iterator<Item = f64> + '_ {
        self.slots.iter().map(|(_, b)| b.p50)
    }

    /// The total energy under the median, in watt-hours.
    #[must_use]
    pub fn total_median_wh(&self) -> f64 {
        self.medians().sum::<f64>() * 0.25
    }

    /// Whether every band is well formed.
    #[must_use]
    pub fn is_ordered(&self) -> bool {
        self.slots.iter().all(|(_, b)| b.is_ordered())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_band_never_goes_below_zero() {
        let b = Band::relative(1000.0, 1.5);
        assert_eq!(b.p10, 0.0);
        assert!(b.is_ordered());
    }

    #[test]
    fn scaling_keeps_the_order_even_when_the_factor_is_negative() {
        // Photovoltaic production is negative in the load convention, so the
        // pessimistic case swaps ends. Sorting is what keeps `p10 ≤ p90` true.
        let b = Band::relative(5000.0, 0.4).scaled(-1.0);
        assert!(b.is_ordered(), "{b:?}");
        assert_eq!(b.p50, -5000.0);
        assert_eq!(b.p10, -7000.0);
    }

    #[test]
    fn a_certain_band_has_no_width() {
        assert_eq!(Band::certain(42.0).width(), 0.0);
    }
}
