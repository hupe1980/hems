//! The planner: what the house should do over the hours ahead.
#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::doc_markdown
)]

#[cfg(not(any(feature = "microlp", feature = "highs")))]
compile_error!(
    "hems-optimizer needs a solver: enable the `microlp` feature (pure Rust, the      default) or `highs` (faster, needs cmake and a C++ toolchain). With both      enabled, HiGHS is used."
);

pub mod model;
pub mod shadow;
pub mod solve;

pub use model::{
    BatteryModel, Commitment, DhwModel, EvSession, HeatPumpModel, Objective, PlanningLimits,
    Problem, Quantile, Realisation, Risk, ScenarioSet, ShiftableRun, ThermalModel, TimedLimit,
};
pub use shadow::Shadow;
pub use solve::{AssetNames, Flows, SolveError, Solved, check_inputs, solve};

/// A compressor's own state, which a plan needs in order to honour the minimum
/// runtime it models across a receding horizon.
///
/// Re-exported because it is part of [`HeatPumpModel`]'s surface: it lives in
/// `hems-core` so the simulator can answer a plan with the same two facts the
/// plan was made from.
pub use hems_core::prelude::CompressorState;
