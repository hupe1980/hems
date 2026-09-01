//! Structured logging, set up once.
//!
//! Two formats and the same events in both: a human one for a terminal and a
//! JSON one for a fleet. The choice is configuration rather than a build
//! feature, because the same binary runs in both places and a `cargo` feature
//! that decides how a daemon logs is a feature that has to be right at build
//! time for a decision made at deploy time.

use tracing_subscriber::EnvFilter;

/// Start logging, and say what the process is.
///
/// Idempotent by construction: the second call fails to install a global
/// subscriber and is ignored rather than panicking, so a test that starts two
/// servers does not have to care.
pub fn init_tracing(identity: crate::Identity, filter: &str, json: bool) {
    let env = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let installed = if json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env)
            .with_current_span(true)
            .try_init()
            .is_ok()
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env)
            .try_init()
            .is_ok()
    };

    if installed {
        tracing::info!(name = identity.name, version = identity.version, "starting");
    }
}
