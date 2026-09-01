//! Asking, failing, backing off, and knowing whether the answer was any use.
//!
//! # The backoff is per source, and it is capped
//!
//! One source being down is not five sources being down, so a failure slows
//! *that* source and leaves the others on their schedule. And the growth is
//! bounded: a fleet of gateway boxes retrying a failed public API every fifteen
//! seconds is a denial of service against somebody who is giving the data away,
//! and the cap is what makes the fleet a good citizen rather than the operator's
//! goodwill.
//!
//! # A successful fetch that taught the cache nothing is not a success
//!
//! The interesting failure is the quiet one: a source that answers `200` with
//! yesterday's curve, for ever. The request succeeded, the parse succeeded, and
//! the box has learned nothing since Tuesday. [`PollOutcome`] separates the two,
//! and the readiness probe is computed from what the cache *covers* rather than
//! from when the last request returned.

use std::collections::BTreeMap;

use hems_core::prelude::{Horizon, Slot};
use hems_tariff::cache::PriceCache;
use hems_tariff::source::Source;
use time::OffsetDateTime;

use crate::upstream::{Upstream, UpstreamError};

/// What one round of asking every source produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PollOutcome {
    /// Sources that answered with something the cache took.
    pub learned_from: Vec<Source>,
    /// Sources that answered and told the cache nothing new.
    pub stale: Vec<Source>,
    /// Sources that did not answer, and why.
    pub failed: Vec<(Source, String)>,
}

impl PollOutcome {
    /// Whether the round moved the cache at all.
    #[must_use]
    pub fn learned_anything(&self) -> bool {
        !self.learned_from.is_empty()
    }
}

/// The state one source's schedule carries between rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Schedule {
    /// How many times in a row it has failed.
    failures: u32,
    /// The earliest instant it may be asked again.
    next_attempt: OffsetDateTime,
}

/// The fetch loop, as a value that takes time as a parameter.
///
/// Everything here is a pure function of `(now, upstream answers)`, which is
/// what lets the whole schedule — the backoff, the recovery, the cap — be a unit
/// test rather than a thing somebody watches for an hour.
pub struct Poller<U: Upstream> {
    upstream: U,
    schedules: BTreeMap<Source, Schedule>,
    interval: time::Duration,
    max_backoff: time::Duration,
}

impl<U: Upstream> Poller<U> {
    /// A poller for `sources`, on `interval`, backing off no further than
    /// `max_backoff`.
    pub fn new(
        upstream: U,
        sources: impl IntoIterator<Item = Source>,
        now: OffsetDateTime,
        interval: time::Duration,
        max_backoff: time::Duration,
    ) -> Self {
        Self {
            upstream,
            schedules: sources
                .into_iter()
                .map(|s| {
                    (
                        s,
                        Schedule {
                            failures: 0,
                            next_attempt: now,
                        },
                    )
                })
                .collect(),
            interval,
            max_backoff,
        }
    }

    /// How long a source waits after `failures` consecutive failures.
    ///
    /// Doubling from the interval, capped. Deterministic rather than jittered,
    /// because a household fleet is not a thundering herd against one endpoint —
    /// each box starts its own clock when it boots — and a deterministic backoff
    /// is one a test can assert on.
    fn backoff(&self, failures: u32) -> time::Duration {
        if failures == 0 {
            return self.interval;
        }
        let doubled = self
            .interval
            .whole_seconds()
            .saturating_mul(1_i64 << failures.min(16));
        time::Duration::seconds(doubled.min(self.max_backoff.whole_seconds()))
    }

    /// Ask every source whose turn it is, and merge what comes back.
    pub async fn poll(&mut self, cache: &mut PriceCache, now: OffsetDateTime) -> PollOutcome {
        let mut outcome = PollOutcome::default();
        let due: Vec<Source> = self
            .schedules
            .iter()
            .filter(|(_, s)| s.next_attempt <= now)
            .map(|(source, _)| *source)
            .collect();

        for source in due {
            match self.upstream.fetch(source).await {
                Ok(fetched) => {
                    let merged = cache.merge(&fetched.series, now);
                    if merged.is_empty() {
                        outcome.stale.push(source);
                    } else {
                        outcome.learned_from.push(source);
                    }
                    if let Some(schedule) = self.schedules.get_mut(&source) {
                        schedule.failures = 0;
                        schedule.next_attempt = now + self.interval;
                    }
                }
                Err(e) => {
                    let failures = self
                        .schedules
                        .get(&source)
                        .map_or(0, |s| s.failures.saturating_add(1));
                    let wait = self.backoff(failures);
                    if let Some(schedule) = self.schedules.get_mut(&source) {
                        schedule.failures = failures;
                        schedule.next_attempt = now + wait;
                    }
                    outcome.failed.push((source, describe(&e)));
                }
            }
        }
        cache.prune(now);
        outcome
    }

    /// When the next source is due.
    #[must_use]
    pub fn next_due(&self) -> Option<OffsetDateTime> {
        self.schedules.values().map(|s| s.next_attempt).min()
    }
}

fn describe(error: &UpstreamError) -> String {
    error.to_string()
}

/// Whether the cache covers enough of the horizon ahead of `now` to call the
/// service ready.
///
/// Deliberately about the **contiguous** run rather than the count: a cache
/// holding tomorrow morning and tomorrow evening with the afternoon missing
/// can plan neither, and a coverage figure of 66 % would call it two thirds
/// ready.
#[must_use]
pub fn is_ready(cache: &PriceCache, now: OffsetDateTime, slots: usize) -> bool {
    if slots == 0 {
        return true;
    }
    let from = Slot::containing(now);
    let Some(last) = cache.contiguous_until(from) else {
        return false;
    };
    usize::try_from(from.distance_to(last) + 1).unwrap_or(0) >= slots
}

/// How much of the next `slots` quarter hours the cache holds, in `[0, 1]`.
#[must_use]
pub fn coverage(cache: &PriceCache, now: OffsetDateTime, slots: usize) -> f64 {
    cache.coverage(Horizon::new(now, slots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::Fetched;
    use hems_tariff::source::{PriceBasis, PriceSeries};
    use rust_decimal::Decimal;
    use std::sync::Mutex;
    use time::macros::datetime;

    const NOON: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    /// An upstream that answers from a script rather than from a socket.
    struct Scripted {
        answers: Mutex<BTreeMap<Source, Vec<Result<Fetched, UpstreamError>>>>,
        asked: Mutex<Vec<Source>>,
    }

    impl Scripted {
        fn new() -> Self {
            Self {
                answers: Mutex::new(BTreeMap::new()),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn will(mut self, source: Source, answers: Vec<Result<Fetched, UpstreamError>>) -> Self {
            self.answers.get_mut().unwrap().insert(source, answers);
            self
        }

        fn asked(&self) -> Vec<Source> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Upstream for Scripted {
        async fn fetch(&self, source: Source) -> Result<Fetched, UpstreamError> {
            self.asked.lock().unwrap().push(source);
            let mut answers = self.answers.lock().unwrap();
            let queue = answers.entry(source).or_default();
            if queue.is_empty() {
                return Err(UpstreamError::Transport {
                    feed: source,
                    detail: "no more scripted answers".into(),
                });
            }
            queue.remove(0)
        }
    }

    fn curve(source: Source, from: Slot, slots: usize, ct: i64) -> Fetched {
        Fetched {
            series: PriceSeries {
                points: (0..slots)
                    .map(|i| (from.offset(i64::try_from(i).unwrap()), Decimal::new(ct, 0)))
                    .collect(),
                source,
                basis: PriceBasis::Wholesale,
                published_minutes: 15,
            },
            co2_g_per_kwh: BTreeMap::new(),
        }
    }

    fn failure(source: Source) -> Result<Fetched, UpstreamError> {
        Err(UpstreamError::Status {
            feed: source,
            status: 503,
        })
    }

    #[tokio::test]
    async fn a_good_round_fills_the_cache_and_reports_which_source_taught_it() {
        let up = Scripted::new().will(
            Source::Smard,
            vec![Ok(curve(Source::Smard, Slot::containing(NOON), 96, 8))],
        );
        let mut poller = Poller::new(
            up,
            [Source::Smard],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        let outcome = poller.poll(&mut cache, NOON).await;
        assert_eq!(outcome.learned_from, vec![Source::Smard]);
        assert!(is_ready(&cache, NOON, 96));
    }

    #[tokio::test]
    async fn a_source_that_answers_with_what_the_cache_already_has_is_stale_not_successful() {
        // The quiet failure: `200 OK` with yesterday's curve, for ever. The
        // request worked and the box has learned nothing since Tuesday.
        let from = Slot::containing(NOON);
        let up = Scripted::new().will(
            Source::Smard,
            vec![
                Ok(curve(Source::Smard, from, 96, 8)),
                Ok(curve(Source::Smard, from, 96, 8)),
            ],
        );
        let mut poller = Poller::new(
            up,
            [Source::Smard],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        assert!(poller.poll(&mut cache, NOON).await.learned_anything());
        let later = NOON + time::Duration::minutes(15);
        let outcome = poller.poll(&mut cache, later).await;
        assert!(!outcome.learned_anything());
        assert_eq!(outcome.stale, vec![Source::Smard]);
    }

    #[tokio::test]
    async fn one_source_failing_does_not_slow_the_others() {
        let from = Slot::containing(NOON);
        let up = Scripted::new()
            .will(
                Source::Entsoe,
                vec![failure(Source::Entsoe), failure(Source::Entsoe)],
            )
            .will(
                Source::Smard,
                vec![
                    Ok(curve(Source::Smard, from, 8, 8)),
                    Ok(curve(Source::Smard, from.offset(8), 8, 9)),
                ],
            );
        let mut poller = Poller::new(
            up,
            [Source::Entsoe, Source::Smard],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        let first = poller.poll(&mut cache, NOON).await;
        assert_eq!(first.failed.len(), 1);
        assert_eq!(first.learned_from, vec![Source::Smard]);

        // A quarter of an hour later SMARD is due again and ENTSO-E, having
        // failed once, is not.
        let later = NOON + time::Duration::minutes(15);
        let second = poller.poll(&mut cache, later).await;
        assert_eq!(second.learned_from, vec![Source::Smard]);
        assert!(second.failed.is_empty(), "ENTSO-E was not asked again yet");
    }

    #[tokio::test]
    async fn the_backoff_doubles_and_then_stops_doubling() {
        let up = Scripted::new().will(
            Source::Entsoe,
            (0..8).map(|_| failure(Source::Entsoe)).collect(),
        );
        let mut poller = Poller::new(
            up,
            [Source::Entsoe],
            NOON,
            time::Duration::minutes(15),
            // A cap of one hour: four times the interval, so the third failure
            // is where the doubling has to stop.
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        let mut now = NOON;
        let mut waits = Vec::new();
        for _ in 0..5 {
            poller.poll(&mut cache, now).await;
            let due = poller.next_due().expect("a source is scheduled");
            waits.push(due - now);
            now = due;
        }
        assert_eq!(
            waits[0],
            time::Duration::minutes(30),
            "first failure doubles"
        );
        assert_eq!(waits[1], time::Duration::hours(1));
        assert_eq!(waits[2], time::Duration::hours(1), "and then it is capped");
        assert_eq!(waits[4], time::Duration::hours(1));
    }

    #[tokio::test]
    async fn a_source_that_recovers_goes_back_to_the_ordinary_interval() {
        let from = Slot::containing(NOON);
        let up = Scripted::new().will(
            Source::Entsoe,
            vec![
                failure(Source::Entsoe),
                failure(Source::Entsoe),
                Ok(curve(Source::Entsoe, from, 96, 8)),
            ],
        );
        let mut poller = Poller::new(
            up,
            [Source::Entsoe],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(4),
        );
        let mut cache = PriceCache::new();
        let mut now = NOON;
        for _ in 0..2 {
            poller.poll(&mut cache, now).await;
            now = poller.next_due().unwrap();
        }
        poller.poll(&mut cache, now).await;
        assert_eq!(
            poller.next_due().unwrap() - now,
            time::Duration::minutes(15),
            "recovery clears the backoff rather than serving it out"
        );
    }

    #[tokio::test]
    async fn readiness_is_about_an_unbroken_run_and_not_a_count() {
        // A cache holding tomorrow morning and tomorrow evening with the
        // afternoon missing can plan neither, and a count would call it two
        // thirds ready.
        let from = Slot::containing(NOON);
        let up = Scripted::new().will(
            Source::Smard,
            vec![Ok(Fetched {
                series: PriceSeries {
                    points: (0..96)
                        .filter(|i| *i != 40)
                        .map(|i| (from.offset(i), Decimal::new(8, 0)))
                        .collect(),
                    source: Source::Smard,
                    basis: PriceBasis::Wholesale,
                    published_minutes: 15,
                },
                co2_g_per_kwh: BTreeMap::new(),
            })],
        );
        let mut poller = Poller::new(
            up,
            [Source::Smard],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        poller.poll(&mut cache, NOON).await;
        assert!(
            coverage(&cache, NOON, 96) > 0.98,
            "almost everything is there"
        );
        assert!(
            !is_ready(&cache, NOON, 96),
            "and it still cannot plan the day"
        );
        assert!(is_ready(&cache, NOON, 40), "up to the hole, it can");
    }

    #[tokio::test]
    async fn nothing_configured_is_never_ready() {
        // A `tariffd` nobody gave an endpoint to must not come up green over an
        // empty cache.
        let mut poller = Poller::new(
            Scripted::new(),
            [],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        let outcome = poller.poll(&mut cache, NOON).await;
        assert!(!outcome.learned_anything());
        assert!(!is_ready(&cache, NOON, 96));
        assert!(poller.next_due().is_none());
    }

    #[tokio::test]
    async fn only_the_sources_that_are_due_are_asked() {
        let from = Slot::containing(NOON);
        let up = Scripted::new().will(
            Source::Smard,
            vec![
                Ok(curve(Source::Smard, from, 4, 8)),
                Ok(curve(Source::Smard, from, 4, 8)),
            ],
        );
        let mut poller = Poller::new(
            up,
            [Source::Smard],
            NOON,
            time::Duration::minutes(15),
            time::Duration::hours(1),
        );
        let mut cache = PriceCache::new();
        poller.poll(&mut cache, NOON).await;
        // One minute later nothing is due, so nothing is asked.
        poller
            .poll(&mut cache, NOON + time::Duration::minutes(1))
            .await;
        assert_eq!(poller.upstream.asked(), vec![Source::Smard]);
    }
}
