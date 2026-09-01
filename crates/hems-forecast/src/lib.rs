//! Forecasts, and how much to trust them.
//!
//! Five things a household energy manager has to predict, and one honest answer
//! about how well it did:
//!
//! | Module | Predicts | From |
//! |---|---|---|
//! | [`solar`] | what the roof would produce under a clear sky | geometry, tilt, azimuth, the inverter's limit |
//! | [`residual`] | what it *will* produce | the same roof's own history against that model |
//! | [`load`] | the household's uncontrolled draw | its own quarter hours, by day type |
//! | [`session`] | when the car comes home and how empty | its own charging sessions, by weekday |
//! | [`building`] | which house this is | indoor and outdoor temperature against the heat put in |
//! | [`naive`] | any of them, badly, with nothing | the last day, or the last hour |
//! | [`metrics`] | nothing — it scores the rest | pinball, coverage, bias, CRPS |
//!
//! Every one of them is a **pure function of a record**: nothing here reads a
//! clock, opens a socket or holds a model file. Where a model needs weather it
//! takes it as an argument, and where it needs history it takes that too. That
//! is P1, and it is what lets a whole simulated year of forecasting run as a
//! unit test.
//!
//! # The band is the product
//!
//! Every forecast comes back as a [`quantile::Band`], never as a number. A point
//! forecast is a lie the planner then optimises against — and the places where
//! it is most confidently wrong (a winter afternoon, a car that usually comes
//! home at six) are exactly the places where the plan bets most on it. A model
//! that has learned nothing says so by returning a wide band, and one that has
//! never been checked against a meter is never allowed to look certain
//! ([`residual::ResidualModel::floor_spread`]).
#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    // Comparing forecast values exactly is right in the tests: an empirical
    // quantile *is* one of the observed samples, bit for bit.
    clippy::float_cmp,
    clippy::doc_markdown
)]

pub mod building;
pub mod load;
pub mod metrics;
pub mod naive;
pub mod quantile;
pub mod residual;
pub mod session;
pub mod solar;
pub mod weather;

pub use building::{Identified, ThermalSample, identify};
pub use load::{DayType, LoadProfile};
pub use metrics::{CALIBRATION_DAYS, Calibration, is_informative};
pub use naive::{persistence, seasonal_naive};
pub use quantile::{Band, Forecast, PowerBand};
pub use residual::{ResidualModel, SETTLED_SAMPLES};
pub use session::{Session, SessionForecast, SessionHistory};
pub use solar::{ArrayModel, SunPosition, clear_sky_ghi, sun_position};
pub use weather::{WeatherError, WeatherPoint, WeatherSeries, open_meteo};
