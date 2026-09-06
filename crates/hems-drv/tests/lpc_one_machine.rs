//! The two § 14a state machines in this workspace, driven in lockstep.
//!
//! `hems-drv`'s module note says it: *"Two implementations of a certifiable
//! state machine disagree, and the one that is wrong is whichever the
//! certification lab is not looking at."* The driver honours it —
//! `hems_grid::LpcState` is **derived** from `eebus`'s machine. But
//! `hems-grid::lpc::LpcMachine` is a complete second implementation, with its
//! own `LimitWrite`, `Nack` set and timers, and it is what the **reference
//! days** run — so every § 14a compliance figure this product quotes comes from
//! a machine a real box does not run. Collapsing the two is on the backlog
//! (D167); what is not optional is something that fails when they disagree.
//!
//! # What is compared
//!
//! The **effective limit**, the **controlled** flag and the state name — the
//! whole contract either machine has with the rest of hems: the guard enforces
//! the ceiling, the evidence record tells an operator's instruction from the
//! household restraining itself (`[A1 7.2]`), and `Lpc::state()` is the
//! derivation the driver promises. Agreement on those three in every reachable
//! state is what makes deleting one of the machines safe.
//!
//! # The alphabet, and why the search terminates
//!
//! A heartbeat, three kinds of limit write, and time grains that land **on**
//! each constant either machine compares against as well as either side of it —
//! a boundary the alphabet steps over is one no test can see. Neither machine
//! reads an absolute instant, so states whose time *differences* agree are
//! bisimilar; the search dedupes on a fingerprint of both machines' observable
//! surface plus the age of the last heartbeat, capped past the largest constant
//! either is compared with.

use hems_core::prelude::{AssetId, Power};
use hems_drv::eebus::{LimitWrite as DrvWrite, Lpc, Use};
use hems_grid::lpc::{
    Direction, LimitWrite as GridWrite, LpcConfig, LpcEvent, LpcMachine, LpcState,
};
use std::collections::{BTreeSet, VecDeque};
use time::{Duration, OffsetDateTime, macros::datetime};

/// Both machines start here.
const T0: OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

/// The failsafe both are configured with — the reference household's own § 14a
/// minimum rather than a flat 4,2 kW, which is what `hemsd` configures.
const FAILSAFE_W: f64 = 10_500.0;

/// The Failsafe Duration Minimum both are configured with, and the smallest the
/// specification allows (`[LPC-022]`).
const FAILSAFE_FOR: std::time::Duration = std::time::Duration::from_secs(2 * 3_600);

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
    /// Time advances by this many seconds, with both machines ticked after.
    ///
    /// The grains land **on** each constant either machine compares against as
    /// well as either side of it, because that is where two implementations of
    /// one specification differ: the whole of the defect this test first found
    /// was one machine reading "no heartbeat for 120 s" as `>` and the other as
    /// `>=`. A boundary the alphabet steps over cannot be observed.
    ///
    /// 30 s is inside every window; 60 s is the `WRITE_WINDOW` and the heartbeat
    /// interval exactly; 120 s is `[LPC-906]`'s timeout exactly and 121 s past
    /// it; 90 min retires a limit that had a duration; and 2 h is the Failsafe
    /// Duration Minimum exactly, where `[LPC-922]` releases the household.
    Advance(i64),
}

const ALPHABET: [Action; 12] = [
    Action::Heartbeat,
    Action::LimitForever,
    Action::LimitNinetyMinutes,
    Action::Deactivate,
    Action::Advance(30),
    Action::Advance(60),
    Action::Advance(61),
    Action::Advance(120),
    Action::Advance(121),
    Action::Advance(90 * 60),
    Action::Advance(2 * 3_600),
    Action::Advance(2 * 3_600 + 1),
];

/// The limit both machines are written, in watts.
const LIMIT_W: f64 = 4_200.0;

/// Both machines, and the clock they share.
struct Pair {
    grid: LpcMachine,
    drv: Lpc,
    now: OffsetDateTime,
    /// When the last heartbeat was injected.
    ///
    /// Neither machine publishes it, and it is part of the state: `WRITE_WINDOW`
    /// makes a limit write acceptable in `init` only within sixty seconds of a
    /// heartbeat, so two states differing only in heartbeat freshness are **not**
    /// bisimilar. A fingerprint built from the public surface alone merges them
    /// and collapses the search, so this is reconstructed from the events the
    /// harness injected.
    last_heartbeat: Option<OffsetDateTime>,
}

impl Pair {
    fn new() -> Self {
        Self {
            grid: LpcMachine::new(
                LpcConfig {
                    direction: Direction::Consumption,
                    failsafe_limit: Power::new(FAILSAFE_W),
                    failsafe_duration_minimum: Duration::seconds(FAILSAFE_FOR.as_secs() as i64),
                    ..LpcConfig::default()
                },
                T0,
            ),
            drv: Lpc::new(
                AssetId::new("netzanschluss").expect("a literal identifier"),
                Use::Lpc,
                Power::new(FAILSAFE_W),
                FAILSAFE_FOR,
                T0,
            ),
            now: T0,
            last_heartbeat: None,
        }
    }

    /// Apply one action to both, in the same order, at the same instant.
    fn step(&mut self, action: Action) {
        match action {
            Action::Heartbeat => {
                let _ = self.grid.handle(LpcEvent::Heartbeat, self.now);
                self.drv.on_heartbeat(self.now);
                self.last_heartbeat = Some(self.now);
            }
            Action::LimitForever => {
                let _ = self.grid.handle(
                    LpcEvent::Limit(GridWrite::Activated {
                        value: Power::new(LIMIT_W),
                        duration: None,
                    }),
                    self.now,
                );
                self.drv.on_limit(&DrvWrite::active(LIMIT_W), self.now);
            }
            Action::LimitNinetyMinutes => {
                let _ = self.grid.handle(
                    LpcEvent::Limit(GridWrite::Activated {
                        value: Power::new(LIMIT_W),
                        duration: Some(Duration::minutes(90)),
                    }),
                    self.now,
                );
                self.drv.on_limit(
                    &DrvWrite::active_for(LIMIT_W, std::time::Duration::from_secs(90 * 60)),
                    self.now,
                );
            }
            Action::Deactivate => {
                let _ = self
                    .grid
                    .handle(LpcEvent::Limit(GridWrite::Deactivated), self.now);
                self.drv.on_limit(&DrvWrite::deactivated(), self.now);
            }
            Action::Advance(seconds) => {
                self.now += Duration::seconds(seconds);
                // Both machines are given the same instant and both are told to
                // run their own timers. Ticking one and not the other would be
                // comparing a machine that had noticed the clock with one that
                // had not.
                self.grid.tick(self.now);
                self.drv.on_timeout(self.now);
            }
        }
    }

    /// What the rest of hems can see: the ceiling, whether an operator is in
    /// control, and the state name the driver derives.
    fn observed(&self) -> (Option<i64>, bool, LpcState, Option<i64>, bool, LpcState) {
        // Watts to the nearest whole one: both machines carry `f64`, and a
        // comparison to the last bit would be asserting that two independent
        // arithmetics round identically rather than that they agree.
        let watts = |p: Option<Power>| p.map(|p| p.get().round() as i64);
        (
            watts(self.grid.effective_limit()),
            self.grid.state().is_controlled(),
            self.grid.state(),
            watts(self.drv.ceiling()),
            self.drv.is_controlled(),
            self.drv.state(),
        )
    }

    /// What makes two explorations of this pair the same.
    ///
    /// Neither machine reads an absolute instant, so what matters is the
    /// differences. The fingerprint is both machines' whole observable surface
    /// plus the age of the last heartbeat and the time since the last state
    /// change, each capped past the largest constant either is compared with —
    /// beyond that cap the machines cannot tell one age from another, so neither
    /// can this.
    fn fingerprint(&self) -> String {
        let (gl, gc, gs, dl, dc, ds) = self.observed();
        // Capped at just past the Failsafe Duration Minimum, which is the
        // largest constant in either machine.
        let cap = 2 * 3_600 + 2;
        let deadline = |d: Option<OffsetDateTime>| {
            d.map(|d| ((d - self.now).whole_seconds()).clamp(-cap, cap))
        };
        // The heartbeat's age, capped the same way. Beyond the cap neither
        // machine can tell one age from another, so neither can this.
        let heartbeat_age = self
            .last_heartbeat
            .map(|at| (self.now - at).whole_seconds().clamp(0, cap));
        format!(
            "{gl:?}|{gc}|{gs:?}|{dl:?}|{dc}|{ds:?}|{:?}|{:?}|{heartbeat_age:?}",
            deadline(self.grid.next_deadline()),
            deadline(self.drv.deadline()),
        )
    }
}

/// Every reachable state of both machines, with an assertion after every edge.
///
/// Breadth-first over the alphabet, deduplicated on the fingerprint, so the
/// search is exhaustive up to bisimilarity rather than a sample of paths.
#[test]
fn the_two_state_machines_agree_in_every_reachable_state() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // The path that led to each frontier entry, so a disagreement is reported as
    // something a person can reproduce rather than as a state they have to
    // reverse-engineer.
    let mut frontier: VecDeque<Vec<Action>> = VecDeque::new();
    frontier.push_back(Vec::new());
    seen.insert(Pair::new().fingerprint());

    let mut disagreements: Vec<String> = Vec::new();
    let mut states = 0_usize;

    while let Some(path) = frontier.pop_front() {
        // A depth bound, because the alphabet is nine wide and the point is to
        // finish. Every state reachable at all is reachable within a few edges
        // of one that is: the machines have five states and three timers.
        if path.len() >= 7 {
            continue;
        }
        for action in ALPHABET {
            let mut pair = Pair::new();
            for earlier in &path {
                pair.step(*earlier);
            }
            pair.step(action);
            states += 1;

            let (gl, gc, gs, dl, dc, ds) = pair.observed();
            if gl != dl || gc != dc || gs != ds {
                let mut route = path.clone();
                route.push(action);
                disagreements.push(format!(
                    "\n  after {route:?}\n    hems-grid: limit={gl:?} controlled={gc} state={gs:?}\
                     \n    eebus:     limit={dl:?} controlled={dc} state={ds:?}"
                ));
            }

            let print = pair.fingerprint();
            if seen.insert(print) {
                let mut next = path.clone();
                next.push(action);
                frontier.push_back(next);
            }
        }
    }

    assert!(
        states > 1_000,
        "an exploration this small has not explored anything: {states} edges"
    );
    assert!(
        disagreements.is_empty(),
        "the two § 14a state machines disagree in {} reachable state(s). Whichever \
         is wrong, it is the one the certification laboratory is not looking at — \
         and the reference days run `hems-grid`'s while a real box runs `eebus`'s.\
         {}",
        disagreements.len(),
        disagreements
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("")
    );
}
