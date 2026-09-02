//! The hems edge daemon.
//!
//! One process per household. It holds the three control planes — guard,
//! arbiter, planner — and the drivers that talk to the hardware, and it is
//! designed so that the house stays safe and lawful when everything above it
//! is unreachable.
//!
//! This build ships the control stack and a simulated house to run it against;
//! the protocol drivers are the next milestone.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::cast_precision_loss,
    // Domain nouns — MiSpeL, SQLite, Nachweis, ENTSO-E — are capitalised because
    // that is how they are spelled, not because they are identifiers. The same
    // allowance the domain crates carry.
    clippy::doc_markdown
)]

pub mod backtest;
pub mod config;
pub mod drivers;
pub mod forecasting;
pub mod report;
pub mod runtime;
pub mod scenario;
pub mod site;
pub mod store;

pub use backtest::{Spread, spread_over_days};
pub use config::{ControlSettings, DriverSettings, Settings, SiteSettings};
pub use forecasting::{Learned, Weather, WeatherSpec};
pub use runtime::{Running, Status};
pub use scenario::{CommunityMembership, DayResult, EvPlan, Scenario, run};
pub use site::{Household, HouseholdConfig};
