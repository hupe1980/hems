//! The forecasts of last resort.
//!
//! Every model in this crate needs something: geometry and a weather feed for
//! [`crate::solar`], weeks of history for [`crate::load::LoadProfile`], sessions
//! for [`crate::session`]. A box on its first morning has none of them, and a
//! box whose WAN has been down since Tuesday has stopped getting the weather.
//!
//! Refusing to forecast is not an option there, because the alternative to a bad
//! plan is the fallback arbiter, which is worse (G3, D20). So there are two
//! forecasts that need nothing but the meter:
//!
//! * **seasonal naive** — tomorrow at 07:15 looks like the most recent day's
//!   07:15. For a household load that is a strong baseline and it is the model
//!   the literature keeps failing to beat by much on single-household series
//!   (`specs/arxiv/arxiv-2512.00856.pdf`).
//! * **persistence** — the next hours look like the last one. Right for a
//!   quantity with no daily shape, and right for photovoltaics over a *short*
//!   horizon on an overcast day, where it beats a clear-sky model outright.
//!
//! Both come back with a wide band, and that is the point rather than a defect:
//! a plan made against a naive forecast should be a plan that does not bet much.

use std::collections::BTreeMap;

use hems_core::prelude::{Horizon, Power, Slot};

use crate::quantile::{Band, Forecast};

/// How wide a naive forecast admits it might be wrong, as a fraction.
///
/// Half. A seasonal-naive household forecast is routinely 50 % out at the
/// quarter-hour grain, and a band that pretended otherwise would let the
/// planner spend a battery on it.
pub const NAIVE_SPREAD: f64 = 0.5;

/// Slots in one day.
const SLOTS_PER_DAY: i64 = 96;

/// "The same time of day, on the most recent day that had one."
///
/// `history` is any measured series; the value used for a slot is the newest
/// observation at the same quarter hour of the local day that lies at or before
/// the start of the horizon. A week back is preferred over yesterday where both
/// exist and the weekday matches — but this deliberately does not know about
/// weekdays: that is [`crate::load::LoadProfile`]'s job, and this is the model
/// that runs when there is not enough history to have one.
#[must_use]
pub fn seasonal_naive(history: &BTreeMap<Slot, Power>, horizon: Horizon, spread: f64) -> Forecast {
    // One pass: the newest value seen at each quarter hour of the local day.
    let mut latest: BTreeMap<u32, (Slot, f64)> = BTreeMap::new();
    for (slot, power) in history {
        let key = slot.index_in_local_day();
        match latest.get(&key) {
            Some((seen, _)) if *seen >= *slot => {}
            _ => {
                latest.insert(key, (*slot, power.get()));
            }
        }
    }
    Forecast {
        slots: horizon
            .slots()
            .map(|slot| {
                let value = latest
                    .get(&slot.index_in_local_day())
                    .map_or(0.0, |(_, v)| *v);
                (slot, Band::relative(value, spread))
            })
            .collect(),
    }
}

/// "The next hours look like the last one."
///
/// `recent` is the most recent observation. The band widens with the distance
/// into the horizon, because persistence is excellent for the next quarter hour
/// and worthless by tomorrow — and a forecast whose band does not grow with the
/// horizon is claiming otherwise.
#[must_use]
pub fn persistence(recent: Power, horizon: Horizon, spread_per_day: f64) -> Forecast {
    let value = recent.get();
    Forecast {
        slots: horizon
            .slots()
            .enumerate()
            .map(|(k, slot)| {
                #[allow(clippy::cast_precision_loss)]
                let days = k as f64 / SLOTS_PER_DAY as f64;
                // Starts at a tenth and grows: right now the last reading is
                // nearly right, and by this time tomorrow it says very little.
                let spread = 0.1 + spread_per_day * days;
                (slot, Band::relative(value, spread))
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn day_of(values: impl Fn(u32) -> f64, from: time::OffsetDateTime) -> BTreeMap<Slot, Power> {
        (0..96)
            .map(|k| {
                let slot = Slot::containing(from + time::Duration::minutes(i64::from(k) * 15));
                (slot, Power::new(values(slot.index_in_local_day())))
            })
            .collect()
    }

    #[test]
    fn yesterday_at_this_time_is_the_forecast() {
        let history = day_of(|k| f64::from(k) * 10.0, datetime!(2026-03-10 00:00:00 UTC));
        let horizon = Horizon::new(datetime!(2026-03-11 00:00:00 UTC), 96);
        let f = seasonal_naive(&history, horizon, NAIVE_SPREAD);
        // The profile is indexed by quarter hour of the *local* day, and
        // 11:00 UTC in March is 12:00 in Berlin — slot 48.
        let noon = Slot::containing(datetime!(2026-03-11 11:00:00 UTC));
        assert_eq!(noon.index_in_local_day(), 48);
        assert!((f.at(noon).expect("in horizon").p50 - 480.0).abs() < 1e-9);
        assert!(f.slots.iter().all(|(_, b)| b.is_ordered()));
    }

    #[test]
    fn the_newest_day_wins() {
        let mut history = day_of(|_| 100.0, datetime!(2026-03-09 00:00:00 UTC));
        history.extend(day_of(|_| 300.0, datetime!(2026-03-10 00:00:00 UTC)));
        let horizon = Horizon::new(datetime!(2026-03-11 00:00:00 UTC), 4);
        let f = seasonal_naive(&history, horizon, 0.0);
        assert!(f.medians().all(|v| (v - 300.0).abs() < 1e-9));
    }

    #[test]
    fn a_slot_with_no_history_forecasts_nothing_rather_than_something() {
        let history = BTreeMap::new();
        let horizon = Horizon::new(datetime!(2026-03-11 00:00:00 UTC), 8);
        let f = seasonal_naive(&history, horizon, NAIVE_SPREAD);
        assert!(f.medians().all(|v| v == 0.0));
    }

    #[test]
    fn persistence_widens_with_the_horizon() {
        let horizon = Horizon::new(datetime!(2026-03-11 00:00:00 UTC), 192);
        let f = persistence(Power::new(1000.0), horizon, 0.6);
        let first = f.at(horizon.get(0).expect("first")).expect("in horizon");
        let last = f.at(horizon.get(191).expect("last")).expect("in horizon");
        assert!(
            last.width() > first.width() * 3.0,
            "{first} → {last}: a persistence forecast has to admit it decays"
        );
        assert!((first.p50 - 1000.0).abs() < 1e-9);
    }
}
