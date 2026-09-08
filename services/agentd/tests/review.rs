//! The whole plane, end to end, from the days a box would report.
//!
//! The unit tests cover each half — a specialist's arithmetic over a `Summary`,
//! the queue's order, who may read it. This is the one that puts them together,
//! and it deliberately starts one step further back than the specialists do:
//! it writes **days**, folds them with `obsd`'s own `Fleet::summarise`, and runs
//! the plane on what comes out.
//!
//! That link is the point. `agentd` reads a summary it does not compute, so a
//! test that assembled one by hand would agree with itself and drift from the
//! service that produces it — the field is populated per site here and per day
//! there, and nothing but this would notice. `obsd` is a **dev**-dependency for
//! exactly this: the two stay separate deployables, and nothing in `src/` names
//! it.
//!
//! It is also `just agent-demo`, which is why it prints. A demonstration that
//! runs a test suite and reports "33 passed" shows that the code compiles; this
//! shows what the code *says*.

use std::sync::Arc;

use agentd::upstream::{Upstream, UpstreamError, Window};
use agentplane::prelude::{JournalStore, RedbStore};
use hems_core::report::{DayKpis, Economics, Summary};

const NOW: time::OffsetDateTime = time::macros::datetime!(2026-01-20 06:00 UTC);
const SILENT_AFTER: time::Duration = time::Duration::days(2);

/// A fleet with findings in it and a lot of ordinary days around them.
///
/// Deliberately not a fleet where everything is wrong. An advisory queue is only
/// worth reading if a quiet fleet produces nothing, so the interesting assertion
/// is that these particular days are what surfaces.
fn a_fleet() -> Vec<DayKpis> {
    let day = |site: &str, on: time::Date| DayKpis {
        site: site.into(),
        date: on,
        respected_the_grid: true,
        self_sufficiency: 0.5,
        ..DayKpis::default()
    };

    let mut days = Vec::new();

    // Twelve households, a fortnight of days, all in good order.
    for house in 0..12 {
        for offset in 0..14 {
            days.push(day(
                &format!("haus-{house}"),
                time::macros::date!(2026 - 01 - 01) + time::Duration::days(offset),
            ));
        }
    }

    // Four of them breached, and three were on the fallback when they did —
    // which is the correlation `obsd`'s two lists cannot make on their own.
    for house in 0..4 {
        days.push(DayKpis {
            respected_the_grid: false,
            worst_overshoot_w: 900.0,
            minutes_without_a_plan: if house < 3 { 140 } else { 0 },
            ..day(
                &format!("haus-{house}"),
                time::macros::date!(2026 - 01 - 15),
            )
        });
    }

    // One command below the [A1 4.5] minimum, reaching five households on one
    // day — one network operator's mistake rather than five households' bad luck.
    for house in 4..9 {
        days.push(DayKpis {
            below_minimum_commanded: true,
            ..day(
                &format!("haus-{house}"),
                time::macros::date!(2026 - 01 - 16),
            )
        });
    }

    // One roof over its § 9 EEG ceiling on four days — a plant configured
    // wrongly, which is a different party to ask and so a different finding.
    //
    // On days **after** the quiet fortnight, and that is not decoration: `Fleet`
    // is keyed on `(site, date)` and a second day for one household replaces the
    // first, because a box re-sending yesterday is correcting itself. A fixture
    // that reused 5–8 January would silently be four days shorter than it looks.
    for offset in 0..4 {
        days.push(DayKpis {
            worst_feed_in_overshoot_w: 1_200.0,
            ..day(
                "haus-11",
                time::macros::date!(2026 - 01 - 17) + time::Duration::days(offset),
            )
        });
    }

    // And two simulated days carrying a saving, against a fleet of real ones
    // that carry none — which is what makes a headline figure worth checking.
    for i in 0..2 {
        days.push(DayKpis {
            economics: Some(Economics::default()),
            ..day(&format!("sim-{i}"), time::macros::date!(2026 - 01 - 10))
        });
    }

    days
}

/// The days, through `obsd`'s own aggregation.
///
/// The step this test exists for: what `agentd` reads is whatever `obsd`
/// produces, so the fixture is built by asking `obsd` rather than by writing a
/// `Summary` that agrees with this file.
fn as_obsd_sees_it(days: Vec<DayKpis>) -> Summary {
    let mut fleet = obsd::Fleet::default();
    for day in days {
        fleet.record(day, NOW);
    }
    fleet.summarise(NOW, SILENT_AFTER)
}

/// An upstream that answers with a summary a test wrote.
struct Recorded(Window);

#[async_trait::async_trait]
impl Upstream for Recorded {
    async fn window(&self) -> Result<Window, UpstreamError> {
        Ok(self.0.clone())
    }
}

fn window(fleet: Summary) -> Recorded {
    Recorded(Window {
        source: "https://obsd.example/v1/fleet".into(),
        fetched_at: NOW,
        fleet,
    })
}

#[tokio::test]
async fn a_review_reads_a_fleet_and_leaves_a_queue_an_operator_can_replay() {
    let store: Arc<dyn JournalStore> =
        Arc::new(RedbStore::open_in_memory().expect("an in-memory journal"));
    let runtime = agentd::runtime(Arc::clone(&store));

    let reviewed = agentd::review_once(&runtime, &window(as_obsd_sees_it(a_fleet())), NOW)
        .await
        .expect("the fleet was read");

    let queue = Arc::new(agentd::Queue::new());
    for one in reviewed {
        queue.record(one).await;
    }
    let read = queue.read().await;

    println!("\n  The advisory queue, 20 January 2026:\n");
    for review in &read {
        for line in review.proposal.lines() {
            println!("    {line}");
        }
    }
    println!(
        "\n  …over {} days from {} households, read from {}.\n",
        read[0].days, read[0].sites, read[0].source
    );

    // ── What it found ────────────────────────────────────────────────────────
    let triage = read
        .iter()
        .find(|r| r.specialist == agentd::skills::compliance::NAME)
        .expect("the compliance specialist ran");

    let headlines: Vec<&str> = triage
        .proposal
        .advice
        .iter()
        .map(|a| a.headline.as_str())
        .collect();
    assert!(
        headlines.iter().any(|h| h.contains("3 of 4")),
        "three of the four breaches were on boxes with no plan: {headlines:?}"
    );
    assert!(
        headlines
            .iter()
            .any(|h| h.contains("2026-01-16") && h.contains("5 households were")),
        "one command below the minimum reached five households on one day: {headlines:?}"
    );
    assert!(
        headlines
            .iter()
            .any(|h| h.contains("§ 9 EEG ceiling") && h.contains("worst: 4")),
        "one roof was over its § 9 ceiling on four days: {headlines:?}"
    );

    // § 14a first, because it is the one with a regulator behind it — and within
    // that, the larger count first.
    assert_eq!(
        triage.proposal.advice[0].at_risk,
        agentd::AtRisk::Households(5),
        "the five-household mistake outranks the four-household pattern"
    );

    // Evidence is bounded and says what it stands for, so a queue does not
    // scroll.
    for advice in &triage.proposal.advice {
        assert!(advice.evidence.len() <= agentd::Proposal::EVIDENCE_SHOWN);
        assert!(advice.covers >= advice.evidence.len());
    }

    // ── And the finding can be replayed to the summary it was drawn from ─────
    let run = agentplane::core::RunId::parse(&triage.run).expect("a run identifier");
    let replayed = runtime
        .replay(run, agentplane::prelude::Mode::Strict)
        .await
        .expect("the replay completed");
    let again: agentd::Proposal =
        serde_json::from_value(replayed.output.expect("an answer").peek().clone())
            .expect("the same shape");
    assert_eq!(
        again, triage.proposal,
        "a replay re-derives the finding rather than re-asking obsd"
    );
    println!(
        "    replayed {} — the same answer, re-derived.\n",
        triage.run
    );
}

#[tokio::test]
async fn a_fleet_in_good_order_produces_a_queue_with_nothing_in_it() {
    // The property that makes the queue worth reading at all. A plane with an
    // opinion every morning is one nobody opens.
    let store: Arc<dyn JournalStore> =
        Arc::new(RedbStore::open_in_memory().expect("an in-memory journal"));
    let runtime = agentd::runtime(store);
    let quiet: Vec<DayKpis> = (0..30)
        .map(|i| DayKpis {
            site: format!("haus-{i}"),
            date: time::macros::date!(2026 - 01 - 19),
            respected_the_grid: true,
            ..DayKpis::default()
        })
        .collect();

    let reviewed = agentd::review_once(&runtime, &window(as_obsd_sees_it(quiet)), NOW)
        .await
        .expect("the fleet was read");

    assert_eq!(
        reviewed.len(),
        agentd::SPECIALISTS.len(),
        "every specialist still ran and still said what it considered"
    );
    for review in &reviewed {
        assert!(
            review.proposal.advice.is_empty(),
            "{} had nothing to say: {:?}",
            review.specialist,
            review.proposal.advice
        );
        assert_eq!(review.proposal.considered, 30);
        assert_eq!(review.days, 30);
        assert_eq!(review.sites, 30);
    }
}

#[tokio::test]
async fn the_findings_are_about_the_days_obsd_actually_named() {
    // The link this test exists for, asserted rather than assumed: what the
    // queue counts is what `obsd`'s own aggregation put in its lists. A summary
    // written by hand in `agentd`'s unit tests cannot fail this way.
    let days = a_fleet();
    let total = days.len();
    let fleet = as_obsd_sees_it(days);

    assert_eq!(fleet.days, total, "obsd counted every day it was given");
    assert_eq!(fleet.breached.len(), 4);
    assert_eq!(fleet.below_minimum.len(), 5);
    assert_eq!(fleet.over_feed_in_ceiling.len(), 4);
    assert_eq!(
        fleet.without_a_plan.len(),
        3,
        "one of the four breaching households had a plan"
    );

    let store: Arc<dyn JournalStore> =
        Arc::new(RedbStore::open_in_memory().expect("an in-memory journal"));
    let runtime = agentd::runtime(store);
    let reviewed = agentd::review_once(&runtime, &window(fleet), NOW)
        .await
        .expect("the fleet was read");
    for review in &reviewed {
        assert_eq!(
            review.proposal.considered, total,
            "{} counted the fleet's own denominator",
            review.specialist
        );
    }
}
