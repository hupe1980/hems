//! What a box, a household and a network operator can ask `histd`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hems_grid::mispel::QuarterHour;
use hems_service::auth::{Authority, Credentials, bearer};
use time::OffsetDateTime;

use crate::{Store, StoreError};

/// What the API writes to and reads from.
///
/// One pool and no writer: the driver is async, so a Data Act export — 11 MB of
/// JSON and 370 ms — yields rather than occupying a runtime worker, and there is
/// no write lock for a fleet's forwarded evidence to queue behind (D156).
#[derive(Clone)]
pub struct History {
    store: Store,
    credentials: Arc<Credentials>,
    mispel: Arc<std::collections::BTreeMap<String, crate::config::MispelSettings>>,
}

impl History {
    /// A handle onto the store, and the credentials it answers to.
    ///
    /// An **empty** [`Credentials`] is a service that answers nothing. What
    /// these routes serve is a household's whole consumption record and the
    /// evidence a network operator settles on, so "nobody configured it" has to
    /// read as "nobody may".
    #[must_use]
    pub fn new(store: Store, credentials: Credentials) -> Self {
        Self {
            store,
            credentials: Arc::new(credentials),
            mispel: Arc::new(std::collections::BTreeMap::new()),
        }
    }

    /// Which MiSpeL option each site has declared, `[MiSpeL Tenor]`.
    ///
    /// Separate from [`History::new`] because it is the operator's *intent*
    /// rather than a connection or a credential: a deployment that settles
    /// nobody is a deployment that keeps the registers and does not compute a
    /// Nachweis from them, which is exactly what every deployment did before
    /// this existed.
    #[must_use]
    pub fn settling(
        mut self,
        declarations: std::collections::BTreeMap<String, crate::config::MispelSettings>,
    ) -> Self {
        self.mispel = Arc::new(declarations);
        self
    }

    /// What `site` has declared, if anything.
    fn declaration(&self, site: &str) -> Option<crate::config::MispelSettings> {
        self.mispel.get(site).copied()
    }

    /// What the request's bearer token is allowed to do.
    ///
    /// One status for every failure — absent, malformed, unknown. Which it was
    /// is an operational fact for whoever runs the fleet and a probing aid for
    /// anybody else.
    fn authority(&self, headers: &HeaderMap) -> Result<Authority, StatusCode> {
        bearer(
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
        )
        .and_then(|token| self.credentials.authority_of(token))
        .ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// A query that failed is a `500`, and it says so once.
///
/// The one place a [`StoreError`] becomes a status, so a route cannot invent a
/// different answer for the same fault — and the one place it is logged, so a
/// failing database is one line per request rather than none or three.
fn failed<T>(outcome: Result<T, StoreError>) -> Result<T, StatusCode> {
    outcome.map_err(|e| {
        tracing::error!(error = %e, "a query failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// The routes.
pub fn router(history: History) -> Router {
    Router::new()
        .route("/v1/sites/{site}/quarter-hours", post(put_quarter_hours))
        .route("/v1/sites/{site}/quarter-hours", get(get_quarter_hours))
        .route("/v1/sites/{site}/events", post(put_event))
        .route("/v1/sites/{site}/nachweis", get(get_nachweis))
        .route("/v1/sites/{site}/export", get(get_export))
        .route("/v1/sites/{site}/mispel", get(get_mispel))
        .with_state(history)
}

/// `?from=<rfc3339>&to=<rfc3339>`.
#[derive(Debug, serde::Deserialize)]
pub struct Window {
    /// Inclusive lower bound.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub from: Option<OffsetDateTime>,
    /// Exclusive upper bound.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub to: Option<OffsetDateTime>,
}

async fn put_quarter_hours(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    axum::Json(quarters): axum::Json<Vec<QuarterHour>>,
) -> Result<StatusCode, StatusCode> {
    deny_unless(state.authority(&headers)?.may_write(&site))?;
    let now = OffsetDateTime::now_utc();
    // One transaction for the whole batch: a day's registers are one fact, and a
    // settlement that can observe half of them is one that can be run on half a
    // day.
    failed(state.store.put_quarter_hours(&site, &quarters, now).await)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_quarter_hours(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    Query(window): Query<Window>,
) -> Result<axum::Json<Vec<QuarterHour>>, StatusCode> {
    deny_unless(state.authority(&headers)?.may_read(&site))?;
    failed(
        state
            .store
            .quarter_hours(&site, window.from, window.to)
            .await,
    )
    .map(axum::Json)
}

async fn put_event(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    axum::Json(event): axum::Json<hems_grid::evidence::ControlEvent>,
) -> Result<StatusCode, StatusCode> {
    deny_unless(state.authority(&headers)?.may_write(&site))?;
    failed(state.store.put_control_event(&site, &event).await).map(|_| StatusCode::CREATED)
}

async fn get_nachweis(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    Query(window): Query<Window>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    // A network operator may read this: it is the record of what *they*
    // commanded and what the connection point drew, `[A1 7.2]`.
    deny_unless(state.authority(&headers)?.may_read(&site))?;
    failed(crate::export::nachweis(&state.store, &site, window.from, window.to).await)
        .map(axum::Json)
}

async fn get_export(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    // Narrower than the Nachweis, and deliberately: Article 4 of Regulation (EU)
    // 2023/2854 is a right of the **user**. This is everything the product
    // generated — when the shower ran, which fortnight nobody was in — and a
    // fleet operator holding a token is not a household.
    deny_unless(state.authority(&headers)?.may_read_everything(&site))?;
    failed(crate::export::data_act(&state.store, &site).await).map(axum::Json)
}

/// `?year=2026&month=10` — the calendar period to settle.
#[derive(Debug, serde::Deserialize)]
pub struct Period {
    /// The calendar year. Required.
    pub year: i32,
    /// The calendar month, `1`–`12`. Required for the Abgrenzungsoption, which
    /// settles per month; refused for the Pauschaloption, which settles per
    /// year.
    #[serde(default)]
    pub month: Option<u8>,
    /// Which **version** of the registers to settle from, RFC 3339.
    ///
    /// Omitted, the settlement reads the registers as they stand, which is what
    /// a first settlement of a period wants. Given, it reads them as they stood
    /// at that instant — which is how a Nachweis already handed over is
    /// reproduced after a register has been restated, and how the delta a
    /// correction produces is computed. The registers are versioned precisely so
    /// this question has an answer.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub as_of: Option<time::OffsetDateTime>,
}

async fn get_mispel(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    Query(period): Query<Period>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    // The **household's** document, on the same footing as the Data Act export
    // rather than the § 14a Nachweis — and the difference is who the reader is.
    // `[A1 7.2]` is the record of what a *network operator* commanded, so an
    // operator may read it. A MiSpeL settlement is the household's own levy
    // privilege and the flows behind it: how full their store was, when it was
    // charged from the grid, what their roof earned. The Festlegung has the
    // *Anlagenbetreiber* produce and submit it, so it is theirs to hand over —
    // and a § 14a operator credential that could read every household's
    // storage economics is a reach nothing granted it.
    deny_unless(state.authority(&headers)?.may_read_everything(&site))?;
    let Some(declared) = state.declaration(&site) else {
        // Refused rather than defaulted: every option produces a different
        // Nachweis from the same registers.
        return Err(StatusCode::NOT_FOUND);
    };
    let outcome = crate::export::mispel(
        &state.store,
        &site,
        Some(declared),
        period.year,
        period.month,
        period.as_of,
    )
    .await;
    match outcome {
        Ok(value) => Ok(axum::Json(value)),
        // The caller asked wrongly — a window the declared option does not
        // settle over, a month that is not one, or a period earlier than the
        // Festlegung that would settle it. The last is a `400` rather than a
        // `404` on purpose: the site exists and its option is declared, and what
        // is wrong is the *period* the caller named.
        Err(
            crate::export::MispelExportError::WrongWindow(_)
            | crate::export::MispelExportError::NotACalendarMonth { .. }
            | crate::export::MispelExportError::BeforeTheRules { .. },
        ) => Err(StatusCode::BAD_REQUEST),
        Err(crate::export::MispelExportError::Undeclared { .. }) => Err(StatusCode::NOT_FOUND),
        // The request was well formed and the **registers** cannot support a
        // settlement — a negative quantity, case A4 without its storage meter,
        // a share whose denominator is zero. That is a fact about the record
        // rather than about the question, which is what `422` is for.
        Err(crate::export::MispelExportError::Arithmetic(e)) => {
            tracing::warn!(error = %e, "a MiSpeL settlement was refused");
            Err(StatusCode::UNPROCESSABLE_ENTITY)
        }
        Err(crate::export::MispelExportError::Store(e)) => {
            tracing::error!(error = %e, "a query failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `403` where a credential is real and does not reach this site.
///
/// Separate from the `401` of [`History::authority`], because "you are nobody"
/// and "you are somebody else" are different facts and an operator debugging a
/// rollout needs to tell them apart.
fn deny_unless(allowed: bool) -> Result<(), StatusCode> {
    if allowed {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}
