//! The record an operator has to be able to produce.
//!
//! `[BK6-22-300 A1 7.2]`: the operator of a controllable device must be able to
//! show, for an individual case and in a way the network operator can follow,
//! that a commanded reduction was actually carried out. `[A1 7.3]`: the records
//! are kept for at least **two years** after the measure.
//!
//! That is a low bar and most systems do not clear it, because the information
//! is spread over a device log, a cloud database and nothing at all. Here it is
//! one append-only record per control event, written by the guard plane at the
//! moment it acts, with the numbers that decide whether the reduction was lawful:
//! what was commanded, what minimum the customer was owed, when it took effect,
//! and what was actually drawn while it lasted.

use hems_core::prelude::{AssetId, GuardRule, Power};
use time::{Duration, OffsetDateTime};

use crate::para14a::ControlMode;

/// The retention period of `[A1 7.3]`.
pub const RETENTION: Duration = Duration::days(2 * 365);

/// A sample of what the site actually drew while a limit was in force.
///
/// `[A1 7.2]` asks the operator to make the implementation followable; a
/// minute-resolution trace of the controlled quantity is the smallest thing that
/// answers it without argument.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ComplianceSample {
    /// When.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
    /// The netzwirksamer Leistungsbezug at that moment, `[A1 2.3]`.
    pub netzwirksam: Power,
    /// The ceiling in force.
    pub ceiling: Power,
}

/// How far over a ceiling a sample may sit before it counts as an overshoot.
///
/// A compliance sample compares two quantities that were both *computed from
/// measurements*, through a subtraction (`[A1 2.3]`) that cancels most of two
/// large numbers. The result carries the arithmetic's own noise, and a strict
/// comparison turns a picowatt of it into a reported breach of a network
/// operator's instruction — which is how a day that respected a reduction
/// throughout came to report `NO, by 0 W`.
///
/// One watt is four orders of magnitude below anything a household device
/// resolves, nine orders below a § 14a ceiling, and far inside the accuracy
/// class of any meter that could produce the inputs. It is deliberately not
/// larger: a real overshoot in this system is hundreds of watts, and a tolerance
/// that could hide one would be worse than no record at all.
pub const COMPLIANCE_TOLERANCE: Power = Power::new_const(1.0);

impl ComplianceSample {
    /// Whether this sample is inside the commanded ceiling.
    #[must_use]
    pub fn is_compliant(&self) -> bool {
        self.netzwirksam <= self.ceiling + COMPLIANCE_TOLERANCE
    }
}

/// One ceiling the network operator commanded, and when.
///
/// A reduction is not one number: an operator may tighten or relax it while it
/// runs, and each of those is a separate instruction with its own timestamp and
/// its own answer to "was it below the minimum this customer is owed?". Keeping
/// only the first value throws away every instruction after it and makes a
/// two-hour event look like one command.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CommandedCeiling {
    /// When it was received.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
    /// The ceiling on the netzwirksamer Leistungsbezug.
    pub value: Power,
    /// The minimum the customer was owed under `[A1 4.5]` at that moment.
    pub minimum_power: Power,
}

impl CommandedCeiling {
    /// Whether this instruction went below the minimum of `[A1 4.5]`.
    #[must_use]
    pub fn below_minimum(&self) -> bool {
        self.value < self.minimum_power
    }
}

/// How a reduction came to be in effect.
///
/// Both satisfy `[A1 4.2]`; they are different facts, and the record has to be
/// able to tell a network operator which one it is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Action {
    /// The manager issued setpoints that brought the household inside the
    /// ceiling.
    Commanded,
    /// The household was already inside it and nothing had to be sent.
    ///
    /// The ordinary outcome of a reduction on a house that was not doing much,
    /// and it is a *result* rather than an absence: the operator's instruction
    /// was honoured from the first second.
    AlreadyBelow,
}

/// One control event, from the moment it arrived to the moment it was released.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ControlEvent {
    /// Which rule produced it.
    pub rule: GuardRule,
    /// Whether the network operator addressed devices individually or the energy
    /// management system as a whole, `[A1 4.4]`.
    pub mode: ControlMode,
    /// When the command was received.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub received_at: OffsetDateTime,
    /// When the reduction took effect.
    ///
    /// `[A1 4.2 S. 5]` puts the duty to act without delay on the operator, and
    /// the gap between these two timestamps is the number that shows it was met.
    ///
    /// **"Took effect" is not the same as "was commanded."** A reduction to
    /// 4,2 kW on a house already drawing 2,1 kW requires nothing to be sent to
    /// anything: the manager is inside the limit from the instant it arrives,
    /// and there is no setpoint because there is no change to make. Timing to
    /// the first setpoint instead reports a message queue rather than a house,
    /// and a network operator reads the gap as a breach of `[A1 4.2]`.
    ///
    /// [`ControlEvent::acted`] says which of the two happened, because an
    /// operator asking "what did you do?" is owed "nothing, we were at 2,1 kW"
    /// rather than a blank.
    #[cfg_attr(
        feature = "serde",
        serde(default, with = "time::serde::rfc3339::option")
    )]
    pub applied_at: Option<OffsetDateTime>,
    /// How the reduction came to be in effect.
    #[cfg_attr(feature = "serde", serde(default))]
    pub acted: Option<Action>,
    /// When the limit stopped applying.
    #[cfg_attr(
        feature = "serde",
        serde(default, with = "time::serde::rfc3339::option")
    )]
    pub released_at: Option<OffsetDateTime>,
    /// Every ceiling commanded while the event lasted, oldest first.
    ///
    /// Never empty: an event only exists because a ceiling arrived.
    pub ceilings: Vec<CommandedCeiling>,
    /// Which devices it was distributed over.
    pub assets: Vec<AssetId>,
    /// Where the command came from — the Steuerbox's EEBUS SKI, or the name of
    /// the relay input.
    #[cfg_attr(feature = "serde", serde(default))]
    pub source: Option<String>,
    /// What was drawn while it lasted.
    #[cfg_attr(feature = "serde", serde(default))]
    pub samples: Vec<ComplianceSample>,
}

impl ControlEvent {
    /// A newly received command.
    #[must_use]
    pub fn received(
        rule: GuardRule,
        mode: ControlMode,
        ceiling: Power,
        minimum_power: Power,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            rule,
            mode,
            received_at: at,
            applied_at: None,
            acted: None,
            released_at: None,
            ceilings: vec![CommandedCeiling {
                at,
                value: ceiling,
                minimum_power,
            }],
            assets: Vec::new(),
            source: None,
            samples: Vec::new(),
        }
    }

    /// The first ceiling commanded — the one that started the event.
    #[must_use]
    pub fn first_ceiling(&self) -> Power {
        self.ceilings.first().map_or(Power::ZERO, |c| c.value)
    }

    /// The ceiling in force at the end of the event.
    #[must_use]
    pub fn last_ceiling(&self) -> Power {
        self.ceilings.last().map_or(Power::ZERO, |c| c.value)
    }

    /// The strictest ceiling the event ever carried — the number a network
    /// operator asks about when they ask how far the house was turned down.
    #[must_use]
    pub fn strictest_ceiling(&self) -> Power {
        self.ceilings
            .iter()
            .map(|c| c.value)
            .reduce(Power::min)
            .unwrap_or(Power::ZERO)
    }

    /// How long it took to act on the command.
    #[must_use]
    pub fn latency(&self) -> Option<Duration> {
        self.applied_at.map(|applied| applied - self.received_at)
    }

    /// Whether the commanded ceiling was below the minimum the customer is owed.
    ///
    /// hems applies such a command anyway — refusing a network operator's
    /// instruction is not a decision a box should take on its own, and
    /// `[A1 4.6 S. 2]` even requires going to the next possible lower value when
    /// the exact one cannot be reached. But it is recorded, because the entitlement
    /// under `[A1 4.5]` is the customer's and the argument is theirs to have.
    #[must_use]
    pub fn below_minimum(&self) -> bool {
        self.ceilings.iter().any(CommandedCeiling::below_minimum)
    }

    /// Whether every sample stayed inside the ceiling.
    #[must_use]
    pub fn fully_compliant(&self) -> bool {
        self.samples.iter().all(ComplianceSample::is_compliant)
    }

    /// The worst overshoot recorded, if any.
    #[must_use]
    pub fn worst_overshoot(&self) -> Option<Power> {
        self.samples
            .iter()
            .filter(|s| !s.is_compliant())
            .map(|s| s.netzwirksam - s.ceiling)
            .reduce(Power::max)
    }

    /// How long the event lasted, if it has ended.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.released_at.map(|end| end - self.received_at)
    }

    /// Whether the record may be discarded at `now` under `[A1 7.3]`.
    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        let end = self.released_at.unwrap_or(self.received_at);
        now - end > RETENTION
    }
}

/// Builds the record of `[A1 7.2]` as the control loop runs.
///
/// An operator has to be able to show, for an individual case and in a way the
/// network operator can follow, that a commanded reduction was actually carried
/// out — and to keep that for two years `[A1 7.3]`. Almost no system can: the
/// evidence is spread across a device log, a cloud database, and nothing at all.
///
/// This is the whole mechanism. Feed it every tick; it opens a record when a
/// ceiling appears, samples what was actually drawn while it lasts, and closes
/// it when the ceiling goes away. Nothing else has to remember to do anything.
///
/// It is sans-I/O like the rest: `now` is a parameter, so a two-year retention
/// policy is a unit test rather than a thing that happens in two years.
#[derive(Debug, Clone, Default)]
pub struct EvidenceRecorder {
    open: Option<ControlEvent>,
    closed: Vec<ControlEvent>,
    /// How often to keep a compliance sample. One a minute is what `[A1 7.2]`
    /// can be answered with; one a second would be a hundred megabytes a year
    /// to say the same thing.
    sample_every: Duration,
    last_sample: Option<OffsetDateTime>,
}

/// What the recorder saw on one tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    /// The ceiling in force, or `None` when no reduction applies.
    pub ceiling: Option<Power>,
    /// Which rule produced it.
    pub rule: GuardRule,
    /// How the network operator addresses this site.
    pub mode: ControlMode,
    /// The minimum the customer is owed, `[A1 4.5]`.
    pub minimum_power: Power,
    /// The netzwirksamer Leistungsbezug measured right now, `[A1 2.3]`.
    pub netzwirksam: Power,
    /// Whether the resulting setpoints have been issued to the devices.
    pub applied: bool,
}

impl EvidenceRecorder {
    /// A recorder that samples once a minute.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sample_every: Duration::minutes(1),
            ..Self::default()
        }
    }

    /// Record one tick.
    ///
    /// Returns a reference to the record that was just closed, if this tick
    /// ended one — the moment to persist it and to emit
    /// `de.hems.grid.evidence.recorded`.
    pub fn observe(
        &mut self,
        observation: Observation,
        assets: &[AssetId],
        now: OffsetDateTime,
    ) -> Option<&ControlEvent> {
        match (observation.ceiling, self.open.is_some()) {
            // A reduction begins.
            (Some(ceiling), false) => {
                let mut event = ControlEvent::received(
                    observation.rule,
                    observation.mode,
                    ceiling,
                    observation.minimum_power,
                    now,
                );
                event.assets = assets.to_vec();
                self.open = Some(event);
                self.last_sample = None;
            }
            // A reduction ends.
            (None, true) => {
                let mut event = self.open.take().expect("checked");
                event.released_at = Some(now);
                self.closed.push(event);
                return self.closed.last();
            }
            _ => {}
        }

        if let Some(event) = self.open.as_mut() {
            if event.applied_at.is_none()
                && let Some(ceiling) = observation.ceiling
            {
                // Commanded, or already inside it — see `ControlEvent::acted`.
                // A household that never had to be told anything satisfies
                // `[A1 4.2]` at the instant the command arrives.
                let action = if observation.applied {
                    Some(Action::Commanded)
                } else if observation.netzwirksam <= ceiling {
                    Some(Action::AlreadyBelow)
                } else {
                    None
                };
                if action.is_some() {
                    event.applied_at = Some(now);
                    event.acted = action;
                }
            }
            // A ceiling that changes without the reduction ending is a new
            // instruction, and the record has to carry it: an operator asking
            // "what did you tell them at 17:40?" needs the answer, not the
            // first value of the hour.
            if let Some(ceiling) = observation.ceiling
                && event.last_ceiling() != ceiling
            {
                event.ceilings.push(CommandedCeiling {
                    at: now,
                    value: ceiling,
                    minimum_power: observation.minimum_power,
                });
            }
            let due = self
                .last_sample
                .is_none_or(|last| now - last >= self.sample_every);
            if due && let Some(ceiling) = observation.ceiling {
                event.samples.push(ComplianceSample {
                    at: now,
                    netzwirksam: observation.netzwirksam,
                    ceiling,
                });
                self.last_sample = Some(now);
            }
        }
        None
    }

    /// The record currently being built, if a reduction is in force.
    #[must_use]
    pub fn open(&self) -> Option<&ControlEvent> {
        self.open.as_ref()
    }

    /// Every finished record, oldest first.
    #[must_use]
    pub fn closed(&self) -> &[ControlEvent] {
        &self.closed
    }

    /// Take every finished record, leaving the recorder holding only the one
    /// still open.
    ///
    /// For a consumer that **persists** them, which is what a box does: the
    /// store is the record and this is a buffer, so a recorder that kept a copy
    /// of everything it had ever built would hold every minute-resolution trace
    /// of two years in memory for nothing.
    ///
    /// Distinct from [`EvidenceRecorder::prune`], which is the right call for a
    /// consumer that has nowhere else to put them — a simulated day, which
    /// reports on its own records at the end.
    pub fn take_closed(&mut self) -> Vec<ControlEvent> {
        core::mem::take(&mut self.closed)
    }

    /// Drop records the retention period no longer covers, `[A1 7.3]`.
    ///
    /// Two years is a floor, not a ceiling — but keeping them for ever is a
    /// data-protection problem rather than a compliance virtue.
    pub fn prune(&mut self, now: OffsetDateTime) {
        self.closed.retain(|e| !e.is_expired(now));
    }

    /// Whether every finished record stayed inside its ceiling.
    #[must_use]
    pub fn fully_compliant(&self) -> bool {
        self.closed.iter().all(ControlEvent::fully_compliant)
    }

    /// The slowest a reduction was acted on, `[A1 4.2 S. 5]`.
    #[must_use]
    pub fn worst_latency(&self) -> Option<Duration> {
        self.closed.iter().filter_map(ControlEvent::latency).max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 17:02:00 UTC);

    fn event() -> ControlEvent {
        ControlEvent::received(
            GuardRule::Lpc,
            ControlMode::Ems,
            Power::from_kw(7.56),
            Power::from_kw(7.56),
            T0,
        )
    }

    #[test]
    fn latency_is_the_gap_between_receiving_and_acting() {
        let mut e = event();
        e.applied_at = Some(T0 + Duration::seconds(3));
        assert_eq!(e.latency(), Some(Duration::seconds(3)));
    }

    #[test]
    fn a_command_below_the_minimum_is_flagged_but_not_refused() {
        let mut e = event();
        e.ceilings[0].value = Power::from_kw(4.0);
        assert!(e.below_minimum(), "7,56 kW was owed, 4,0 kW was commanded");
    }

    #[test]
    fn compliance_is_judged_sample_by_sample() {
        let mut e = event();
        e.samples = vec![
            ComplianceSample {
                at: T0,
                netzwirksam: Power::from_kw(7.0),
                ceiling: e.first_ceiling(),
            },
            ComplianceSample {
                at: T0 + Duration::minutes(1),
                netzwirksam: Power::from_kw(9.0),
                ceiling: e.first_ceiling(),
            },
        ];
        assert!(!e.fully_compliant());
        assert!((e.worst_overshoot().unwrap().kw() - 1.44).abs() < 1e-9);
    }

    #[test]
    fn records_expire_two_years_after_the_measure_ended() {
        let mut e = event();
        e.released_at = Some(T0 + Duration::hours(1));
        assert!(!e.is_expired(T0 + Duration::days(729)));
        assert!(e.is_expired(T0 + Duration::days(731)));
    }

    #[test]
    fn an_open_event_is_retained_from_when_it_started() {
        let e = event();
        assert!(!e.is_expired(T0 + Duration::days(700)));
        assert!(e.is_expired(T0 + Duration::days(731)));
    }
}

#[cfg(test)]
mod recorder_tests {
    use super::*;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

    fn observation(ceiling: Option<f64>, netzwirksam: f64) -> Observation {
        Observation {
            ceiling: ceiling.map(Power::from_kw),
            rule: GuardRule::Lpc,
            mode: ControlMode::Ems,
            minimum_power: Power::from_kw(7.56),
            netzwirksam: Power::from_kw(netzwirksam),
            applied: true,
        }
    }

    fn assets() -> Vec<AssetId> {
        vec![
            AssetId::new("wallbox").unwrap(),
            AssetId::new("battery").unwrap(),
        ]
    }

    /// Run `minutes` of a reduction and return the recorder.
    fn run(ceiling: Option<f64>, netzwirksam: f64, minutes: i64) -> EvidenceRecorder {
        let mut r = EvidenceRecorder::new();
        for m in 0..minutes {
            r.observe(
                observation(ceiling, netzwirksam),
                &assets(),
                T0 + Duration::minutes(m),
            );
        }
        r
    }

    #[test]
    fn a_reduction_opens_a_record_and_releasing_it_closes_one() {
        let mut r = run(Some(7.56), 6.0, 90);
        assert!(r.open().is_some());
        assert!(r.closed().is_empty());

        let closed = r
            .observe(
                observation(None, 0.0),
                &assets(),
                T0 + Duration::minutes(90),
            )
            .expect("the record closed");
        assert_eq!(closed.duration(), Some(Duration::minutes(90)));
        assert!(r.open().is_none());
        assert_eq!(r.closed().len(), 1);
    }

    #[test]
    fn the_record_carries_a_sample_a_minute() {
        let r = run(Some(7.56), 6.0, 90);
        // One at the start plus one a minute after.
        assert_eq!(r.open().unwrap().samples.len(), 90);
    }

    #[test]
    fn an_overshoot_is_recorded_rather_than_smoothed_away() {
        let mut r = EvidenceRecorder::new();
        for m in 0..10 {
            // One minute over the ceiling in the middle.
            let drawn = if m == 5 { 9.0 } else { 6.0 };
            r.observe(
                observation(Some(7.56), drawn),
                &assets(),
                T0 + Duration::minutes(m),
            );
        }
        r.observe(
            observation(None, 0.0),
            &assets(),
            T0 + Duration::minutes(10),
        );
        assert!(!r.fully_compliant());
        let worst = r.closed()[0].worst_overshoot().unwrap();
        assert!((worst.kw() - 1.44).abs() < 1e-9, "{worst}");
    }

    #[test]
    fn the_record_says_how_quickly_the_reduction_was_acted_on() {
        // [A1 4.2 S. 5]: without delay. The gap between receiving and taking
        // effect is the number that shows it — and the household starts *above*
        // the ceiling here, so taking effect really does need a command.
        let mut r = EvidenceRecorder::new();
        let mut first = observation(Some(7.56), 9.0);
        first.applied = false;
        r.observe(first, &assets(), T0);
        r.observe(
            observation(Some(7.56), 6.0),
            &assets(),
            T0 + Duration::seconds(4),
        );
        r.observe(
            observation(None, 0.0),
            &assets(),
            T0 + Duration::minutes(30),
        );
        assert_eq!(r.worst_latency(), Some(Duration::seconds(4)));
        assert_eq!(r.closed()[0].acted, Some(Action::Commanded));
    }

    #[test]
    fn a_household_already_under_the_ceiling_acted_on_it_at_once() {
        // A reduction to 7,56 kW on a house drawing 6 kW needs nothing sent to
        // anything: no setpoint, because there is no change to make. Timing to
        // the next thing that happens to move would report the household as
        // minutes late when it was compliant from the first second.
        let mut r = EvidenceRecorder::new();
        let mut quiet = observation(Some(7.56), 6.0);
        quiet.applied = false;
        r.observe(quiet, &assets(), T0);
        r.observe(quiet, &assets(), T0 + Duration::minutes(8));
        r.observe(
            observation(None, 0.0),
            &assets(),
            T0 + Duration::minutes(30),
        );

        assert_eq!(r.worst_latency(), Some(Duration::ZERO));
        assert_eq!(
            r.closed()[0].acted,
            Some(Action::AlreadyBelow),
            "and the record has to say *why* it needed nothing, or an operator \
             reading a zero cannot tell it from a manager that never acted"
        );
    }

    #[test]
    fn a_household_that_never_gets_inside_the_ceiling_records_no_latency_at_all() {
        // The case the tolerant reading must not swallow: a household that stays
        // above the limit has not acted on it, and the record must not invent a
        // timestamp for it.
        let mut r = EvidenceRecorder::new();
        let mut over = observation(Some(7.56), 9.0);
        over.applied = false;
        for m in 0..5 {
            r.observe(over, &assets(), T0 + Duration::minutes(m));
        }
        r.observe(
            observation(None, 0.0),
            &assets(),
            T0 + Duration::minutes(30),
        );
        assert_eq!(r.worst_latency(), None);
        assert_eq!(r.closed()[0].acted, None);
        assert!(!r.fully_compliant(), "and it was not compliant either");
    }

    #[test]
    fn a_command_below_the_minimum_is_flagged_on_the_record() {
        let mut r = EvidenceRecorder::new();
        r.observe(observation(Some(4.0), 3.0), &assets(), T0);
        assert!(
            r.open().unwrap().below_minimum(),
            "7,56 kW was owed, 4,0 kW commanded"
        );
    }

    #[test]
    fn records_are_pruned_once_the_retention_period_passes() {
        let mut r = run(Some(7.56), 6.0, 5);
        r.observe(observation(None, 0.0), &assets(), T0 + Duration::minutes(5));
        assert_eq!(r.closed().len(), 1);
        r.prune(T0 + Duration::days(700));
        assert_eq!(r.closed().len(), 1, "still inside the two years");
        r.prune(T0 + Duration::days(740));
        assert!(r.closed().is_empty());
    }

    #[test]
    fn no_reduction_produces_no_record_at_all() {
        let r = run(None, 0.0, 120);
        assert!(r.open().is_none());
        assert!(r.closed().is_empty());
    }

    #[test]
    fn back_to_back_reductions_are_separate_records() {
        let mut r = EvidenceRecorder::new();
        r.observe(observation(Some(7.56), 6.0), &assets(), T0);
        r.observe(
            observation(None, 0.0),
            &assets(),
            T0 + Duration::minutes(30),
        );
        r.observe(
            observation(Some(4.2), 4.0),
            &assets(),
            T0 + Duration::hours(2),
        );
        r.observe(observation(None, 0.0), &assets(), T0 + Duration::hours(3));
        assert_eq!(r.closed().len(), 2);
        assert_eq!(r.closed()[0].first_ceiling(), Power::from_kw(7.56));
        assert_eq!(r.closed()[1].first_ceiling(), Power::from_kw(4.2));
    }

    #[test]
    fn a_ceiling_that_changes_mid_event_is_a_second_instruction_in_the_same_record() {
        // A network operator may tighten a reduction while it runs. Keeping only
        // the first value makes a two-hour event look like one command, and
        // leaves the household unable to show which instruction was in force
        // when — which is precisely what `[A1 7.2]` asks them to show.
        let mut r = EvidenceRecorder::new();
        r.observe(observation(Some(7.56), 6.0), &assets(), T0);
        r.observe(
            observation(Some(4.2), 8.0),
            &assets(),
            T0 + Duration::minutes(20),
        );
        r.observe(
            observation(None, 0.0),
            &assets(),
            T0 + Duration::minutes(40),
        );

        let event = &r.closed()[0];
        assert_eq!(event.ceilings.len(), 2);
        assert_eq!(event.first_ceiling(), Power::from_kw(7.56));
        assert_eq!(event.last_ceiling(), Power::from_kw(4.2));
        assert_eq!(event.strictest_ceiling(), Power::from_kw(4.2));
        assert_eq!(event.ceilings[1].at, T0 + Duration::minutes(20));
        // The first instruction was lawful, the second was not: 4,2 kW against
        // 8 kW owed. `below_minimum` has to see the whole sequence to say so.
        assert!(!event.ceilings[0].below_minimum());
        assert!(event.ceilings[1].below_minimum());
        assert!(event.below_minimum());
    }
}
