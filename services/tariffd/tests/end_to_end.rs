//! The whole daemon, without a network.
//!
//! Fetch, reconcile, publish readiness, serve — driven through [`Upstream`] with
//! captured bodies rather than a socket, which is what lets a test that covers
//! the *service* run in a millisecond on a machine that is offline.

use std::collections::BTreeMap;
use std::sync::Mutex;

use hems_core::prelude::Slot;
use hems_tariff::cache::PriceCache;
use hems_tariff::source::Source;
use tariffd::poller::{Poller, coverage, is_ready};
use tariffd::upstream::{Fetched, Upstream, UpstreamError, parse};
use time::OffsetDateTime;
use time::macros::datetime;

/// The instant every captured body below is about.
const NOON: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

/// A captured aWATTar answer: four hours from 12:00 UTC, hourly, in €/MWh.
fn awattar_body() -> String {
    let start = NOON.unix_timestamp() * 1000;
    let hour = 3_600_000_i64;
    let entries: Vec<String> = (0..4)
        .map(|i| {
            format!(
                r#"{{"start_timestamp":{},"end_timestamp":{},"marketprice":{},"unit":"Eur/MWh"}}"#,
                start + i * hour,
                start + (i + 1) * hour,
                80.0 + i as f64
            )
        })
        .collect();
    format!(r#"{{"object":"list","data":[{}]}}"#, entries.join(","))
}

/// The same four hours from SMARD, which restates the auction to the same cent.
fn smard_body() -> String {
    let start = NOON.unix_timestamp() * 1000;
    let hour = 3_600_000_i64;
    let series: Vec<String> = (0..4)
        .map(|i| format!("[{},{}]", start + i * hour, 80.0 + i as f64))
        .collect();
    format!(r#"{{"series":[{}]}}"#, series.join(","))
}

/// An upstream that answers from a table of captured bodies.
struct Captured {
    bodies: BTreeMap<Source, Vec<Result<String, u16>>>,
    asked: Mutex<Vec<Source>>,
}

impl Captured {
    fn new(bodies: BTreeMap<Source, Vec<Result<String, u16>>>) -> Self {
        Self {
            bodies,
            asked: Mutex::new(Vec::new()),
        }
    }
}

impl Upstream for Captured {
    async fn fetch(&self, source: Source) -> Result<Fetched, UpstreamError> {
        let mut asked = self.asked.lock().unwrap();
        let seen = asked.iter().filter(|s| **s == source).count();
        asked.push(source);
        match self.bodies.get(&source).and_then(|v| v.get(seen)) {
            Some(Ok(body)) => parse(source, body),
            Some(Err(status)) => Err(UpstreamError::Status {
                feed: source,
                status: *status,
            }),
            None => Err(UpstreamError::Transport {
                feed: source,
                detail: "no capture for this attempt".into(),
            }),
        }
    }
}

fn captured(pairs: Vec<(Source, Vec<Result<String, u16>>)>) -> Captured {
    Captured::new(pairs.into_iter().collect())
}

#[test]
fn a_captured_awattar_body_becomes_priced_quarter_hours() {
    // The seam itself: the daemon's dispatch and `hems-tariff`'s parser, with a
    // body that never came off a socket.
    let fetched = parse(Source::Awattar, &awattar_body()).expect("the capture parses");
    assert_eq!(fetched.series.len(), 16, "four hours are sixteen quarters");
    assert_eq!(fetched.series.published_minutes, 60);
}

#[tokio::test]
async fn a_day_of_fetching_ends_with_a_cache_the_planner_can_use() {
    let mut cache = PriceCache::new();
    let mut poller = Poller::new(
        captured(vec![(Source::Awattar, vec![Ok(awattar_body())])]),
        [Source::Awattar],
        NOON,
        time::Duration::minutes(15),
        time::Duration::hours(1),
    );
    let outcome = poller.poll(&mut cache, NOON).await;
    assert_eq!(outcome.learned_from, vec![Source::Awattar]);
    assert!(is_ready(&cache, NOON, 16), "the four hours are contiguous");
    assert!(!is_ready(&cache, NOON, 17), "and no further");
    assert!((coverage(&cache, NOON, 16) - 1.0).abs() < 1e-12);
}

#[tokio::test]
async fn the_more_trusted_source_decides_where_two_disagree() {
    // aWATTar first, SMARD second, and SMARD is the Bundesnetzagentur restating
    // the auction — so what stands afterwards is SMARD's, whatever order they
    // arrived in.
    let mut cache = PriceCache::new();
    let mut poller = Poller::new(
        captured(vec![
            (Source::Awattar, vec![Ok(awattar_body())]),
            (Source::Smard, vec![Ok(smard_body())]),
        ]),
        [Source::Awattar, Source::Smard],
        NOON,
        time::Duration::minutes(15),
        time::Duration::hours(1),
    );
    poller.poll(&mut cache, NOON).await;
    assert_eq!(
        cache.at(Slot::containing(NOON)).unwrap().source,
        Source::Smard
    );
}

#[tokio::test]
async fn an_outage_leaves_the_box_able_to_plan_from_what_it_already_has() {
    // The property the whole cache exists for: `tariffd` fetches once, the WAN
    // goes away, and the household still has prices. A design that answered from
    // the last request rather than from a store would have nothing.
    let mut cache = PriceCache::new();
    let mut poller = Poller::new(
        captured(vec![(
            Source::Awattar,
            vec![Ok(awattar_body()), Err(503), Err(503), Err(503)],
        )]),
        [Source::Awattar],
        NOON,
        time::Duration::minutes(15),
        time::Duration::hours(1),
    );
    poller.poll(&mut cache, NOON).await;

    let mut now = NOON;
    for _ in 0..3 {
        now = poller.next_due().expect("still scheduled");
        let outcome = poller.poll(&mut cache, now).await;
        assert_eq!(outcome.failed.len(), 1);
    }
    // An hour of failures later, the prices are still there and still usable.
    assert!(is_ready(&cache, NOON, 16));
    assert!(now - NOON >= time::Duration::hours(1), "and it backed off");
}

#[tokio::test]
async fn prices_more_than_two_days_old_are_dropped() {
    // A gateway box's flash is not a time-series database. Everything the cache
    // holds is within two days of now, in both directions.
    let mut cache = PriceCache::new();
    let mut poller = Poller::new(
        captured(vec![(Source::Awattar, vec![Ok(awattar_body())])]),
        [Source::Awattar],
        NOON,
        time::Duration::minutes(15),
        time::Duration::hours(1),
    );
    poller.poll(&mut cache, NOON).await;
    assert_eq!(cache.len(), 16);

    // Three days later nothing in it is worth keeping, and one more poll — which
    // fails, because the capture is spent — prunes it.
    let much_later = NOON + time::Duration::days(3);
    poller.poll(&mut cache, much_later).await;
    assert!(cache.is_empty(), "the old curve went");
}
