//! What a box reports and what an operator asks.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hems_core::report::DayKpis;
use hems_events::webhook::{self, WebhookError};
use hems_service::auth::{Credentials, bearer};
use tokio::sync::RwLock;

use crate::fleet::Fleet;

/// What the API reads and writes.
#[derive(Clone)]
pub struct Observed {
    fleet: Arc<RwLock<Fleet>>,
    silent_after: time::Duration,
    secrets: Arc<Vec<String>>,
    tolerance: time::Duration,
    readers: Arc<Credentials>,
}

impl Observed {
    /// A handle onto the shared fleet.
    ///
    /// `secrets` are the shared secrets a box's report may be signed with. An
    /// **empty** list is a service that accepts no report at all, which is the
    /// safe reading of "nobody configured this": see
    /// [`crate::Settings::webhook_secrets`].
    #[must_use]
    pub fn new(
        fleet: Arc<RwLock<Fleet>>,
        silent_after: time::Duration,
        secrets: Vec<String>,
        tolerance: time::Duration,
        readers: Credentials,
    ) -> Self {
        Self {
            fleet,
            silent_after,
            secrets: Arc::new(secrets),
            tolerance,
            readers: Arc::new(readers),
        }
    }

    /// Whether this request may read `site` — or, for `None`, the whole fleet.
    ///
    /// Writing is authenticated by a signature over the body and reading
    /// by a bearer token, because they are different callers: a box has a secret
    /// it signs with, and a person or an internal service has a credential it
    /// presents.
    fn may_read(&self, headers: &HeaderMap, site: Option<&str>) -> Result<(), StatusCode> {
        let authority = bearer(
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
        )
        .and_then(|token| self.readers.authority_of(token))
        .ok_or(StatusCode::UNAUTHORIZED)?;
        if site.is_none_or(|s| authority.may_read(s)) {
            Ok(())
        } else {
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// The routes.
pub fn router(observed: Observed) -> Router {
    Router::new()
        .route("/v1/days", post(report))
        .route("/v1/fleet", get(summary))
        .route("/v1/sites/{site}", get(site))
        .with_state(observed)
}

/// A box reporting one day, as a signed CloudEvent.
///
/// Idempotent by date: a reconnecting box re-sending yesterday is correcting
/// itself, not adding a day.
///
/// The body is taken as **bytes**. Deserialising a `DayKpis` first and
/// re-serialising it to check would verify a document this service produced
/// rather than the one the box sent, and the two agree until a field order or a
/// `serde` version does not.
///
/// Every refusal is one `401`: which of "no signature", "too old" and "wrong
/// signature" it was goes to the log, where an operator needs it and a prober
/// is not.
async fn report(
    State(state): State<Observed>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, StatusCode> {
    let now = time::OffsetDateTime::now_utc();
    verify(&state, &headers, &body, now).map_err(|e| {
        tracing::warn!(error = %e, "a report was refused");
        StatusCode::UNAUTHORIZED
    })?;

    let event = hems_events::Event::<DayKpis>::parse(&body, hems_events::SITE_DAY_REPORTED)
        .map_err(|e| {
            tracing::warn!(error = %e, "a signed report was not a day report");
            StatusCode::BAD_REQUEST
        })?;

    let day = event.data;
    let attention = day.needs_attention();
    let site = day.site.clone();
    state.fleet.write().await.record(day, now);
    if attention {
        // At `warn`, once, at the moment it arrives. A finding that is only
        // visible by asking the summary is a finding nobody sees until they ask.
        tracing::warn!(site, "a reported day needs attention");
    }
    Ok(StatusCode::ACCEPTED)
}

/// Whether the three Standard Webhooks headers say this body came from a box.
fn verify(
    state: &Observed,
    headers: &HeaderMap,
    body: &[u8],
    now: time::OffsetDateTime,
) -> Result<(), WebhookError> {
    let header = |name: &'static str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or(WebhookError::MissingHeader(name))
    };
    webhook::verify(
        &state.secrets,
        header(webhook::ID_HEADER)?,
        header(webhook::TIMESTAMP_HEADER)?,
        header(webhook::SIGNATURE_HEADER)?,
        body,
        now,
        state.tolerance,
    )
}

async fn summary(
    State(state): State<Observed>,
    headers: HeaderMap,
) -> Result<axum::Json<crate::Summary>, StatusCode> {
    // The whole fleet, so only a credential that reaches the whole fleet.
    state.may_read(&headers, None)?;
    let fleet = state.fleet.read().await;
    Ok(axum::Json(fleet.summarise(
        time::OffsetDateTime::now_utc(),
        state.silent_after,
    )))
}

async fn site(
    State(state): State<Observed>,
    Path(site): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Vec<DayKpis>>, StatusCode> {
    state.may_read(&headers, Some(&site))?;
    let fleet = state.fleet.read().await;
    let history = fleet.site(&site).ok_or(StatusCode::NOT_FOUND)?;
    Ok(axum::Json(history.days().cloned().collect()))
}
