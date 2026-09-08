//! `agentd` — the advisory plane for hems.
//!
//! # What it is for
//!
//! Every crate below this one answers a question about **one** thing: is this
//! setpoint inside the guard's bound, does this quarter hour settle, was this
//! § 14a reduction respected. Those answers are exact, and they are the ones
//! that decide what a household draws.
//!
//! None of them answers the question an operator actually has, which is about a
//! **population**: of forty § 14a breaches this week, does one cause account for
//! most of them; of a fleet's days, how many stand behind the saving on the
//! dashboard. Those answers are correlations across many exact answers, and
//! nothing else in the workspace is positioned to make one.
//!
//! # Advisory only, and it is a property
//!
//! An agent **proposes**; the control planes decide. Two things make that
//! structural rather than a promise, and both are in [`advice`]:
//!
//! * the output type is a leaf — nothing in this workspace consumes an
//!   [`advice::Advice`], so there is no path from an agent's answer into a
//!   device;
//! * a specialist's authority is derived by
//!   [`hems_service::Authority::attenuate`], which refuses to widen, and
//!   [`advice::advisory`] is the only constructor — so no agent can hold a
//!   capability that writes a household's record, and none can take the Data Act
//!   export, which is a right of the *user*. Tests assert both.
//!
//! # The journal is why this is a runtime and not a cron job
//!
//! The specialists are pure functions. `agentplane` runs them anyway, because
//! what it provides is not inference: the run, its input, its answer and every
//! effect go into an append-only hash-chained log, and a replay re-executes the
//! logic while reading each effect back rather than performing it again. "Why
//! did the queue say that in March" becomes a replay instead of an argument —
//! and for a pure function the replay is exact.
//!
//! # Layout
//!
//! | Module | Purpose |
//! |---|---|
//! | [`advice`] | what a specialist may say, and the reason it cannot say anything else |
//! | [`api`] | the one route an operator reads, and why there is no second one |
//! | [`config`] | the journal, the fleet it reads, and how often |
//! | [`mcp_server`] | the same queue, for an agent rather than a person |
//! | [`review`] | the cadence, and what each specialist is run to answer |
//! | [`skills`] | the specialists, whose work is computation |
//! | [`upstream`] | where the days come from, and why it is `obsd`'s API rather than its database |

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

use std::sync::Arc;

use agentplane::prelude::*;

pub mod advice;
pub mod api;
pub mod config;
pub mod mcp_server;
pub mod review;
pub mod skills;
pub mod upstream;

pub use advice::{Advice, AtRisk, Proposal, advisory};
pub use api::{Advisory, router};
pub use config::Settings;
pub use review::{Queue, Reviewed, SPECIALISTS, Specialist, review_loop, review_once};
pub use upstream::{Obsd, Upstream, UpstreamError, Window};

/// Every specialist this daemon registers, by name.
///
/// One list, read by the runtime builder and by the subscription table's own
/// test — so a specialist that is subscribed and not wired is a build failure
/// rather than a row that dispatches into nothing.
#[must_use]
pub fn registered_specialists() -> Vec<&'static str> {
    vec![skills::compliance::NAME, skills::provenance::NAME]
}

/// Build the runtime with every specialist wired.
///
/// The store is the caller's: an embedded file for a single instance, or a
/// database several instances share. What the daemon owns is which specialists
/// exist, and that is [`registered_specialists`].
#[must_use]
pub fn runtime(store: Arc<dyn JournalStore>) -> Arc<Runtime> {
    Runtime::builder(store)
        .skill(skills::compliance::ComplianceTriage)
        .skill(skills::provenance::SavingProvenance)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn an_unreadable_input_fails_the_run_rather_than_the_daemon() {
        // A specialist reads whatever the review handed it, and a document this
        // build cannot parse is a bad day rather than a crash: the queue keeps
        // whatever it said last, and the households it is about are unaffected
        // either way.
        let store: Arc<dyn JournalStore> =
            Arc::new(RedbStore::open_in_memory().expect("an in-memory journal"));
        let outcome = runtime(store)
            .run(
                skills::provenance::NAME,
                Tainted::trusted(json!({ "days": "not a list of days" })),
            )
            .await
            .expect("the run completed");
        assert!(outcome.success().is_err(), "it failed, and it said why");
    }

    #[test]
    fn the_daemons_own_configuration_parses() {
        // The defaults describe a single-tenant deployment, which is the one a
        // `"*"` tenant is right for.
        let settings = Settings::default();
        assert_eq!(settings.tenant, hems_service::auth::EVERY_TENANT);
        // The shipped example, so one that has drifted from the struct it
        // documents fails the build rather than misleading an operator.
        let example: Settings =
            toml::from_str(include_str!("../agentd.example.toml")).expect("the example parses");
        assert_eq!(example.tenant, hems_service::auth::EVERY_TENANT);
        assert_eq!(example.service.listen.port(), 7880);

        // …and the shape a shared deployment writes.
        let shared: Settings = toml::from_str(
            r#"
            tenant = "stadtwerke-nord"

            [tenants]
            stadtwerke-nord = ["haus-1", "haus-2"]
            "#,
        )
        .expect("a tenant and its households");
        assert_eq!(shared.tenants["stadtwerke-nord"].len(), 2);
    }
}
