//! The vocabulary of § 14a limitation — and deliberately not a state machine.
//!
//! The machine is [`eebus`]'s, reached through `hems_drv::eebus::Lpc`, and there
//! is exactly one of it in the workspace: two implementations of a certifiable
//! state machine disagree, and the one that is wrong is whichever the
//! certification laboratory is not looking at (D186). The reference days drive
//! that one, and `hems-drv`'s `lpc_exhaustive.rs` explores it.
//!
//! What lives here is the **name of the state**, because that is grid-rule
//! vocabulary rather than protocol machinery: the guard reads it, the `[A1 7.2]`
//! evidence record turns on it — an operator reducing a household and a household
//! restraining *itself* look identical at the connection point and are entirely
//! different events — and a consumer of this crate that links no EEBUS stack
//! still needs to say which of the five a household was in.
//!
//! [`eebus`]: https://docs.rs/eebus

use core::fmt;

/// The five states of the Controllable System, § 2.3.2.
///
/// Derived from `eebus`'s own rather than tracked beside it
/// (`hems_drv::eebus::Lpc::state`), so there is one machine and one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LpcState {
    /// Just (re)started. Limited by the failsafe value until the Energy Guard
    /// makes contact — so a device that reboots during a grid emergency comes
    /// back up restrained, not at full power (`[LPC-901/1]`).
    Init,
    /// In contact with the Energy Guard, no limit active (`[LPC-009/2]`).
    UnlimitedControlled,
    /// A limit from the Energy Guard is in force (`[LPC-009/1]`).
    Limited,
    /// The Energy Guard's heartbeat stopped; the failsafe value applies.
    Failsafe,
    /// Out of contact for long enough that the failsafe was released. The device
    /// runs as if no external limitation existed (`[LPC-922]`).
    UnlimitedAutonomous,
}

impl LpcState {
    /// Whether the Energy Guard is considered present.
    ///
    /// The question the evidence record turns on: a ceiling that applies because
    /// an operator asked for it is a control event `[A1 7.2]` documents, and one
    /// that applies because nobody is talking to the box is the household
    /// restraining itself. The two are the same number of kilowatts at the
    /// connection point.
    #[must_use]
    pub const fn is_controlled(self) -> bool {
        matches!(self, LpcState::UnlimitedControlled | LpcState::Limited)
    }
}

impl fmt::Display for LpcState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LpcState::Init => "init",
            LpcState::UnlimitedControlled => "unlimited/controlled",
            LpcState::Limited => "limited",
            LpcState::Failsafe => "failsafe",
            LpcState::UnlimitedAutonomous => "unlimited/autonomous",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_states_with_a_guard_on_the_other_end_are_controlled() {
        // The distinction the `[A1 7.2]` record is built on. `Init` and
        // `Failsafe` both hold the household at its failsafe value and neither
        // is an operator reducing anybody.
        assert!(LpcState::Limited.is_controlled());
        assert!(LpcState::UnlimitedControlled.is_controlled());
        for alone in [
            LpcState::Init,
            LpcState::Failsafe,
            LpcState::UnlimitedAutonomous,
        ] {
            assert!(!alone.is_controlled(), "{alone} is the household by itself");
        }
    }
}
