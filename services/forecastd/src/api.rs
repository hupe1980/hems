//! What a box can ask `forecastd`.
//!
//! # Open on purpose
//!
//! Nothing here is authenticated, and that is a decision rather than an
//! omission: ICON-D2 irradiance over a location is public weather, not a
//! household's data. It does spend the operator's own upstream quota, so a
//! deployment that cares rate-limits it. `histd` and `obsd` carry household data
//! and are authenticated per site.
//!
//! Two routes, and the split between them is the architecture: `/weather` is the
//! sky, `/production` is what an array of a given geometry would make of it.
//! Neither is a *forecast* — that needs the residual correction only the box's
//! own meter can teach — and the naming says so, because a route called
//! `/forecast` would be one somebody eventually planned against uncorrected.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use hems_core::prelude::Power;
use tokio::sync::RwLock;

use crate::poller::Run;

/// What the API serves from.
#[derive(Clone)]
pub struct Weather {
    runs: Arc<RwLock<BTreeMap<String, Run>>>,
}

impl Weather {
    /// A handle onto the shared runs.
    #[must_use]
    pub fn new(runs: Arc<RwLock<BTreeMap<String, Run>>>) -> Self {
        Self { runs }
    }
}

/// The routes.
pub fn router(weather: Weather) -> Router {
    Router::new()
        .route("/v1/weather/{location}", get(weather_handler))
        .route("/v1/production/{location}", get(production_handler))
        .with_state(weather)
}

/// One quarter hour of sky.
#[derive(Debug, serde::Serialize)]
pub struct Point {
    /// The quarter hour's start.
    #[serde(with = "time::serde::rfc3339")]
    pub slot: time::OffsetDateTime,
    /// Global horizontal irradiance, W/m².
    pub ghi_w_per_m2: f64,
    /// Air temperature, °C.
    pub temperature_c: f64,
    /// Cloud cover in `[0, 1]`, where the model publishes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_cover: Option<f64>,
}

/// A location's current run.
#[derive(Debug, serde::Serialize)]
pub struct Answer {
    /// When the run was fetched.
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: time::OffsetDateTime,
    /// The resolution the model published at.
    pub published_minutes: u16,
    /// The points.
    pub points: Vec<Point>,
}

async fn weather_handler(
    State(state): State<Weather>,
    Path(location): Path<String>,
) -> Result<axum::Json<Answer>, StatusCode> {
    let runs = state.runs.read().await;
    let run = runs.get(&location).ok_or(StatusCode::NOT_FOUND)?;
    Ok(axum::Json(Answer {
        fetched_at: run.fetched_at,
        published_minutes: run.series.published_minutes,
        points: run
            .series
            .slots
            .iter()
            .map(|(slot, point)| Point {
                slot: slot.start(),
                ghi_w_per_m2: point.ghi_w_per_m2,
                temperature_c: point.temperature_c,
                cloud_cover: point.cloud_cover,
            })
            .collect(),
    }))
}

/// `?kwp=..&ac_kw=..&tilt=..&azimuth=..` — one household's roof.
#[derive(Debug, serde::Deserialize)]
pub struct Array {
    /// Installed DC power, kWp.
    pub kwp: f64,
    /// The inverter's AC limit, kW. Defaults to 80 % of the array, which is
    /// what an ordinary German installation is sized at.
    #[serde(default)]
    pub ac_kw: Option<f64>,
    /// Tilt from horizontal, degrees.
    #[serde(default = "default_tilt")]
    pub tilt: f64,
    /// Azimuth clockwise from north; 180 is due south.
    #[serde(default = "default_azimuth")]
    pub azimuth: f64,
}

fn default_tilt() -> f64 {
    35.0
}

fn default_azimuth() -> f64 {
    180.0
}

/// What a roof would make, before the box's own correction.
#[derive(Debug, serde::Serialize)]
pub struct Modelled {
    /// The quarter hour's start.
    #[serde(with = "time::serde::rfc3339")]
    pub slot: time::OffsetDateTime,
    /// Modelled production, watts, as a positive magnitude.
    pub watts: f64,
}

async fn production_handler(
    State(state): State<Weather>,
    Path(location): Path<String>,
    Query(array): Query<Array>,
) -> Result<axum::Json<Vec<Modelled>>, StatusCode> {
    let runs = state.runs.read().await;
    let run = runs.get(&location).ok_or(StatusCode::NOT_FOUND)?;
    // The sun position is computed at the coordinates the **run** was fetched
    // for, not at anything the caller supplies. A household may describe its own
    // roof — the array is its property — and it may not move its own latitude,
    // because the irradiance it is being given is somebody else's sky otherwise.
    let model = hems_forecast::ArrayModel::new(
        Power::from_kw(array.kwp),
        Power::from_kw(array.ac_kw.unwrap_or(array.kwp * 0.8)),
        array.tilt,
        array.azimuth,
    );
    Ok(axum::Json(
        run.series
            .modelled_production(&model, run.at)
            .into_iter()
            .map(|(slot, watts)| Modelled {
                slot: slot.start(),
                watts,
            })
            .collect(),
    ))
}
