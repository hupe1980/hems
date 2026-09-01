//! What a box can ask `tariffd`.
//!
//! # Open on purpose
//!
//! Nothing here is authenticated, and that is a decision rather than an
//! omission: a day-ahead curve is a published auction result, not a household's
//! data. What an open endpoint does cost is the operator's own API quota — the
//! upstream token is theirs — so a deployment that cares puts this behind
//! whatever it already uses for rate limiting. `histd` and `obsd` are the
//! opposite case and are authenticated per site.
//!
//! Three routes and one idea: the answer says how much of the question it could
//! answer. A price service that returns two hundred quarter hours where a
//! hundred and ninety-two were asked for, and does not say which, is a service
//! whose caller has to re-derive the coverage it already knew.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use hems_core::prelude::Horizon;
use hems_tariff::cache::PriceCache;
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// What the API serves from.
#[derive(Clone)]
pub struct Prices {
    cache: Arc<RwLock<PriceCache>>,
}

impl Prices {
    /// A handle onto a shared cache.
    #[must_use]
    pub fn new(cache: Arc<RwLock<PriceCache>>) -> Self {
        Self { cache }
    }
}

/// The routes.
pub fn router(prices: Prices) -> Router {
    Router::new()
        .route("/v1/prices", get(prices_handler))
        .route("/v1/prices/coverage", get(coverage_handler))
        .with_state(prices)
}

/// `?from=<rfc3339>&slots=<n>` — both optional.
#[derive(Debug, serde::Deserialize)]
pub struct Window {
    /// The first instant of interest; the slot containing it is the first slot.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub from: Option<OffsetDateTime>,
    /// How many quarter hours.
    #[serde(default)]
    pub slots: Option<usize>,
}

impl Window {
    /// The horizon this window names, with the daemon's own defaults.
    fn horizon(&self, now: OffsetDateTime) -> Horizon {
        Horizon::new(self.from.unwrap_or(now), self.slots.unwrap_or(96).min(384))
    }
}

/// One quarter hour, as it is served.
#[derive(Debug, serde::Serialize)]
pub struct Point {
    /// The quarter hour's start.
    #[serde(with = "time::serde::rfc3339")]
    pub slot: OffsetDateTime,
    /// The wholesale price, ct/kWh, as an exact decimal string.
    pub price_ct: String,
    /// Which source it came from.
    pub source: hems_tariff::source::Source,
}

/// The answer to a price question.
#[derive(Debug, serde::Serialize)]
pub struct Answer {
    /// The points that are known, in order. Slots the cache cannot price are
    /// **absent** rather than filled with a guess — inventing one here is the
    /// mistake `SolveError::ForecastTooShort` exists to refuse one layer up.
    pub points: Vec<Point>,
    /// How much of the window is priced, in `[0, 1]`.
    pub coverage: f64,
    /// The last quarter hour reachable from the start without a gap, if any.
    #[serde(with = "time::serde::rfc3339::option")]
    pub contiguous_until: Option<OffsetDateTime>,
}

async fn prices_handler(
    State(prices): State<Prices>,
    Query(window): Query<Window>,
) -> (StatusCode, axum::Json<Answer>) {
    let now = OffsetDateTime::now_utc();
    let horizon = window.horizon(now);
    let cache = prices.cache.read().await;
    let points: Vec<Point> = horizon
        .slots()
        .filter_map(|slot| {
            cache.at(slot).map(|observed| Point {
                slot: slot.start(),
                price_ct: observed.price_ct.to_string(),
                source: observed.source,
            })
        })
        .collect();
    let answer = Answer {
        coverage: cache.coverage(horizon),
        contiguous_until: horizon
            .get(0)
            .and_then(|first| cache.contiguous_until(first))
            .map(|slot| slot.start()),
        points,
    };
    // A partial answer is a `200` with a coverage below one, not a `404`: the
    // caller asked what the service has, and "less than you wanted" is an
    // answer to that question.
    (StatusCode::OK, axum::Json(answer))
}

async fn coverage_handler(
    State(prices): State<Prices>,
    Query(window): Query<Window>,
) -> axum::Json<serde_json::Value> {
    let now = OffsetDateTime::now_utc();
    let horizon = window.horizon(now);
    let cache = prices.cache.read().await;
    axum::Json(serde_json::json!({
        "coverage": cache.coverage(horizon),
        "cached_slots": cache.len(),
    }))
}
