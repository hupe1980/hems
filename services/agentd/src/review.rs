//! The cadence, and what it leaves behind.
//!
//! # A review, not a subscription
//!
//! Both specialists answer a question about a **population over a window** —
//! which of a week's § 14a breaches share a cause, what a fleet's saving figure
//! rests on. One box reporting one day changes such an answer by one row, so a
//! per-event trigger would be ten thousand reviews a day on a fleet of ten
//! thousand households, each reading the whole window to reach very nearly the
//! previous answer. The trigger is a **cadence**, and what a run reads is stated
//! by [`SPECIALISTS`] (D170).
//!
//! # One read, every specialist
//!
//! The summary is fetched **once** per review and handed to each specialist as
//! its run input. Two properties, not optimisations: the specialists cannot
//! disagree about the fleet, and the input is in the journal — so *"why did the
//! queue say that in March"* is answered by reading the March run rather than by
//! asking `obsd` what it says now.
//!
//! It is labelled [`Tainted::from_source`] rather than trusted, because it
//! crossed a socket. Nothing here acts on it (D118), but a plane that recorded
//! fleet data as operator-vouched would be a plane whose labels mean nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentplane::core::{RunId, SourceId};
use agentplane::prelude::{Runtime, Tainted};
use hems_service::{Health, Shutdown};
use time::OffsetDateTime;
use tokio::sync::RwLock;

use crate::advice::Proposal;
use crate::upstream::{Upstream, UpstreamError, Window};

/// One specialist, and the question it is run to answer.
///
/// The table is what the review loop iterates, so a specialist listed here and
/// not registered with the runtime is a run that fails on its first tick rather
/// than a row that dispatches into nothing — and a test holds the two lists to
/// each other in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Specialist {
    /// The name its [`agentplane::prelude::SkillDescriptor`] states.
    pub name: &'static str,
    /// What it is run to find out, in one line, for an operator reading the
    /// queue rather than the source.
    pub question: &'static str,
}

/// Every specialist a review runs, in the order their findings are listed.
///
/// Deliberately small. A queue with something in it every morning is a queue
/// nobody reads, and the way that happens is a plane that grew a specialist for
/// every number somebody thought was interesting.
pub const SPECIALISTS: &[Specialist] = &[
    Specialist {
        name: crate::skills::compliance::NAME,
        question: "whether one cause accounts for most of the week's § 14a breaches, \
                   and whether a ceiling below the [A1 4.5] minimum reached many \
                   households on one day",
    },
    Specialist {
        name: crate::skills::provenance::NAME,
        question: "what the fleet's saving and forecast-coverage figures rest on — \
                   modelled days, days run with the weather known in advance, and \
                   whether there are enough independent days to call a band calibrated",
    },
];

/// What one specialist said, and about which window.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Reviewed {
    /// Which specialist.
    pub specialist: &'static str,
    /// The run in the journal that produced it, so a finding can be replayed.
    ///
    /// A [`RunId`] rather than the string it used to be. Nothing changes on the
    /// wire — `agentplane` 0.32 made `Serialize` write the same prefixed form
    /// `Display` already wrote (`run_01J8Z…`) — and two things change here: a
    /// replay parses nothing, and a run identifier that is not one cannot be
    /// constructed. This type's own test used to build `run: "r".into()`, which
    /// is not a run and which no reader would have caught (D177).
    pub run: RunId,
    /// When the review ran.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Which service answered.
    pub source: String,
    /// How many days of the fleet's record the finding is drawn from.
    ///
    /// The denominator, carried beside the findings rather than left to be
    /// inferred: *three breaches* means one thing over ninety days and another
    /// over three, and a queue that showed only the numerator would read the
    /// same either way.
    pub days: usize,
    /// How many households were in scope.
    pub sites: usize,
    /// The findings.
    pub proposal: Proposal,
}

/// The findings an operator reads, newest per specialist.
///
/// **A projection, not a record.** The record is the journal — every run, its
/// input, its answer, hash-chained — and this is the one answer per specialist
/// that a dashboard shows. Losing it on a restart costs a refresh; the next
/// review rebuilds it, and the runs it was built from are still there.
#[derive(Debug, Default)]
pub struct Queue {
    latest: RwLock<BTreeMap<&'static str, Reviewed>>,
}

impl Queue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything on record, in [`SPECIALISTS`] order.
    ///
    /// The table's order rather than the map's, because the map is keyed by name
    /// and a queue that reordered itself alphabetically would put the saving
    /// provenance above the § 14a findings — and § 14a is the one with a
    /// regulator behind it.
    pub async fn read(&self) -> Vec<Reviewed> {
        let latest = self.latest.read().await;
        SPECIALISTS
            .iter()
            .filter_map(|s| latest.get(s.name).cloned())
            .collect()
    }

    /// Take in one specialist's answer, replacing whatever it said before.
    pub async fn record(&self, reviewed: Reviewed) {
        self.latest
            .write()
            .await
            .insert(reviewed.specialist, reviewed);
    }

    /// Whether any review has landed yet.
    pub async fn is_empty(&self) -> bool {
        self.latest.read().await.is_empty()
    }
}

/// What the readiness surface calls the link to `obsd`.
pub const UPSTREAM: &str = "obsd";

/// The probe a daemon registers **before** it serves.
///
/// `Health::new` starts a daemon ready, which is right for one with nothing to
/// check and wrong for this one: an advisory queue that has never been filled
/// looks exactly like a fleet in good order, so a plane reported ready before
/// its first review would answer "nothing to report" about a fleet it has not
/// read. Registering the probe unready is how that becomes a state an
/// orchestrator can see (`Health::new`'s own note says this is the caller's
/// decision, made by registering a probe before serving).
#[must_use]
pub fn not_reviewed_yet() -> hems_service::Probe {
    hems_service::Probe::bad("no review has completed yet")
}

/// Run every specialist once over one window.
///
/// Returns what each said, in [`SPECIALISTS`] order. A specialist whose run
/// failed is **absent** rather than reported as having found nothing: an empty
/// queue means a compliant fleet, and a failure that rendered as one would be
/// the most expensive kind of wrong answer this daemon can give.
///
/// # Errors
/// [`UpstreamError`] where the window could not be read at all. Nothing is
/// recorded then, so the queue keeps the last answer it had — which is stale and
/// says which window it was about, rather than empty and saying nothing.
pub async fn review_once(
    runtime: &Runtime,
    upstream: &dyn Upstream,
    now: OffsetDateTime,
) -> Result<Vec<Reviewed>, UpstreamError> {
    let window = upstream.window().await?;
    Ok(run_specialists(runtime, &window, now).await)
}

/// Every specialist, over a window somebody already read.
///
/// Split out so a test can drive the whole plane — runtime, journal, ranking —
/// against a window it constructed, which is the level the interesting cases
/// live at.
pub async fn run_specialists(
    runtime: &Runtime,
    window: &Window,
    now: OffsetDateTime,
) -> Vec<Reviewed> {
    let Ok(value) = serde_json::to_value(window) else {
        // Unreachable for a `Window`, whose every field is a plain document —
        // and an error rather than an `unwrap` because the alternative to a
        // review is never a crash of the process that serves the last one.
        tracing::error!("a window could not be turned into a run input");
        return Vec::new();
    };
    // Untrusted: it came from `obsd` over a socket. See the module note.
    let input = Tainted::from_source(value, SourceId::new(format!("obsd:{}", window.source)));

    let mut out = Vec::with_capacity(SPECIALISTS.len());
    for specialist in SPECIALISTS {
        let outcome = match runtime.run(specialist.name, input.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(
                    specialist = specialist.name,
                    %error,
                    "a specialist could not be run"
                );
                continue;
            }
        };
        let run = outcome.run_id;
        let answer = match outcome.success() {
            Ok(answer) => answer,
            Err(failure) => {
                tracing::error!(
                    specialist = specialist.name,
                    %run,
                    %failure,
                    "a specialist failed; the queue keeps whatever it said last"
                );
                continue;
            }
        };
        match serde_json::from_value::<Proposal>(answer.peek().clone()) {
            Ok(proposal) => out.push(Reviewed {
                specialist: specialist.name,
                run,
                at: now,
                source: window.source.clone(),
                days: window.fleet.days,
                sites: window.fleet.sites,
                proposal,
            }),
            Err(error) => tracing::error!(
                specialist = specialist.name,
                %run,
                %error,
                "a specialist answered with something this build cannot read"
            ),
        }
    }
    out
}

/// The review loop: one round every `every`, until shutdown.
///
/// **Vital.** A plane whose review loop has died goes on serving whatever it
/// found last, dated, to an operator who has no way to tell — which is the
/// failure `Health::vital` exists for (D132, D146). A round that could not read
/// the window marks the upstream *degraded* with the instant it was last good,
/// so a readiness body says how old the answer is rather than merely that
/// something is wrong.
pub async fn review_loop(
    runtime: Arc<Runtime>,
    upstream: Arc<dyn Upstream>,
    queue: Arc<Queue>,
    health: Health,
    every: std::time::Duration,
    signal: Shutdown,
) {
    loop {
        let now = OffsetDateTime::now_utc();
        match review_once(&runtime, upstream.as_ref(), now).await {
            Ok(reviewed) => {
                let findings: usize = reviewed.iter().map(|r| r.proposal.advice.len()).sum();
                for one in reviewed {
                    queue.record(one).await;
                }
                health.good(UPSTREAM, now);
                tracing::info!(findings, "a review completed");
            }
            Err(error) => {
                // **Not ready**, which is the same answer `forecastd` gives when
                // its fetch fails and for the same reason: what this daemon then
                // serves is a queue about a window it can no longer confirm, and
                // an advisory queue read as current when it is a day old is
                // worse than one that is visibly absent. `Health::bad` keeps the
                // instant it was last good, so the readiness body says *how*
                // stale rather than only that something is wrong.
                health.bad(UPSTREAM, error.to_string());
                if error.is_worth_retrying() {
                    tracing::warn!(%error, "a review could not read the fleet; retrying next round");
                } else {
                    // A credential this deployment has to fix. Said at `error`
                    // and said every round, because a refusal that scrolled past
                    // once is a plane that quietly stops reviewing.
                    tracing::error!(%error, "a review was refused; this will not fix itself");
                }
            }
        }

        let waiting = signal.clone();
        tokio::select! {
            () = waiting.wait() => return,
            () = tokio::time::sleep(every) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(fleet: hems_core::report::Summary) -> Window {
        Window {
            source: "https://obsd.example/v1/fleet".into(),
            fetched_at: time::macros::datetime!(2026-01-15 06:00 UTC),
            fleet,
        }
    }

    #[test]
    fn every_specialist_in_the_table_is_one_the_daemon_registers() {
        // The row that dispatches into nothing, in both directions. A name here
        // that no `skill()` call matches is a run that fails on the first tick.
        let registered = crate::registered_specialists();
        for specialist in SPECIALISTS {
            assert!(
                registered.contains(&specialist.name),
                "{} is reviewed and not registered",
                specialist.name
            );
        }
        for name in registered {
            assert!(
                SPECIALISTS.iter().any(|s| s.name == name),
                "{name} is registered and no review runs it"
            );
        }
    }

    #[tokio::test]
    async fn one_window_reaches_every_specialist_and_each_run_is_replayable() {
        // The property the journal is for, over the whole plane rather than one
        // skill: two specialists, one set of days, and every finding carries the
        // run that produced it.
        let store: Arc<dyn agentplane::prelude::JournalStore> =
            Arc::new(agentplane::prelude::RedbStore::open_in_memory().expect("a journal"));
        let runtime = crate::runtime(store);

        let finding = |site: String| hems_core::report::Finding {
            site,
            date: time::macros::date!(2026 - 01 - 15),
            detail: "as obsd described it".into(),
        };
        let fleet = hems_core::report::Summary {
            days: 56,
            sites: 4,
            breached: (0..4).map(|i| finding(format!("haus-{i}"))).collect(),
            without_a_plan: (0..4).map(|i| finding(format!("haus-{i}"))).collect(),
            ..hems_core::report::Summary::default()
        };

        let now = time::macros::datetime!(2026-01-15 06:00 UTC);
        let reviewed = run_specialists(&runtime, &window(fleet), now).await;

        assert_eq!(
            reviewed.iter().map(|r| r.specialist).collect::<Vec<_>>(),
            SPECIALISTS.iter().map(|s| s.name).collect::<Vec<_>>(),
            "every specialist ran, in the table's order"
        );
        let triage = &reviewed[0];
        assert_eq!(triage.proposal.considered, 56);
        assert!(triage.proposal.advice[0].headline.contains("4 of 4"));
        assert_eq!(triage.source, "https://obsd.example/v1/fleet");
        assert_eq!(triage.days, 56, "the denominator travels with the finding");
        assert_eq!(triage.sites, 4);

        // The run identifier is not decoration: it is what makes a finding
        // answerable months later.
        let replayed = runtime
            .replay(triage.run, agentplane::prelude::Mode::Strict)
            .await
            .expect("the replay completed");
        let proposal: Proposal =
            serde_json::from_value(replayed.output.expect("an answer").peek().clone())
                .expect("the same answer");
        assert_eq!(proposal, triage.proposal, "the same answer, re-derived");
    }

    /// The identifier an operator's dashboard reads is unchanged by having
    /// become a type.
    ///
    /// `Reviewed.run` was a `String` built with `RunId::to_string()`; it is now
    /// a `RunId`. That is only safe to do silently because `agentplane` 0.32
    /// made `Serialize` write what `Display` already wrote, and "only safe
    /// because of an upstream release note" is exactly the kind of claim that
    /// wants an assertion rather than a comment. A field on a served API is a
    /// wire form, and a wire form nothing pins is one that moves.
    #[test]
    fn the_run_identifier_is_the_same_string_on_the_wire_as_in_a_log() {
        let id = RunId::generate();
        let reviewed = Reviewed {
            specialist: "compliance-triage",
            run: id,
            at: time::macros::datetime!(2026-01-15 06:00 UTC),
            source: "https://obsd.example/v1/fleet".into(),
            days: 56,
            sites: 4,
            proposal: Proposal::default(),
        };
        let json = serde_json::to_value(&reviewed).expect("a serialisable review");
        assert_eq!(
            json["run"],
            serde_json::Value::String(id.to_string()),
            "the served field is what an operator would grep a log for"
        );
        // And it is the prefixed form, so the string says what kind of id it is
        // wherever it lands.
        assert!(
            json["run"].as_str().is_some_and(|s| s.starts_with("run_")),
            "an identifier should be self-describing: {}",
            json["run"]
        );
        // Round-trip, because a dashboard that shows it is a dashboard that may
        // hand it back.
        assert_eq!(
            json["run"].as_str().unwrap().parse::<RunId>().unwrap(),
            id,
            "what is served parses back to what produced it"
        );
    }

    #[tokio::test]
    async fn a_queue_is_read_in_the_tables_order_whatever_order_it_was_written_in() {
        // The map is keyed by name, and alphabetically `compliance-triage` comes
        // first only by luck. § 14a is first because it is the one with a
        // regulator behind it, and that has to survive a rename.
        let queue = Queue::new();
        assert!(queue.is_empty().await);
        let reviewed = |name: &'static str| Reviewed {
            specialist: name,
            run: RunId::generate(),
            at: time::macros::datetime!(2026-01-15 06:00 UTC),
            source: "s".into(),
            days: 60,
            sites: 3,
            proposal: Proposal::default(),
        };
        for specialist in SPECIALISTS.iter().rev() {
            queue.record(reviewed(specialist.name)).await;
        }
        assert_eq!(
            queue
                .read()
                .await
                .iter()
                .map(|r| r.specialist)
                .collect::<Vec<_>>(),
            SPECIALISTS.iter().map(|s| s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_plane_that_has_not_read_the_fleet_is_not_ready() {
        // The distinction that makes an empty queue legible. `Health::new`
        // starts a daemon ready, and a queue that has never been filled looks
        // exactly like a fleet in good order — so this probe is what says which
        // of the two it is, to an orchestrator rather than only to a reader.
        let health = Health::new();
        assert!(health.readiness().ready, "nothing registered yet");
        health.set(UPSTREAM, not_reviewed_yet());
        assert!(
            !health.readiness().ready,
            "a plane that has read nothing must not report itself ready"
        );
        health.good(UPSTREAM, OffsetDateTime::now_utc());
        assert!(health.readiness().ready, "and ready once a review lands");
    }

    #[tokio::test]
    async fn a_failed_run_leaves_the_last_answer_rather_than_an_empty_queue() {
        // The most expensive wrong answer this daemon can give is "nothing to
        // report" when it means "I could not look": an empty queue is what a
        // compliant fleet looks like.
        struct Broken;
        #[async_trait::async_trait]
        impl Upstream for Broken {
            async fn window(&self) -> Result<Window, UpstreamError> {
                Err(UpstreamError::Unreachable {
                    source_url: "https://obsd.example/v1/days".into(),
                    detail: "connection refused".into(),
                })
            }
        }
        let store: Arc<dyn agentplane::prelude::JournalStore> =
            Arc::new(agentplane::prelude::RedbStore::open_in_memory().expect("a journal"));
        let runtime = crate::runtime(store);
        let error = review_once(&runtime, &Broken, OffsetDateTime::now_utc())
            .await
            .expect_err("the fleet could not be read");
        assert!(error.is_worth_retrying());
    }
}
