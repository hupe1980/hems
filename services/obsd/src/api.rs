//! What a box reports and what an operator asks.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hems_core::report::DayKpis;
use hems_events::webhook::{self, WebhookError};
use hems_service::auth::Credentials;
use tokio::sync::RwLock;

use crate::fleet::Fleet;

/// What the API reads and writes.
#[derive(Clone)]
pub struct Observed {
    fleet: Arc<RwLock<Fleet>>,
    silent_after: time::Duration,
    /// Which household each accepted signing key belongs to.
    secrets: Arc<Vec<(String, String)>>,
    tolerance: time::Duration,
    readers: Arc<Credentials>,
}

impl Observed {
    /// A handle onto the shared fleet.
    ///
    /// `secrets` pairs each signing key with the household that holds it. One
    /// key per box rather than one for the fleet: a signature over a shared
    /// secret authenticates the bytes and says nothing about the sender, so any
    /// holder could attribute a day to any site (D114).
    ///
    /// An **empty** list is a service that accepts no report at all, which is
    /// the safe reading of "nobody configured this": see
    /// [`crate::Settings::webhook_secrets`].
    #[must_use]
    pub fn new(
        fleet: Arc<RwLock<Fleet>>,
        silent_after: time::Duration,
        secrets: Vec<(String, String)>,
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

    /// The authority behind a request, whoever it is.
    ///
    /// Writing is authenticated by a signature over the body and reading by a
    /// bearer token, because they are different callers: a box has a secret it
    /// signs with, and a person or an internal service has a credential it
    /// presents.
    fn authority(&self, headers: &HeaderMap) -> Result<hems_service::Authority, StatusCode> {
        self.readers
            .authority_in(
                headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
            )
            .ok_or(StatusCode::UNAUTHORIZED)
    }

    /// Whether this request may read one household's days.
    fn may_read(&self, headers: &HeaderMap, site: &str) -> Result<(), StatusCode> {
        if self.authority(headers)?.may_read(site) {
            Ok(())
        } else {
            Err(StatusCode::FORBIDDEN)
        }
    }

    /// The caller, if it may ask about every household in its scope.
    ///
    /// Separate from [`Observed::may_read`] because it is a different question,
    /// and asking it as `may_read(None)` is how it came to be answered wrongly:
    /// `Option::is_none_or` is `true` for `None`, so **every** valid credential
    /// — including one household's own box token — read a summary naming every
    /// household that failed to respect a network operator's reduction (D112).
    fn may_read_the_fleet(
        &self,
        headers: &HeaderMap,
    ) -> Result<hems_service::Authority, StatusCode> {
        let authority = self.authority(headers)?;
        if authority.may_read_the_fleet() {
            Ok(authority)
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
    let signer = verify(&state, &headers, &body, now)
        .map_err(|e| {
            tracing::warn!(error = %e, "a report was refused");
            StatusCode::UNAUTHORIZED
        })?
        .to_owned();

    let event = hems_events::Event::<DayKpis>::parse(&body, hems_events::SITE_DAY_REPORTED)
        .map_err(|e| {
            tracing::warn!(error = %e, "a signed report was not a day report");
            StatusCode::BAD_REQUEST
        })?;

    let day = event.data;
    // The key says who sent it; the payload says which household it is about.
    // They have to agree, or a box that holds a key can write a § 14a breach
    // onto a household that is not its own (D114). Refused as a `401` like every
    // other credential failure, because from outside it is one.
    if day.site != signer {
        tracing::warn!(
            signer,
            claimed = day.site,
            "a box signed a day for a household that is not its own"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }
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

/// Which household the three Standard Webhooks headers say this body came from.
///
/// The **site**, not merely "somebody with a key": each box signs with a key of
/// its own, so the index `verify` reports is an identity. A receiver that only
/// asked "did this verify" and then believed the site in the payload would let
/// any box write a day for any household (D114).
fn verify<'s>(
    state: &'s Observed,
    headers: &HeaderMap,
    body: &[u8],
    now: time::OffsetDateTime,
) -> Result<&'s str, WebhookError> {
    let header = |name: &'static str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or(WebhookError::MissingHeader(name))
    };
    let keys: Vec<&str> = state.secrets.iter().map(|(_, key)| key.as_str()).collect();
    let which = webhook::verify(
        &keys,
        header(webhook::ID_HEADER)?,
        header(webhook::TIMESTAMP_HEADER)?,
        header(webhook::SIGNATURE_HEADER)?,
        body,
        now,
        state.tolerance,
    )?;
    Ok(&state.secrets[which].0)
}

async fn summary(
    State(state): State<Observed>,
    headers: HeaderMap,
) -> Result<axum::Json<crate::Summary>, StatusCode> {
    // The whole fleet, so only a credential that reaches a whole fleet — and
    // only the households in *its* scope.
    let caller = state.may_read_the_fleet(&headers)?;
    let fleet = state.fleet.read().await;
    Ok(axum::Json(fleet.summarise_within(
        caller.sites(),
        time::OffsetDateTime::now_utc(),
        state.silent_after,
    )))
}

async fn site(
    State(state): State<Observed>,
    Path(site): Path<String>,
    headers: HeaderMap,
) -> Result<axum::Json<Vec<DayKpis>>, StatusCode> {
    state.may_read(&headers, &site)?;
    let fleet = state.fleet.read().await;
    let history = fleet.site(&site).ok_or(StatusCode::NOT_FOUND)?;
    Ok(axum::Json(history.days().cloned().collect()))
}
