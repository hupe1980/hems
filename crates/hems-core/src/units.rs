//! Physical quantities, with one sign convention for the whole workspace.
//!
//! # The load convention
//!
//! Every power and energy value in hems uses the **load convention**: a positive
//! value is power flowing *into* the thing being measured, a negative value is
//! power flowing *out of* it.
//!
//! | Thing | Positive means | Negative means |
//! |---|---|---|
//! | Grid connection | import (Netzbezug) | export (Einspeisung) |
//! | PV array | — (only at night: standby draw) | production |
//! | Battery | charging | discharging |
//! | Wallbox | charging the car | discharging it (V2H/V2G) |
//! | Heat pump, household load | consumption | — |
//!
//! The reason to pay the small cost of "PV production is negative" is one
//! invariant that then holds everywhere and can be tested:
//!
//! ```text
//! grid connection power  ==  Σ (power of every asset behind it)
//! ```
//!
//! [`crate::site::Site::balance_residual`] is that equation, and
//! `hems-realtime` uses it to detect a missing or mis-signed meter.
//!
//! # Non-finite values
//!
//! The constructors here are infallible and cheap; they `debug_assert!` that the
//! input is finite. The gate that matters is at the boundary where a number
//! becomes an action: [`crate::setpoint::Setpoint::new`] refuses a non-finite
//! command, so a NaN produced by a broken driver or a degenerate solve can never
//! reach a device.

use core::fmt;
use core::iter::Sum;
use core::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

use crate::error::UnitError;

macro_rules! scalar_unit {
    (
        $(#[$meta:meta])*
        $name:ident, $unit:literal, $si:literal
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(transparent))]
        pub struct $name(f64);

        impl $name {
            #[doc = concat!("Zero ", $si, ".")]
            pub const ZERO: Self = Self(0.0);

            #[doc = concat!("A value in ", $si, " (`", $unit, "`).")]
            #[must_use]
            pub fn new(value: f64) -> Self {
                debug_assert!(value.is_finite(), concat!(stringify!($name), " must be finite"));
                Self(value)
            }

            #[doc = concat!("A compile-time constant in ", $si, ", unchecked.")]
            #[must_use]
            pub const fn new_const(value: f64) -> Self {
                Self(value)
            }

            #[doc = concat!("The value in ", $si, ".")]
            #[must_use]
            pub const fn get(self) -> f64 {
                self.0
            }

            /// `true` when the value is finite — the precondition every setpoint
            /// is checked against before it leaves the process.
            #[must_use]
            pub fn is_finite(self) -> bool {
                self.0.is_finite()
            }

            /// The larger of two values. Propagates `NaN` rather than hiding it.
            #[must_use]
            pub fn max(self, other: Self) -> Self {
                if self.0.is_nan() || other.0.is_nan() {
                    Self(f64::NAN)
                } else if self.0 >= other.0 {
                    self
                } else {
                    other
                }
            }

            /// The smaller of two values. Propagates `NaN` rather than hiding it.
            #[must_use]
            pub fn min(self, other: Self) -> Self {
                if self.0.is_nan() || other.0.is_nan() {
                    Self(f64::NAN)
                } else if self.0 <= other.0 {
                    self
                } else {
                    other
                }
            }

            /// Clamped into `[lo, hi]`.
            ///
            /// # Panics
            /// Panics when `lo > hi`, which is a programming error in the caller
            /// rather than a runtime condition.
            #[must_use]
            pub fn clamp(self, lo: Self, hi: Self) -> Self {
                assert!(lo.0 <= hi.0, "clamp range inverted: {lo:?} > {hi:?}");
                self.max(lo).min(hi)
            }

            /// The magnitude, sign discarded.
            #[must_use]
            pub fn abs(self) -> Self {
                Self(self.0.abs())
            }

            /// The part flowing *in* (positive side of the load convention), or zero.
            #[must_use]
            pub fn inflow(self) -> Self {
                Self(self.0.max(0.0))
            }

            /// The part flowing *out* as a non-negative magnitude, or zero.
            #[must_use]
            pub fn outflow(self) -> Self {
                Self((-self.0).max(0.0))
            }
        }

        impl Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self { Self(self.0 + rhs.0) }
        }
        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) { self.0 += rhs.0; }
        }
        impl Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self { Self(self.0 - rhs.0) }
        }
        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) { self.0 -= rhs.0; }
        }
        impl Neg for $name {
            type Output = Self;
            fn neg(self) -> Self { Self(-self.0) }
        }
        impl Mul<f64> for $name {
            type Output = Self;
            fn mul(self, rhs: f64) -> Self { Self(self.0 * rhs) }
        }
        impl Mul<$name> for f64 {
            type Output = $name;
            fn mul(self, rhs: $name) -> $name { $name(self * rhs.0) }
        }
        impl Div<f64> for $name {
            type Output = Self;
            fn div(self, rhs: f64) -> Self { Self(self.0 / rhs) }
        }
        impl Div for $name {
            type Output = f64;
            fn div(self, rhs: Self) -> f64 { self.0 / rhs.0 }
        }
        impl Sum for $name {
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, Add::add)
            }
        }
        impl<'a> Sum<&'a $name> for $name {
            fn sum<I: Iterator<Item = &'a Self>>(iter: I) -> Self {
                iter.fold(Self::ZERO, |acc, v| acc + *v)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:.1} {}", self.0, $unit)
            }
        }
    };
}

scalar_unit!(
    /// Active power in watts, load convention (see the module documentation).
    Power, "W", "watts"
);
scalar_unit!(
    /// Energy in watt-hours, load convention: positive is energy taken in.
    Energy, "Wh", "watt-hours"
);
scalar_unit!(
    /// Apparent power in volt-amperes — the quantity the VDE-AR-N 4100
    /// unbalance rule (≤ 4,6 kVA Schieflast) is expressed in.
    ApparentPower, "VA", "volt-amperes"
);
scalar_unit!(
    /// Current in amperes, load convention.
    Current, "A", "amperes"
);
scalar_unit!(
    /// Voltage in volts (always positive in practice).
    Voltage, "V", "volts"
);

impl Power {
    /// A power given in kilowatts — the unit every datasheet and every BNetzA
    /// Festlegung uses.
    #[must_use]
    pub fn from_kw(kw: f64) -> Self {
        Self::new(kw * 1000.0)
    }

    /// The value in kilowatts.
    #[must_use]
    pub fn kw(self) -> f64 {
        self.0 / 1000.0
    }

    /// The energy this power delivers if held for `duration`.
    #[must_use]
    pub fn over(self, duration: time::Duration) -> Energy {
        Energy::new(self.0 * duration.as_seconds_f64() / 3600.0)
    }

    /// Single-phase current at `voltage`, ignoring power factor.
    #[must_use]
    pub fn to_current_1p(self, voltage: Voltage) -> Current {
        Current::new(self.0 / voltage.get())
    }

    /// Three-phase current at `voltage` (phase-to-neutral), ignoring power factor.
    #[must_use]
    pub fn to_current_3p(self, voltage: Voltage) -> Current {
        Current::new(self.0 / (3.0 * voltage.get()))
    }
}

impl Energy {
    /// An energy given in kilowatt-hours.
    #[must_use]
    pub fn from_kwh(kwh: f64) -> Self {
        Self::new(kwh * 1000.0)
    }

    /// The value in kilowatt-hours.
    #[must_use]
    pub fn kwh(self) -> f64 {
        self.0 / 1000.0
    }

    /// The constant power that would move this energy over `duration`.
    #[must_use]
    pub fn over(self, duration: time::Duration) -> Power {
        Power::new(self.0 * 3600.0 / duration.as_seconds_f64())
    }
}

impl Current {
    /// Single-phase power drawn at `voltage`, ignoring power factor.
    #[must_use]
    pub fn to_power_1p(self, voltage: Voltage) -> Power {
        Power::new(self.0 * voltage.get())
    }

    /// Three-phase power drawn at `voltage` (phase-to-neutral), ignoring power factor.
    #[must_use]
    pub fn to_power_3p(self, voltage: Voltage) -> Power {
        Power::new(3.0 * self.0 * voltage.get())
    }
}

/// The nominal phase-to-neutral voltage of a German low-voltage connection.
pub const NOMINAL_VOLTAGE: Voltage = Voltage::new_const(230.0);

// ── State of charge ──────────────────────────────────────────────────────────

/// State of charge as a fraction in `[0, 1]`.
///
/// Constructed strictly, because a SoC outside the interval is either a driver
/// bug or a unit mix-up (percent vs. fraction) and both are worth failing on.
/// [`Soc::clamped`] exists for sensor noise around the endpoints.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "f64", into = "f64"))]
pub struct Soc(f64);

impl Soc {
    /// Empty.
    pub const EMPTY: Self = Self(0.0);
    /// No backup reserve — the default for a battery that keeps nothing back.
    pub const ZERO_RESERVE: Self = Self(0.0);
    /// Full.
    pub const FULL: Self = Self(1.0);

    /// A state of charge from a fraction in `[0, 1]`.
    ///
    /// # Errors
    /// [`UnitError::SocOutOfRange`] when the value is outside `[0, 1]` or not finite.
    pub fn new(fraction: f64) -> Result<Self, UnitError> {
        if fraction.is_finite() && (0.0..=1.0).contains(&fraction) {
            Ok(Self(fraction))
        } else {
            Err(UnitError::SocOutOfRange(fraction))
        }
    }

    /// A state of charge from a percentage in `[0, 100]`.
    ///
    /// # Errors
    /// [`UnitError::SocOutOfRange`] when the value is outside `[0, 100]`.
    pub fn from_percent(percent: f64) -> Result<Self, UnitError> {
        Self::new(percent / 100.0).map_err(|_| UnitError::SocOutOfRange(percent))
    }

    /// A state of charge clamped into `[0, 1]` — for sensors that report 100.4 %.
    /// A non-finite input becomes [`Soc::EMPTY`], the safe end for every decision
    /// that reads a SoC (never discharge on a broken reading).
    #[must_use]
    pub fn clamped(fraction: f64) -> Self {
        if fraction.is_finite() {
            Self(fraction.clamp(0.0, 1.0))
        } else {
            Self::EMPTY
        }
    }

    /// The fraction in `[0, 1]`.
    #[must_use]
    pub const fn fraction(self) -> f64 {
        self.0
    }

    /// The percentage in `[0, 100]`.
    #[must_use]
    pub fn percent(self) -> f64 {
        self.0 * 100.0
    }

    /// The energy stored in a battery of `capacity` at this state of charge.
    #[must_use]
    pub fn energy_in(self, capacity: Energy) -> Energy {
        capacity * self.0
    }
}

impl TryFrom<f64> for Soc {
    type Error = UnitError;
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Soc> for f64 {
    fn from(value: Soc) -> Self {
        value.0
    }
}

impl fmt::Display for Soc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1} %", self.percent())
    }
}

// ── Phases ───────────────────────────────────────────────────────────────────

/// One of the three outer conductors of a German low-voltage connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "UPPERCASE"))]
pub enum Phase {
    /// Outer conductor L1.
    L1,
    /// Outer conductor L2.
    L2,
    /// Outer conductor L3.
    L3,
}

impl Phase {
    /// All three phases in order.
    pub const ALL: [Phase; 3] = [Phase::L1, Phase::L2, Phase::L3];

    /// The zero-based index of this phase, for array access.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Phase::L1 => 0,
            Phase::L2 => 1,
            Phase::L3 => 2,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::L1 => f.write_str("L1"),
            Phase::L2 => f.write_str("L2"),
            Phase::L3 => f.write_str("L3"),
        }
    }
}

/// A value measured or commanded per outer conductor.
///
/// The unbalance rule of VDE-AR-N 4100 is stated per phase, so anything that has
/// to satisfy it — a single-phase wallbox, a heating rod, the site as a whole —
/// carries its numbers in here rather than as a scalar.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PerPhase<T> {
    /// Value on L1.
    pub l1: T,
    /// Value on L2.
    pub l2: T,
    /// Value on L3.
    pub l3: T,
}

impl<T: Copy> PerPhase<T> {
    /// The same value on all three phases.
    pub const fn splat(value: T) -> Self {
        Self {
            l1: value,
            l2: value,
            l3: value,
        }
    }

    /// The value on one phase.
    #[must_use]
    pub const fn get(&self, phase: Phase) -> T {
        match phase {
            Phase::L1 => self.l1,
            Phase::L2 => self.l2,
            Phase::L3 => self.l3,
        }
    }

    /// Replace the value on one phase.
    pub const fn set(&mut self, phase: Phase, value: T) {
        match phase {
            Phase::L1 => self.l1 = value,
            Phase::L2 => self.l2 = value,
            Phase::L3 => self.l3 = value,
        }
    }

    /// The three values, L1 first.
    #[must_use]
    pub const fn as_array(&self) -> [T; 3] {
        [self.l1, self.l2, self.l3]
    }

    /// Apply `f` to every phase.
    #[must_use]
    pub fn map<U: Copy>(&self, mut f: impl FnMut(T) -> U) -> PerPhase<U> {
        PerPhase {
            l1: f(self.l1),
            l2: f(self.l2),
            l3: f(self.l3),
        }
    }

    /// Combine two per-phase values elementwise.
    #[must_use]
    pub fn zip_with<U: Copy, V: Copy>(
        &self,
        other: &PerPhase<U>,
        mut f: impl FnMut(T, U) -> V,
    ) -> PerPhase<V> {
        PerPhase {
            l1: f(self.l1, other.l1),
            l2: f(self.l2, other.l2),
            l3: f(self.l3, other.l3),
        }
    }

    /// Iterate over `(phase, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (Phase, T)> + '_ {
        Phase::ALL.into_iter().map(move |p| (p, self.get(p)))
    }
}

impl PerPhase<Power> {
    /// All three phases at zero.
    pub const ZERO: Self = Self::splat(Power::ZERO);

    /// The total across the three phases.
    #[must_use]
    pub fn total(&self) -> Power {
        self.l1 + self.l2 + self.l3
    }

    /// The **Unsymmetrieleistung**: the largest difference between any two outer
    /// conductors, expressed as apparent power.
    ///
    /// VDE-AR-N 4100 Abschnitt 5.5.2 caps it at 4,6 kVA for a customer
    /// installation — the figure `metering::power_quality::UNSYMMETRIE_LIMIT_KVA`
    /// carries with its derivation, and the same limit as the 20 A per
    /// Außenleiter the VDE FNN Hinweis states it as.
    ///
    /// **Which devices count.** The requirement reaches only equipment that can
    /// feed in or store — generation, storage, charge points — so the caller
    /// sums those and not the household's own load. See
    /// [`Asset::symmetry_relevant`](crate::asset::Asset::symmetry_relevant).
    ///
    /// **kVA, not kW.** The limit is apparent power, and an inverter at
    /// cos φ < 1 — which VDE-AR-N 4105 requires it to be capable of — moves more
    /// kVA than kW. This is computed from *active* power because that is what a
    /// household driver reports, so it **understates** the unbalance exactly
    /// when the grid has asked for reactive support. A meter that reports
    /// apparent power per conductor should be used as it stands.
    #[must_use]
    pub fn unbalance(&self) -> ApparentPower {
        let [a, b, c] = self.as_array().map(Power::get);
        let max = a.max(b).max(c);
        let min = a.min(b).min(c);
        ApparentPower::new(max - min)
    }
}

impl Add for PerPhase<Power> {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        self.zip_with(&rhs, |a, b| a + b)
    }
}

impl Sub for PerPhase<Power> {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        self.zip_with(&rhs, |a, b| a - b)
    }
}

impl Sum for PerPhase<Power> {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

/// Which conductors an asset is using **right now**.
///
/// The wiring is [`PhaseConnection`]; this is the state a switchable device is
/// in at this moment. They are different questions, and answering the second
/// with the first is how an eleven-kilowatt wallbox ends up capped at the
/// 4,6 kVA a *single-phase* device is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PhaseMode {
    /// One outer conductor. A charge point here draws a third of the power and
    /// has a third of the minimum — 1,4 kW instead of 4,1 kW — which is the
    /// whole reason switching is worth the contactor.
    Single,
    /// All three, symmetrically.
    #[default]
    Three,
}

impl PhaseMode {
    /// The number of conductors in use.
    #[must_use]
    pub const fn count(self) -> u8 {
        match self {
            PhaseMode::Single => 1,
            PhaseMode::Three => 3,
        }
    }

    /// The other one.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            PhaseMode::Single => PhaseMode::Three,
            PhaseMode::Three => PhaseMode::Single,
        }
    }
}

impl fmt::Display for PhaseMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhaseMode::Single => f.write_str("1p"),
            PhaseMode::Three => f.write_str("3p"),
        }
    }
}

/// How many outer conductors an asset is **wired** to, and which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum PhaseConnection {
    /// Connected to exactly one outer conductor.
    ///
    /// VDE-AR-N 4100 lets a single-phase device up to 4,6 kVA be connected this
    /// way, and lets the network operator name the conductor.
    Single {
        /// The conductor the device sits on.
        phase: Phase,
    },
    /// Connected to all three outer conductors, drawing symmetrically.
    Three,
    /// Able to switch between one and three phases at runtime (most modern
    /// wallboxes). `phase` names the conductor used while in single-phase mode.
    Switchable {
        /// The conductor used in single-phase mode.
        phase: Phase,
    },
}

impl PhaseConnection {
    /// Whether the device can change mode at all.
    #[must_use]
    pub const fn is_switchable(&self) -> bool {
        matches!(self, PhaseConnection::Switchable { .. })
    }

    /// The mode this wiring is in when nothing has said otherwise.
    ///
    /// A switchable device starts three-phase: it is the mode that charges a car
    /// fastest, and the one a wallbox powers up in.
    #[must_use]
    pub const fn default_mode(&self) -> PhaseMode {
        match self {
            PhaseConnection::Single { .. } => PhaseMode::Single,
            PhaseConnection::Three | PhaseConnection::Switchable { .. } => PhaseMode::Three,
        }
    }

    /// Whether this wiring can be in `mode`.
    #[must_use]
    pub const fn supports(&self, mode: PhaseMode) -> bool {
        match self {
            PhaseConnection::Single { .. } => matches!(mode, PhaseMode::Single),
            PhaseConnection::Three => matches!(mode, PhaseMode::Three),
            PhaseConnection::Switchable { .. } => true,
        }
    }

    /// `mode` if this wiring supports it, otherwise the one it is stuck in.
    #[must_use]
    pub const fn clamp_mode(&self, mode: PhaseMode) -> PhaseMode {
        if self.supports(mode) {
            mode
        } else {
            self.default_mode()
        }
    }

    /// The conductor used in single-phase mode, if there is one.
    #[must_use]
    pub const fn single_phase_conductor(&self) -> Option<Phase> {
        match self {
            PhaseConnection::Single { phase } | PhaseConnection::Switchable { phase } => {
                Some(*phase)
            }
            PhaseConnection::Three => None,
        }
    }

    /// Distribute a symmetric total across the conductors in use in `mode`.
    #[must_use]
    pub fn distribute(&self, total: Power, mode: PhaseMode) -> PerPhase<Power> {
        match (self.clamp_mode(mode), self.single_phase_conductor()) {
            (PhaseMode::Single, Some(phase)) => {
                let mut p = PerPhase::ZERO;
                p.set(phase, total);
                p
            }
            _ => PerPhase::splat(total / 3.0),
        }
    }

    /// The number of conductors in use in `mode`.
    #[must_use]
    pub const fn count(&self, mode: PhaseMode) -> u8 {
        self.clamp_mode(mode).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_round_trips_through_kilowatts() {
        assert!((Power::from_kw(4.2).kw() - 4.2).abs() < 1e-12);
        assert_eq!(Power::from_kw(4.2), Power::new(4200.0));
    }

    #[test]
    fn inflow_and_outflow_split_the_sign() {
        let importing = Power::from_kw(3.0);
        let exporting = Power::from_kw(-3.0);
        assert_eq!(importing.inflow(), Power::from_kw(3.0));
        assert_eq!(importing.outflow(), Power::ZERO);
        assert_eq!(exporting.inflow(), Power::ZERO);
        assert_eq!(exporting.outflow(), Power::from_kw(3.0));
    }

    #[test]
    fn energy_and_power_are_inverse_over_a_quarter_hour() {
        let quarter = time::Duration::minutes(15);
        let e = Power::from_kw(4.0).over(quarter);
        assert!((e.kwh() - 1.0).abs() < 1e-12);
        assert!((e.over(quarter).kw() - 4.0).abs() < 1e-12);
    }

    #[test]
    fn soc_rejects_out_of_range_and_clamps_on_request() {
        assert!(Soc::new(1.004).is_err());
        assert!(Soc::from_percent(100.4).is_err());
        assert_eq!(Soc::clamped(1.004), Soc::FULL);
        assert_eq!(Soc::clamped(f64::NAN), Soc::EMPTY);
    }

    #[test]
    fn unbalance_is_the_spread_between_conductors() {
        // A 4,6 kW single-phase wallbox on L1 and nothing else.
        let p = PerPhase {
            l1: Power::from_kw(4.6),
            l2: Power::ZERO,
            l3: Power::ZERO,
        };
        assert!((p.unbalance().get() - 4600.0).abs() < 1e-9);
        // Symmetric draw has no unbalance however large it is.
        assert_eq!(
            PerPhase::splat(Power::from_kw(11.0)).unbalance(),
            ApparentPower::ZERO
        );
    }

    #[test]
    fn switchable_connection_moves_between_one_and_three_conductors() {
        let c = PhaseConnection::Switchable { phase: Phase::L2 };
        let single = c.distribute(Power::from_kw(3.6), PhaseMode::Single);
        assert_eq!(single.l2, Power::from_kw(3.6));
        assert_eq!(single.l1, Power::ZERO);
        let three = c.distribute(Power::from_kw(11.0), PhaseMode::Three);
        assert!((three.l1.kw() - 11.0 / 3.0).abs() < 1e-12);
        assert_eq!(three.total(), Power::from_kw(11.0));
    }

    #[test]
    fn a_wiring_that_cannot_switch_ignores_a_mode_it_does_not_have() {
        // The bug this prevents: asking a fixed three-phase connection "would you
        // be single-phase if switched?" and getting `true`, which is how a
        // symmetric device acquires an unbalance limit it can never breach.
        let fixed = PhaseConnection::Three;
        assert!(!fixed.is_switchable());
        assert_eq!(fixed.clamp_mode(PhaseMode::Single), PhaseMode::Three);
        assert_eq!(fixed.count(PhaseMode::Single), 3);
        assert_eq!(
            fixed.distribute(Power::from_kw(3.0), PhaseMode::Single).l1,
            Power::from_kw(1.0)
        );

        let fixed_single = PhaseConnection::Single { phase: Phase::L1 };
        assert_eq!(fixed_single.clamp_mode(PhaseMode::Three), PhaseMode::Single);
        assert_eq!(fixed_single.count(PhaseMode::Three), 1);
    }

    #[test]
    fn a_switchable_wallbox_starts_three_phase() {
        let c = PhaseConnection::Switchable { phase: Phase::L1 };
        assert_eq!(c.default_mode(), PhaseMode::Three);
        assert!(c.supports(PhaseMode::Single) && c.supports(PhaseMode::Three));
        assert_eq!(c.single_phase_conductor(), Some(Phase::L1));
    }

    #[test]
    fn nan_propagates_through_min_and_max_instead_of_being_swallowed() {
        let nan = Power::new_const(f64::NAN);
        assert!(!nan.max(Power::ZERO).is_finite());
        assert!(!nan.min(Power::ZERO).is_finite());
    }
}
