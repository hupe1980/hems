//! Binding a socket and letting go of it again.
//!
//! One function that every daemon's `main` ends in: take the router the daemon
//! built, add the three endpoints they all have, bind, serve until a signal, and
//! stop within the grace period.
//!
//! # The three endpoints
//!
//! | Path | Question | Who asks |
//! |---|---|---|
//! | `/livez` | is this process wedged? | the orchestrator, to decide whether to restart it |
//! | `/readyz` | can it serve traffic? | the orchestrator, to decide whether to route to it |
//! | `/version` | what is running here? | a person, and the fleet's own inventory |
//!
//! They are added *last*, so a daemon cannot accidentally shadow its own health
//! probe with a route of its own and discover it during an incident.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use thiserror::Error;

use crate::{Health, Identity, Settings, Shutdown, shutdown};

/// Why a server could not run.
#[derive(Debug, Error)]
pub enum ServerError {
    /// The socket could not be bound — usually already in use.
    #[error("cannot bind {address}: {source}")]
    Bind {
        /// The address that was asked for.
        address: SocketAddr,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The server stopped with an error rather than because it was asked to.
    #[error("the server stopped: {0}")]
    Serve(#[source] std::io::Error),
}

/// A daemon's HTTP surface.
pub struct Server {
    identity: Identity,
    settings: Settings,
    health: Health,
    router: Router,
}

impl Server {
    /// A server for `identity`, serving `router` on top of the shared
    /// endpoints.
    ///
    /// # Panics
    /// When `router` already defines `/livez`, `/readyz` or `/version`. The
    /// merge happens **here**, in the constructor, rather than at the moment the
    /// socket is bound — so a daemon that would shadow the orchestrator's own
    /// probe fails in `main` on the first line, loudly, instead of coming up
    /// healthy and answering the probe with something of its own. It is the one
    /// place in this crate a panic is the right answer: there is no runtime
    /// recovery from a daemon whose readiness endpoint is not its readiness.
    #[must_use]
    pub fn new(identity: Identity, settings: Settings, health: Health, router: Router) -> Self {
        Self {
            identity,
            settings,
            health: health.clone(),
            router: router.merge(shared_routes(identity, health)),
        }
    }

    /// The health this server reports, so background tasks can update it.
    #[must_use]
    pub fn health(&self) -> Health {
        self.health.clone()
    }

    /// Bind, serve until a signal, and return when everything in flight is done
    /// or the grace period has run out.
    ///
    /// # Errors
    /// [`ServerError::Bind`] when the address is unavailable, and
    /// [`ServerError::Serve`] when the server itself fails.
    pub async fn run(self) -> Result<(), ServerError> {
        let (signal, trigger) = Shutdown::channel();
        tokio::spawn(shutdown::on_signal(trigger));
        self.run_until(signal).await
    }

    /// The same, stopping on `signal` rather than on a process signal.
    ///
    /// This is what a test drives, and it is the same code path production
    /// takes — a shutdown reachable only by sending a real signal is a shutdown
    /// nothing covers.
    ///
    /// # Errors
    /// As [`Server::run`].
    pub async fn run_until(self, signal: Shutdown) -> Result<(), ServerError> {
        let Self {
            identity,
            settings,
            health: _,
            router: app,
        } = self;
        let _ = identity;

        let listener = tokio::net::TcpListener::bind(settings.listen)
            .await
            .map_err(|source| ServerError::Bind {
                address: settings.listen,
                source,
            })?;
        let bound = listener.local_addr().unwrap_or(settings.listen);
        tracing::info!(%bound, "listening");

        let grace = std::time::Duration::from_secs(settings.shutdown_grace_s);
        let served = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                signal.wait().await;
                tracing::info!(grace_s = settings.shutdown_grace_s, "draining");
            })
            .into_future();

        // The grace period is bounded on purpose: an unbounded one is not a
        // graceful shutdown, it is a hang that ends in the `SIGKILL` the
        // graceful path existed to avoid.
        let Ok(result) = tokio::time::timeout(grace, served).await else {
            tracing::warn!(
                grace_s = settings.shutdown_grace_s,
                "grace period expired with work still in flight"
            );
            return Ok(());
        };
        result.map_err(ServerError::Serve)
    }
}

/// The endpoints every daemon has.
fn shared_routes(identity: Identity, health: Health) -> Router {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .with_state(SharedState { identity, health })
}

#[derive(Clone)]
struct SharedState {
    identity: Identity,
    health: Health,
}

async fn livez(State(state): State<SharedState>) -> StatusCode {
    if state.health.is_live() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Readiness answers with the **whole picture**, not a bare code.
///
/// A `503` tells an orchestrator what to do and tells a person nothing. The body
/// names every dependency, whether it is passing and when it was last good, so
/// the first thing anybody does in an incident — open the readiness endpoint —
/// is also the last thing they need to do to know which upstream is down.
async fn readyz(State(state): State<SharedState>) -> (StatusCode, axum::Json<crate::Readiness>) {
    let readiness = state.health.readiness();
    let code = if readiness.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, axum::Json(readiness))
}

async fn version(State(state): State<SharedState>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "name": state.identity.name,
        "version": state.identity.version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Start a server on an ephemeral port and return the address and the
    /// trigger that stops it.
    async fn start(
        health: Health,
        router: Router,
    ) -> (SocketAddr, crate::shutdown::ShutdownTrigger) {
        let settings = Settings {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            shutdown_grace_s: 2,
            ..Settings::default()
        };
        // Bind first so the test knows the port, then hand the listener's own
        // address back through the settings.
        let listener = tokio::net::TcpListener::bind(settings.listen)
            .await
            .unwrap();
        let bound = listener.local_addr().unwrap();
        drop(listener);
        let settings = Settings {
            listen: bound,
            ..settings
        };
        let (signal, trigger) = Shutdown::channel();
        let server = Server::new(crate::identity!(), settings, health, router);
        tokio::spawn(async move {
            server.run_until(signal).await.unwrap();
        });
        // Wait for the port to answer rather than sleeping a guessed amount.
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(bound).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        (bound, trigger)
    }

    /// The smallest HTTP/1.1 client that can ask one question, so the crate
    /// needs no HTTP client dependency to test its own server.
    async fn http_get(address: SocketAddr, path: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        let status = raw
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        (status, body)
    }

    #[tokio::test]
    async fn the_three_shared_endpoints_answer() {
        let health = Health::new();
        let (address, trigger) = start(health, Router::new()).await;

        assert_eq!(http_get(address, "/livez").await.0, 200);
        assert_eq!(http_get(address, "/readyz").await.0, 200);
        let (code, body) = http_get(address, "/version").await;
        assert_eq!(code, 200);
        assert!(body.contains("hems-service"), "{body}");
        trigger.trigger();
    }

    #[tokio::test]
    async fn a_bad_dependency_is_a_503_that_names_it_and_the_process_stays_live() {
        // The whole point of separating the two probes: an upstream outage takes
        // the daemon out of rotation and does *not* get it restarted, and the
        // body says which upstream so the first click in an incident is also the
        // last.
        let health = Health::new();
        health.good("prices", datetime!(2026-06-21 12:00:00 UTC));
        let (address, trigger) = start(health.clone(), Router::new()).await;

        assert_eq!(http_get(address, "/readyz").await.0, 200);
        health.bad("prices", "ENTSO-E returned 503");

        let (code, body) = http_get(address, "/readyz").await;
        assert_eq!(code, 503);
        assert!(body.contains("ENTSO-E returned 503"), "{body}");
        assert_eq!(http_get(address, "/livez").await.0, 200, "still live");
        trigger.trigger();
    }

    #[test]
    #[should_panic(expected = "Overlapping method route")]
    fn a_daemon_that_would_shadow_its_own_health_probe_does_not_start() {
        // The alternative is a daemon that comes up healthy, answers the
        // orchestrator's readiness probe with something of its own, and is found
        // out during an incident. Failing in the constructor means failing in
        // `main` before a socket exists.
        let router = Router::new().route("/readyz", get(|| async { "mine" }));
        let _ = Server::new(
            crate::identity!(),
            Settings::default(),
            Health::new(),
            router,
        );
    }

    #[tokio::test]
    async fn the_server_stops_when_it_is_asked_to() {
        let (address, trigger) = start(Health::new(), Router::new()).await;
        assert_eq!(http_get(address, "/livez").await.0, 200);
        trigger.trigger();
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(address).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the server did not stop");
    }
}
