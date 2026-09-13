//! Every reachable state of the § 14a machine a real box runs, with an
//! assertion in each.
//!
//! # Why breadth-first over an alphabet rather than random walks
//!
//! The exploration is exhaustive **up to bisimulation**, not a sample of paths.
//! A random walk of two thousand steps sampled past a write-window defect for
//! the life of the project; the first breadth-first run found it. Nothing here
//! reads an absolute instant — every decision compares a *difference* against a
//! constant — so two states whose differences agree are the same state, and the
//! frontier is deduplicated on a fingerprint of exactly those differences,
//! capped just past the largest constant any of them is compared with.
//!
//! The time grains land **on** each constant as well as either side of it,
//! because a boundary the alphabet steps over is one no test can see: `[LPC-906]`
//! means the release happens on the stroke of 120 s, and that second is where a
//! certification laboratory measures.
//!
//! # The invariants
//!
//! 1. **`limited` names a limit.** A machine in [`LpcState::Limited`] always has
//!    a ceiling.
//! 2. **`init` and `failsafe` hold the failsafe value** (`[LPC-901]`,
//!    `[LPC-021]`) — the household restraining *itself* for want of an Energy
//!    Guard, which the evidence record has to tell from being reduced.
//! 3. **A restrained machine always has a deadline.** Wherever there is a
//!    ceiling there is something to wake for: the `[LPC-922]` release valve as
//!    an invariant, so no reachable state holds a household down while waiting
//!    on nothing.
//! 4. **A controlled machine has heard a heartbeat within 120 s** (`[LPC-906]`).
//!
//! And one **liveness** property, checked from every reachable state rather than
//! asserted on a scenario: with no further help from the Energy Guard — only
//! time passing — the machine stops restraining the household. That is the whole
//! of *a broken Steuerbox must not block a heat pump for ever*, quantified over
//! everything that can happen first.

use std::collections::{BTreeSet, VecDeque};

use hems_core::prelude::{AssetId, Power};
use hems_drv::eebus::{FAILSAFE_DURATION_RANGE, HEARTBEAT_TIMEOUT, LimitWrite, Lpc, Use};
use hems_grid::LpcState;
use time::{Duration, OffsetDateTime, macros::datetime};

/// The machine starts here.
const T0: OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

/// The failsafe it is configured with — the reference household's own § 14a
/// minimum rather than a flat 4,2 kW, which is what `hemsd` configures.
const FAILSAFE_W: f64 = 10_500.0;

/// The limit the Energy Guard writes, in watts.
const LIMIT_W: f64 = 4_200.0;

/// What the world can do, one action per edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// A heartbeat arrives from the Energy Guard, `[LPC-031]`.
    Heartbeat,
    /// A limit is activated with no duration of its own.
    LimitForever,
    /// A limit is activated for ninety minutes, `[LPC-909]`.
    LimitNinetyMinutes,
    /// The limit is deactivated.
    Deactivate,
    /// A minute passes, with the machine ticked after — the heartbeat cadence,
    /// and the grain a household actually experiences.
    Tick,
    /// Time advances to exactly the machine's own next deadline, and to one
    /// second past it.
    ///
    /// **Adaptive rather than a table of constants**, and that is what keeps the
    /// search both finite and sharp. A fixed set of grains lands on the
    /// boundaries somebody thought of and generates a distinct state for every
    /// intermediate age in between — five hundred times the exploration for less
    /// of the coverage that matters. Asking the machine where its next decision
    /// is lands on **every** boundary it actually has, including the ones nobody
    /// listed, and either side of each.
    ToDeadline,
    /// One second past it, which is where a `>` and a `>=` part company.
    PastDeadline,
}

const ALPHABET: [Action; 7] = [
    Action::Heartbeat,
    Action::LimitForever,
    Action::LimitNinetyMinutes,
    Action::Deactivate,
    Action::Tick,
    Action::ToDeadline,
    Action::PastDeadline,
];

/// The machine, the clock, and the one fact it does not publish.
struct Node {
    machine: Lpc,
    now: OffsetDateTime,
    /// When the last heartbeat was injected.
    ///
    /// Part of the state and not on the machine's surface: the write window
    /// makes a limit acceptable in `init` only within sixty seconds of a
    /// heartbeat, so two states differing only in heartbeat freshness are **not**
    /// bisimilar. A fingerprint built from the public surface alone merges them
    /// and collapses the search.
    last_heartbeat: Option<OffsetDateTime>,
}

impl Node {
    fn start() -> Self {
        Self {
            machine: Lpc::new(
                AssetId::new("netzanschluss").expect("a literal identifier"),
                Use::Lpc,
                Power::new(FAILSAFE_W),
                *FAILSAFE_DURATION_RANGE.start(),
                T0,
            ),
            now: T0,
            last_heartbeat: None,
        }
    }

    /// Apply one action.
    fn step(&mut self, action: Action) {
        match action {
            Action::Heartbeat => {
                self.machine.on_heartbeat(self.now);
                self.last_heartbeat = Some(self.now);
            }
            Action::LimitForever => {
                self.machine
                    .on_limit(&LimitWrite::active(LIMIT_W), self.now);
            }
            Action::LimitNinetyMinutes => {
                self.machine.on_limit(
                    &LimitWrite::active_for(LIMIT_W, std::time::Duration::from_secs(90 * 60)),
                    self.now,
                );
            }
            Action::Deactivate => {
                self.machine.on_limit(&LimitWrite::deactivated(), self.now);
            }
            Action::Tick => {
                self.now += Duration::seconds(60);
                self.machine.on_timeout(self.now);
            }
            Action::ToDeadline | Action::PastDeadline => {
                // **When the household stops being restrained**, not when the
                // box next owes a heartbeat. `deadline()` is both, and the
                // heartbeat half is an outgoing obligation every sixty seconds —
                // chasing it walks the clock through a two-hour failsafe window
                // one minute at a time and turns an exhaustive search into a
                // quarter of an hour of arithmetic about nothing. The decision
                // that matters is `[LPC-922]`'s and `[LPC-909]`'s, and that is
                // what `limit_ends_at` publishes.
                let Some(deadline) = self
                    .machine
                    .limit_ends_at()
                    .or_else(|| self.machine.deadline())
                else {
                    return;
                };
                let slack = if action == Action::PastDeadline {
                    Duration::seconds(1)
                } else {
                    Duration::ZERO
                };
                // Never backwards: a deadline already behind the clock would
                // otherwise rewind it.
                let to = (deadline + slack).max(self.now + Duration::seconds(1));
                self.now = to;
                self.machine.on_timeout(self.now);
            }
        }
    }

    /// What makes two explorations the same.
    fn fingerprint(&self) -> String {
        // Capped just past the Failsafe Duration Minimum, the largest constant
        // the machine compares anything with. Beyond the cap it cannot tell one
        // age from another, so neither can this.
        let cap = 2 * 3_600 + 2;
        let ahead = |at: Option<OffsetDateTime>| {
            at.map(|at| (at - self.now).whole_seconds().clamp(-cap, cap))
        };
        format!(
            "{:?}|{}|{:?}|{:?}|{:?}|{:?}",
            self.machine.state(),
            self.machine.is_controlled(),
            self.machine.ceiling().map(|p| p.get().round() as i64),
            ahead(self.machine.deadline()),
            ahead(self.machine.limit_ends_at()),
            self.last_heartbeat
                .map(|at| (self.now - at).whole_seconds().clamp(0, cap)),
        )
    }

    /// Invariants 1–4, in every reachable state.
    fn check(&self, route: &[Action]) -> Option<String> {
        let state = self.machine.state();
        let ceiling = self.machine.ceiling();
        let fail = |why: &str| {
            Some(format!(
                "\n  after {route:?}\n    {why}\n    state={state:?} ceiling={ceiling:?} \
                 deadline={:?}",
                self.machine.deadline()
            ))
        };

        if state == LpcState::Limited && ceiling.is_none() {
            return fail("`limited` without a limit");
        }
        if matches!(state, LpcState::Init | LpcState::Failsafe)
            && ceiling != Some(Power::new(FAILSAFE_W))
        {
            return fail("the failsafe value does not apply while holding itself down");
        }
        if ceiling.is_some() && self.machine.deadline().is_none() {
            return fail(
                "restrained with nothing to wait for — the [LPC-922] release valve is unreachable",
            );
        }
        if state.is_controlled() {
            let timeout = Duration::try_from(HEARTBEAT_TIMEOUT).unwrap_or(Duration::ZERO);
            let fresh = self
                .last_heartbeat
                .is_some_and(|at| self.now - at <= timeout);
            if !fresh {
                return fail("controlled without a heartbeat in the last 120 s");
            }
        }
        None
    }

    /// Only time passing, until nothing restrains the household.
    ///
    /// The liveness half, and it is checked from **every** reachable state
    /// rather than from a scenario somebody chose.
    fn released_by_silence_alone(&mut self) -> Option<String> {
        let started = self.now;
        // Generous: the failsafe minimum, the heartbeat timeout, a limit's own
        // duration, and slack. The point is that it terminates at all.
        let horizon = started + Duration::hours(30);
        while self.machine.ceiling().is_some() {
            let Some(deadline) = self.machine.deadline() else {
                return Some("restrained with no deadline, so silence never ends it".into());
            };
            let to = (deadline + Duration::seconds(1)).max(self.now + Duration::seconds(1));
            if to > horizon {
                return Some(format!(
                    "not released within thirty hours of silence, still {:?}",
                    self.machine.state()
                ));
            }
            self.now = to;
            self.machine.on_timeout(self.now);
        }
        None
    }
}

#[test]
fn every_reachable_state_upholds_the_invariants_and_releases_on_silence() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // The path that led to each frontier entry, so a failure is something a
    // person can reproduce rather than a state they have to reverse-engineer.
    let mut frontier: VecDeque<Vec<Action>> = VecDeque::new();
    frontier.push_back(Vec::new());
    seen.insert(Node::start().fingerprint());

    let mut failures: Vec<String> = Vec::new();
    let mut states = 0_usize;

    while let Some(path) = frontier.pop_front() {
        for action in ALPHABET {
            // Replayed from the start rather than cloned: the machine owns a
            // SPINE engine and is deliberately not `Clone`, and a harness that
            // copied one would be exploring a copy.
            let mut node = Node::start();
            for earlier in &path {
                node.step(*earlier);
            }
            node.step(action);
            states += 1;

            let mut route = path.clone();
            route.push(action);
            if let Some(why) = node.check(&route) {
                failures.push(why);
            }

            let print = node.fingerprint();
            if seen.insert(print) {
                // Liveness from this state, on the node already in hand: the
                // fingerprint is taken first, so letting the check run the clock
                // forward costs nothing. Replaying the path a second time to get
                // a pristine copy is what made this test take two minutes.
                if let Some(why) = node.released_by_silence_alone() {
                    failures.push(format!("\n  after {route:?}\n    {why}"));
                }
                frontier.push_back(route);
            }
        }
    }

    // The floor is a collapse detector rather than a target. It explores about
    // two hundred thousand edges; a change that quietly merged states — a
    // coarser fingerprint, an action that stopped doing anything — would show up
    // here as a search that finished suspiciously early rather than as a test
    // that still passed.
    assert!(
        states > 50_000,
        "an exploration this small has not explored anything: {states} edges"
    );
    assert!(
        failures.is_empty(),
        "the § 14a machine a real box runs breaks {} invariant(s) over {} \
         reachable state(s):{}",
        failures.len(),
        seen.len(),
        failures.join("")
    );
}
