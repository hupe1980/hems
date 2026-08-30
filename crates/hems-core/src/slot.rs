//! The 15-minute planning grid.
//!
//! # Why quarter hours, and why they are pure UTC
//!
//! Fifteen minutes is the settlement grain of the German market and, since the
//! SDAC move to 15-minute market time units on 1 October 2025, also the grain of
//! the day-ahead price. So it is the grain the planner works in.
//!
//! Every offset Europe/Berlin has ever used is a whole number of hours
//! (UTC+1 / UTC+2). A quarter-hour boundary in UTC is therefore also a
//! quarter-hour boundary in local time, and slot arithmetic can be plain UTC
//! arithmetic that stays correct across both DST transitions. What genuinely
//! needs the time zone is
//!
//! * which **local day** a slot belongs to (a day has 92, 96 or 100 slots), and
//! * the **wall-clock time** a slot starts at, which is what a § 14a Modul 3
//!   time window or a heat-pump comfort schedule is written in.
//!
//! Both go through [`metering::calendar`], so hems and the metering layer can
//! never disagree about what a quarter hour is.

use core::fmt;

use time::{Date, Duration, OffsetDateTime};

/// The length of one slot.
pub const SLOT: Duration = Duration::minutes(15);

/// Slots in a day without a DST transition.
pub const SLOTS_PER_DAY: usize = 96;

/// One quarter-hour of the planning grid, identified by its UTC start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Slot {
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    start: OffsetDateTime,
}

impl Slot {
    /// The slot that contains `instant`, i.e. `instant` rounded down to the
    /// quarter hour.
    #[must_use]
    pub fn containing(instant: OffsetDateTime) -> Self {
        let utc = instant.to_offset(time::UtcOffset::UTC);
        let secs_into_hour = i64::from(utc.minute()) * 60 + i64::from(utc.second());
        let secs_into_slot = secs_into_hour % SLOT.whole_seconds();
        let start = utc
            - Duration::seconds(secs_into_slot)
            - Duration::nanoseconds(i64::from(utc.nanosecond()));
        Self { start }
    }

    /// The slot starting exactly at `instant`.
    ///
    /// Returns `None` when `instant` is not on a quarter-hour boundary — use
    /// [`Slot::containing`] when rounding is what you want.
    #[must_use]
    pub fn starting_at(instant: OffsetDateTime) -> Option<Self> {
        let slot = Self::containing(instant);
        (slot.start == instant.to_offset(time::UtcOffset::UTC)).then_some(slot)
    }

    /// The instant this slot starts, in UTC.
    #[must_use]
    pub const fn start(&self) -> OffsetDateTime {
        self.start
    }

    /// The instant this slot ends (exclusive), in UTC.
    #[must_use]
    pub fn end(&self) -> OffsetDateTime {
        self.start + SLOT
    }

    /// `true` when `instant` falls inside `[start, end)`.
    #[must_use]
    pub fn contains(&self, instant: OffsetDateTime) -> bool {
        let utc = instant.to_offset(time::UtcOffset::UTC);
        utc >= self.start && utc < self.end()
    }

    /// The next slot.
    #[must_use]
    pub fn next(&self) -> Self {
        Self {
            start: self.start + SLOT,
        }
    }

    /// The previous slot.
    #[must_use]
    pub fn prev(&self) -> Self {
        Self {
            start: self.start - SLOT,
        }
    }

    /// The slot `n` places away (negative goes backwards).
    #[must_use]
    pub fn offset(&self, n: i64) -> Self {
        Self {
            start: self.start + SLOT * i32::try_from(n).unwrap_or(i32::MAX),
        }
    }

    /// How many slots separate `self` from `other` (negative when `other` is earlier).
    #[must_use]
    pub fn distance_to(&self, other: Self) -> i64 {
        (other.start - self.start).whole_seconds() / SLOT.whole_seconds()
    }

    /// The German calendar day this slot belongs to.
    ///
    /// Uses the Europe/Berlin day boundary, so the slot starting at 22:00 UTC in
    /// summer belongs to the *next* day — which is what a daily energy figure or
    /// a Modul 3 calendar means.
    #[must_use]
    pub fn local_date(&self) -> Date {
        metering::calendar::local_day(self.start)
    }

    /// Minutes since local midnight at the start of this slot, `0..1440`.
    ///
    /// This is the number a wall-clock rule is written against — a § 14a Modul 3
    /// window ("17:00–19:00"), a heat-pump night setback, an EV departure time.
    ///
    /// On the day the clocks go forward the values 120–179 never occur; on the
    /// day they go back, 120–179 occur twice. Both are correct: the regulation
    /// speaks in local wall-clock time, so an hour that happens twice is inside
    /// the window twice.
    #[must_use]
    pub fn local_minute_of_day(&self) -> u16 {
        let local = metering::calendar::to_berlin(self.start);
        u16::from(local.hour()) * 60 + u16::from(local.minute())
    }

    /// The slot's ordinal within its local day, counting elapsed real time.
    ///
    /// `0` for the first slot of the day; the last slot of a 25-hour day is
    /// `99`. Use this — not [`Slot::local_minute_of_day`] — to index a per-day
    /// array, because the wall clock repeats on the long day.
    #[must_use]
    pub fn index_in_local_day(&self) -> u32 {
        let day_start = metering::calendar::day_start_utc(self.local_date());
        u32::try_from((self.start - day_start).whole_seconds() / SLOT.whole_seconds()).unwrap_or(0)
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let local = metering::calendar::to_berlin(self.start);
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02} local",
            local.year(),
            u8::from(local.month()),
            local.day(),
            local.hour(),
            local.minute()
        )
    }
}

/// A run of consecutive slots — what the planner plans over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Horizon {
    /// The first slot.
    pub first: Slot,
    /// How many slots the horizon spans.
    pub len: usize,
}

impl Horizon {
    /// A horizon of `len` slots starting with the slot containing `from`.
    #[must_use]
    pub fn new(from: OffsetDateTime, len: usize) -> Self {
        Self {
            first: Slot::containing(from),
            len,
        }
    }

    /// A 48-hour horizon — the default: long enough to see tomorrow's prices
    /// and a full heating cycle, short enough to solve in a second.
    #[must_use]
    pub fn two_days(from: OffsetDateTime) -> Self {
        Self::new(from, 2 * SLOTS_PER_DAY)
    }

    /// The slots, in order.
    pub fn slots(&self) -> impl ExactSizeIterator<Item = Slot> + '_ {
        let first = self.first;
        (0..self.len).map(move |i| first.offset(i as i64))
    }

    /// The slot at index `i`, if the horizon reaches that far.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<Slot> {
        (i < self.len).then(|| self.first.offset(i as i64))
    }

    /// The position of `slot` in this horizon, if it is inside it.
    #[must_use]
    pub fn index_of(&self, slot: Slot) -> Option<usize> {
        let d = self.first.distance_to(slot);
        (d >= 0 && (d as usize) < self.len).then_some(d as usize)
    }

    /// The instant the horizon ends (exclusive).
    #[must_use]
    pub fn end(&self) -> OffsetDateTime {
        self.first.start() + SLOT * i32::try_from(self.len).unwrap_or(i32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn containing_rounds_down_to_the_quarter_hour() {
        let s = Slot::containing(datetime!(2026-03-15 10:07:42.5 UTC));
        assert_eq!(s.start(), datetime!(2026-03-15 10:00:00 UTC));
        assert_eq!(s.end(), datetime!(2026-03-15 10:15:00 UTC));
        assert!(s.contains(datetime!(2026-03-15 10:14:59 UTC)));
        assert!(!s.contains(datetime!(2026-03-15 10:15:00 UTC)));
    }

    #[test]
    fn containing_normalises_a_non_utc_input() {
        let berlin_noon = datetime!(2026-06-01 12:07:00 +2);
        assert_eq!(
            Slot::containing(berlin_noon).start(),
            datetime!(2026-06-01 10:00:00 UTC)
        );
    }

    #[test]
    fn starting_at_rejects_an_unaligned_instant() {
        assert!(Slot::starting_at(datetime!(2026-03-15 10:00:00 UTC)).is_some());
        assert!(Slot::starting_at(datetime!(2026-03-15 10:01:00 UTC)).is_none());
    }

    #[test]
    fn local_minute_of_day_follows_the_wall_clock_across_dst() {
        let winter = Slot::containing(datetime!(2026-01-15 16:00:00 UTC));
        assert_eq!(winter.local_minute_of_day(), 17 * 60);
        let summer = Slot::containing(datetime!(2026-07-15 15:00:00 UTC));
        assert_eq!(summer.local_minute_of_day(), 17 * 60);
    }

    #[test]
    fn the_short_spring_day_has_92_slots_and_the_long_autumn_day_100() {
        for (day, expected) in [
            (time::macros::date!(2026 - 03 - 29), 92),
            (time::macros::date!(2026 - 10 - 25), 100),
        ] {
            let mut s = Slot::containing(metering::calendar::day_start_utc(day));
            let mut n = 0;
            while s.local_date() == day {
                n += 1;
                s = s.next();
            }
            assert_eq!(n, expected, "slots in {day}");
        }
    }

    #[test]
    fn the_repeated_hour_maps_to_the_same_wall_clock_window_twice() {
        let first = Slot::containing(datetime!(2026-10-25 00:15:00 UTC));
        let second = Slot::containing(datetime!(2026-10-25 01:15:00 UTC));
        assert_eq!(first.local_minute_of_day(), 2 * 60 + 15);
        assert_eq!(second.local_minute_of_day(), 2 * 60 + 15);
        assert_ne!(first, second);
        assert_ne!(first.index_in_local_day(), second.index_in_local_day());
    }

    #[test]
    fn horizon_indexes_round_trip() {
        let h = Horizon::two_days(datetime!(2026-05-01 08:03:00 UTC));
        assert_eq!(h.len, 192);
        assert_eq!(h.slots().count(), 192);
        let s = h.get(100).unwrap();
        assert_eq!(h.index_of(s), Some(100));
        assert_eq!(h.index_of(h.first.prev()), None);
        assert_eq!(h.end(), datetime!(2026-05-03 08:00:00 UTC));
    }
}
