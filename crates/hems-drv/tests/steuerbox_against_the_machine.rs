//! A simulated Steuerbox against the § 14a machine a real box runs.
//!
//! What each case is about is not an implementation but the ways an operator's
//! box **goes wrong** — it never configures anything, it stops talking
//! mid-event, it comes back, it never comes back. Those are what decide whether
//! a household is safe and lawful, and arranging them with real hardware on a
//! desk is close to impossible.
//!
//! The translation from the simulator's vocabulary to the driver's is one line
//! per command, which is the whole of what a simulator owes a state machine it
//! is not a copy of (D186).

use hems_core::prelude::{AssetId, Power};
use hems_drv::eebus::{LimitWrite, Lpc, Use};
use hems_grid::LpcState;
use hems_sim::{Command, SteuerboxSim};
use time::{Duration, OffsetDateTime, macros::datetime};

const T0: OffsetDateTime = datetime!(2026-01-15 16:00:00 UTC);

/// The failsafe the household is owed, and the minimum the specification allows
/// it to be held for (`[LPC-022]`).
const FAILSAFE: Power = Power::new_const(4_200.0);
const FAILSAFE_FOR: std::time::Duration = std::time::Duration::from_secs(2 * 3_600);

fn machine() -> Lpc {
    Lpc::new(
        AssetId::new("netzanschluss").expect("a literal identifier"),
        Use::Lpc,
        FAILSAFE,
        FAILSAFE_FOR,
        T0,
    )
}

/// Drive the machine with the box from minute `from` to minute `to`.
///
/// The same translation `hemsd::scenario` makes.
fn run(box_sim: &mut SteuerboxSim, machine: &mut Lpc, from: i64, to: i64) {
    for m in from..to {
        let now = T0 + Duration::minutes(m);
        for command in box_sim.poll(now) {
            match command {
                Command::Heartbeat => machine.on_heartbeat(now),
                Command::Limit { value, duration } => {
                    let write = match duration.and_then(|d| d.try_into().ok()) {
                        Some(d) => LimitWrite::active_for(value.get(), d),
                        None => LimitWrite::active(value.get()),
                    };
                    machine.on_limit(&write, now);
                }
                Command::Release => {
                    machine.on_limit(&LimitWrite::deactivated(), now);
                }
            }
        }
        machine.on_timeout(now);
    }
}

#[test]
fn a_box_that_never_writes_a_limit_frees_the_house_after_two_minutes() {
    // [LPC-906]. A heartbeat alone does not leave `init` — the implementation
    // guide § 2.2 wants a write to follow — so after 120 seconds the manager
    // concludes there is nothing controlling it and stops holding itself at the
    // failsafe value. Anything else would leave a house permanently restrained
    // by a control box that was installed and never configured.
    let mut b = SteuerboxSim::quiet();
    let mut m = machine();
    run(&mut b, &mut m, 0, 30);
    assert_eq!(m.state(), LpcState::UnlimitedAutonomous);
    assert_eq!(m.ceiling(), None);
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
    assert_eq!(m.ceiling(), Some(Power::from_kw(7.56)));

    run(&mut b, &mut m, 10, 100);
    assert_eq!(m.state(), LpcState::UnlimitedControlled);
    assert_eq!(m.ceiling(), None);
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

    // Silence: after two minutes the failsafe takes over, and the household is
    // restraining *itself* rather than being reduced — which the evidence
    // record has to tell apart.
    run(&mut b, &mut m, 9, 15);
    assert_eq!(m.state(), LpcState::Failsafe);
    assert_eq!(m.ceiling(), Some(FAILSAFE));

    // The box comes back and re-states the limit.
    run(&mut b, &mut m, 15, 45);
    assert_eq!(m.state(), LpcState::Limited);
    assert_eq!(m.ceiling(), Some(Power::from_kw(6.0)));
}

#[test]
fn a_long_outage_eventually_frees_the_house() {
    // [LPC-922]: a Steuerbox that never comes back must not hold a heat pump
    // down for ever. The Failsafe Duration Minimum runs, and then the household
    // is its own again.
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
    assert_eq!(m.ceiling(), None);
}

/// And the instant the planner needs: when the thing in force stops.
///
/// `Lpc::limit_ends_at` is what `hemsd`'s receding horizon plans against: a
/// reduction that lapses at 18:30 is a different plan from one that does not.
#[test]
fn the_machine_says_when_what_is_in_force_stops() {
    let mut b = SteuerboxSim::quiet();
    let mut m = machine();

    // Before anything has been written, the household is holding *itself* down
    // and knows exactly when `[LPC-922]` releases it.
    assert_eq!(
        m.limit_ends_at(),
        Some(T0 + Duration::hours(2)),
        "the Failsafe Duration Minimum, counted from the state it entered at"
    );

    // Under a limit with a duration of its own, `[LPC-909]`.
    run(&mut b, &mut m, 0, 1);
    let at = T0 + Duration::minutes(1);
    m.on_heartbeat(at);
    m.on_limit(
        &LimitWrite::active_for(6_000.0, std::time::Duration::from_secs(90 * 60)),
        at,
    );
    assert_eq!(m.state(), LpcState::Limited);
    assert_eq!(m.limit_ends_at(), Some(at + Duration::minutes(90)));

    // …and a limit written without one does not end on a clock at all.
    m.on_limit(&LimitWrite::active(6_000.0), at);
    assert_eq!(m.limit_ends_at(), None);
}
