//! Errors the domain model itself can raise.

use thiserror::Error;

/// A value that cannot be a physical quantity.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum UnitError {
    /// A state of charge outside `[0, 1]` (or `[0, 100]` %), or not finite.
    #[error("state of charge out of range: {0}")]
    SocOutOfRange(f64),
}

/// A value that cannot be an identifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    /// The identifier was empty.
    #[error("identifier must not be empty")]
    Empty,
    /// The identifier was longer than 64 characters.
    #[error("identifier is longer than 64 characters: {0} characters")]
    TooLong(usize),
    /// The identifier contained a character outside `[a-z0-9._-]`.
    #[error("identifier contains an unsupported character {0:?}; allowed: a-z, 0-9, '.', '_', '-'")]
    BadCharacter(char),
}

/// A site description that cannot be acted on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SiteError {
    /// Two assets, or two circuits, share an identifier.
    #[error("duplicate {kind} identifier: {id}")]
    DuplicateId {
        /// What was duplicated (`asset` or `circuit`).
        kind: &'static str,
        /// The identifier that appeared twice.
        id: String,
    },
    /// An asset names a circuit the site does not have.
    #[error("asset {asset} is on unknown circuit {circuit}")]
    UnknownCircuit {
        /// The asset with the dangling reference.
        asset: String,
        /// The circuit it named.
        circuit: String,
    },
    /// A circuit names a parent the site does not have.
    #[error("circuit {circuit} has unknown parent {parent}")]
    UnknownParent {
        /// The circuit with the dangling reference.
        circuit: String,
        /// The parent it named.
        parent: String,
    },
    /// The circuit parent references form a cycle.
    #[error("circuit hierarchy contains a cycle through {circuit}")]
    CircuitCycle {
        /// A circuit on the cycle.
        circuit: String,
    },
}

/// A command that must not be sent to a device.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SetpointError {
    /// The commanded value was `NaN` or infinite.
    ///
    /// This is the gate described in [`crate::units`]: quantities are cheap and
    /// infallible to construct, and the check happens once, where a number turns
    /// into an action.
    #[error("setpoint for {asset} is not a finite value")]
    NotFinite {
        /// The asset the command was aimed at.
        asset: String,
    },
}
