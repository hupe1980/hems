//! The planner: what the house should do over the next two days.
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
    BatteryModel, DhwModel, EvSession, HeatPumpModel, Objective, PlanningLimits, Problem, Quantile,
    ThermalModel, TimedLimit,
};
pub use shadow::Shadow;
pub use solve::{AssetNames, Flows, SolveError, Solved, solve};
