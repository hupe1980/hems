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
//! | [`config`] | the journal's path, and which tenant the specialists read |
//! | [`skills`] | the specialists, whose work is computation |
//! | [`subscriptions`] | which `CloudEvent` type reaches which specialist |

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

use std::sync::Arc;

use agentplane::prelude::*;

pub mod advice;
pub mod config;
pub mod skills;
pub mod subscriptions;

pub use advice::{Advice, AtRisk, Proposal, advisory};
pub use config::Settings;
pub use subscriptions::{Subscription, specialists_for};

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
    use hems_core::report::DayKpis;
    use serde_json::json;

    #[tokio::test]
    async fn a_specialist_runs_and_the_run_replays_to_the_same_answer() {
        // The property that makes the journal worth having even for a pure
        // function: the replay re-executes the logic and reads every effect
        // back, so "why did the queue say that" is answered rather than argued.
        let store: Arc<dyn JournalStore> =
            Arc::new(RedbStore::open_in_memory().expect("an in-memory journal"));
        let runtime = runtime(Arc::clone(&store));

        let days: Vec<DayKpis> = (0..4)
            .map(|i| DayKpis {
                site: format!("haus-{i}"),
                date: time::macros::date!(2026 - 01 - 15),
                respected_the_grid: false,
                minutes_without_a_plan: 90,
                ..DayKpis::default()
            })
            .collect();

        let outcome = runtime
            .run(
                skills::compliance::NAME,
                Tainted::trusted(json!({ "days": days })),
            )
            .await
            .expect("the run completed");
        let run_id = outcome.run_id;
        let output = outcome.output.clone();

        let answer = outcome.success().expect("an answer");
        let proposal: Proposal = serde_json::from_value(answer.peek().clone()).expect("a proposal");
        assert_eq!(proposal.considered, 4);
        assert_eq!(proposal.advice.len(), 1);
        assert!(proposal.advice[0].headline.contains("4 of 4"));

        let replayed = runtime
            .replay(run_id, Mode::Strict)
            .await
            .expect("the replay completed");
        assert_eq!(replayed.output, output, "the same answer, re-derived");
    }

    #[tokio::test]
    async fn an_unreadable_input_fails_the_run_rather_than_the_daemon() {
        // A specialist reads whatever an event carried, and an event this build
        // cannot parse is a bad day rather than a crash: the queue loses one
        // finding, and the box it is about is unaffected either way.
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

    #[test]
    fn every_registered_specialist_is_subscribed_to_something() {
        // The mirror of the subscription table's own test. A specialist nothing
        // wakes is a specialist that never runs.
        for name in registered_specialists() {
            assert!(
                subscriptions::TABLE.iter().any(|s| s.specialist == name),
                "{name} is registered and nothing wakes it"
            );
        }
    }
}
