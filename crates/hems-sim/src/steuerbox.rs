//! A Steuerbox, as far as the energy manager can tell.
//!
//! The FNN control box is the box the metering point operator installs and the
//! network operator talks to; from the energy manager's side it is an EEBUS
//! *Energy Guard* that sends a heartbeat every minute and occasionally writes a
//! limit. What makes it worth simulating is not the happy path but the ways it
//! goes wrong: it stops talking mid-event, it comes back without saying
//! anything, it sends a limit below the minimum the customer is owed.
//!
//! Those are the cases that decide whether a house is safe and lawful, and they
//! are almost impossible to arrange with real hardware on a desk.

use hems_core::prelude::Power;
use hems_grid::lpc::{HEARTBEAT_INTERVAL, LimitWrite, LpcEvent};
use time::{Duration, OffsetDateTime};

/// One thing the network operator does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Instruction {
    /// When it happens.
    pub at: OffsetDateTime,
    /// The limit to write, or `None` to release.
    pub limit: Option<Power>,
    /// How long the limit is valid, if it carries a duration.
    pub duration: Option<Duration>,
}

/// A scripted Steuerbox.
#[derive(Debug, Clone)]
pub struct SteuerboxSim {
    /// What the operator does, in time order.
    instructions: Vec<Instruction>,
    /// Windows in which the box is silent — no heartbeat, no writes.
    outages: Vec<(OffsetDateTime, OffsetDateTime)>,
    /// When the last heartbeat was emitted.
    last_heartbeat: Option<OffsetDateTime>,
    /// How many instructions have been delivered.
    delivered: usize,
    /// The limit currently in force, so it can be re-stated after an outage.
    current_limit: Option<Power>,
    /// Whether the box was unreachable at the previous poll.
    was_unreachable: bool,
}

impl SteuerboxSim {
    /// A box that only sends heartbeats.
    #[must_use]
    pub fn quiet() -> Self {
        Self {
            instructions: Vec::new(),
            outages: Vec::new(),
            last_heartbeat: None,
            delivered: 0,
            current_limit: None,
            was_unreachable: false,
        }
    }

    /// A box that reduces to `limit` from `from` until `until`.
    ///
    /// The shape of a real § 14a event: a network area gets busy at teatime, the
    /// operator reduces, and an hour and a half later it releases.
    #[must_use]
    pub fn with_event(mut self, from: OffsetDateTime, until: OffsetDateTime, limit: Power) -> Self {
        self.instructions.push(Instruction {
            at: from,
            limit: Some(limit),
            duration: None,
        });
        self.instructions.push(Instruction {
            at: until,
            limit: None,
            duration: None,
        });
        self.instructions.sort_by_key(|i| i.at);
        self
    }

    /// A window in which the box says nothing at all.
    #[must_use]
    pub fn with_outage(mut self, from: OffsetDateTime, until: OffsetDateTime) -> Self {
        self.outages.push((from, until));
        self
    }

    /// Whether the box is reachable at `now`.
    #[must_use]
    pub fn is_reachable(&self, now: OffsetDateTime) -> bool {
        !self
            .outages
            .iter()
            .any(|(from, until)| now >= *from && now < *until)
    }

    /// The events the box emits at `now`.
    ///
    /// Call once per control tick; the box decides for itself when a heartbeat
    /// is due. During an outage it emits nothing, which is precisely what makes
    /// the energy manager fall into the failsafe.
    pub fn poll(&mut self, now: OffsetDateTime) -> Vec<LpcEvent> {
        if !self.is_reachable(now) {
            self.was_unreachable = true;
            return Vec::new();
        }
        let mut events = Vec::new();

        let due = self
            .last_heartbeat
            .is_none_or(|last| now - last >= HEARTBEAT_INTERVAL);
        if due {
            events.push(LpcEvent::Heartbeat);
            self.last_heartbeat = Some(now);
        }

        // Coming back from an outage, a real control box re-states what it wants
        // rather than leaving the energy manager to guess. Without this the
        // manager sees a heartbeat with no write, and after 120 seconds the
        // EEBUS rules — correctly — free it (`[LPC-906]`).
        if core::mem::take(&mut self.was_unreachable) {
            if !due {
                events.push(LpcEvent::Heartbeat);
                self.last_heartbeat = Some(now);
            }
            events.push(LpcEvent::Limit(match self.current_limit {
                Some(value) => LimitWrite::Activated {
                    value,
                    duration: None,
                },
                None => LimitWrite::Deactivated,
            }));
        }

        while let Some(instruction) = self.instructions.get(self.delivered) {
            if instruction.at > now {
                break;
            }
            // A write only counts once contact has been re-established, so a
            // heartbeat always goes first.
            if !due && events.is_empty() {
                events.push(LpcEvent::Heartbeat);
                self.last_heartbeat = Some(now);
            }
            events.push(LpcEvent::Limit(match instruction.limit {
                Some(value) => LimitWrite::Activated {
                    value,
                    duration: instruction.duration,
                },
                None => LimitWrite::Deactivated,
            }));
            self.current_limit = instruction.limit;
            self.delivered += 1;
        }

        events
    }
}

impl Default for SteuerboxSim {
    fn default() -> Self {
        Self::quiet()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_grid::lpc::{LpcConfig, LpcMachine, LpcState};
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 16:00:00 UTC);

    fn machine() -> LpcMachine {
        LpcMachine::new(
            LpcConfig {
                failsafe_limit: Power::from_kw(4.2),
                ..LpcConfig::default()
            },
            T0,
        )
    }

    /// Run a simulated box against a real state machine from minute `from` to
    /// minute `to` after `T0`. Taking both ends makes the calls composable —
    /// a helper that always restarts at zero silently replays the past.
    fn run(box_sim: &mut SteuerboxSim, machine: &mut LpcMachine, from: i64, to: i64) {
        for m in from..to {
            let now = T0 + Duration::minutes(m);
            for event in box_sim.poll(now) {
                let _ = machine.handle(event, now);
            }
            while let Some(deadline) = machine.next_deadline() {
                if deadline > now || machine.tick(now).is_none() {
                    break;
                }
            }
        }
    }

    #[test]
    fn a_box_that_never_writes_a_limit_frees_the_house_after_two_minutes() {
        // [LPC-906]. A heartbeat alone does not leave `init` — § 2.2 wants a
        // write to follow — so after 120 seconds the manager concludes there is
        // nothing controlling it and stops holding itself at the failsafe value.
        // Anything else would leave a house permanently restrained by a control
        // box that was installed but never configured.
        let mut b = SteuerboxSim::quiet();
        let mut m = machine();
        run(&mut b, &mut m, 0, 30);
        assert_eq!(m.state(), LpcState::UnlimitedAutonomous);
        assert_eq!(m.effective_limit(), None);
    }

    #[test]
    fn a_scripted_event_limits_and_then_releases() {
        let mut b = SteuerboxSim::quiet().with_event(
            T0 + Duration::minutes(5),
            T0 + Duration::minutes(95),
            Power::from_kw(7.56),
        );
        let mut m = machine();

        run(&mut b, &mut m, 0, 10);
        assert_eq!(m.state(), LpcState::Limited);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(7.56)));

        run(&mut b, &mut m, 10, 100);
        assert_eq!(m.state(), LpcState::UnlimitedControlled);
        assert_eq!(m.effective_limit(), None);
    }

    #[test]
    fn an_outage_drops_the_manager_into_the_failsafe_and_it_recovers() {
        let mut b = SteuerboxSim::quiet()
            .with_event(
                T0 + Duration::minutes(1),
                T0 + Duration::hours(8),
                Power::from_kw(6.0),
            )
            .with_outage(T0 + Duration::minutes(10), T0 + Duration::minutes(40));
        let mut m = machine();

        run(&mut b, &mut m, 0, 9);
        assert_eq!(m.state(), LpcState::Limited);

        // Silence: after two minutes the failsafe takes over.
        run(&mut b, &mut m, 9, 15);
        assert_eq!(m.state(), LpcState::Failsafe);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(4.2)));

        // The box comes back and re-states the limit.
        run(&mut b, &mut m, 15, 45);
        assert_eq!(m.state(), LpcState::Limited);
        assert_eq!(m.effective_limit(), Some(Power::from_kw(6.0)));
    }

    #[test]
    fn a_long_outage_eventually_frees_the_house() {
        // [LPC-922]: a Steuerbox that never comes back must not hold a heat pump
        // down for ever.
        let mut b = SteuerboxSim::quiet()
            .with_event(
                T0 + Duration::minutes(1),
                T0 + Duration::hours(24),
                Power::from_kw(6.0),
            )
            .with_outage(T0 + Duration::minutes(10), T0 + Duration::hours(24));
        let mut m = machine();
        run(&mut b, &mut m, 0, 60 * 4);
        assert_eq!(m.state(), LpcState::UnlimitedAutonomous);
        assert_eq!(m.effective_limit(), None);
    }
}
