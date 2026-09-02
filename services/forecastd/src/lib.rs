//! `forecastd` — what the sky is going to do, for a household's own roof.
//!
//! `hems-forecast::weather` parses ICON-D2 out of Open-Meteo and
//! `hems-forecast::solar` turns irradiance into what *this* array would make.
//! Both are pure functions. This is the process that fetches, caches and serves
//! them.
//!
//! # It serves weather, not a forecast
//!
//! The distinction is the whole architecture. A weather model knows about the
//! sky and knows nothing about the tree that shades the east string, the chimney,
//! or the fact that this roof has not been cleaned since 2023. Turning modelled
//! irradiance into a *forecast* is `hems-forecast::residual`'s job and it happens
//! **on the box**, from that box's own metering — because the correction is a
//! property of one roof and cannot be learned centrally without the meter that
//! sees it.
//!
//! So what crosses the wire is irradiance and temperature, and the household's
//! own residual model is applied to it locally. A fleet service that shipped
//! finished production forecasts would be a fleet service that had to know every
//! roof.
//!
//! # Why the cache is the same shape as `tariffd`'s
//!
//! For the same reason: a box with no WAN still has to plan. A weather run is
//! good for hours, so an outage that lasts one is invisible; the readiness probe
//! is computed from how much of the horizon is *covered* rather than from when
//! the last request returned.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    // Domain nouns — MiSpeL, SQLite, PostgreSQL, ENTSO-E, ICON-D2 — are
    // capitalised because that is how they are spelled, not because they are
    // identifiers. The same allowance the domain crates carry.
    clippy::doc_markdown,
    clippy::similar_names,
    // A coverage ratio is two counts of quarter hours.
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]

pub mod api;
pub mod config;
pub mod mcp_server;
pub mod poller;
pub mod upstream;

pub use config::{Location, Settings};
pub use poller::{PollOutcome, Poller};
pub use upstream::{Http, Upstream, UpstreamError};
