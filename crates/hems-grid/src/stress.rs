//! § 14a grid stress: what this household has actually lived through.
//!
//! # Two records about the same thing, and only one of them is about *you*
//!
//! Since 01.03.2025 every network operator publishes its control actions on
//! VNBdigital in a federally agreed format (BDEW, *Empfehlungen für das Format
//! von Veröffentlichungspflichten nach § 14a EnWG*, v1.0, 30.01.2025). It is a
//! **monthly aggregate per Netzbereich** — how many SteuVE were affected, how
//! many hours in the calendar month, an intensity per cent — and it carries **no
//! timestamps at all**. So it answers *how exposed is my area* and it can never
//! answer *when*, nor anything about one connection point.
//!
//! The window-shaped record is therefore the box's **own** evidence `[A1 7.2]`,
//! which it keeps for the two years of `[A1 7.3]` because the Festlegung
//! requires it. It says exactly when this household was reduced, to what, and by
//! whom. A frequency per (day type, quarter hour) learned from it is
//! [`StressProfile`] — a pure function of a record, needing no fleet service and
//! no publication.
//!
//! # What it is for, and what it is deliberately not for
//!
//! It answers a household's own question — *your operator reduces you at
//! teatime, four times in five* — and it stops there. Feeding anticipated
//! windows into the planner was built, measured on four scenarios and removed
//! (D129): on a dynamic tariff a reduction lands in the evening peak, which is
//! exactly where the price has already told the plan not to be, so the
//! anticipation is very nearly redundant; on a flat tariff it is worth less than
//! nothing. The machinery that produced those windows — a confidence policy, a
//! coalescer, and the published Eingriffsdauer as a cap on it — went with the
//! consumer rather than being kept for a caller that had been measured not to
//! want it. A module with no caller is not a feature.
//!
//! What is left is what `hemsd` actually reads: [`StressProfile::observe`] over
//! the box's own control events, and [`StressProfile::busiest`] and
//! [`StressProfile::hours_reduced`] on the way to `/v1/status`.

use std::collections::BTreeMap;

use hems_core::prelude::{DayType, GuardRule, Power, Slot};
use metering::holiday::Bundesland;

use crate::evidence::ControlEvent;

/// What one bucket of the household's own history has seen.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Bucket {
    /// Quarter hours in this bucket the box has a record for.
    observed: u32,
    /// How many of them a network operator's reduction was in force in.
    reduced: u32,
    /// The ceilings commanded in them, for the typical one.
    ceilings: Vec<f64>,
}

impl Bucket {
    fn frequency(&self) -> f64 {
        if self.observed == 0 {
            0.0
        } else {
            f64::from(self.reduced) / f64::from(self.observed)
        }
    }

    /// The median ceiling commanded in this bucket.
    fn typical_ceiling(&self) -> Option<Power> {
        if self.ceilings.is_empty() {
            return None;
        }
        let mut sorted = self.ceilings.clone();
        sorted.sort_by(f64::total_cmp);
        Some(Power::new(sorted[sorted.len() / 2]))
    }
}

/// The quarter hour of the day this household is most often reduced in.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Busiest {
    /// Which class of day.
    pub day: DayType,
    /// Which quarter hour of the local day, `0`–`95`.
    pub index: u32,
    /// How often a reduction is in force in it, in `[0, 1]`.
    pub frequency: f64,
    /// How many of them the box has a record for — the denominator, because a
    /// share without one cannot be wrong.
    pub observed: u32,
    /// The ceiling typically commanded there.
    pub ceiling: Option<Power>,
}

/// What this household has actually lived through, learned from its own
/// `[A1 7.2]` record.
///
/// Bucketed by **day type and quarter hour of the local day**, and not by
/// weekday, which is a decision about what the thing being learned actually
/// looks like.
///
/// What operators do in practice is a *time window*: the field reports for 2026
/// are that few of them send genuinely state-driven commands and that a rough
/// fixed-window scheme is the common implementation — and präventive Steuerung
/// is that by construction, since `[A1 10.5]`'s limitation applies
/// **kalendertäglich** in the same windows (the BDEW format says so, which is
/// why its daily and monthly intensities normally agree). A window that repeats
/// every calendar day shows up in a day-type bucket at a frequency near one.
///
/// Three day classes rather than seven weekdays because network load follows
/// working day / Saturday / Sunday, because a public holiday behaves like a
/// Sunday, and because seven times the buckets need seven times the evidence to
/// say the same thing. A household reduced on some working days and not others
/// gets that fraction as its frequency, which is the honest answer: a box cannot
/// know *which* Tuesday, and a profile that claimed to would be fitting noise.
///
/// The quarter hour is of the **local** day, because the evening peak an
/// operator is managing keeps local time and a fixed offset would move it by an
/// hour every summer.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StressProfile {
    /// Which Bundesland's holidays decide a day type.
    pub land: Bundesland,
    /// One bucket per `(day type, quarter hour of the local day)`.
    ///
    /// A sequence rather than a map, for the reason `LoadProfile` learned the
    /// hard way (D98): a tuple key is something JSON cannot express, so the
    /// derive compiles, every other format accepts it, and the one a box
    /// actually stores in fails at run time with no symptom.
    cells: Vec<(DayType, u32, Bucket)>,
}

impl StressProfile {
    /// A household that has not been reduced yet, or has not looked.
    #[must_use]
    pub fn new(land: Bundesland) -> Self {
        Self {
            land,
            cells: Vec::new(),
        }
    }

    fn bucket_mut(&mut self, day: DayType, index: u32) -> &mut Bucket {
        if let Some(position) = self
            .cells
            .iter()
            .position(|(d, i, _)| *d == day && *i == index)
        {
            return &mut self.cells[position].2;
        }
        self.cells.push((day, index, Bucket::default()));
        let last = self.cells.len() - 1;
        &mut self.cells[last].2
    }

    fn bucket(&self, day: DayType, index: u32) -> Option<&Bucket> {
        self.cells
            .iter()
            .find(|(d, i, _)| *d == day && *i == index)
            .map(|(_, _, b)| b)
    }

    /// Learn from every quarter hour of `window` and the events that fell in it.
    ///
    /// `window` is the denominator and the caller has to state it, because only
    /// the caller knows how much of it the box was watching. That is R28's
    /// lesson applied here: a frequency whose denominator is invisible cannot be
    /// wrong. A box that was **off** for part of the window has those quarter
    /// hours counted as "observed, not reduced" and therefore *understates* how
    /// exposed the household was — which is the safe direction for a figure a
    /// household reads about its own network operator.
    ///
    /// Only a **network operator's** reduction counts. A record opened because
    /// the manager was holding *itself* at its failsafe value carries
    /// [`GuardRule::Failsafe`] and is a fact about a lost heartbeat rather than
    /// about the network: counting it would teach a household to pre-charge for
    /// its own Steuerbox going quiet.
    pub fn observe(
        &mut self,
        window: (time::OffsetDateTime, time::OffsetDateTime),
        events: &[ControlEvent],
    ) {
        let (from, to) = window;
        if to <= from {
            return;
        }
        // The reduced quarter hours, and the ceiling each was under.
        let mut reduced: BTreeMap<Slot, f64> = BTreeMap::new();
        for event in events.iter().filter(|e| e.rule == GuardRule::Lpc) {
            let began = event.applied_at.unwrap_or(event.received_at);
            // An event with no release is still running, so it reaches the end
            // of the window rather than lasting a single instant.
            let ended = event.released_at.unwrap_or(to);
            let Some(ceiling) = event
                .ceilings
                .iter()
                .map(|c| c.value.get())
                .reduce(f64::min)
            else {
                continue;
            };
            let mut slot = Slot::containing(began.max(from));
            while slot.start() < ended.min(to) {
                reduced
                    .entry(slot)
                    .and_modify(|c| *c = c.min(ceiling))
                    .or_insert(ceiling);
                slot = slot.offset(1);
            }
        }

        let mut slot = Slot::containing(from);
        while slot.start() < to {
            let day = DayType::of(slot, self.land);
            let index = slot.index_in_local_day();
            let seen = reduced.get(&slot).copied();
            let bucket = self.bucket_mut(day, index);
            bucket.observed = bucket.observed.saturating_add(1);
            if let Some(ceiling) = seen {
                bucket.reduced = bucket.reduced.saturating_add(1);
                bucket.ceilings.push(ceiling);
            }
            slot = slot.offset(1);
        }
    }

    /// How often a reduction has been in force in this bucket, and on how much
    /// evidence.
    #[must_use]
    pub fn frequency(&self, day: DayType, index: u32) -> Option<(f64, u32)> {
        self.bucket(day, index).map(|b| (b.frequency(), b.observed))
    }

    /// Whether anything has been learned at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.iter().all(|(_, _, b)| b.reduced == 0)
    }

    /// Hours the household was reduced for, over the whole record.
    ///
    /// What a household would call "how much have they actually taken from me".
    #[must_use]
    pub fn hours_reduced(&self) -> f64 {
        f64::from(self.cells.iter().map(|(_, _, b)| b.reduced).sum::<u32>())
            * hems_core::prelude::SLOT_HOURS
    }

    /// The quarter hour of the day a reduction is most often in force in.
    ///
    /// The single most useful sentence a household can be told about § 14a —
    /// *your operator reduces you at teatime, four times in five* — and the one
    /// thing `[A1 8.4]`'s publication, being a monthly aggregate with no
    /// timestamps in it, can never answer about **this** connection.
    ///
    /// Ties break towards the earlier quarter hour, so a household with two
    /// equally busy ones is told about the first rather than an arbitrary one.
    #[must_use]
    pub fn busiest(&self) -> Option<Busiest> {
        self.cells
            .iter()
            .filter(|(_, _, b)| b.reduced > 0)
            .max_by(|(_, ia, a), (_, ib, b)| {
                a.frequency().total_cmp(&b.frequency()).then(ib.cmp(ia))
            })
            .map(|(day, index, bucket)| Busiest {
                day: *day,
                index: *index,
                frequency: bucket.frequency(),
                observed: bucket.observed,
                ceiling: bucket.typical_ceiling(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::para14a::ControlMode;
    use time::macros::datetime;
    use time::{Duration, OffsetDateTime};

    const LAND: Bundesland = Bundesland::Be;

    /// Local midnight on the first Monday of 2026.
    const FIRST: OffsetDateTime = datetime!(2026-01-05 00:00:00 +01:00);

    /// A reduction to 4,2 kW between two local hours of `day`.
    fn reduction(day: OffsetDateTime, from_h: i64, to_h: i64) -> ControlEvent {
        let began = day + Duration::hours(from_h);
        let mut event = ControlEvent::received(
            GuardRule::Lpc,
            ControlMode::Ems,
            Power::from_kw(4.2),
            Power::from_kw(10.5),
            began,
        );
        event.applied_at = Some(began);
        event.released_at = Some(day + Duration::hours(to_h));
        event
    }

    /// `days` consecutive local midnights from the first Monday of 2026.
    fn days(count: i64) -> Vec<OffsetDateTime> {
        (0..count).map(|d| FIRST + Duration::days(d)).collect()
    }

    /// The window those days cover, half-open.
    fn window(days: &[OffsetDateTime]) -> (OffsetDateTime, OffsetDateTime) {
        (days[0], days[days.len() - 1] + Duration::days(1))
    }

    /// Eight weeks of a household reduced from 17:00 to 19:00 **every calendar
    /// day** — the fixed-window scheme operators actually run, and what
    /// präventive Steuerung is by construction (`[A1 10.5]`, kalendertäglich).
    fn a_household_under_a_daily_window() -> StressProfile {
        let mut profile = StressProfile::new(LAND);
        let all = days(56);
        let events: Vec<ControlEvent> = all.iter().map(|d| reduction(*d, 17, 19)).collect();
        profile.observe(window(&all), &events);
        profile
    }

    #[test]
    fn a_window_that_repeats_every_day_is_learned() {
        let profile = a_household_under_a_daily_window();

        // Every working day of the window saw it, so the teatime bucket is
        // certain and the dawn one has never seen anything.
        let (teatime, observed) = profile
            .frequency(DayType::Workday, 17 * 4)
            .expect("a teatime bucket");
        assert!(observed >= 8, "eight weeks is evidence: {observed}");
        assert!((teatime - 1.0).abs() < 1e-9, "every one of them: {teatime}");
        assert!(
            profile
                .frequency(DayType::Workday, 3 * 4)
                .is_some_and(|(f, _)| f.abs() < 1e-12),
            "nothing has ever happened at three in the morning"
        );

        // …and a Sunday too, because the window is kalendertäglich.
        assert!(
            profile
                .frequency(DayType::Sunday, 17 * 4)
                .is_some_and(|(f, _)| (f - 1.0).abs() < 1e-9),
            "a daily window does not take the weekend off"
        );

        // And the sentence a household is told: the first quarter hour of the
        // window, at the ceiling the operator actually commands. Ties break
        // towards the earlier quarter hour, so a two-hour window reports its
        // start rather than an arbitrary slot inside it.
        let busiest = profile.busiest().expect("a busiest quarter hour");
        assert_eq!(busiest.index, 17 * 4, "teatime");
        assert!((busiest.frequency - 1.0).abs() < 1e-9);
        assert_eq!(busiest.ceiling, Some(Power::from_kw(4.2)));

        // Two hours a day for eight weeks.
        assert!(
            (profile.hours_reduced() - 2.0 * 56.0).abs() < 1e-9,
            "{}",
            profile.hours_reduced()
        );
    }

    #[test]
    fn a_window_that_happens_sometimes_is_reported_and_not_planned_around() {
        // Eight weeks, reduced on **one working day in five**. The frequency is
        // the honest answer — a box cannot know *which* day, and a profile that
        // claimed to would be fitting noise — so it is counted and not acted on.
        let mut profile = StressProfile::new(LAND);
        let all = days(56);
        let events: Vec<ControlEvent> = all
            .iter()
            .filter(|d| d.weekday() == time::Weekday::Tuesday)
            .map(|d| reduction(*d, 17, 19))
            .collect();
        profile.observe(window(&all), &events);

        let (frequency, observed) = profile
            .frequency(DayType::Workday, 17 * 4)
            .expect("a bucket");
        assert!(observed >= 35, "eight weeks of working days: {observed}");
        assert!(
            (0.1..0.3).contains(&frequency),
            "one working day in five: {frequency}"
        );
    }

    #[test]
    fn the_box_holding_itself_at_its_failsafe_teaches_nothing() {
        // `GuardRule::Failsafe` is the manager restraining *itself* for want of
        // a heartbeat. Counting it would teach a household to pre-charge for its
        // own Steuerbox going quiet, and would report an operator intervening on
        // a day nobody did.
        let mut profile = StressProfile::new(LAND);
        let all = days(56);
        let events: Vec<ControlEvent> = all
            .iter()
            .map(|d| {
                let mut e = reduction(*d, 17, 19);
                e.rule = GuardRule::Failsafe;
                e
            })
            .collect();
        profile.observe(window(&all), &events);

        assert!(profile.is_empty(), "nothing was learned from a failsafe");
        assert!(profile.busiest().is_none());
        assert!(profile.hours_reduced().abs() < 1e-12);
    }

    #[test]
    fn a_weekend_pattern_and_a_working_day_pattern_are_different_facts() {
        // Eight weeks reduced at teatime on **working days only** — the shape a
        // network's own load curve has. The weekend must not inherit it, or a
        // household would pre-charge every Saturday for something that has
        // never happened.
        let mut profile = StressProfile::new(LAND);
        let all = days(56);
        let events: Vec<ControlEvent> = all
            .iter()
            .filter(|d| !matches!(d.weekday(), time::Weekday::Saturday | time::Weekday::Sunday))
            .map(|d| reduction(*d, 17, 19))
            .collect();
        profile.observe(window(&all), &events);

        assert!(
            profile
                .frequency(DayType::Workday, 17 * 4)
                .is_some_and(|(f, _)| (f - 1.0).abs() < 1e-9),
            "every working day"
        );
        assert!(
            profile
                .frequency(DayType::Saturday, 17 * 4)
                .is_some_and(|(f, _)| f.abs() < 1e-12),
            "and never a Saturday"
        );

        // …and the one sentence the household is told names a working day.
        assert_eq!(
            profile.busiest().map(|b| b.day),
            Some(DayType::Workday),
            "a Saturday has no pattern to report"
        );
    }

    #[test]
    fn a_thin_record_reports_its_own_denominator() {
        // A household whose box has only just been commissioned. Four days of a
        // repeating window is a real pattern and almost no evidence, and the
        // profile has to say both — a frequency of 1,0 read without its
        // denominator is a coin toss quoted to three significant figures (R28).
        let mut profile = StressProfile::new(LAND);
        let all = days(4);
        let events: Vec<ControlEvent> = all.iter().map(|d| reduction(*d, 17, 19)).collect();
        profile.observe(window(&all), &events);

        let busiest = profile.busiest().expect("a busiest quarter hour");
        assert!((busiest.frequency - 1.0).abs() < 1e-9);
        assert!(
            busiest.observed < 8,
            "and the denominator says how little that means: {}",
            busiest.observed
        );
    }

    #[test]
    fn a_box_that_was_off_reports_less_exposure_rather_than_more() {
        // The denominator this module cannot see: a box that was unplugged for
        // half the window has those quarter hours counted as observed and not
        // reduced, so its frequency comes out low and it *understates* what the
        // household lived through. That is the safe direction for a figure a
        // household reads, and it is why `observe` makes the caller state the
        // window.
        let mut honest = StressProfile::new(LAND);
        let mut with_a_gap = StressProfile::new(LAND);
        let all = days(56);
        // The reductions the box actually saw: only the first four weeks.
        let seen: Vec<ControlEvent> = all.iter().take(28).map(|d| reduction(*d, 17, 19)).collect();

        honest.observe(window(&all[..28]), &seen);
        with_a_gap.observe(window(&all), &seen);

        let f = |p: &StressProfile| p.frequency(DayType::Workday, 17 * 4).expect("a bucket").0;
        assert!((f(&honest) - 1.0).abs() < 1e-9);
        assert!(
            f(&with_a_gap) < f(&honest),
            "the wider window dilutes it: {} against {}",
            f(&with_a_gap),
            f(&honest)
        );
    }
}
