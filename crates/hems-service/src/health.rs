//! Liveness and readiness, which are two different questions.
//!
//! An orchestrator asks both and does opposite things with the answers. **Live**
//! means "this process is not wedged" and a `false` gets it killed. **Ready**
//! means "this process can serve traffic" and a `false` takes it out of
//! rotation and leaves it alone.
//!
//! Answering the second with the first is the mistake that makes a fleet
//! oscillate: a daemon whose upstream price source is down is not *broken*, and
//! restarting it does not bring the price source back. It is not ready, it says
//! so, and it stays up long enough to notice when the source returns.
//!
//! # A probe is a fact with a timestamp
//!
//! "The tariff feed is stale" is only useful with "since when". Every probe
//! carries the instant it last succeeded, so a readiness page is a diagnosis
//! rather than a light.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use time::OffsetDateTime;

/// What one dependency of a daemon is doing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Probe {
    /// Whether the daemon can currently do its job through this dependency.
    pub ready: bool,
    /// What is wrong, where something is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When this dependency was last known good.
    ///
    /// `None` means it has never been good, which is a different state from
    /// "good a long time ago" and reads differently on a page.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_good: Option<OffsetDateTime>,
}

impl Probe {
    /// A dependency that is working, as of `at`.
    #[must_use]
    pub const fn good(at: OffsetDateTime) -> Self {
        Self {
            ready: true,
            detail: None,
            last_good: Some(at),
        }
    }

    /// A dependency that is not, with what is wrong.
    #[must_use]
    pub fn bad(detail: impl Into<String>) -> Self {
        Self {
            ready: false,
            detail: Some(detail.into()),
            last_good: None,
        }
    }

    /// The same, keeping the instant it was last good.
    #[must_use]
    pub fn degraded(detail: impl Into<String>, last_good: Option<OffsetDateTime>) -> Self {
        Self {
            ready: false,
            detail: Some(detail.into()),
            last_good,
        }
    }
}

/// The whole readiness picture, as it is served.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Readiness {
    /// Whether every probe that must pass is passing.
    pub ready: bool,
    /// One entry per named dependency.
    pub probes: BTreeMap<String, Probe>,
}

/// A daemon's own health, shared between the tasks that update it and the
/// handler that serves it.
///
/// Cheap to clone: the state is behind an `Arc`, so every task holds the same
/// one and a probe set from a background task is visible to the next request.
#[derive(Debug, Clone, Default)]
pub struct Health {
    probes: Arc<RwLock<BTreeMap<String, Probe>>>,
    live: Arc<RwLock<bool>>,
}

impl Health {
    /// A daemon that is alive and has no dependencies yet.
    ///
    /// It starts **ready**, which is deliberate: a daemon with nothing to check
    /// is ready, and a daemon that has not yet checked anything reports the
    /// probes it has. Starting not-ready and waiting for a first success is the
    /// caller's decision, made by registering a probe before serving.
    #[must_use]
    pub fn new() -> Self {
        Self {
            probes: Arc::new(RwLock::new(BTreeMap::new())),
            live: Arc::new(RwLock::new(true)),
        }
    }

    /// Record what one dependency is doing.
    pub fn set(&self, name: &str, probe: Probe) {
        if let Ok(mut probes) = self.probes.write() {
            probes.insert(name.to_owned(), probe);
        }
    }

    /// Record that a dependency is working, as of `at`, keeping its name.
    pub fn good(&self, name: &str, at: OffsetDateTime) {
        self.set(name, Probe::good(at));
    }

    /// Record that a dependency is not working, keeping the instant it last was.
    pub fn bad(&self, name: &str, detail: impl Into<String>) {
        let last_good = self
            .probes
            .read()
            .ok()
            .and_then(|p| p.get(name).and_then(|probe| probe.last_good));
        self.set(name, Probe::degraded(detail, last_good));
    }

    /// Whether the process itself is still working.
    ///
    /// The only thing that should ever set this to `false` is a fault a restart
    /// would clear — a poisoned lock, a background task that has died and cannot
    /// be respawned. An upstream being down is **not** one of those.
    pub fn set_live(&self, live: bool) {
        if let Ok(mut flag) = self.live.write() {
            *flag = live;
        }
    }

    /// Whether the process is alive.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.live.read().is_ok_and(|live| *live)
    }

    /// The readiness picture right now.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        let probes = self.probes.read().map(|p| p.clone()).unwrap_or_default();
        Readiness {
            ready: probes.values().all(|p| p.ready),
            probes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    #[test]
    fn a_daemon_with_nothing_to_check_is_ready() {
        let health = Health::new();
        assert!(health.readiness().ready);
        assert!(health.is_live());
    }

    #[test]
    fn one_bad_dependency_is_not_ready_and_the_process_is_still_live() {
        // The distinction the module exists for. A price source being down is
        // not a reason to restart the process, and an orchestrator told
        // otherwise will restart it every thirty seconds for as long as the
        // outage lasts.
        let health = Health::new();
        health.good("prices", NOW);
        health.good("store", NOW);
        assert!(health.readiness().ready);

        health.bad("prices", "ENTSO-E returned 503");
        assert!(!health.readiness().ready);
        assert!(
            health.is_live(),
            "an upstream outage is not a process fault"
        );
    }

    #[test]
    fn a_dependency_that_goes_bad_keeps_the_instant_it_was_last_good() {
        // "The tariff feed is stale" is only useful with "since when".
        let health = Health::new();
        health.good("prices", NOW);
        health.bad("prices", "connection refused");
        let readiness = health.readiness();
        let probe = &readiness.probes["prices"];
        assert_eq!(probe.last_good, Some(NOW));
        assert_eq!(probe.detail.as_deref(), Some("connection refused"));
    }

    #[test]
    fn a_dependency_that_has_never_worked_says_so_differently() {
        let health = Health::new();
        health.bad("prices", "never reached");
        assert_eq!(health.readiness().probes["prices"].last_good, None);
    }

    #[test]
    fn recovery_clears_the_detail() {
        let health = Health::new();
        health.bad("prices", "connection refused");
        health.good("prices", NOW);
        let readiness = health.readiness();
        assert!(readiness.ready);
        assert_eq!(readiness.probes["prices"].detail, None);
    }
}
