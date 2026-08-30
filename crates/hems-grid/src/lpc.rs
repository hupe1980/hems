//! The EEBUS power-limitation state machine, sans-I/O.
//!
//! This is the mechanism that carries a network operator's § 14a EnWG reduction
//! (Limitation of Power **Consumption**) and a § 9 EEG feed-in reduction
//! (Limitation of Power **Production**) the last few metres, from the FNN
//! Steuerbox to the energy management system. Both use cases are the same
//! machine with the sign flipped, so [`Direction`] parameterises it.
//!
//! Everything here is from `EEBus_UC_TS_LimitationOfPowerConsumption_V1.0.0`
//! (`specs/eebus/`), cited as `[LPC-nnn]`. The five states and their transitions
//! are §§ 2.2, 2.3.2 and 2.3.3; the behaviour table is Table 1.
//!
//! # Why this is a machine and not an `if`
//!
//! Three timers interact, and the interesting behaviour is what happens when
//! they interfere:
//!
//! * the **heartbeat**, sent at least every 60 s in both directions
//!   (`[LPC-005]`, `[LPC-006]`); missing it for 120 s means the Energy Guard is
//!   gone;
//! * the **limit's own duration** — a limit may expire on its own (`[LPC-909]`);
//! * the **Failsafe Duration Minimum**, 2–24 h (`[LPC-022]`), which is *not* a
//!   safety timer but a release valve: after it expires with the heartbeat still
//!   missing the device goes **unlimited** (`[LPC-922]`), because a broken
//!   Steuerbox must not block a heat pump forever.
//!
//! That last rule is the one an implementation written from intuition gets
//! backwards. It is [`LpcState::UnlimitedAutonomous`], and the test
//! `a_dead_energy_guard_eventually_releases_the_device` pins it.
//!
//! # Sans-I/O
//!
//! Feed it events with the time they happened, and call [`LpcMachine::tick`] at
//! or after [`LpcMachine::next_deadline`]. No sockets, no clock, no async: a
//! week of Steuerbox behaviour runs as a unit test in microseconds.

use core::fmt;

use hems_core::prelude::{GuardRule, Power};
use thiserror::Error;
use time::{Duration, OffsetDateTime};

/// The interval at which both sides must send a heartbeat, `[LPC-005]`,
/// `[LPC-006]`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::seconds(60);

/// How long a missing heartbeat is tolerated before the failsafe applies,
/// `[LPC-906]`, `[LPC-914/2]`.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::seconds(120);

/// The shortest Failsafe Duration Minimum the standard allows, `[LPC-022/1]`.
pub const FAILSAFE_DURATION_MIN: Duration = Duration::hours(2);

/// The longest Failsafe Duration Minimum the standard allows, `[LPC-022/1]`.
pub const FAILSAFE_DURATION_MAX: Duration = Duration::hours(24);

/// Which quantity is being limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Direction {
    /// **LPC** — the consumption limit of § 14a EnWG.
    #[default]
    Consumption,
    /// **LPP** — the production limit of § 9 EEG.
    Production,
}

impl Direction {
    /// The guard rule a limit in this direction produces.
    #[must_use]
    pub const fn guard_rule(self) -> GuardRule {
        match self {
            Direction::Consumption => GuardRule::Lpc,
            Direction::Production => GuardRule::Lpp,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::Consumption => f.write_str("LPC"),
            Direction::Production => f.write_str("LPP"),
        }
    }
}

/// The five states of the Controllable System, § 2.3.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LpcState {
    /// Just (re)started. Limited by the failsafe value until the Energy Guard
    /// makes contact — so a device that reboots during a grid emergency comes
    /// back up restrained, not at full power (`[LPC-901/1]`).
    Init,
    /// In contact with the Energy Guard, no limit active (`[LPC-009/2]`).
    UnlimitedControlled,
    /// A limit from the Energy Guard is in force (`[LPC-009/1]`).
    Limited,
    /// The Energy Guard's heartbeat stopped; the failsafe value applies.
    Failsafe,
    /// Out of contact for long enough that the failsafe was released. The device
    /// runs as if no external limitation existed (`[LPC-922]`).
    UnlimitedAutonomous,
}

impl LpcState {
    /// Whether the Energy Guard is considered present.
    #[must_use]
    pub const fn is_controlled(self) -> bool {
        matches!(self, LpcState::UnlimitedControlled | LpcState::Limited)
    }
}

impl fmt::Display for LpcState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LpcState::Init => "init",
            LpcState::UnlimitedControlled => "unlimited/controlled",
            LpcState::Limited => "limited",
            LpcState::Failsafe => "failsafe",
            LpcState::UnlimitedAutonomous => "unlimited/autonomous",
        };
        f.write_str(s)
    }
}

/// A limit as the Energy Guard writes it, `[LPC-011]`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "state"))]
pub enum LimitWrite {
    /// No limit — the device may draw (or feed in) what it likes.
    Deactivated,
    /// A limit, optionally with a validity period after which it lapses by
    /// itself (`[LPC-909]`).
    Activated {
        /// The ceiling, as a non-negative magnitude.
        value: Power,
        /// How long it is valid. `None` means until revoked.
        duration: Option<Duration>,
    },
}

/// What the Energy Guard or the local configuration can tell the machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LpcEvent {
    /// The Controllable System (re)started, § 2.2.
    Restart,
    /// A heartbeat arrived from the Energy Guard, `[LPC-031]`.
    Heartbeat,
    /// The Energy Guard wrote the active power limit, `[LPC-011]`.
    Limit(LimitWrite),
    /// The Energy Guard wrote the failsafe limit, `[LPC-021/2]`.
    FailsafeLimit(Power),
    /// The Energy Guard wrote the Failsafe Duration Minimum, `[LPC-022/2]`.
    FailsafeDuration(Duration),
}

/// Why a write was refused, `[LPC-003]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Nack {
    /// "A limit lower than 0W SHALL be rejected" (§ 2.2).
    #[error("a limit below zero is not a limit")]
    NegativeLimit,
    /// A limit or failsafe value that is not a finite number.
    #[error("limit is not a finite value")]
    NotFinite,
    /// In `init`, `failsafe` and `unlimited/autonomous` a write is only accepted
    /// after a heartbeat has re-established contact.
    #[error("no heartbeat has been received in this state yet")]
    NoHeartbeatYet,
    /// The Failsafe Duration Minimum must lie between 2 and 24 hours,
    /// `[LPC-022/3]`.
    #[error("failsafe duration minimum must be between 2 and 24 hours")]
    FailsafeDurationOutOfRange,
    /// The value exceeds this device's accepted maximum. Per `[LPC-022/4, /5]`
    /// the machine refuses the write and then sets the value to its own maximum,
    /// so the Energy Guard's intent — "as long as you can" — is still honoured.
    #[error("failsafe duration minimum exceeds this device's maximum")]
    FailsafeDurationTooLong,
}

/// The answer to a write, `[LPC-002]` / `[LPC-003]`.
pub type Ack = Result<(), Nack>;

/// What handling an event produced.
#[derive(Debug, Clone, Copy, PartialEq)]
#[must_use = "the Energy Guard is owed the acknowledgement"]
pub struct Outcome {
    /// The acknowledgement to send back, `[LPC-002]` / `[LPC-003]`.
    pub ack: Ack,
    /// The state change it caused, if any.
    pub transition: Option<Transition>,
}

impl Outcome {
    /// Accepted, no state change.
    const fn ok() -> Self {
        Self {
            ack: Ok(()),
            transition: None,
        }
    }

    /// Accepted, with a state change.
    const fn moved(transition: Option<Transition>) -> Self {
        Self {
            ack: Ok(()),
            transition,
        }
    }

    /// Refused.
    const fn nack(nack: Nack) -> Self {
        Self {
            ack: Err(nack),
            transition: None,
        }
    }

    /// Whether the write was accepted.
    pub fn is_accepted(&self) -> bool {
        self.ack.is_ok()
    }
}

/// What the Controllable System accepts and how it behaves.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LpcConfig {
    /// Consumption or production.
    pub direction: Direction,
    /// The limit that applies in `init` and `failsafe`, `[LPC-021]`.
    ///
    /// Pre-configured by the manufacturer or installer; the Energy Guard may
    /// change it. Setting it to the device's nominal power — a common default —
    /// makes the failsafe a no-op, which is legal and is exactly what an
    /// operator should be shown before they agree to it.
    pub failsafe_limit: Power,
    /// How long the failsafe holds before the device frees itself, `[LPC-022]`.
    pub failsafe_duration_minimum: Duration,
    /// The largest Failsafe Duration Minimum this device accepts from the Energy
    /// Guard, `[LPC-022/4]`. Between the pre-configured value and 24 hours.
    pub failsafe_duration_maximum: Duration,
    /// Whether this Controllable System is a customer energy manager.
    ///
    /// A CEM is allowed to exceed the failsafe limit while uncontrollable loads
    /// or self-protection prevent it from keeping it (§ 2.2); a single appliance
    /// is not. hems is a CEM, so this is `true` for the site machine and `false`
    /// for the machines it runs towards individual devices.
    pub is_cem: bool,
}

impl Default for LpcConfig {
    fn default() -> Self {
        Self {
            direction: Direction::Consumption,
            failsafe_limit: Power::ZERO,
            failsafe_duration_minimum: FAILSAFE_DURATION_MIN,
            failsafe_duration_maximum: FAILSAFE_DURATION_MAX,
            is_cem: true,
        }
    }
}

/// A state change, for the log and the evidence record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transition {
    /// Where it came from.
    pub from: LpcState,
    /// Where it went.
    pub to: LpcState,
    /// When.
    pub at: OffsetDateTime,
}

/// The Controllable System half of the LPC/LPP use case.
#[derive(Debug, Clone)]
pub struct LpcMachine {
    config: LpcConfig,
    state: LpcState,
    entered_at: OffsetDateTime,
    /// The most recent heartbeat, whenever it arrived.
    last_heartbeat: Option<OffsetDateTime>,
    /// The first heartbeat since entering the current state — starts the 120 s
    /// window in which a write must follow (§ 2.3.2, `unlimited/autonomous`).
    heartbeat_in_state: Option<OffsetDateTime>,
    /// The limit in force while [`LpcState::Limited`].
    limit: Option<Power>,
    /// When that limit lapses by itself, `[LPC-909]`.
    limit_expires_at: Option<OffsetDateTime>,
}

impl LpcMachine {
    /// A machine that has just started, in [`LpcState::Init`].
    #[must_use]
    pub fn new(config: LpcConfig, now: OffsetDateTime) -> Self {
        Self {
            config,
            state: LpcState::Init,
            entered_at: now,
            last_heartbeat: None,
            heartbeat_in_state: None,
            limit: None,
            limit_expires_at: None,
        }
    }

    /// The current state.
    #[must_use]
    pub const fn state(&self) -> LpcState {
        self.state
    }

    /// The configuration, including any values the Energy Guard has written.
    #[must_use]
    pub const fn config(&self) -> &LpcConfig {
        &self.config
    }

    /// The limit in force, or `None` when the device is unlimited.
    ///
    /// This is the one number the guard plane consumes; everything else in this
    /// module exists to compute it correctly.
    #[must_use]
    pub fn effective_limit(&self) -> Option<Power> {
        match self.state {
            LpcState::Init | LpcState::Failsafe => Some(self.config.failsafe_limit),
            LpcState::Limited => self.limit,
            LpcState::UnlimitedControlled | LpcState::UnlimitedAutonomous => None,
        }
    }

    /// When the limit in force lapses of its own accord, if it does.
    ///
    /// `[LPC-909]` lets the Energy Guard send a duration with a limit, and the
    /// failsafe releases after its own minimum `[LPC-022]`. Both are ordinary
    /// facts about the *future* and the planner has every right to them: a
    /// ninety-minute reduction applied to a forty-eight-hour plan is a different
    /// plan from the one the network operator asked for, and it is the more
    /// expensive one.
    ///
    /// `None` means either that nothing is limiting the device or that the limit
    /// has no end the box can name — which is the honest answer for a limit sent
    /// without a duration, and the one that keeps the plan conservative.
    #[must_use]
    pub fn limit_ends_at(&self) -> Option<OffsetDateTime> {
        match self.state {
            LpcState::Limited => self.limit_expires_at,
            // The manager is holding *itself* down for want of an Energy Guard,
            // and it knows when it will stop: `[LPC-922]` releases the device
            // once the Failsafe Duration Minimum has run.
            LpcState::Init | LpcState::Failsafe => {
                Some(self.entered_at + self.config.failsafe_duration_minimum)
            }
            LpcState::UnlimitedControlled | LpcState::UnlimitedAutonomous => None,
        }
    }

    /// When [`LpcMachine::tick`] must next be called, if a timer is running.
    ///
    /// The caller sleeps until this instant (or until the next event, whichever
    /// is sooner) — the sans-I/O contract that lets the same machine run on a
    /// tokio task, in a simulation and in a test with virtual time.
    #[must_use]
    pub fn next_deadline(&self) -> Option<OffsetDateTime> {
        let heartbeat_deadline = |from: OffsetDateTime| from + HEARTBEAT_TIMEOUT;
        match self.state {
            // No heartbeat *and* write within 120 s of starting → autonomous.
            LpcState::Init => Some(heartbeat_deadline(self.entered_at)),
            // Contact is lost 120 s after the last heartbeat.
            LpcState::UnlimitedControlled => self.last_heartbeat.map(heartbeat_deadline),
            // Either the limit lapses or contact is lost, whichever comes first.
            LpcState::Limited => {
                match (
                    self.limit_expires_at,
                    self.last_heartbeat.map(heartbeat_deadline),
                ) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                }
            }
            // The failsafe is released after its minimum duration; a heartbeat
            // without a following write also releases it after 120 s.
            LpcState::Failsafe => {
                let release = self.entered_at + self.config.failsafe_duration_minimum;
                match self.heartbeat_in_state.map(heartbeat_deadline) {
                    Some(hb) => Some(release.min(hb)),
                    None => Some(release),
                }
            }
            LpcState::UnlimitedAutonomous => None,
        }
    }

    /// Advance the timers to `now`.
    ///
    /// Returns the transition it made, if any. Calling it more often than
    /// [`LpcMachine::next_deadline`] asks is harmless.
    pub fn tick(&mut self, now: OffsetDateTime) -> Option<Transition> {
        let contact_lost = self
            .last_heartbeat
            .is_none_or(|hb| now - hb > HEARTBEAT_TIMEOUT);

        let next = match self.state {
            // [LPC-906] no heartbeat *and* limit write within 120 s of starting.
            LpcState::Init if now - self.entered_at > HEARTBEAT_TIMEOUT => {
                Some(LpcState::UnlimitedAutonomous)
            }
            // Contact lost → failsafe.
            LpcState::UnlimitedControlled | LpcState::Limited if contact_lost => {
                Some(LpcState::Failsafe)
            }
            // [LPC-909] the limit lapsed on its own; contact is still good.
            LpcState::Limited if self.limit_expires_at.is_some_and(|expiry| now >= expiry) => {
                Some(LpcState::UnlimitedControlled)
            }
            LpcState::Failsafe => {
                let min_elapsed = now - self.entered_at >= self.config.failsafe_duration_minimum;
                // A heartbeat arrived but no write followed within 120 s.
                let contact_without_command = self
                    .heartbeat_in_state
                    .is_some_and(|hb| now - hb > HEARTBEAT_TIMEOUT);
                // [LPC-922] the release valve: a dead Energy Guard must not hold
                // the device down for ever.
                (min_elapsed || contact_without_command).then_some(LpcState::UnlimitedAutonomous)
            }
            _ => None,
        }?;

        Some(self.transition_to(next, now))
    }

    /// Feed the machine an event.
    ///
    /// Returns the acknowledgement the Energy Guard is owed (`[LPC-002]` /
    /// `[LPC-003]`) and the transition, if any.
    pub fn handle(&mut self, event: LpcEvent, now: OffsetDateTime) -> Outcome {
        match event {
            LpcEvent::Restart => {
                self.last_heartbeat = None;
                self.limit = None;
                self.limit_expires_at = None;
                Outcome::moved(Some(self.transition_to(LpcState::Init, now)))
            }

            LpcEvent::Heartbeat => {
                self.last_heartbeat = Some(now);
                if self.heartbeat_in_state.is_none() {
                    self.heartbeat_in_state = Some(now);
                }
                // A heartbeat alone never leaves `init`, `failsafe` or
                // `unlimited/autonomous`: § 2.2 requires a *write* to follow.
                Outcome::ok()
            }

            LpcEvent::Limit(write) => self.handle_limit(write, now),

            LpcEvent::FailsafeLimit(value) => {
                if !value.is_finite() {
                    return Outcome::nack(Nack::NotFinite);
                }
                if value < Power::ZERO {
                    return Outcome::nack(Nack::NegativeLimit);
                }
                self.config.failsafe_limit = value;
                Outcome::ok()
            }

            LpcEvent::FailsafeDuration(duration) => {
                if !(FAILSAFE_DURATION_MIN..=FAILSAFE_DURATION_MAX).contains(&duration) {
                    return Outcome::nack(Nack::FailsafeDurationOutOfRange);
                }
                if duration > self.config.failsafe_duration_maximum {
                    // [LPC-022/5] refuse, then adopt our own maximum.
                    self.config.failsafe_duration_minimum = self.config.failsafe_duration_maximum;
                    return Outcome::nack(Nack::FailsafeDurationTooLong);
                }
                self.config.failsafe_duration_minimum = duration;
                Outcome::ok()
            }
        }
    }

    fn handle_limit(&mut self, write: LimitWrite, now: OffsetDateTime) -> Outcome {
        // § 2.2: in these states a write is only accepted once a heartbeat has
        // re-established contact.
        let needs_contact = matches!(
            self.state,
            LpcState::Init | LpcState::Failsafe | LpcState::UnlimitedAutonomous
        );
        if needs_contact && self.heartbeat_in_state.is_none() {
            return Outcome::nack(Nack::NoHeartbeatYet);
        }

        match write {
            LimitWrite::Deactivated => {
                self.limit = None;
                self.limit_expires_at = None;
                // [LPC-905], [LPC-920] — a deactivated limit means "controlled,
                // but free", not "no longer supervised".
                let t = (self.state != LpcState::UnlimitedControlled)
                    .then(|| self.transition_to(LpcState::UnlimitedControlled, now));
                Outcome::moved(t)
            }
            LimitWrite::Activated { value, duration } => {
                if !value.is_finite() {
                    return Outcome::nack(Nack::NotFinite);
                }
                if value < Power::ZERO {
                    // § 2.2: "A limit lower than 0W SHALL be rejected."
                    return Outcome::nack(Nack::NegativeLimit);
                }
                self.limit = Some(value);
                self.limit_expires_at = duration.map(|d| now + d);
                // A zero-length limit has already expired: honour the write, but
                // do not enter `limited` for an instant that has passed.
                if self.limit_expires_at.is_some_and(|e| e <= now) {
                    self.limit = None;
                    self.limit_expires_at = None;
                    let t = (self.state != LpcState::UnlimitedControlled)
                        .then(|| self.transition_to(LpcState::UnlimitedControlled, now));
                    return Outcome::moved(t);
                }
                // [LPC-904], [LPC-910], [LPC-919]
                let t = (self.state != LpcState::Limited)
                    .then(|| self.transition_to(LpcState::Limited, now));
                Outcome::moved(t)
            }
        }
    }

    fn transition_to(&mut self, to: LpcState, at: OffsetDateTime) -> Transition {
        let from = core::mem::replace(&mut self.state, to);
        self.entered_at = at;
        self.heartbeat_in_state = None;
        if to != LpcState::Limited {
            self.limit = None;
            self.limit_expires_at = None;
        }
        Transition { from, to, at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

    fn machine() -> LpcMachine {
        LpcMachine::new(
            LpcConfig {
                failsafe_limit: Power::from_kw(4.2),
                ..LpcConfig::default()
            },
            T0,
        )
    }

    /// Bring a machine into `limited` the way a real Steuerbox does.
    fn limited_at(m: &mut LpcMachine, t: OffsetDateTime, kw: f64) {
        let _ = m.handle(LpcEvent::Heartbeat, t);
        let out = m.handle(
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(kw),
                duration: None,
            }),
            t,
        );
        assert_eq!(out.ack, Ok(()));
        assert_eq!(m.state(), LpcState::Limited);
    }

    #[test]
    fn a_restarted_device_comes_back_limited_not_free() {
        // [LPC-901/1]: the point of `init`. A heat pump that reboots during a
        // grid emergency must not return at full power.
        let m = machine();
        assert_eq!(m.state(), LpcState::Init);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(4.2)));
    }

    #[test]
    fn init_needs_a_heartbeat_before_it_accepts_a_limit() {
        let mut m = machine();
        let out = m.handle(
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(3.0),
                duration: None,
            }),
            T0,
        );
        assert_eq!(out.ack, Err(Nack::NoHeartbeatYet));
        assert!(out.transition.is_none());
        assert_eq!(m.state(), LpcState::Init);
    }

    #[test]
    fn heartbeat_then_limit_moves_init_to_limited() {
        let mut m = machine();
        limited_at(&mut m, T0 + Duration::seconds(5), 3.0);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(3.0)));
    }

    #[test]
    fn heartbeat_then_deactivated_limit_moves_init_to_unlimited_controlled() {
        let mut m = machine();
        let _ = m.handle(LpcEvent::Heartbeat, T0);
        let out = m.handle(LpcEvent::Limit(LimitWrite::Deactivated), T0);
        assert_eq!(out.ack, Ok(()));
        assert_eq!(out.transition.unwrap().to, LpcState::UnlimitedControlled);
        assert_eq!(m.effective_limit(), None);
    }

    #[test]
    fn init_without_contact_frees_the_device_after_120_seconds() {
        // [LPC-906]: no Energy Guard at all — a device with no Steuerbox on the
        // network must not stay at its failsafe value for ever.
        let mut m = machine();
        assert_eq!(m.next_deadline(), Some(T0 + HEARTBEAT_TIMEOUT));
        assert!(m.tick(T0 + Duration::seconds(119)).is_none());
        let t = m.tick(T0 + Duration::seconds(121)).unwrap();
        assert_eq!(t.to, LpcState::UnlimitedAutonomous);
        assert_eq!(m.effective_limit(), None);
    }

    #[test]
    fn losing_the_heartbeat_while_limited_enters_failsafe() {
        let mut m = machine();
        limited_at(&mut m, T0, 3.0);
        assert_eq!(m.next_deadline(), Some(T0 + HEARTBEAT_TIMEOUT));
        let t = m.tick(T0 + Duration::seconds(121)).unwrap();
        assert_eq!(t.from, LpcState::Limited);
        assert_eq!(t.to, LpcState::Failsafe);
        assert_eq!(
            m.effective_limit(),
            Some(Power::from_kw(4.2)),
            "the failsafe value, not the limit"
        );
    }

    #[test]
    fn a_heartbeat_inside_the_window_keeps_the_limit() {
        let mut m = machine();
        limited_at(&mut m, T0, 3.0);
        for i in 1..10 {
            let t = T0 + Duration::seconds(60 * i);
            let _ = m.handle(LpcEvent::Heartbeat, t);
            assert!(m.tick(t).is_none(), "no transition at minute {i}");
            assert_eq!(m.state(), LpcState::Limited);
        }
    }

    #[test]
    fn a_dead_energy_guard_eventually_releases_the_device() {
        // [LPC-922] and § 2.3.2. The rule an implementation written from
        // intuition gets backwards: the Failsafe Duration Minimum is not a
        // safety timer that keeps the device down, it is the release valve that
        // lets it go once the Energy Guard has clearly failed.
        let mut m = machine();
        limited_at(&mut m, T0, 3.0);
        let failsafe_at = T0 + Duration::seconds(121);
        assert_eq!(m.tick(failsafe_at).unwrap().to, LpcState::Failsafe);

        // Two hours of silence: still limited by the failsafe value.
        let almost = failsafe_at + FAILSAFE_DURATION_MIN - Duration::seconds(1);
        assert!(m.tick(almost).is_none());
        assert_eq!(m.effective_limit(), Some(Power::from_kw(4.2)));

        // Then it frees itself.
        let t = m.tick(failsafe_at + FAILSAFE_DURATION_MIN).unwrap();
        assert_eq!(t.to, LpcState::UnlimitedAutonomous);
        assert_eq!(m.effective_limit(), None);
        assert_eq!(m.next_deadline(), None, "nothing left to wait for");
    }

    #[test]
    fn a_heartbeat_without_a_following_write_also_releases_the_failsafe() {
        // § 2.3.2, third bullet: contact returns but the Energy Guard says
        // nothing about the limit for 120 s.
        let mut m = machine();
        limited_at(&mut m, T0, 3.0);
        let failsafe_at = T0 + Duration::seconds(121);
        m.tick(failsafe_at);
        let hb = failsafe_at + Duration::minutes(5);
        let _ = m.handle(LpcEvent::Heartbeat, hb);
        assert_eq!(
            m.next_deadline(),
            Some(hb + HEARTBEAT_TIMEOUT),
            "the 120 s window, not the 2 h one"
        );
        let t = m.tick(hb + Duration::seconds(121)).unwrap();
        assert_eq!(t.to, LpcState::UnlimitedAutonomous);
    }

    #[test]
    fn a_write_after_a_heartbeat_pulls_the_device_out_of_failsafe() {
        // [LPC-919]: the normal recovery path.
        let mut m = machine();
        limited_at(&mut m, T0, 3.0);
        m.tick(T0 + Duration::seconds(121));
        assert_eq!(m.state(), LpcState::Failsafe);

        let back = T0 + Duration::minutes(10);
        let _ = m.handle(LpcEvent::Heartbeat, back);
        let out = m.handle(
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(6.0),
                duration: None,
            }),
            back,
        );
        assert_eq!(out.ack, Ok(()));
        assert_eq!(out.transition.unwrap().to, LpcState::Limited);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(6.0)));
    }

    #[test]
    fn a_limit_with_a_duration_lapses_by_itself() {
        // [LPC-909]. The Steuerbox says "4,2 kW for 45 minutes" and the device
        // frees itself afterwards without another message.
        let mut m = machine();
        let _ = m.handle(LpcEvent::Heartbeat, T0);
        let _ = m.handle(
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(4.2),
                duration: Some(Duration::minutes(45)),
            }),
            T0,
        );
        assert_eq!(m.state(), LpcState::Limited);

        // Keep contact alive across the 45 minutes.
        let mut t = T0;
        while t < T0 + Duration::minutes(45) {
            t += Duration::seconds(60);
            let _ = m.handle(LpcEvent::Heartbeat, t);
            m.tick(t);
        }
        assert_eq!(m.state(), LpcState::UnlimitedControlled);
        assert_eq!(m.effective_limit(), None);
    }

    #[test]
    fn a_negative_limit_is_rejected() {
        let mut m = machine();
        let _ = m.handle(LpcEvent::Heartbeat, T0);
        let out = m.handle(
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(-1.0),
                duration: None,
            }),
            T0,
        );
        assert_eq!(out.ack, Err(Nack::NegativeLimit));
        assert!(out.transition.is_none());
    }

    #[test]
    fn a_non_finite_limit_is_rejected() {
        let mut m = machine();
        let _ = m.handle(LpcEvent::Heartbeat, T0);
        let out = m.handle(
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::new_const(f64::NAN),
                duration: None,
            }),
            T0,
        );
        assert_eq!(out.ack, Err(Nack::NotFinite));
    }

    #[test]
    fn the_failsafe_duration_is_held_inside_its_two_to_twenty_four_hour_range() {
        let mut m = machine();
        assert_eq!(
            m.handle(LpcEvent::FailsafeDuration(Duration::minutes(30)), T0)
                .ack,
            Err(Nack::FailsafeDurationOutOfRange)
        );
        assert_eq!(
            m.handle(LpcEvent::FailsafeDuration(Duration::hours(48)), T0)
                .ack,
            Err(Nack::FailsafeDurationOutOfRange)
        );
        assert_eq!(
            m.handle(LpcEvent::FailsafeDuration(Duration::hours(6)), T0)
                .ack,
            Ok(())
        );
        assert_eq!(m.config().failsafe_duration_minimum, Duration::hours(6));
    }

    #[test]
    fn a_duration_above_the_devices_maximum_is_refused_and_the_maximum_adopted() {
        // [LPC-022/4] and [LPC-022/5] together: refuse, then move to our own
        // maximum, so the Energy Guard's intent still lands as far as it can.
        let mut m = LpcMachine::new(
            LpcConfig {
                failsafe_duration_maximum: Duration::hours(8),
                ..LpcConfig::default()
            },
            T0,
        );
        let out = m.handle(LpcEvent::FailsafeDuration(Duration::hours(20)), T0);
        assert_eq!(out.ack, Err(Nack::FailsafeDurationTooLong));
        assert_eq!(m.config().failsafe_duration_minimum, Duration::hours(8));
    }

    #[test]
    fn the_energy_guard_can_rewrite_the_failsafe_value() {
        // [LPC-021/2]
        let mut m = machine();
        assert_eq!(
            m.handle(LpcEvent::FailsafeLimit(Power::from_kw(2.0)), T0)
                .ack,
            Ok(())
        );
        assert_eq!(
            m.effective_limit(),
            Some(Power::from_kw(2.0)),
            "init uses the new value"
        );
        assert_eq!(
            m.handle(LpcEvent::FailsafeLimit(Power::from_kw(-1.0)), T0)
                .ack,
            Err(Nack::NegativeLimit)
        );
    }

    #[test]
    fn a_restart_returns_to_init_from_anywhere() {
        let mut m = machine();
        limited_at(&mut m, T0, 3.0);
        let out = m.handle(LpcEvent::Restart, T0 + Duration::hours(1));
        assert_eq!(out.transition.unwrap().to, LpcState::Init);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(4.2)));
    }

    #[test]
    fn lpp_is_the_same_machine_with_another_name() {
        let mut m = LpcMachine::new(
            LpcConfig {
                direction: Direction::Production,
                failsafe_limit: Power::ZERO,
                ..LpcConfig::default()
            },
            T0,
        );
        assert_eq!(m.config().direction.guard_rule(), GuardRule::Lpp);
        limited_at(&mut m, T0, 5.0);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(5.0)));
    }

    /// The property the guard plane depends on: at every instant the machine
    /// either names a limit or is provably out of contact.
    #[test]
    fn a_controlled_machine_always_knows_its_limit() {
        let mut m = machine();
        let mut t = T0;
        let mut rng: u64 = 0x5eed;
        for step in 0..2000 {
            // A cheap deterministic PRNG: xorshift, so the sequence is fixed and
            // a failure is reproducible from the step number alone.
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            t += Duration::seconds(i64::from(u32::try_from(rng % 90).unwrap()) + 1);

            match rng % 5 {
                0 | 1 => drop(m.handle(LpcEvent::Heartbeat, t)),
                2 => drop(m.handle(
                    LpcEvent::Limit(LimitWrite::Activated {
                        value: Power::from_kw((rng % 12) as f64),
                        duration: None,
                    }),
                    t,
                )),
                3 => drop(m.handle(LpcEvent::Limit(LimitWrite::Deactivated), t)),
                _ => {}
            }
            while let Some(deadline) = m.next_deadline() {
                if deadline > t {
                    break;
                }
                if m.tick(t).is_none() {
                    break;
                }
            }

            match m.state() {
                LpcState::Limited => assert!(
                    m.effective_limit().is_some(),
                    "step {step}: limited without a limit"
                ),
                LpcState::Init | LpcState::Failsafe => assert_eq!(
                    m.effective_limit(),
                    Some(m.config().failsafe_limit),
                    "step {step}: failsafe state must use the failsafe value"
                ),
                LpcState::UnlimitedControlled | LpcState::UnlimitedAutonomous => {
                    assert_eq!(
                        m.effective_limit(),
                        None,
                        "step {step}: unlimited with a limit"
                    );
                }
            }
        }
    }
}
