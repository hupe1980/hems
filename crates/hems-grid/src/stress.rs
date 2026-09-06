//! § 14a grid stress: what the operator publishes, and what this household has
//! actually lived through.
//!
//! Two records about the same thing, with two different evidentiary statuses,
//! and telling them apart is most of this module.
//!
//! # What the operator publishes, `[A1 8.4]`
//!
//! Since 01.03.2025, by the 15th of the following month, every network operator
//! publishes its control actions on the shared platform VNBdigital in a
//! federally agreed format (BDEW, *Empfehlungen für das Format von
//! Veröffentlichungspflichten nach § 14a EnWG*, v1.0, 30.01.2025). It is
//! [`AreaExposure`], and **it is a monthly aggregate per Netzbereich**:
//!
//! | Field | What it is |
//! |---|---|
//! | Netzbereich-ID | assigned by the central code office, so a SteuVE lies in exactly one area |
//! | Postleitzahl | many-to-many — an area may span several, a postcode lie in several |
//! | Art der Steuerung | netzorientiert `[A1 4]` or präventiv `[A1 10.5]` |
//! | Anzahl der betroffenen SteuVE | a Fallgruppe summarised under `[A1 2.4.2]` counts as one |
//! | Eingriffsdauer | **hours in the calendar month** |
//! | Eingriffsintensität | per cent, by the formula in [`AreaExposure::daily_intensity`] |
//!
//! There are **no timestamps in it at all**, and that decides what can be built
//! on it. It answers *how exposed is my area* and it cannot answer *when* — so
//! it is a prior, not a schedule. This workspace's own notes claimed for a long
//! time that it "is a set of windows"; it is not, and a design that waited for
//! one from here would have waited for ever.
//!
//! Two facts in it are load-bearing anyway. The Eingriffsdauer is explicitly a
//! **maximum** for any individual device — the format says so, because a SteuVE
//! that drew nothing during an intervention was not really reduced — so it
//! *bounds* an anticipation learned elsewhere. And **präventive Steuerung
//! repeats**: the format notes that its daily intensity normally equals its
//! monthly one, because the same limitation applies in the same windows every
//! calendar day. A präventiv area is therefore far more anticipable than a
//! netzorientiert one, which is a fact about how much to trust a learned
//! profile rather than a different algorithm.
//!
//! # What the household lived through, `[A1 7.2]`
//!
//! The window-shaped record is the box's **own** evidence, which it keeps for
//! the two years of `[A1 7.3]` because the Festlegung requires it. It says
//! exactly when this household was reduced, to what, and by whom. A frequency
//! per (day type, quarter hour) learned from it is [`StressProfile`] — a pure
//! function of a record, needing no fleet service and no publication — and it
//! is what finally produces an [`AnticipatedReduction`].
//!
//! # What anticipation may and may not do
//!
//! It may only ever ask for **less**, so it cannot cause a breach; the worst it
//! can do is cost a household money by preparing for a reduction that does not
//! come. Three rules keep that cost small and honest:
//!
//! * it never covers a slot that is about to be **executed**. The limit in force
//!   now is a *fact* from the Steuerbox, and restraining the house on a guess
//!   instead would throttle it for nothing — see
//!   [`Anticipation::skip_slots`];
//! * it needs **evidence**, not a coincidence: a bucket with two observations is
//!   not a pattern ([`Anticipation::min_observations`]);
//! * and it is bounded by what the operator itself published, where that is
//!   known ([`Anticipation::monthly_hours`]).
//!
//! Nothing here decides anything on its own: the profile reports windows with a
//! confidence, and the caller — which is the daemon, because it is the layer
//! that owns both the evidence and the planner — turns them into limits.

use std::collections::BTreeMap;

use hems_core::prelude::{DayType, GuardRule, Horizon, Power, SLOTS_PER_DAY, Slot};
use metering::holiday::Bundesland;

use crate::evidence::ControlEvent;

/// Which control regime an intervention belonged to, as `[A1 8.4]` publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Steuerungsart {
    /// Netzorientierte Steuerung, `[A1 4]` — a reduction in response to the
    /// state of the network, sent when the operator's Netzzustandsermittlung
    /// calls for one.
    #[default]
    Netzorientiert,
    /// Präventive Steuerung, `[A1 10.5]` — a limitation on planning data,
    /// permitted until 31.12.2028.
    ///
    /// It repeats: the same limitation in the same windows every calendar day,
    /// which is why the published daily and monthly intensities normally agree
    /// and why an area under it is much more anticipable.
    Praeventiv,
}

impl Steuerungsart {
    /// Whether interventions under this regime repeat on the same daily
    /// schedule.
    ///
    /// True for präventive Steuerung, which is a fact from the format's own
    /// worked example rather than an assumption. It is what justifies trusting
    /// a learned profile of such an area at a lower confidence.
    #[must_use]
    pub const fn repeats_daily(self) -> bool {
        matches!(self, Self::Praeventiv)
    }
}

/// One Netzbereich's published month, `[A1 8.4]`.
///
/// See the module header for the format and for why this cannot produce a
/// window. What it can do is say how exposed an area is, and **bound** an
/// anticipation learned from a household's own record.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AreaExposure {
    /// The Netzbereich-ID, assigned by the central code-issuing body so that a
    /// SteuVE lies in exactly one area.
    pub netzbereich: String,
    /// The postcodes the area covers.
    ///
    /// Many-to-many by construction: an area may span several postcodes and a
    /// postcode may lie in several areas, so this is never an identifier.
    #[cfg_attr(feature = "serde", serde(default))]
    pub postleitzahlen: Vec<String>,
    /// Which regime the interventions belonged to.
    pub art: Steuerungsart,
    /// The calendar year published.
    pub year: i32,
    /// The calendar month, 1–12.
    pub month: u8,
    /// How many steuerbare Verbrauchseinrichtungen were affected.
    pub affected_steuve: u32,
    /// **Eingriffsdauer**: the sum of the hours in which an intervention took
    /// place in this area, over the calendar month.
    ///
    /// Independent of how many devices there are and of how many interventions
    /// happened — and explicitly a **maximum** for any individual one of them,
    /// because a SteuVE that drew nothing while a reduction was in force, or was
    /// unplugged, saw less. That is exactly what makes it usable as a bound
    /// rather than as an estimate.
    pub hours: f64,
    /// **Eingriffsintensität**, per cent — see
    /// [`AreaExposure::daily_intensity`].
    pub intensity_percent: f64,
}

impl AreaExposure {
    /// One calendar day's Eingriffsintensität, by the format's own formula.
    ///
    /// ```text
    /// Σᵢ (installed − allowed_maxᵢ)/installed × durationᵢ/24 h
    /// ```
    ///
    /// `interventions` is one `(allowed maximum, hours)` pair per intervention
    /// of the day. The share of *installed* power that was withheld, weighted by
    /// how much of the day it was withheld for.
    ///
    /// Returns zero for a non-positive installed power rather than an infinity:
    /// an area with no controllable capacity had no intensity, and a division
    /// nobody bounded would poison every figure downstream.
    #[must_use]
    pub fn daily_intensity(installed: Power, interventions: &[(Power, f64)]) -> f64 {
        let total = installed.get();
        if !(total.is_finite() && total > 0.0) {
            return 0.0;
        }
        interventions
            .iter()
            .filter(|(_, hours)| hours.is_finite() && *hours > 0.0)
            .map(|(allowed, hours)| {
                let withheld = ((total - allowed.get()) / total).clamp(0.0, 1.0);
                withheld * (hours / 24.0)
            })
            .sum()
    }

    /// The month's Eingriffsintensität: the mean of its daily ones.
    ///
    /// Over the **days of the month** rather than over the days that had an
    /// intervention — which is the difference between "how reduced was this area"
    /// and "how hard were its reductions", and the format asks for the first.
    #[must_use]
    pub fn monthly_intensity(daily: &[f64], days_in_month: u32) -> f64 {
        if days_in_month == 0 {
            return 0.0;
        }
        daily.iter().filter(|d| d.is_finite()).sum::<f64>() / f64::from(days_in_month)
    }

    /// The most hours any single device in this area can have been reduced for.
    ///
    /// The published figure, named for what it actually is. See
    /// [`AreaExposure::hours`].
    #[must_use]
    pub fn most_hours_one_device_saw(&self) -> f64 {
        self.hours.max(0.0)
    }
}

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

/// A reduction this household has reason to expect.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AnticipatedReduction {
    /// The first slot it is expected in.
    pub from: Slot,
    /// The first slot it is **not** expected in — half-open, like
    /// `TimedLimit::until` and `EvSession::departure`, and for the same reason.
    pub until: Slot,
    /// The ceiling to plan against.
    ///
    /// The **tightest** typical ceiling across the window, and that direction is
    /// deliberate: a plan that prepared for the looser of two and met the
    /// tighter is short, where the reverse costs one slightly earlier charge.
    pub ceiling: Power,
    /// How often a reduction was in force in these buckets, in `[0, 1]`.
    ///
    /// The **lowest** frequency of the slots the window covers, so a window is
    /// only as well evidenced as its weakest quarter hour.
    pub confidence: f64,
}

impl AnticipatedReduction {
    /// How many quarter hours it covers.
    #[must_use]
    pub fn slots(&self) -> i64 {
        self.from.distance_to(self.until).max(0)
    }

    /// How long it lasts, in hours.
    #[must_use]
    pub fn hours(&self) -> f64 {
        self.slots() as f64 / 4.0
    }
}

/// How eagerly a household anticipates a reduction.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Anticipation {
    /// How often a reduction has to have been in force before a bucket is
    /// planned around, in `[0, 1]`.
    ///
    /// Half by default. Below it the household would be preparing for something
    /// that usually does not happen, and every preparation costs a little.
    pub min_confidence: f64,
    /// How many observations a bucket needs before its frequency is evidence.
    ///
    /// Eight — about two months of one weekday, at a bucket per quarter hour per
    /// day type. Two identical Tuesdays are a coincidence.
    pub min_observations: u32,
    /// How many slots at the head of the horizon are never anticipated.
    ///
    /// **Two**, and the derivation matters: the arbiter follows a plan for up to
    /// `ArbiterConfig::max_plan_age` (twenty minutes), so slots 0 and 1 of a
    /// plan may both be executed under it. The limit in force *now* is a fact
    /// from the Steuerbox, and a guess that restrained the house instead would
    /// throttle it for nothing — with the arbiter obeying, because an envelope
    /// is an instruction.
    pub skip_slots: usize,
    /// The most hours a month the household will anticipate, where the operator
    /// has published a figure.
    ///
    /// `[A1 8.4]`'s Eingriffsdauer, which the format states is a **maximum** for
    /// any individual device. A profile that would anticipate more than the
    /// operator says it did is a profile about a different area — a Netzbereich
    /// reassignment, a mis-recorded month — and the published number is the
    /// honest cap.
    pub monthly_hours: Option<f64>,
}

impl Default for Anticipation {
    fn default() -> Self {
        Self {
            min_confidence: 0.5,
            min_observations: 8,
            skip_slots: 2,
            monthly_hours: None,
        }
    }
}

impl Anticipation {
    /// Cap the anticipation at what the operator published for this area.
    #[must_use]
    pub fn bounded_by(mut self, exposure: &AreaExposure) -> Self {
        self.monthly_hours = Some(exposure.most_hours_one_device_saw());
        self
    }

    /// The hours of anticipation a horizon of `slots` quarter hours may carry.
    ///
    /// The monthly budget, pro-rated onto the horizon. A two-day horizon out of
    /// a thirty-day month may spend a fifteenth of the month's hours, which is
    /// the only translation that does not let a plan spend a whole month's
    /// budget every time it is made.
    fn budget_hours(&self, slots: usize) -> Option<f64> {
        let monthly = self.monthly_hours?;
        let days = slots as f64 / f64::from(u32::try_from(SLOTS_PER_DAY).unwrap_or(96));
        Some((monthly * days / 30.0).max(0.0))
    }
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
    /// hours counted as "observed, not reduced" and therefore anticipates *less*
    /// than it should — which is the safe direction, since every anticipation
    /// costs a little and none of them can breach anything.
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
        f64::from(self.cells.iter().map(|(_, _, b)| b.reduced).sum::<u32>()) / 4.0
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

    /// The reductions to expect over `horizon`, tightest ceiling per window.
    ///
    /// Contiguous slots that pass the confidence and evidence tests are
    /// coalesced into one window, because a plan wants "expect 4,2 kW from
    /// 17:00 to 19:30" rather than ten separate quarter hours of the same
    /// statement.
    #[must_use]
    pub fn anticipate(&self, horizon: Horizon, config: Anticipation) -> Vec<AnticipatedReduction> {
        let mut runs: Vec<AnticipatedReduction> = Vec::new();
        let budget = config.budget_hours(horizon.len);

        for (position, slot) in horizon.slots().enumerate() {
            // Never a slot that is about to be executed: the limit in force now
            // is a fact, and a guess must not restrain a house on its own.
            if position < config.skip_slots {
                continue;
            }
            let day = DayType::of(slot, self.land);
            let Some(bucket) = self.bucket(day, slot.index_in_local_day()) else {
                continue;
            };
            if bucket.observed < config.min_observations
                || bucket.frequency() < config.min_confidence
            {
                continue;
            }
            let Some(ceiling) = bucket.typical_ceiling() else {
                continue;
            };
            match runs.last_mut() {
                // Contiguous with the run in progress: extend it, keeping the
                // tightest ceiling and the weakest confidence.
                Some(run) if run.until == slot => {
                    run.until = slot.offset(1);
                    run.ceiling = run.ceiling.min(ceiling);
                    run.confidence = run.confidence.min(bucket.frequency());
                }
                _ => runs.push(AnticipatedReduction {
                    from: slot,
                    until: slot.offset(1),
                    ceiling,
                    confidence: bucket.frequency(),
                }),
            }
        }

        // The operator's own publication is the cap. Where the profile would
        // anticipate more than the operator says any one device saw, the
        // best-evidenced windows are kept and the rest dropped — a profile that
        // over-anticipates is a profile about a different area.
        if let Some(budget) = budget {
            runs.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
            let mut spent = 0.0;
            runs.retain(|run| {
                if spent + run.hours() <= budget {
                    spent += run.hours();
                    true
                } else {
                    false
                }
            });
            runs.sort_by_key(|run| run.from);
        }
        runs
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
    fn a_household_under_a_daily_window() -> (StressProfile, Vec<OffsetDateTime>) {
        let mut profile = StressProfile::new(LAND);
        let all = days(56);
        let events: Vec<ControlEvent> = all.iter().map(|d| reduction(*d, 17, 19)).collect();
        profile.observe(window(&all), &events);
        (profile, all)
    }

    #[test]
    fn a_window_that_repeats_every_day_is_anticipated() {
        let (profile, all) = a_household_under_a_daily_window();

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

        // The next day's plan expects it: one window, 17:00 to 19:00, at the
        // ceiling the operator actually commands.
        let next = *all.last().expect("a day") + Duration::days(1);
        let expected = profile.anticipate(Horizon::new(next, 96), Anticipation::default());
        assert_eq!(expected.len(), 1, "one window: {expected:?}");
        assert!((expected[0].hours() - 2.0).abs() < 1e-9, "17:00 to 19:00");
        assert_eq!(expected[0].ceiling, Power::from_kw(4.2));
        assert!((expected[0].confidence - 1.0).abs() < 1e-9);
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

        let next = *all.last().expect("a day") + Duration::days(1);
        assert!(
            profile
                .anticipate(Horizon::new(next, 96), Anticipation::default())
                .is_empty(),
            "below the confidence threshold, so nothing is planned around"
        );
    }

    #[test]
    fn the_slots_about_to_be_executed_are_never_anticipated() {
        // The one way anticipation could cost a household real money: the limit
        // in force *now* is a fact from the Steuerbox, and a guess that
        // restrained the house instead would throttle it for nothing — with the
        // arbiter obeying, because an envelope is an instruction.
        let (profile, all) = a_household_under_a_daily_window();

        // A horizon that opens exactly as the expected reduction does.
        let opens_at = *all.last().expect("a day") + Duration::days(1) + Duration::hours(17);
        let horizon = Horizon::new(opens_at, 96);
        let expected = profile.anticipate(horizon, Anticipation::default());
        let first = expected.first().expect("teatime is still anticipated");
        assert!(
            horizon.index_of(first.from).is_some_and(|i| i >= 2),
            "the first two slots are the plan's own to execute: {:?}",
            first.from
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
        let next = *all.last().expect("a day") + Duration::days(1);
        assert!(
            profile
                .anticipate(Horizon::new(next, 96), Anticipation::default())
                .is_empty()
        );
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

        // A Saturday's plan anticipates nothing; the Monday after it does.
        // 7 March 2026, a Saturday well after the window — and still on winter
        // time, because the clocks go forward on the 29th.
        let saturday = datetime!(2026-03-07 00:00:00 +01:00);
        assert_eq!(saturday.weekday(), time::Weekday::Saturday);
        assert!(
            profile
                .anticipate(Horizon::new(saturday, 96), Anticipation::default())
                .is_empty(),
            "a Saturday has no pattern to plan around"
        );
    }

    #[test]
    fn the_operators_own_publication_caps_what_a_box_anticipates() {
        // `[A1 8.4]`'s Eingriffsdauer is explicitly a *maximum* for any single
        // device, so a profile that would anticipate more than the operator says
        // it did is a profile about a different area — a Netzbereich
        // reassignment, a mis-recorded month. The publication is the honest cap.
        let mut profile = StressProfile::new(LAND);
        let all = days(56);
        // Six hours every day: far more than the operator will admit to.
        let events: Vec<ControlEvent> = all.iter().map(|d| reduction(*d, 14, 20)).collect();
        profile.observe(window(&all), &events);

        let next = *all.last().expect("a day") + Duration::days(1);
        let horizon = Horizon::new(next, 96);
        let unbounded: f64 = profile
            .anticipate(horizon, Anticipation::default())
            .iter()
            .map(AnticipatedReduction::hours)
            .sum();
        assert!(
            unbounded >= 5.9,
            "six hours are anticipated with no cap: {unbounded}"
        );

        let published = AreaExposure {
            netzbereich: "NB-0001".into(),
            postleitzahlen: vec!["10115".into()],
            art: Steuerungsart::Netzorientiert,
            year: 2026,
            month: 3,
            affected_steuve: 42,
            // Twelve hours in the month, so one day of horizon may spend 0,4.
            hours: 12.0,
            intensity_percent: 5.2,
        };
        let capped: f64 = profile
            .anticipate(horizon, Anticipation::default().bounded_by(&published))
            .iter()
            .map(AnticipatedReduction::hours)
            .sum();
        assert!(
            capped < 1.0,
            "the publication bounds it: {capped} against a budget near 0,4"
        );
    }

    #[test]
    fn the_published_intensity_matches_the_formats_own_worked_example() {
        // The format's example, § 2.6: an 11 kW SteuVE reduced to its 4,2 kW
        // minimum for two hours a day comes to 5,2 %. Reproducing their
        // arithmetic to the published decimal is the only way to know this
        // module read the formula the way they wrote it.
        let daily =
            AreaExposure::daily_intensity(Power::from_kw(11.0), &[(Power::from_kw(4.2), 2.0)]);
        assert!(
            (daily * 100.0 - 5.2).abs() < 0.05,
            "the format says 5,2 %: {:.2} %",
            daily * 100.0
        );

        // And under präventive Steuerung the same limitation applies every
        // calendar day, so the monthly figure equals the daily one — which the
        // format states in as many words.
        let month = AreaExposure::monthly_intensity(&[daily; 31], 31);
        assert!((month - daily).abs() < 1e-12);
        assert!(Steuerungsart::Praeventiv.repeats_daily());
        assert!(!Steuerungsart::Netzorientiert.repeats_daily());
    }

    #[test]
    fn an_area_with_no_controllable_capacity_has_no_intensity() {
        // Rather than an infinity from dividing by an installed power of zero,
        // which would poison every figure downstream of it.
        assert!(AreaExposure::daily_intensity(Power::ZERO, &[(Power::ZERO, 2.0)]).abs() < 1e-12);
        assert!(AreaExposure::monthly_intensity(&[1.0], 0).abs() < 1e-12);
    }

    #[test]
    fn a_fortnight_is_not_yet_evidence() {
        // The evidence gate, on the shape that actually reaches it: a household
        // whose box has only just been commissioned. Four days of a repeating
        // window is a real pattern and still too little to spend money on.
        let mut profile = StressProfile::new(LAND);
        let all = days(4);
        let events: Vec<ControlEvent> = all.iter().map(|d| reduction(*d, 17, 19)).collect();
        profile.observe(window(&all), &events);

        let next = *all.last().expect("a day") + Duration::days(1);
        assert!(
            profile
                .anticipate(Horizon::new(next, 96), Anticipation::default())
                .is_empty(),
            "four observations is below `min_observations`"
        );
        // …and the frequency is still *counted*, because refusing to act on a
        // small sample is not the same as refusing to observe it.
        assert!(
            profile
                .frequency(DayType::Workday, 17 * 4)
                .is_some_and(|(f, n)| (f - 1.0).abs() < 1e-9 && n < 8)
        );
    }

    #[test]
    fn a_box_that_was_off_anticipates_less_rather_than_more() {
        // The denominator this module cannot see: a box that was unplugged for
        // half the window has those quarter hours counted as observed and not
        // reduced, so its frequency comes out low and it anticipates *less*.
        // That is the safe direction — every anticipation costs a little and
        // none of them can breach anything — and it is why `observe` makes the
        // caller state the window.
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
