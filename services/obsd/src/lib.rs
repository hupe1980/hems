//! `obsd` — what the fleet is actually doing.
//!
//! Every box reports its day as a [`hems_core::report::DayKpis`]; this service
//! keeps them and answers the three questions a fleet operator has.
//!
//! # The questions are not the same shape, and that is the design
//!
//! **"How are we doing?"** is an average — the saving, the self-sufficiency, the
//! forecast scores — and an average over enough days is the only honest way to
//! quote any of them.
//!
//! **"Who is broken?"** is a *count*, and averaging it is how a real finding
//! disappears. One household in ten thousand that failed to respect a § 14a
//! reduction is a compliance incident with a name and a date; the same fact
//! expressed as "99,99 % compliance" is a number that reads as success. So
//! [`fleet::Summary`] carries the failures as a list of sites and never as a
//! rate.
//!
//! **"Is the forecast honest?"** is neither: forecast error is correlated across
//! a day, so ninety-six quarter hours of one Tuesday are close to **one** draw.
//! A single day's coverage figure is a coin toss reported to three significant
//! figures, and only the merge of many days can be compared with the 80 % the
//! band promises — which is exactly why a box reports its scores rather than
//! judging them.
//!
//! # It holds a window, not a history
//!
//! The *record* is `histd`'s: the § 14a evidence `[A1 7.3]` keeps for two years
//! and the quarter-hour registers a settlement is computed from. What lives here
//! is derived, bounded and rebuildable, and losing it costs a dashboard rather
//! than a Nachweis.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::similar_names,
    // Every ratio here is a count of days over a count of days.
    clippy::cast_precision_loss
)]

pub mod api;
pub mod config;
pub mod fleet;
pub mod mcp_server;

pub use config::Settings;
pub use fleet::{Fleet, Summary};
