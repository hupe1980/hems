//! Fetching every location on a schedule, and knowing when the answer is old.

use std::collections::BTreeMap;

use hems_core::prelude::{Horizon, Slot};
use hems_forecast::WeatherSeries;
use time::OffsetDateTime;

use crate::upstream::Upstream;

/// What one round produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PollOutcome {
    /// Locations whose run was refreshed.
    pub refreshed: Vec<String>,
    /// Locations that did not answer, and why.
    pub failed: Vec<(String, String)>,
}

/// One location's cached run.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    /// The weather.
    pub series: WeatherSeries,
    /// **Where** it is the weather for.
    ///
    /// Carried on the run rather than looked up again when somebody asks. The
    /// sun position a production figure is computed from has to be the one the
    /// irradiance was fetched at, and a configuration edited between the fetch
    /// and the question would otherwise move a household's sun without moving
    /// its sky.
    pub at: hems_core::prelude::GeoPoint,
    /// When it was fetched.
    pub fetched_at: OffsetDateTime,
}

impl Run {
    /// How much of the next `slots` quarter hours from `now` this run covers,
    /// as an unbroken sequence.
    ///
    /// Contiguous rather than counted, for the same reason `tariffd`'s cache is:
    /// a run with a hole in the afternoon cannot plan the afternoon, and a count
    /// would call it nearly complete.
    #[must_use]
    pub fn contiguous_from(&self, now: OffsetDateTime) -> usize {
        let mut slot = Slot::containing(now);
        let mut covered = 0;
        while self.series.at(slot).is_some() {
            covered += 1;
            slot = slot.next();
        }
        covered
    }

    /// How much of `horizon` it holds at all, in `[0, 1]`.
    #[must_use]
    pub fn coverage(&self, horizon: Horizon) -> f64 {
        if horizon.len == 0 {
            return 1.0;
        }
        let held = horizon
            .slots()
            .filter(|s| self.series.at(*s).is_some())
            .count();
        held as f64 / horizon.len as f64
    }
}

/// One location's schedule.
#[derive(Debug, Clone, Copy)]
struct Schedule {
    failures: u32,
    next_attempt: OffsetDateTime,
}

/// The fetch loop, taking time as a parameter.
pub struct Poller<U: Upstream> {
    upstream: U,
    locations: Vec<crate::Location>,
    schedules: BTreeMap<String, Schedule>,
    interval: time::Duration,
    max_backoff: time::Duration,
}

impl<U: Upstream> Poller<U> {
    /// A poller for `locations`.
    pub fn new(
        upstream: U,
        locations: Vec<crate::Location>,
        now: OffsetDateTime,
        interval: time::Duration,
        max_backoff: time::Duration,
    ) -> Self {
        let schedules = locations
            .iter()
            .map(|l| {
                (
                    l.id.clone(),
                    Schedule {
                        failures: 0,
                        next_attempt: now,
                    },
                )
            })
            .collect();
        Self {
            upstream,
            locations,
            schedules,
            interval,
            max_backoff,
        }
    }

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

    /// Fetch every location whose turn it is.
    ///
    /// A **failed** fetch leaves the previous run in place. That is the whole
    /// reason there is a store rather than a passthrough: a weather run is good
    /// for hours, so an outage shorter than one costs the household nothing, and
    /// dropping the last good run on the first `503` would turn a two-minute
    /// blip into a day with no plan.
    pub async fn poll(
        &mut self,
        runs: &mut BTreeMap<String, Run>,
        now: OffsetDateTime,
    ) -> PollOutcome {
        let mut outcome = PollOutcome::default();
        let due: Vec<crate::Location> = self
            .locations
            .iter()
            .filter(|l| {
                self.schedules
                    .get(&l.id)
                    .is_some_and(|s| s.next_attempt <= now)
            })
            .cloned()
            .collect();

        for location in due {
            match self.upstream.fetch(&location).await {
                Ok(series) => {
                    runs.insert(
                        location.id.clone(),
                        Run {
                            series,
                            at: location.point(),
                            fetched_at: now,
                        },
                    );
                    if let Some(schedule) = self.schedules.get_mut(&location.id) {
                        schedule.failures = 0;
                        schedule.next_attempt = now + self.interval;
                    }
                    outcome.refreshed.push(location.id);
                }
                Err(e) => {
                    let failures = self
                        .schedules
                        .get(&location.id)
                        .map_or(0, |s| s.failures.saturating_add(1));
                    let wait = self.backoff(failures);
                    if let Some(schedule) = self.schedules.get_mut(&location.id) {
                        schedule.failures = failures;
                        schedule.next_attempt = now + wait;
                    }
                    outcome.failed.push((location.id, e.to_string()));
                }
            }
        }
        outcome
    }

    /// When the next location is due.
    #[must_use]
    pub fn next_due(&self) -> Option<OffsetDateTime> {
        self.schedules.values().map(|s| s.next_attempt).min()
    }
}

/// Whether every configured location can be planned for `slots` ahead.
///
/// **Every**, not any: a fleet service that is ready while one of the households
/// it serves has no weather is a service that is not ready for that household,
/// and the one thing an aggregate must not do is average away the case that
/// matters.
#[must_use]
pub fn is_ready(
    runs: &BTreeMap<String, Run>,
    locations: &[crate::Location],
    now: OffsetDateTime,
    slots: usize,
) -> bool {
    !locations.is_empty()
        && locations.iter().all(|l| {
            runs.get(&l.id)
                .is_some_and(|run| run.contiguous_from(now) >= slots)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::UpstreamError;
    use hems_forecast::weather::WeatherPoint;
    use std::sync::Mutex;
    use time::macros::datetime;

    const NOON: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    fn berlin() -> crate::Location {
        crate::Location {
            id: "berlin".into(),
            latitude: 52.5,
            longitude: 13.4,
            altitude_m: 34.0,
        }
    }

    fn munich() -> crate::Location {
        crate::Location {
            id: "munich".into(),
            latitude: 48.1,
            longitude: 11.6,
            altitude_m: 519.0,
        }
    }

    fn run(slots: usize) -> WeatherSeries {
        WeatherSeries {
            slots: (0..slots)
                .map(|i| {
                    (
                        Slot::containing(NOON).offset(i64::try_from(i).unwrap()),
                        WeatherPoint {
                            ghi_w_per_m2: 500.0,
                            temperature_c: 20.0,
                            cloud_cover: None,
                        },
                    )
                })
                .collect(),
            published_minutes: 15,
        }
    }

    struct Scripted {
        answers: Mutex<BTreeMap<String, Vec<Result<WeatherSeries, ()>>>>,
    }

    impl Scripted {
        fn new(pairs: Vec<(&str, Vec<Result<WeatherSeries, ()>>)>) -> Self {
            Self {
                answers: Mutex::new(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()),
            }
        }
    }

    impl Upstream for Scripted {
        async fn fetch(&self, location: &crate::Location) -> Result<WeatherSeries, UpstreamError> {
            let mut answers = self.answers.lock().unwrap();
            let queue = answers.entry(location.id.clone()).or_default();
            if queue.is_empty() {
                return Err(UpstreamError::Status {
                    location: location.id.clone(),
                    status: 503,
                });
            }
            queue.remove(0).map_err(|()| UpstreamError::Status {
                location: location.id.clone(),
                status: 503,
            })
        }
    }

    #[tokio::test]
    async fn a_good_round_makes_every_location_plannable() {
        let mut poller = Poller::new(
            Scripted::new(vec![
                ("berlin", vec![Ok(run(288))]),
                ("munich", vec![Ok(run(288))]),
            ]),
            vec![berlin(), munich()],
            NOON,
            time::Duration::hours(1),
            time::Duration::hours(4),
        );
        let mut runs = BTreeMap::new();
        let outcome = poller.poll(&mut runs, NOON).await;
        assert_eq!(outcome.refreshed.len(), 2);
        assert!(is_ready(&runs, &[berlin(), munich()], NOON, 96));
    }

    #[tokio::test]
    async fn one_location_without_weather_is_not_ready_even_if_the_other_has_it() {
        // A fleet aggregate must not average away the household that has no
        // forecast.
        let mut poller = Poller::new(
            Scripted::new(vec![
                ("berlin", vec![Ok(run(288))]),
                ("munich", vec![Err(())]),
            ]),
            vec![berlin(), munich()],
            NOON,
            time::Duration::hours(1),
            time::Duration::hours(4),
        );
        let mut runs = BTreeMap::new();
        poller.poll(&mut runs, NOON).await;
        assert!(!is_ready(&runs, &[berlin(), munich()], NOON, 96));
        assert!(is_ready(&runs, &[berlin()], NOON, 96));
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_run_it_already_had() {
        // The whole reason there is a store rather than a passthrough: a weather
        // run is good for hours, so a two-minute outage must not become a day
        // with no plan.
        let mut poller = Poller::new(
            Scripted::new(vec![("berlin", vec![Ok(run(288)), Err(())])]),
            vec![berlin()],
            NOON,
            time::Duration::hours(1),
            time::Duration::hours(4),
        );
        let mut runs = BTreeMap::new();
        poller.poll(&mut runs, NOON).await;
        let later = poller.next_due().unwrap();
        let outcome = poller.poll(&mut runs, later).await;
        assert_eq!(outcome.failed.len(), 1);
        assert!(
            runs.contains_key("berlin"),
            "the last good run is still there"
        );
        assert!(is_ready(&runs, &[berlin()], NOON, 96));
    }

    #[tokio::test]
    async fn a_run_that_has_aged_past_the_horizon_stops_being_ready() {
        // The honest half of keeping the last good run: it does not last for
        // ever, and readiness is about what the run still *covers* from now
        // rather than about the fact that a run exists.
        let mut poller = Poller::new(
            Scripted::new(vec![("berlin", vec![Ok(run(100))])]),
            vec![berlin()],
            NOON,
            time::Duration::hours(1),
            time::Duration::hours(4),
        );
        let mut runs = BTreeMap::new();
        poller.poll(&mut runs, NOON).await;
        assert!(is_ready(&runs, &[berlin()], NOON, 96));
        let tomorrow = NOON + time::Duration::hours(20);
        assert!(!is_ready(&runs, &[berlin()], tomorrow, 96));
    }

    #[tokio::test]
    async fn nothing_configured_is_never_ready() {
        let runs = BTreeMap::new();
        assert!(!is_ready(&runs, &[], NOON, 96));
    }

    #[tokio::test]
    async fn the_backoff_doubles_and_is_capped() {
        let mut poller = Poller::new(
            Scripted::new(vec![("berlin", vec![Err(()), Err(()), Err(()), Err(())])]),
            vec![berlin()],
            NOON,
            time::Duration::hours(1),
            time::Duration::hours(4),
        );
        let mut runs = BTreeMap::new();
        let mut now = NOON;
        let mut waits = Vec::new();
        for _ in 0..4 {
            poller.poll(&mut runs, now).await;
            let due = poller.next_due().unwrap();
            waits.push(due - now);
            now = due;
        }
        assert_eq!(waits[0], time::Duration::hours(2));
        assert_eq!(waits[1], time::Duration::hours(4));
        assert_eq!(waits[2], time::Duration::hours(4), "capped");
    }
}
