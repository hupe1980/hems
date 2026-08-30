//! The interval of power an asset may currently use.
//!
//! Every layer of hems narrows an interval rather than picking a number:
//! the guard narrows it for grid and safety reasons, the plan narrows it to what
//! it wants, the arbiter picks a point inside what is left. The idea is
//! OpenEMS's — a scheduler that hands each controller "an interval of possible
//! solutions" that later controllers can only shrink — made explicit, so that
//! "the grid limit was respected" is an intersection, not a code path someone
//! has to remember to write.

use core::fmt;

use crate::units::Power;

/// A closed interval of active power, load convention.
///
/// `floor` may be negative (the asset may be required to feed in or discharge)
/// and `ceiling` may be positive. An empty interval — `floor > ceiling` — is a
/// real and meaningful outcome: two rules that cannot both be satisfied. It is
/// resolved by [`Envelope::resolve`], which keeps the stricter ceiling, because
/// exceeding a grid limit is worse than falling short of a floor.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Envelope {
    /// The lowest permitted power.
    pub floor: Power,
    /// The highest permitted power.
    pub ceiling: Power,
}

impl Envelope {
    /// Everything a device could physically do, before any rule applies.
    pub const UNBOUNDED: Self = Self {
        floor: Power::new_const(f64::NEG_INFINITY),
        ceiling: Power::new_const(f64::INFINITY),
    };

    /// An interval.
    #[must_use]
    pub const fn new(floor: Power, ceiling: Power) -> Self {
        Self { floor, ceiling }
    }

    /// Only an upper bound — what a grid limit is.
    #[must_use]
    pub const fn at_most(ceiling: Power) -> Self {
        Self {
            floor: Power::new_const(f64::NEG_INFINITY),
            ceiling,
        }
    }

    /// Only a lower bound.
    #[must_use]
    pub const fn at_least(floor: Power) -> Self {
        Self {
            floor,
            ceiling: Power::new_const(f64::INFINITY),
        }
    }

    /// Exactly one value.
    #[must_use]
    pub const fn exactly(value: Power) -> Self {
        Self {
            floor: value,
            ceiling: value,
        }
    }

    /// The intersection of two intervals — the operation the whole design rests
    /// on. Narrowing is the only thing any layer is allowed to do.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            floor: self.floor.max(other.floor),
            ceiling: self.ceiling.min(other.ceiling),
        }
    }

    /// `true` when no value satisfies both bounds.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.floor > self.ceiling
    }

    /// `true` when `value` lies inside.
    #[must_use]
    pub fn contains(self, value: Power) -> bool {
        value >= self.floor && value <= self.ceiling
    }

    /// The value inside the interval closest to `wanted`.
    ///
    /// When the interval is empty the ceiling wins: a grid or safety ceiling is
    /// a duty towards someone else, a floor is a comfort promise to the
    /// household, and breaking the second is recoverable.
    #[must_use]
    pub fn clamp(self, wanted: Power) -> Power {
        if self.is_empty() {
            return self.ceiling;
        }
        wanted.max(self.floor).min(self.ceiling)
    }

    /// The interval with an empty one collapsed onto its ceiling, so downstream
    /// code never has to ask again.
    #[must_use]
    pub fn resolve(self) -> Self {
        if self.is_empty() {
            Self::exactly(self.ceiling)
        } else {
            self
        }
    }

    /// How much room there is between the bounds, or zero when empty.
    #[must_use]
    pub fn width(self) -> Power {
        if self.is_empty() {
            Power::ZERO
        } else {
            self.ceiling - self.floor
        }
    }
}

impl fmt::Display for Envelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (
            self.floor.get().is_infinite(),
            self.ceiling.get().is_infinite(),
        ) {
            (true, true) => f.write_str("unbounded"),
            (true, false) => write!(f, "≤ {}", self.ceiling),
            (false, true) => write!(f, "≥ {}", self.floor),
            (false, false) => write!(f, "{} … {}", self.floor, self.ceiling),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_only_ever_narrows() {
        let a = Envelope::new(Power::from_kw(0.0), Power::from_kw(11.0));
        let b = Envelope::at_most(Power::from_kw(4.2));
        let n = a.intersect(b);
        assert_eq!(n.ceiling, Power::from_kw(4.2));
        assert_eq!(n.floor, Power::ZERO);
        assert!(n.width() <= a.width());
    }

    #[test]
    fn an_unsatisfiable_pair_resolves_towards_the_ceiling() {
        // A wallbox that cannot go below 6 A (1,4 kW) meeting a 1 kW grid limit.
        let device = Envelope::at_least(Power::from_kw(1.4));
        let grid = Envelope::at_most(Power::from_kw(1.0));
        let both = device.intersect(grid);
        assert!(both.is_empty());
        assert_eq!(
            both.clamp(Power::from_kw(5.0)),
            Power::from_kw(1.0),
            "the grid limit wins"
        );
        assert_eq!(both.resolve(), Envelope::exactly(Power::from_kw(1.0)));
    }

    #[test]
    fn clamping_keeps_a_wanted_value_that_already_fits() {
        let e = Envelope::new(Power::ZERO, Power::from_kw(11.0));
        assert_eq!(e.clamp(Power::from_kw(4.0)), Power::from_kw(4.0));
        assert_eq!(e.clamp(Power::from_kw(20.0)), Power::from_kw(11.0));
        assert_eq!(e.clamp(Power::from_kw(-5.0)), Power::ZERO);
    }

    #[test]
    fn unbounded_is_the_identity_of_intersection() {
        let e = Envelope::new(Power::from_kw(-5.0), Power::from_kw(5.0));
        assert_eq!(e.intersect(Envelope::UNBOUNDED), e);
    }
}
