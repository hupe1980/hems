//! The shell every hems daemon shares.
//!
//! Six daemons is six copies of the same forty lines: read a configuration file,
//! let the environment override it, start structured logging, bind a socket,
//! answer a health probe, and stop when the orchestrator says so. Written six
//! times, those forty lines diverge — and they diverge in the direction that
//! costs most, because the one that is wrong is the one whose readiness probe
//! lies.
//!
//! # What it deliberately is not
//!
//! It is **not** `mako-service`. Extracting that was considered and rejected
//! its OIDC layer carries a `mako_roles` claim and a `Sparte` grant, and
//! its Cedar schema is built on market roles a household energy manager does not
//! have. What was left after removing them was five domain-free modules, and
//! copying five domain-free modules is cheaper than maintaining a diff guard
//! against a fork that is *supposed* to diverge.
//!
//! So this is small on purpose. It owns configuration, logging, the health
//! surface and the shutdown, and it owns nothing about energy. A daemon that
//! needs an HTTP client, a database or a scheduler brings its own.
//!
//! # Sans-I/O ends here
//!
//! Every domain crate in this workspace takes time as a parameter and opens no
//! socket. This crate is where that stops being true, and it is the
//! only shared place it does: `hems-core`, `hems-grid`, `hems-optimizer` and the
//! rest stay testable in a millisecond because the clock and the socket live
//! here instead.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]

pub mod auth;
pub mod config;
pub mod health;
pub mod mcp;
pub mod serve;
pub mod shutdown;
pub mod telemetry;
pub mod update;

pub use auth::{Authority, Capabilities, Credentials, OperatorCredential, SiteScope};
pub use config::{ConfigError, Secret, Settings, load, load_from};
pub use health::{Health, Probe, Readiness};
pub use mcp::{McpAuth, McpSettings};
pub use serve::{Server, ServerError};
pub use shutdown::Shutdown;
pub use telemetry::init_tracing;
pub use update::{Manifest, Release, SignedConfig, UpdateError};

/// The name and version of a daemon, as it appears in its logs and on its own
/// health endpoint.
///
/// Taken from the calling crate's `Cargo.toml` rather than typed out, because a
/// version string that has to be remembered is a version string that is wrong
/// after the first release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The daemon's name, `env!("CARGO_PKG_NAME")`.
    pub name: &'static str,
    /// Its version, `env!("CARGO_PKG_VERSION")`.
    pub version: &'static str,
}

impl Identity {
    /// The identity of the crate this is called from.
    ///
    /// ```
    /// let me = hems_service::identity!();
    /// assert_eq!(me.name, "hems-service");
    /// ```
    #[must_use]
    pub const fn new(name: &'static str, version: &'static str) -> Self {
        Self { name, version }
    }
}

/// The [`Identity`] of the crate this macro is expanded in.
#[macro_export]
macro_rules! identity {
    () => {
        $crate::Identity::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
    };
}
