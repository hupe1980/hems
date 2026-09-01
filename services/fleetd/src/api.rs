//! What a box asks the fleet.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use hems_service::{Release, SignedConfig};
use tokio::sync::RwLock;

use crate::registry::{EnrolmentError, Registry};

/// What the API reads and writes.
#[derive(Clone)]
pub struct Fleet {
    registry: Arc<RwLock<Registry>>,
    releases: Arc<BTreeMap<String, Release>>,
}

impl Fleet {
    /// A handle onto the registry and the releases on offer.
    #[must_use]
    pub fn new(registry: Arc<RwLock<Registry>>, releases: BTreeMap<String, Release>) -> Self {
        Self {
            registry,
            releases: Arc::new(releases),
        }
    }
}

/// The routes.
pub fn router(fleet: Fleet) -> Router {
    Router::new()
        .route("/v1/enrol", post(enrol))
        .route("/v1/config", get(config))
        .route("/v1/config/running", post(report_running))
        .route("/v1/releases/{component}", get(release))
        .route("/v1/fleet", get(states))
        .with_state(fleet)
}

/// What a box sends to be adopted.
#[derive(Debug, serde::Deserialize)]
pub struct EnrolRequest {
    /// Which site it claims to be.
    pub site: String,
    /// The secret an installer put on it.
    pub secret: String,
}

async fn enrol(
    State(fleet): State<Fleet>,
    axum::Json(request): axum::Json<EnrolRequest>,
) -> Result<axum::Json<crate::Enrolled>, StatusCode> {
    let now = time::OffsetDateTime::now_utc();
    let mut registry = fleet.registry.write().await;
    match registry.enrol(&request.site, &request.secret, now, mint_token) {
        Ok(enrolled) => {
            tracing::info!(site = %enrolled.site, "enrolled");
            Ok(axum::Json(enrolled))
        }
        Err(e) => {
            // The *log* distinguishes them, because that is where an operator
            // needs the distinction. The response does not, because that is
            // where an attacker would use it to enumerate the fleet.
            tracing::warn!(site = %request.site, error = %e, "enrolment refused");
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// The configuration a box should be running, and the signature over it.
///
/// The box verifies it against the Ed25519 key it was **built** with, exactly as
/// it verifies a release — so this endpoint being authenticated is a convenience
/// and not the security property. See [`hems_service::SignedConfig`].
async fn config(
    State(fleet): State<Fleet>,
    headers: HeaderMap,
) -> Result<axum::Json<SignedConfig>, StatusCode> {
    let registry = fleet.registry.read().await;
    let site = authenticate(&registry, &headers)?.to_owned();
    let entry = registry
        .config_for(&site)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if entry.config_signature.trim().is_empty() {
        // An operator who has not signed a document has not published one. The
        // alternative — serve it unsigned and let the box decide — is a box that
        // has to have a "trust it anyway" path, and that path is the one an
        // attacker aims at.
        tracing::error!(
            site,
            "this site's configuration is unsigned and was not served"
        );
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    Ok(axum::Json(SignedConfig {
        version: entry.config_version.clone(),
        config: entry.config.clone(),
        signature: entry.config_signature.clone(),
        site,
    }))
}

/// What a box says it is running.
#[derive(Debug, serde::Deserialize)]
pub struct RunningReport {
    /// The configuration version it has taken.
    pub config_version: String,
}

async fn report_running(
    State(fleet): State<Fleet>,
    headers: HeaderMap,
    axum::Json(report): axum::Json<RunningReport>,
) -> Result<StatusCode, StatusCode> {
    let mut registry = fleet.registry.write().await;
    let site = authenticate(&registry, &headers)?.to_owned();
    registry.report(
        &site,
        &report.config_version,
        time::OffsetDateTime::now_utc(),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The signed release on offer for one component.
///
/// Served to an **enrolled** box only — not because the manifest is a secret
/// (it is signed, and its whole point is to be checkable by anyone) but because
/// a fleet that will describe its release inventory to an unauthenticated caller
/// is a fleet that has published which of its boxes are behind.
async fn release(
    State(fleet): State<Fleet>,
    headers: HeaderMap,
    Path(component): Path<String>,
) -> Result<axum::Json<Release>, StatusCode> {
    let registry = fleet.registry.read().await;
    authenticate(&registry, &headers)?;
    fleet
        .releases
        .get(&component)
        .cloned()
        .map(axum::Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn states(State(fleet): State<Fleet>) -> axum::Json<Vec<crate::registry::BoxState>> {
    axum::Json(fleet.registry.read().await.states())
}

/// The site a bearer token belongs to.
fn authenticate<'r>(registry: &'r Registry, headers: &HeaderMap) -> Result<&'r str, StatusCode> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    registry.site_for(token.trim()).map_err(|e| {
        debug_assert_eq!(e, EnrolmentError::UnknownToken);
        StatusCode::UNAUTHORIZED
    })
}

/// A credential a box presents from now on.
///
/// Two hundred and fifty-six bits from the operating system's own entropy —
/// rather than from the site name, the secret or the clock, any of which would
/// make the token computable from something an installer wrote down.
///
/// # Panics
/// If the operating system cannot produce randomness at all, which on every
/// platform this ships to means the process is already in a state where issuing
/// a credential would be worse than stopping.
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system has no entropy");
    hex::encode(bytes)
}
