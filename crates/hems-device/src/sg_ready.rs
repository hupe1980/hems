//! The SG Ready interface, as the Bundesverband Wärmepumpe defines it.
//!
//! Two dry contacts, three states — not four. Version 1.1 of the BWP interface
//! specification (`specs/sg-ready/sg-ready-interface-1.1-en.pdf`) dropped the
//! old "start command" state, and an implementation that still sends four is
//! talking to a device that no longer listens.
//!
//! | State | SG1 | SG2 | Meaning |
//! |---|---|---|---|
//! | 1 | 1 | 0 or 1 | Power limitation. **Not** necessarily off. |
//! | 2 | 0 | 0 | Normal operation |
//! | 3 | 0 | 1 | Boost — store surplus as heat |
//!
//! The interesting one is state 1. It is a *limitation*, and the specification
//! recommends that manufacturers implement it as the § 14a EnWG minimum: 4,2 kW
//! for a grid connection power up to 11 kW, 40 % above that — the same two
//! numbers as `[BK6-22-300 A1 4.5.1]`. So a heat pump in state 1 is not off,
//! and a controller that assumes it is will plan a house cold for no reason.
//!
//! It is also the coarsest interface in the workspace: three states against a
//! planner that thinks in watts. From 1 July 2027 a heat pump funded under the
//! BEG needs an interoperable digital interface in a Code-of-Conduct format —
//! EEBUS per VDE-AR-E 2829-6 — so this is a bridge to an installed base rather
//! than a destination.

use core::fmt;

use hems_core::prelude::Power;

/// One of the three SG Ready operating states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SgReadyState {
    /// **State 1** — power limitation, `SG1 = 1`.
    ///
    /// The device reduces to a configured value, which may be zero but is more
    /// often the § 14a minimum. The specification also permits the device to
    /// *defer* entering this state to finish a defrost cycle, so a controller
    /// must not treat the command as instantaneous.
    Limited,
    /// **State 2** — normal operation. The default.
    #[default]
    Normal,
    /// **State 3** — boost: raise buffer and hot-water targets to store surplus
    /// electricity as heat.
    Boost,
}

impl SgReadyState {
    /// The two contacts, `(SG1, SG2)`.
    #[must_use]
    pub const fn contacts(self) -> (bool, bool) {
        match self {
            SgReadyState::Limited => (true, false),
            SgReadyState::Normal => (false, false),
            SgReadyState::Boost => (false, true),
        }
    }

    /// The state a pair of contacts represents.
    ///
    /// `SG1 = 1` is state 1 whatever SG2 says — the specification gives SG1
    /// priority, because it is the network operator's signal.
    #[must_use]
    pub const fn from_contacts(sg1: bool, sg2: bool) -> Self {
        match (sg1, sg2) {
            (true, _) => SgReadyState::Limited,
            (false, false) => SgReadyState::Normal,
            (false, true) => SgReadyState::Boost,
        }
    }

    /// The state number used in the specification and on device labels.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            SgReadyState::Limited => 1,
            SgReadyState::Normal => 2,
            SgReadyState::Boost => 3,
        }
    }

    /// The state for a number, if it is one of the three.
    #[must_use]
    pub const fn from_number(n: u8) -> Option<Self> {
        match n {
            1 => Some(SgReadyState::Limited),
            2 => Some(SgReadyState::Normal),
            3 => Some(SgReadyState::Boost),
            _ => None,
        }
    }
}

impl fmt::Display for SgReadyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SgReadyState::Limited => "1 (limited)",
            SgReadyState::Normal => "2 (normal)",
            SgReadyState::Boost => "3 (boost)",
        };
        f.write_str(s)
    }
}

/// The power a device draws in state 1, per the specification's recommendation.
///
/// `[A1 4.5.1]` again, arrived at from the other direction: 4,2 kW up to a grid
/// connection power of 11 kW, 40 % of it above. A manufacturer may configure
/// something lower, including zero — this is what a controller should *expect*,
/// not what it can rely on.
#[must_use]
pub fn recommended_limited_power(grid_connection_power: Power) -> Power {
    if grid_connection_power > Power::from_kw(11.0) {
        grid_connection_power * 0.4
    } else {
        Power::from_kw(4.2)
    }
}

/// The fraction of its rating a heat pump is taken to draw in normal operation.
///
/// The device decides for itself in state 2, so this is only the reference the
/// mapping uses to answer "is the planner asking for less than normal, or more?".
/// Half the rating is a reasonable stand-in for a modulating unit over a
/// heating hour.
pub const NORMAL_DUTY: f64 = 0.5;

/// Above this fraction of its rating, a request means "take everything".
pub const BOOST_DUTY: f64 = 0.95;

/// Choose a state for a wanted power.
///
/// Three states cannot express a continuous target, so the mapping answers the
/// only question the interface can hear:
///
/// * clearly **less** than normal operation → state 1, the only reducing state;
/// * essentially **all** of it → state 3, which is how surplus is stored;
/// * anything else → state 2, and the device decides.
///
/// The thresholds are fractions of the unit's own rating rather than of the
/// state-1 value, because the two are unrelated: a 3 kW heat pump on a 9 kW
/// connection has a state-1 value of 4,2 kW, which limits it not at all. What
/// the device will *actually* draw in the chosen state is
/// [`expected_power`] — and it is frequently not what was asked for. That is the
/// interface, not the mapping.
#[must_use]
pub fn state_for(wanted: Power, nominal: Power, _grid_connection_power: Power) -> SgReadyState {
    if wanted < nominal * NORMAL_DUTY {
        SgReadyState::Limited
    } else if wanted >= nominal * BOOST_DUTY {
        SgReadyState::Boost
    } else {
        SgReadyState::Normal
    }
}

/// What a device is expected to draw in `state`.
///
/// The number the guard and the arbiter should reason with, because it is what
/// the meter will see. In state 1 it is the smaller of the § 14a value and the
/// unit's own rating — a heat pump cannot be limited to more than it can draw.
#[must_use]
pub fn expected_power(state: SgReadyState, nominal: Power, grid_connection_power: Power) -> Power {
    match state {
        SgReadyState::Limited => recommended_limited_power(grid_connection_power).min(nominal),
        SgReadyState::Normal => nominal * NORMAL_DUTY,
        SgReadyState::Boost => nominal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contacts_round_trip() {
        for state in [
            SgReadyState::Limited,
            SgReadyState::Normal,
            SgReadyState::Boost,
        ] {
            let (sg1, sg2) = state.contacts();
            assert_eq!(SgReadyState::from_contacts(sg1, sg2), state);
            assert_eq!(SgReadyState::from_number(state.number()), Some(state));
        }
    }

    #[test]
    fn sg1_wins_whatever_sg2_says() {
        // The specification gives SG1 priority: it is the network operator's
        // signal, and 1/1 is still state 1.
        assert_eq!(
            SgReadyState::from_contacts(true, true),
            SgReadyState::Limited
        );
    }

    #[test]
    fn there_is_no_fourth_state() {
        assert_eq!(SgReadyState::from_number(4), None);
        assert_eq!(SgReadyState::from_number(0), None);
    }

    #[test]
    fn state_one_follows_the_paragraph_14a_numbers() {
        // Up to 11 kW: the flat 4,2 kW of [A1 4.5.1].
        assert_eq!(
            recommended_limited_power(Power::from_kw(9.0)),
            Power::from_kw(4.2)
        );
        assert_eq!(
            recommended_limited_power(Power::from_kw(11.0)),
            Power::from_kw(4.2)
        );
        // Above it: 40 % of the connection power.
        assert!((recommended_limited_power(Power::from_kw(20.0)).kw() - 8.0).abs() < 1e-9);
    }

    #[test]
    fn a_limited_heat_pump_is_not_an_off_heat_pump() {
        // The mistake this module exists to prevent: state 1 draws the § 14a
        // minimum, not nothing, so a planner that assumes zero heats a house it
        // did not need to.
        assert!(recommended_limited_power(Power::from_kw(9.0)) > Power::ZERO);
    }

    #[test]
    fn the_mapping_has_a_non_empty_band_for_every_state() {
        let nominal = Power::from_kw(5.0);
        let connection = Power::from_kw(9.0);
        assert_eq!(
            state_for(Power::ZERO, nominal, connection),
            SgReadyState::Limited
        );
        assert_eq!(
            state_for(Power::from_kw(2.0), nominal, connection),
            SgReadyState::Limited
        );
        assert_eq!(
            state_for(Power::from_kw(3.0), nominal, connection),
            SgReadyState::Normal
        );
        assert_eq!(
            state_for(Power::from_kw(5.0), nominal, connection),
            SgReadyState::Boost
        );
    }

    #[test]
    fn the_bands_stay_ordered_for_a_small_unit_on_a_large_connection() {
        // A 3 kW heat pump on a 9 kW connection has a state-1 value of 4,2 kW,
        // which does not limit it at all. Keying the mapping on the state-1
        // value instead of the rating would collapse every request into state 1.
        let nominal = Power::from_kw(3.0);
        let connection = Power::from_kw(9.0);
        assert_eq!(
            state_for(Power::ZERO, nominal, connection),
            SgReadyState::Limited
        );
        assert_eq!(
            state_for(Power::from_kw(2.0), nominal, connection),
            SgReadyState::Normal
        );
        assert_eq!(
            state_for(Power::from_kw(3.0), nominal, connection),
            SgReadyState::Boost
        );
    }

    #[test]
    fn the_expected_power_is_what_the_meter_will_see_not_what_was_asked_for() {
        let nominal = Power::from_kw(5.0);
        let connection = Power::from_kw(9.0);
        // Asking for nothing gets state 1 — which still draws 4,2 kW.
        assert_eq!(
            state_for(Power::ZERO, nominal, connection),
            SgReadyState::Limited
        );
        assert_eq!(
            expected_power(SgReadyState::Limited, nominal, connection),
            Power::from_kw(4.2)
        );
        // And a unit smaller than the § 14a value is not limited by it at all.
        assert_eq!(
            expected_power(SgReadyState::Limited, Power::from_kw(3.0), connection),
            Power::from_kw(3.0)
        );
        assert_eq!(
            expected_power(SgReadyState::Boost, nominal, connection),
            nominal
        );
    }
}
