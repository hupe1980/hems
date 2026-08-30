//! Deterministic simulators, on virtual time.
//!
//! Two halves. [`device`] and [`steuerbox`] are the *hardware*: a battery with
//! its efficiencies, an inverter that takes a second to obey a curtailment, a
//! charge point with a contactor, a heat pump with its own thermostat, a tank
//! that runs out of hot water, and a Steuerbox that sends EEBUS limitation
//! events on a script. [`weather`] is the *day*: the cloud that passes at 12:19,
//! the afternoon three kelvin milder than the model said, the shower that ran
//! long.
//!
//! The second half is what stops a simulated day being a lie. A day whose
//! forecast is the same series the simulator runs measures a planner that has
//! been shown the answer, and every saving it reports is an upper bound nobody
//! in the field can reach. [`weather::Realisation`] is the difference between
//! the two series — deterministic, seeded, and part of the scenario, so a day
//! still replays to the last cent while the planner still has to be wrong about
//! it first.
#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::cast_precision_loss
)]

pub mod device;
pub mod steuerbox;
pub mod weather;

pub use device::{BatterySim, BuildingSim, EvseSim, PvSim, TankSim, VehicleSim};
pub use steuerbox::{Instruction, SteuerboxSim};
pub use weather::Realisation;
