//! The guard plane and the one-second control loop.
//!
//! Two things live here, and the order between them is the design:
//!
//! * [`guard`] turns every limit — the network operator's, the fuses', the
//!   device's, the household's backup reserve — into an interval per asset, and
//!   shares out the ones that several devices spend together;
//! * [`arbiter`] picks a point inside what is left, following the plan's energy
//!   and tracking the measured imbalance, and attaches the reason;
//! * [`phases`] decides, for a charge point that can, whether it should be on
//!   one conductor or three — the largest single lever in surplus charging and
//!   the easiest to turn into a chattering contactor.
//!
//! Nothing the arbiter does can widen what the guard decided. That is not a
//! convention: the guard runs first, the arbiter only clamps into its intervals,
//! and a randomised property test over households, measurements, plans and user
//! overrides asserts the § 14a promise on every tick.
#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::cast_precision_loss
)]

pub mod allocate;
pub mod arbiter;
pub mod guard;
pub mod phases;

pub use allocate::{Claim, Grant, allocate, allocate_indivisible};
pub use arbiter::{Arbiter, ArbiterConfig, Decision, Tick};
pub use guard::{
    Binding, GridLimits, Guard, GuardConfig, GuardVerdict, SiteState, is_controllable,
    minimum_useful_power, physical_headroom,
};
pub use phases::{PhaseState, PhaseSwitchConfig};
