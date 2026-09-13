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
use time::{Duration, OffsetDateTime};

/// One thing an Energy Guard does on the wire.
///
/// **The simulator's own vocabulary**, deliberately, rather than the state
/// machine's. This crate simulates the *operator's* box; what the household's
/// machine makes of a write is the machine's business, and a simulator that
/// spoke its types would be a simulator that could only ever drive one of them.
/// `hemsd` is what translates these into whichever Controllable System it is
/// running — which is what made collapsing the workspace's two § 14a machines
/// into one possible (D186).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// The heartbeat of `[LPC-031]`, which is what keeps the household out of
    /// its failsafe.
    Heartbeat,
    /// Activate a limit, optionally for a duration of its own `[LPC-909]`.
    Limit {
        /// The ceiling the operator is asking for.
        value: Power,
        /// How long it stands, where the write carries one.
        duration: Option<Duration>,
    },
    /// Release it.
    Release,
}

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

/// How often a real FNN control box sends its heartbeat, `[LPC-031]`.
///
/// A **default** rather than the truth: it is the cadence of the *operator's*
/// box, and the household's machine has its own timeout to compare against.
/// `SteuerboxSim::every` is what a caller that knows the protocol sets it from,
/// and `hemsd` does — so the two cannot drift into a simulator that starves a
/// machine it is supposed to be keeping alive (D140).
pub const DEFAULT_HEARTBEAT: Duration = Duration::seconds(60);

/// A scripted Steuerbox.
#[derive(Debug, Clone)]
pub struct SteuerboxSim {
    /// How often it sends a heartbeat.
    heartbeat_every: Duration,
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
            heartbeat_every: DEFAULT_HEARTBEAT,
            instructions: Vec::new(),
            outages: Vec::new(),
            last_heartbeat: None,
            delivered: 0,
            current_limit: None,
            was_unreachable: false,
        }
    }

    /// Send a heartbeat this often.
    ///
    /// The cadence belongs to whoever knows the protocol, which is not this
    /// crate: a simulator with a period of its own is one that can silently stop
    /// feeding the machine it is driving fast enough to keep it out of the
    /// failsafe, and nothing would report it as anything but a household that
    /// was reduced (D140).
    #[must_use]
    pub const fn every(mut self, heartbeat: Duration) -> Self {
        self.heartbeat_every = heartbeat;
        self
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
    pub fn poll(&mut self, now: OffsetDateTime) -> Vec<Command> {
        if !self.is_reachable(now) {
            self.was_unreachable = true;
            return Vec::new();
        }
        let mut events = Vec::new();

        let due = self
            .last_heartbeat
            .is_none_or(|last| now - last >= self.heartbeat_every);
        if due {
            events.push(Command::Heartbeat);
            self.last_heartbeat = Some(now);
        }

        // Coming back from an outage, a real control box re-states what it wants
        // rather than leaving the energy manager to guess. Without this the
        // manager sees a heartbeat with no write, and after 120 seconds the
        // EEBUS rules — correctly — free it (`[LPC-906]`).
        if core::mem::take(&mut self.was_unreachable) {
            if !due {
                events.push(Command::Heartbeat);
                self.last_heartbeat = Some(now);
            }
            events.push(match self.current_limit {
                Some(value) => Command::Limit {
                    value,
                    duration: None,
                },
                None => Command::Release,
            });
        }

        while let Some(instruction) = self.instructions.get(self.delivered) {
            if instruction.at > now {
                break;
            }
            // A write only counts once contact has been re-established, so a
            // heartbeat always goes first.
            if !due && events.is_empty() {
                events.push(Command::Heartbeat);
                self.last_heartbeat = Some(now);
            }
            events.push(match instruction.limit {
                Some(value) => Command::Limit {
                    value,
                    duration: instruction.duration,
                },
                None => Command::Release,
            });
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
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 16:00:00 UTC);

    /// Every command the box emits from minute `from` to minute `to`.
    ///
    /// Taking both ends makes the calls composable: a helper that always
    /// restarted at zero would silently replay the past.
    fn run(box_sim: &mut SteuerboxSim, from: i64, to: i64) -> Vec<Command> {
        (from..to)
            .flat_map(|m| box_sim.poll(T0 + Duration::minutes(m)))
            .collect()
    }

    /// What this crate owns is the **wire**: which commands an operator's box
    /// sends and when. What a household's state machine makes of them is
    /// `hems-drv`'s, and `steuerbox_against_the_machine.rs` is where the two
    /// meet — against the machine a real box runs rather than a second one
    /// written to be driven by this (D186).
    #[test]
    fn a_quiet_box_sends_a_heartbeat_and_never_a_limit() {
        let mut b = SteuerboxSim::quiet();
        let commands = run(&mut b, 0, 30);
        assert_eq!(commands.len(), 30, "one a minute, at the default cadence");
        assert!(commands.iter().all(|c| *c == Command::Heartbeat));
    }

    #[test]
    fn the_cadence_is_the_callers_and_not_this_crates() {
        // D140: a period that belonged to the simulator could silently stop
        // feeding a machine fast enough to keep it out of its failsafe, and
        // nothing would report that as anything but a household being reduced.
        let mut slow = SteuerboxSim::quiet().every(Duration::minutes(5));
        assert_eq!(run(&mut slow, 0, 30).len(), 6);
    }

    #[test]
    fn a_scripted_event_writes_a_limit_and_then_releases_it() {
        let mut b = SteuerboxSim::quiet().with_event(
            T0 + Duration::minutes(5),
            T0 + Duration::minutes(95),
            Power::from_kw(7.56),
        );
        let early = run(&mut b, 0, 10);
        assert!(early.contains(&Command::Limit {
            value: Power::from_kw(7.56),
            duration: None
        }));
        assert!(!early.contains(&Command::Release));
        assert!(run(&mut b, 10, 100).contains(&Command::Release));
    }

    #[test]
    fn a_box_in_an_outage_says_nothing_at_all_and_re_states_itself_on_return() {
        // The silence is the point — it is what drops a household into its
        // failsafe — and so is the re-statement: a real control box coming back
        // says what it wants rather than leaving the manager to guess, and
        // without it the manager is freed by `[LPC-906]` after two minutes.
        let mut b = SteuerboxSim::quiet()
            .with_event(
                T0 + Duration::minutes(1),
                T0 + Duration::hours(8),
                Power::from_kw(6.0),
            )
            .with_outage(T0 + Duration::minutes(10), T0 + Duration::minutes(40));

        assert!(!run(&mut b, 0, 9).is_empty());
        assert!(
            run(&mut b, 10, 40).is_empty(),
            "an outage is silence, not a slower heartbeat"
        );
        let back = run(&mut b, 40, 42);
        assert!(back.contains(&Command::Heartbeat));
        assert!(
            back.contains(&Command::Limit {
                value: Power::from_kw(6.0),
                duration: None
            }),
            "a box that came back without re-stating its limit would be freed"
        );
    }
}
