//! What a device will actually accept.
//!
//! The control planes decide in watts. Devices do not: a charge point takes
//! amperes and refuses anything under 6 A, an SG Ready heat pump takes one of
//! three contact states, an inverter takes a ceiling. This crate is the
//! translation and the capability model the arbiter dispatches on — pure
//! functions of an asset and a decision, so the whole of it is testable without
//! any hardware.
#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::doc_markdown
)]

pub mod command;
pub mod sg_ready;

pub use command::{Decision, commands_for, realisable};
pub use sg_ready::{SgReadyState, expected_power, recommended_limited_power, state_for};
