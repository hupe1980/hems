//! The household's flexibility, in the language Europe agreed on.
//!
//! [S2](https://s2standard.org) — **EN 50491-12-2** — is the interface between a
//! Customer Energy Manager and the Resource Managers in a building. Its central
//! idea is worth stating plainly, because it is what makes it different from
//! every protocol that came before:
//!
//! > A device describes **what it can do**, not what it is for.
//!
//! A battery, a hot water tank and a parked car are all *storage with a fill
//! level*. Once they say so in the same words, an energy manager can plan all
//! three without knowing what any of them is — and a device that arrives next
//! year works without a driver being written for it. EEBUS, by contrast,
//! organises around named use cases, which is why it needs a new one for each
//! new thing a device might want to do.
//!
//! hems plans in S2's terms internally and speaks EEBUS where the German grid
//! requires it. This crate is the first half: which control type each asset
//! belongs to, the description a Customer Energy Manager would be sent, and the
//! translation of an instruction back into something [`hems_device`] can issue.
//!
//! The wire types are [`s2energy`], generated from the official schema by the
//! standard's own authors. Writing our own would be a second opinion about a
//! wire format, which is the one thing a standard exists to prevent.
//!
//! # The five control types
//!
//! | Control type | For | Example |
//! |---|---|---|
//! | **FRBC** Fill Rate Based Control | anything with a fill level and a rate | battery, hot water tank, the building's own thermal mass |
//! | **PEBC** Power Envelope Based Control | anything that just needs a bound | charge point, inverter curtailment |
//! | **OMBC** Operation Mode Based Control | discrete states | SG Ready heat pump, a two-speed pump |
//! | **PPBC** Power Profile Based Control | a fixed sequence started in a window | washing machine, dishwasher |
//! | **DDBC** Demand Driven Based Control | actuators serving a reported demand | a heat pump following a heat demand |

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::doc_markdown
)]

pub mod describe;
pub mod instruct;
pub mod map;

pub use describe::{
    BatteryDescription, HeatPumpDescription, describe_battery, describe_evse, describe_heat_pump,
    describe_pv, resource_manager_details,
};
pub use instruct::{InstructError, battery_power, envelope_command, envelope_now, heat_pump_state};
pub use map::{ControlType, control_type_for, roles_for};
