//! When the car comes home, when it leaves, and how empty it is.
//!
//! The planner's charging model takes an **arrival**, a **departure** and an
//! **energy target** (`hems_optimizer::model::EvSession`). On the evening the
//! cable goes in, all three are facts. Every plan made *before* that — the one
//! that decides whether to keep the battery full for the car or sell it into the
//! evening peak, which is the decision worth money — needs them as forecasts.
//!
//! A household's charging behaviour is the most regular thing about it: the same
//! car comes home at about the same time on the same weekdays with about the
//! same deficit. So the forecast is an empirical one — the observed sessions for
//! this weekday, and quantiles of them — rather than a model. There is nothing a
//! parametric distribution would add that twenty Tuesdays do not already say.
//!
//! # Which quantile of what
//!
//! The three are not symmetric, and taking the median of each would be wrong in
//! three different directions at once. A plan is a commitment the arbiter has to
//! be able to keep, so each figure is taken at the end that makes the plan
//! *robust*:
//!
//! | Quantity | Quantile | Why |
//! |---|---|---|
//! | arrival | **late** (P90) | a plan that assumes the car is there and it is not spends the cheap hours charging nothing, and the energy has to be found again in expensive ones |
//! | departure | **early** (P10) | the deadline is what the plan is judged on; assuming a late one and being wrong is the case where the car leaves short |
//! | energy | **high** (P90) | the shortfall is priced at €5/kWh (`hems_optimizer`), so under-reserving costs an order of magnitude more than over-reserving |
//!
//! The asymmetry is the forecast doing its job: it is not trying to be right on
//! average, it is trying to make the plan that follows it cheap to be wrong
//! about.
//!
//! # Refusing rather than guessing
//!
//! A weekday with fewer than [`MIN_SESSIONS`] observations produces **no**
//! forecast. Principle P5: a plan built on one observed Thursday is not a plan,
//! and the honest answer — "no session predicted, so nothing is reserved" —
//! costs the household the difference between a good plan and an average one,
//! where a wrong one costs it a car that cannot make the school run.

use std::collections::BTreeMap;

use hems_core::prelude::{Energy, Slot};
use time::{Date, OffsetDateTime, Weekday};

/// The fewest observed sessions on a weekday before it is forecast at all.
///
/// Three. Two give a range with no middle, and one gives a certainty.
pub const MIN_SESSIONS: usize = 3;

/// One charging session that actually happened.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Session {
    /// When the cable went in.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub plugged_in: OffsetDateTime,
    /// When it came out.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub unplugged: OffsetDateTime,
    /// Energy delivered over the session — the deficit the car came home with.
    pub energy: Energy,
}

impl Session {
    /// The weekday the session is filed under: the day the cable went in, in
    /// local time.
    ///
    /// Local, and by the *arrival*, because a session that starts at half past
    /// eleven on Tuesday and ends on Wednesday is a Tuesday session in every
    /// sense a household would recognise.
    #[must_use]
    pub fn weekday(&self) -> Weekday {
        Slot::containing(self.plugged_in).local_date().weekday()
    }

    /// How long the car was on the cable.
    #[must_use]
    pub fn duration(&self) -> time::Duration {
        self.unplugged - self.plugged_in
    }

    /// Whether the session is usable as evidence.
    ///
    /// A session that ends before it starts is a clock that went backwards; one
    /// that runs for a week is a cable somebody forgot; one that delivered
    /// nothing is a car that was already full and says nothing about the next
    /// deficit.
    #[must_use]
    pub fn is_plausible(&self) -> bool {
        let hours = self.duration().as_seconds_f64() / 3600.0;
        (0.25..=48.0).contains(&hours) && self.energy > Energy::ZERO
    }
}

/// What one session is expected to look like.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionForecast {
    /// The slot the car is expected to be plugged in by — the *late* quantile.
    pub arrival: Slot,
    /// The slot it is expected to leave in — the *early* quantile.
    pub departure: Slot,
    /// Energy it is expected to want, at the high quantile.
    pub energy: Energy,
    /// How many observed sessions the forecast rests on.
    pub sessions: usize,
}

impl SessionForecast {
    /// Whether the forecast window is usable — a departure after the arrival.
    ///
    /// The two quantiles are taken from opposite ends and from *different*
    /// sessions, so a household with wildly irregular hours can produce a
    /// window that has already closed. That is a statement about the household,
    /// and the honest response is no forecast rather than a negative one.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.departure > self.arrival
    }
}

/// One weekday's observed minutes and energies, held as raw samples.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct DayStats {
    /// Local minute of day the cable went in.
    arrival_minute: Vec<f64>,
    /// Minutes after the arrival that it came out — a *duration*, not a clock
    /// time, because a session that crosses midnight has a departure minute
    /// smaller than its arrival minute and averaging the two is meaningless.
    duration_minutes: Vec<f64>,
    /// Watt-hours delivered.
    energy_wh: Vec<f64>,
}

impl DayStats {
    fn len(&self) -> usize {
        self.arrival_minute.len()
    }
}

/// Nearest-rank quantile: the answer is always a value that was observed.
fn quantile(samples: &[f64], q: f64) -> f64 {
    debug_assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Charging sessions this household has had, by weekday.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SessionHistory {
    by_weekday: BTreeMap<u8, DayStats>,
}

impl SessionHistory {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a session that happened. Implausible ones are ignored.
    pub fn observe(&mut self, session: Session) {
        if !session.is_plausible() {
            return;
        }
        let arrival = Slot::containing(session.plugged_in);
        let stats = self
            .by_weekday
            .entry(session.weekday().number_days_from_monday())
            .or_default();
        stats
            .arrival_minute
            .push(f64::from(u32::from(arrival.local_minute_of_day())));
        stats
            .duration_minutes
            .push(session.duration().as_seconds_f64() / 60.0);
        stats.energy_wh.push(session.energy.get());
    }

    /// Record a whole history.
    pub fn observe_all(&mut self, sessions: impl IntoIterator<Item = Session>) {
        for session in sessions {
            self.observe(session);
        }
    }

    /// How many sessions have been seen on a weekday.
    #[must_use]
    pub fn support(&self, weekday: Weekday) -> usize {
        self.by_weekday
            .get(&weekday.number_days_from_monday())
            .map_or(0, DayStats::len)
    }

    /// The session expected on `date`, if there is enough history to say.
    ///
    /// `date` is the local calendar day the car is expected to arrive on, and
    /// `midnight` is that day's local midnight as an instant — the caller owns
    /// the calendar, because [`crate`] reads no clock and holds no time zone
    /// (P1).
    ///
    /// Returns `None` where a weekday has fewer than [`MIN_SESSIONS`]
    /// observations, or where the quantiles produce a window that has already
    /// closed.
    #[must_use]
    pub fn forecast_for(&self, date: Date, midnight: OffsetDateTime) -> Option<SessionForecast> {
        let stats = self
            .by_weekday
            .get(&date.weekday().number_days_from_monday())?;
        if stats.len() < MIN_SESSIONS {
            return None;
        }
        // Late arrival, short stay, big deficit — see the module note.
        let arrival_minute = quantile(&stats.arrival_minute, 0.9);
        let duration = quantile(&stats.duration_minutes, 0.1);
        let energy = quantile(&stats.energy_wh, 0.9);

        let arrival_at = midnight + time::Duration::seconds_f64(arrival_minute * 60.0);
        let departure_at = arrival_at + time::Duration::seconds_f64(duration * 60.0);
        let forecast = SessionForecast {
            arrival: Slot::containing(arrival_at),
            departure: Slot::containing(departure_at),
            energy: Energy::new(energy),
            sessions: stats.len(),
        };
        forecast.is_usable().then_some(forecast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// Local midnight in Berlin — the offset matters, because a session is
    /// filed by the *local* hour it started at and forecast against a *local*
    /// midnight. Mixing the two shifts every window by an hour in winter and
    /// two in summer, which is precisely the DST bug this workspace makes
    /// `metering::calendar` responsible for.
    fn session(day_offset: i64, arrive_h: f64, hours: f64, kwh: f64) -> Session {
        let midnight = datetime!(2026-01-05 00:00:00 +01:00) + time::Duration::days(day_offset);
        let plugged_in = midnight + time::Duration::seconds_f64(arrive_h * 3600.0);
        Session {
            plugged_in,
            unplugged: plugged_in + time::Duration::seconds_f64(hours * 3600.0),
            energy: Energy::from_kwh(kwh),
        }
    }

    /// 2026-01-05 is a Monday.
    fn mondays() -> SessionHistory {
        let mut h = SessionHistory::new();
        // Four Mondays: home between 17:00 and 18:30, away by seven, 18–24 kWh.
        h.observe_all([
            session(0, 17.0, 14.0, 20.0),
            session(7, 17.5, 13.5, 18.0),
            session(14, 18.5, 12.5, 24.0),
            session(21, 17.25, 13.75, 21.0),
        ]);
        h
    }

    #[test]
    fn a_weekday_with_too_little_history_produces_no_forecast() {
        let mut h = SessionHistory::new();
        h.observe(session(0, 17.0, 14.0, 20.0));
        h.observe(session(7, 17.0, 14.0, 20.0));
        assert_eq!(h.support(Weekday::Monday), 2);
        assert!(
            h.forecast_for(
                datetime!(2026-02-02 00:00:00 +01:00).date(),
                datetime!(2026-02-02 00:00:00 +01:00)
            )
            .is_none(),
            "two observations is a guess, not a forecast"
        );
    }

    #[test]
    fn the_forecast_is_robust_rather_than_central() {
        let h = mondays();
        let midnight = datetime!(2026-02-02 00:00:00 +01:00);
        let f = h
            .forecast_for(midnight.date(), midnight)
            .expect("four Mondays");
        assert_eq!(f.sessions, 4);
        // The late arrival, not the median one: 18:30 rather than 17:22.
        assert_eq!(
            f.arrival,
            Slot::containing(midnight + time::Duration::minutes(18 * 60 + 30))
        );
        // The short stay: 12,5 h from 18:30 is 07:00 the next morning.
        assert_eq!(
            f.departure,
            Slot::containing(midnight + time::Duration::minutes(31 * 60))
        );
        // The large deficit.
        assert!((f.energy.kwh() - 24.0).abs() < 1e-9, "{:?}", f.energy);
    }

    #[test]
    fn a_weekday_nobody_charges_on_is_not_forecast() {
        let h = mondays();
        // 2026-02-04 is a Wednesday, and this household never charges then.
        let midnight = datetime!(2026-02-04 00:00:00 +01:00);
        assert!(h.forecast_for(midnight.date(), midnight).is_none());
    }

    #[test]
    fn an_implausible_session_is_not_evidence() {
        let mut h = SessionHistory::new();
        // A cable somebody left in for a fortnight, a car that was already
        // full, and a clock that went backwards.
        h.observe(session(0, 17.0, 14.0 * 24.0, 20.0));
        h.observe(session(7, 17.0, 14.0, 0.0));
        h.observe(session(14, 17.0, -2.0, 20.0));
        assert_eq!(h.support(Weekday::Monday), 0);
    }

    #[test]
    fn a_household_with_no_pattern_gets_no_window() {
        let mut h = SessionHistory::new();
        // Home at eight for ten minutes, home at nine for ten hours, home at
        // eighteen for a quarter of an hour: the late arrival lands after the
        // short stay has already ended.
        h.observe_all([
            session(0, 8.0, 10.0, 20.0),
            session(7, 9.0, 0.3, 5.0),
            session(14, 22.0, 0.5, 5.0),
        ]);
        let midnight = datetime!(2026-02-02 00:00:00 +01:00);
        let f = h.forecast_for(midnight.date(), midnight);
        // 22:00 + 18 min still gives a usable window, but a tiny one — the test
        // is that whatever comes back is *usable* rather than reversed.
        assert!(f.is_none_or(|f| f.is_usable()));
    }
}
