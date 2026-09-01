//! What a box, a household and a network operator can ask `histd`.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hems_grid::mispel::QuarterHour;
use hems_service::auth::{Authority, Credentials, bearer};
use time::OffsetDateTime;

use crate::{Db, Store, StoreError};

/// What the API writes to and reads from.
///
/// # Every query runs off the runtime, and reads do not queue behind writes
///
/// `rusqlite` is synchronous, so a call inside an `async` handler occupies a
/// runtime worker for as long as it takes. A household's Data Act export is the
/// two years of `[A1 7.3]` — about 11 MB of JSON and 370 ms — and there are only
/// as many workers as cores, so a few concurrent exports stall *every* request
/// including `/livez` and `/readyz`. Everything therefore goes through
/// [`tokio::task::spawn_blocking`].
///
/// Off the runtime, a single connection still serialises them: eight exports put
/// a box's evidence write 2,7 s behind. SQLite in WAL mode allows many readers
/// and one writer, so a read opens its own connection through [`Db`] and only
/// writes share one.
#[derive(Clone)]
pub struct History {
    db: Db,
    writer: Arc<std::sync::Mutex<Store>>,
    credentials: Arc<Credentials>,
}

impl History {
    /// A handle onto the store, and the credentials it answers to.
    ///
    /// An **empty** [`Credentials`] is a service that answers nothing. What
    /// these routes serve is a household's whole consumption record and the
    /// evidence a network operator settles on, so "nobody configured it" has to
    /// read as "nobody may".
    #[must_use]
    pub fn new(db: Db, writer: Arc<std::sync::Mutex<Store>>, credentials: Credentials) -> Self {
        Self {
            db,
            writer,
            credentials: Arc::new(credentials),
        }
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

    /// Run one **read** off the runtime, on a connection of its own.
    async fn read<T, F>(&self, work: F) -> Result<T, StatusCode>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    {
        let db = self.db.clone();
        Self::off_the_runtime(move || work(&db.connect()?)).await
    }

    /// Run one **write** off the runtime, on the connection that owns the write
    /// lock.
    ///
    /// A poisoned mutex is a previous write that panicked while holding it.
    /// `into_inner` takes the store anyway: the panic was in *this* code rather
    /// than in SQLite, a transaction is either committed or not, and refusing
    /// every write afterwards would turn one bad query into an outage of the
    /// § 14a record.
    async fn write<T, F>(&self, work: F) -> Result<T, StatusCode>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    {
        let writer = Arc::clone(&self.writer);
        Self::off_the_runtime(move || {
            let mut guard = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            work(&mut guard)
        })
        .await
    }

    async fn off_the_runtime<T, F>(work: F) -> Result<T, StatusCode>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|e| {
                tracing::error!(error = %e, "a query failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })
    }
}

/// The routes.
pub fn router(history: History) -> Router {
    Router::new()
        .route("/v1/sites/{site}/quarter-hours", post(put_quarter_hours))
        .route("/v1/sites/{site}/quarter-hours", get(get_quarter_hours))
        .route("/v1/sites/{site}/events", post(put_event))
        .route("/v1/sites/{site}/nachweis", get(get_nachweis))
        .route("/v1/sites/{site}/export", get(get_export))
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
    state
        .write(move |store| store.put_quarter_hours(&site, &quarters, now))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_quarter_hours(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    Query(window): Query<Window>,
) -> Result<axum::Json<Vec<QuarterHour>>, StatusCode> {
    deny_unless(state.authority(&headers)?.may_read(&site))?;
    state
        .read(move |store| store.quarter_hours(&site, window.from, window.to))
        .await
        .map(axum::Json)
}

async fn put_event(
    State(state): State<History>,
    Path(site): Path<String>,
    headers: HeaderMap,
    axum::Json(event): axum::Json<hems_grid::evidence::ControlEvent>,
) -> Result<StatusCode, StatusCode> {
    deny_unless(state.authority(&headers)?.may_write(&site))?;
    state
        .write(move |store| store.put_control_event(&site, &event))
        .await
        .map(|_| StatusCode::CREATED)
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
    state
        .read(move |store| crate::export::nachweis(store, &site, window.from, window.to))
        .await
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
    state
        .read(move |store| crate::export::data_act(store, &site))
        .await
        .map(axum::Json)
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
