//! Exhaustive exploration of the LPC state machine — the model-checking
//! artefact beside the property tests.
//!
//! The randomised walk in `lpc.rs` samples paths; this enumerates them. Every
//! reachable state of the **production** [`LpcMachine`] — not a parallel model
//! of it, which would test the parallel model — is visited by breadth-first
//! search over a finite event alphabet, and four invariants are checked in
//! every one of them. A certifier reading this file gets the two things a
//! hand-written test cannot give: the claim that the properties hold in *all*
//! reachable states of the machine as shipped, and the alphabet and abstraction
//! under which "all" is meant.
//!
//! # The event alphabet
//!
//! What the world can do to a Controllable System, one action per edge:
//!
//! * a **heartbeat** arrives from the Energy Guard (`[LPC-031]`);
//! * the Energy Guard **writes a limit** — activated without a duration,
//!   activated for ninety minutes (`[LPC-909]`), or deactivated;
//! * the Controllable System **restarts** (§ 2.2);
//! * **time passes** — to the machine's own next deadline, one second past it,
//!   or sixty seconds, with `tick` called after each. The three grains cover
//!   both sides of every strict/non-strict comparison in the machine and the
//!   interleavings between the three timers.
//!
//! # Why the exploration terminates
//!
//! Wall-clock time grows without bound, but the machine never reads an absolute
//! instant — every decision compares a *difference* (time in state, heartbeat
//! age, time to limit expiry) against a constant. Two states whose differences
//! agree are bisimilar, so states are deduplicated on a fingerprint of those
//! differences, capped just past the largest constant each is ever compared
//! with. The differences are reconstructed from the machine's own public
//! surface plus the events this harness injected, so the fingerprint cannot
//! disagree with the machine about what happened.
//!
//! # The invariants
//!
//! 1. **`limited` names a limit.** A machine in [`LpcState::Limited`] always
//!    has an [`LpcMachine::effective_limit`].
//! 2. **`init` and `failsafe` hold the failsafe value** (`[LPC-901]`,
//!    `[LPC-021]`).
//! 3. **A restrained machine always has a deadline.** Wherever
//!    `effective_limit()` is `Some`, `next_deadline()` is `Some` — the
//!    `[LPC-922]` release valve as an invariant: no reachable state restrains
//!    the household while waiting on nothing.
//! 4. **A controlled machine has heard a heartbeat within 120 s** (`[LPC-906]`):
//!    after every advance-and-tick, `is_controlled()` implies a fresh contact.
//!
//! And one liveness property, checked from **every** reachable state rather
//! than asserted on a scenario: with no further help from the Energy Guard —
//! only time passing — the machine stops restraining the household within the
//! Failsafe Duration Minimum plus the heartbeat timeout. That is the whole of
//! "a broken Steuerbox must not block a heat pump forever", quantified over
//! everything that can happen first.

use std::collections::{HashSet, VecDeque};

use hems_core::prelude::Power;
use hems_grid::lpc::{
    FAILSAFE_DURATION_MIN, HEARTBEAT_TIMEOUT, LimitWrite, LpcConfig, LpcEvent, LpcMachine, LpcState,
};
use time::{Duration, OffsetDateTime};

const T0: OffsetDateTime = time::macros::datetime!(2026-01-15 17:00:00 UTC);
const LIMIT_KW: f64 = 4.2;
const LIMIT_DURATION: Duration = Duration::minutes(90);

/// One node of the exploration: the real machine, the clock, and the mirror of
/// the timing facts the fingerprint needs.
#[derive(Clone)]
struct Node {
    machine: LpcMachine,
    now: OffsetDateTime,
    /// When the machine last changed state, from the transitions it returned.
    entered_at: OffsetDateTime,
    /// The last heartbeat this harness delivered, if any.
    last_heartbeat: Option<OffsetDateTime>,
    /// The first heartbeat delivered since the last transition — the machine's
    /// own `heartbeat_in_state`, mirrored.
    heartbeat_in_state: Option<OffsetDateTime>,
}

impl Node {
    fn start() -> Self {
        let config = LpcConfig {
            failsafe_limit: Power::from_kw(LIMIT_KW),
            ..LpcConfig::default()
        };
        Self {
            machine: LpcMachine::new(config, T0),
            now: T0,
            entered_at: T0,
            last_heartbeat: None,
            heartbeat_in_state: None,
        }
    }

    /// Record a transition the machine reported.
    fn moved(&mut self, transition: Option<hems_grid::lpc::Transition>) {
        if let Some(t) = transition {
            self.entered_at = t.at;
            self.heartbeat_in_state = None;
        }
    }

    fn handle(&mut self, event: LpcEvent) {
        if matches!(event, LpcEvent::Heartbeat) {
            self.last_heartbeat = Some(self.now);
            if self.heartbeat_in_state.is_none() {
                self.heartbeat_in_state = Some(self.now);
            }
        }
        if matches!(event, LpcEvent::Restart) {
            self.last_heartbeat = None;
        }
        let outcome = self.machine.handle(event, self.now);
        self.moved(outcome.transition);
    }

    fn advance(&mut self, to: OffsetDateTime) {
        self.now = to;
        let t = self.machine.tick(self.now);
        self.moved(t);
    }

    /// The bisimulation fingerprint: every difference the machine ever compares
    /// against a constant, capped just past the largest such constant.
    fn fingerprint(&self) -> Fingerprint {
        let cap = |d: Duration, at: Duration| -> i64 { d.min(at).whole_seconds() };
        let since = |from: Option<OffsetDateTime>, at: Duration| -> Option<i64> {
            from.map(|f| cap(self.now - f, at))
        };
        let heartbeat_cap = HEARTBEAT_TIMEOUT + Duration::seconds(60);
        let state_cap = self.machine.config().failsafe_duration_minimum + Duration::minutes(2);
        Fingerprint {
            state: self.machine.state(),
            limit_milliwatts: self
                .machine
                .effective_limit()
                .map(|p| (p.get() * 1000.0).round() as i64),
            in_state_s: cap(self.now - self.entered_at, state_cap),
            heartbeat_age_s: since(self.last_heartbeat, heartbeat_cap),
            heartbeat_in_state_age_s: since(self.heartbeat_in_state, heartbeat_cap),
            // Only meaningful in `limited`; `limit_ends_at` in `init` and
            // `failsafe` is derived from `in_state_s`, which is already here.
            expires_in_s: (self.machine.state() == LpcState::Limited)
                .then(|| {
                    self.machine
                        .limit_ends_at()
                        .map(|e| (e - self.now).whole_seconds())
                })
                .flatten(),
        }
    }

    /// The next nodes the alphabet can reach from this one.
    fn successors(&self) -> Vec<(String, Node)> {
        let mut out = Vec::new();
        for event in [
            LpcEvent::Heartbeat,
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(LIMIT_KW),
                duration: None,
            }),
            LpcEvent::Limit(LimitWrite::Activated {
                value: Power::from_kw(LIMIT_KW),
                duration: Some(LIMIT_DURATION),
            }),
            LpcEvent::Limit(LimitWrite::Deactivated),
            LpcEvent::Restart,
        ] {
            let mut next = self.clone();
            next.handle(event);
            out.push((format!("{event:?}"), next));
        }
        let mut advances = vec![self.now + Duration::seconds(60)];
        if let Some(deadline) = self.machine.next_deadline() {
            advances.push(deadline);
            advances.push(deadline + Duration::seconds(1));
        }
        for to in advances {
            if to > self.now {
                let mut next = self.clone();
                next.advance(to);
                out.push((format!("advance to {to}"), next));
            }
        }
        out
    }

    /// Invariants 1–4, checked in every reachable state.
    fn check(&self) {
        let state = self.machine.state();
        let limit = self.machine.effective_limit();
        assert!(
            state != LpcState::Limited || limit.is_some(),
            "`limited` without a limit: {self:?}"
        );
        if matches!(state, LpcState::Init | LpcState::Failsafe) {
            assert_eq!(
                limit,
                Some(self.machine.config().failsafe_limit),
                "the failsafe value does not apply in {state}: {self:?}"
            );
        }
        assert!(
            limit.is_none() || self.machine.next_deadline().is_some(),
            "restrained with nothing to wait for — the [LPC-922] release valve \
             is unreachable from here: {self:?}"
        );
        if state.is_controlled() {
            let fresh = self
                .last_heartbeat
                .is_some_and(|hb| self.now - hb <= HEARTBEAT_TIMEOUT);
            assert!(
                fresh,
                "controlled without a heartbeat in the last 120 s: {self:?}"
            );
        }
    }

    /// The liveness check: only time passing, until nothing restrains the
    /// household. Returns how long it took.
    fn release_by_time_alone(&self) -> Duration {
        let mut node = self.clone();
        let start = node.now;
        // Generous bound: the failsafe minimum, the heartbeat timeout, the limit
        // duration and slack. The assertion below is the tight one.
        let horizon = start + Duration::hours(30);
        while node.machine.effective_limit().is_some() {
            let Some(deadline) = node.machine.next_deadline() else {
                unreachable!("invariant 3 holds, so a restrained machine has a deadline");
            };
            let to = (deadline + Duration::seconds(1)).max(node.now + Duration::seconds(1));
            assert!(
                to <= horizon,
                "not released within thirty hours of silence: {node:?}"
            );
            node.advance(to);
        }
        node.now - start
    }
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at {} (entered {}, heartbeat {:?}, limit {:?})",
            self.machine.state(),
            self.now,
            self.entered_at,
            self.last_heartbeat,
            self.machine.effective_limit()
        )
    }
}

/// What makes two nodes the same state of the machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Fingerprint {
    state: LpcState,
    limit_milliwatts: Option<i64>,
    in_state_s: i64,
    heartbeat_age_s: Option<i64>,
    heartbeat_in_state_age_s: Option<i64>,
    expires_in_s: Option<i64>,
}

#[test]
fn every_reachable_state_upholds_the_invariants_and_releases_on_silence() {
    let mut seen: HashSet<Fingerprint> = HashSet::new();
    let mut queue: VecDeque<(usize, Node)> = VecDeque::new();
    let mut trail: Vec<(usize, String)> = Vec::new();

    let start = Node::start();
    seen.insert(start.fingerprint());
    trail.push((0, "start".into()));
    queue.push_back((0, start));

    let mut visited = 0_usize;
    let mut longest_release = Duration::ZERO;
    while let Some((idx, node)) = queue.pop_front() {
        visited += 1;
        if let Err(e) = std::panic::catch_unwind(|| node.check()) {
            let mut path = Vec::new();
            let mut i = idx;
            loop {
                path.push(trail[i].1.clone());
                if i == 0 {
                    break;
                }
                i = trail[i].0;
            }
            path.reverse();
            panic!(
                "invariant violated after: {}
{e:?}",
                path.join(" -> ")
            );
        }
        longest_release = longest_release.max(node.release_by_time_alone());
        for (action, next) in node.successors() {
            if seen.insert(next.fingerprint()) {
                trail.push((idx, action));
                queue.push_back((trail.len() - 1, next));
            }
        }
    }

    // The exploration has to have actually explored: a state space of a handful
    // of nodes would mean the alphabet or the fingerprint collapsed, and every
    // "all reachable states" claim above would be about nothing.
    assert!(
        visited > 1_000,
        "only {visited} states — the exploration collapsed"
    );
    // And the tight form of the liveness bound: silence releases the household
    // within the Failsafe Duration Minimum plus one heartbeat window — the
    // failsafe entered at the last possible moment, then `[LPC-922]`.
    assert!(
        longest_release <= FAILSAFE_DURATION_MIN + HEARTBEAT_TIMEOUT + Duration::seconds(5),
        "a state took {longest_release} of silence to release"
    );
}
