//! Stopping on purpose.
//!
//! A daemon that is killed loses whatever it was in the middle of. For most of
//! this fleet that is a request; for `histd` it is a write to the two-year
//! § 14a evidence record `[A1 7.3]`, and losing one of those is losing the
//! evidence that the household obeyed a network operator.
//!
//! So every daemon waits for the same two signals — `SIGTERM`, which is what an
//! orchestrator sends, and `SIGINT`, which is what a person sends — hands the
//! server a future that resolves on either, and gives the in-flight work a
//! bounded grace period to finish.
//!
//! # Why the grace period is bounded
//!
//! An unbounded one is not a graceful shutdown, it is a hang: the orchestrator's
//! own patience runs out and sends `SIGKILL`, which is exactly the outcome the
//! graceful path existed to avoid, arrived at slowly.

use std::future::Future;

use tokio::sync::watch;

/// A shutdown signal every task can wait on.
///
/// Cheap to clone; each clone observes the same signal.
#[derive(Debug, Clone)]
pub struct Shutdown {
    rx: watch::Receiver<bool>,
}

impl Shutdown {
    /// A signal that fires when the process is asked to stop, and the handle
    /// that fires it.
    ///
    /// The handle is returned so a test — or a daemon that decides to stop
    /// itself — can trigger the same path the signal handler does. A shutdown
    /// that can only be reached by sending a real signal is a shutdown no test
    /// covers.
    #[must_use]
    pub fn channel() -> (Self, ShutdownTrigger) {
        let (tx, rx) = watch::channel(false);
        (Self { rx }, ShutdownTrigger { tx })
    }

    /// Wait until the process is asked to stop.
    pub async fn wait(mut self) {
        // Already asked, before this task ever looked.
        if *self.rx.borrow_and_update() {
            return;
        }
        // The sender is held by the process; if it is dropped the process is
        // going away, which is also a reason to stop.
        let _ = self.rx.changed().await;
    }

    /// Whether the signal has already fired.
    #[must_use]
    pub fn is_triggered(&self) -> bool {
        *self.rx.borrow()
    }
}

/// The handle that fires a [`Shutdown`].
#[derive(Debug, Clone)]
pub struct ShutdownTrigger {
    tx: watch::Sender<bool>,
}

impl ShutdownTrigger {
    /// Ask everything holding the matching [`Shutdown`] to stop.
    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }
}

/// Wait for `SIGTERM` or `SIGINT` and fire `trigger`.
///
/// Spawned by [`crate::Server`]; a daemon that builds its own server loop can
/// spawn it too. On a platform with no Unix signals only `Ctrl-C` is waited on,
/// which is the whole of what that platform has.
pub async fn on_signal(trigger: ShutdownTrigger) {
    signal().await;
    tracing::info!("shutdown signal received");
    trigger.trigger();
}

#[cfg(unix)]
async fn signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot listen for SIGTERM");
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn signal() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Run `work` until it finishes or `shutdown` fires, whichever comes first.
///
/// Returns `Some` with the work's own result, or `None` where the shutdown won.
pub async fn until<F: Future>(shutdown: Shutdown, work: F) -> Option<F::Output> {
    tokio::select! {
        out = work => Some(out),
        () = shutdown.wait() => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_trigger_releases_everything_waiting_on_it() {
        let (shutdown, trigger) = Shutdown::channel();
        let a = shutdown.clone();
        let b = shutdown.clone();
        trigger.trigger();
        a.wait().await;
        b.wait().await;
        assert!(shutdown.is_triggered());
    }

    #[tokio::test]
    async fn a_trigger_that_has_already_fired_is_still_observed() {
        // A task spawned *after* the signal must not wait for a second one.
        let (shutdown, trigger) = Shutdown::channel();
        trigger.trigger();
        let late = shutdown.clone();
        tokio::time::timeout(std::time::Duration::from_millis(100), late.wait())
            .await
            .expect("a late waiter should return immediately");
    }

    #[tokio::test]
    async fn work_that_finishes_first_keeps_its_answer() {
        let (shutdown, _trigger) = Shutdown::channel();
        let out = until(shutdown, async { 42 }).await;
        assert_eq!(out, Some(42));
    }

    #[tokio::test]
    async fn work_that_is_still_running_is_abandoned() {
        let (shutdown, trigger) = Shutdown::channel();
        trigger.trigger();
        let out = until(shutdown, std::future::pending::<u8>()).await;
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn dropping_the_trigger_also_stops_the_waiters() {
        // The process is going away; a task blocked for ever on a signal that
        // can no longer arrive is a process that will not exit.
        let (shutdown, trigger) = Shutdown::channel();
        drop(trigger);
        tokio::time::timeout(std::time::Duration::from_millis(100), shutdown.wait())
            .await
            .expect("a dropped trigger should release the waiters");
    }
}
