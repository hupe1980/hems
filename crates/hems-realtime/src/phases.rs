//! One conductor or three: when a charge point should switch, and when it
//! should be left alone.
//!
//! A three-phase wallbox cannot charge below **6 A on every conductor**, which
//! at 230 V is 4,14 kW. On one conductor the same 6 A is 1,38 kW. So a household
//! with 2 kW of photovoltaic surplus either charges its car or does not,
//! depending entirely on a contactor — and on a German roof that gap covers most
//! of the morning, most of the evening and nearly all of the winter.
//!
//! It is the single largest lever in surplus charging, and it is also the
//! easiest to ruin. Switching interrupts the session: IEC 61851 has the vehicle
//! re-negotiate, which takes seconds, and a controller that chases a passing
//! cloud will spend the afternoon toggling a contactor instead of charging a
//! car. Every implementation that works has the same three ingredients, and this
//! module is those three:
//!
//! | Ingredient | Why |
//! |---|---|
//! | **Hysteresis** on the way up | switching to three phases at exactly the three-phase minimum guarantees an immediate switch back |
//! | A **confirmation** window | the power has to stay on the other side of the threshold, not merely cross it |
//! | A **dwell** time | however convincing the case, a mode that has just been entered is not left again straight away |
//!
//! The confirmation window is **asymmetric**, and the asymmetry is the point.
//! Failing to switch *down* stops the session altogether — the wallbox is above
//! its three-phase minimum and the surplus is not — while failing to switch *up*
//! merely charges the car more slowly than it could. The expensive mistake is
//! the one to make quickly.
//!
//! # Sans-I/O, like everything else
//!
//! [`decide`] is a pure function of the previous [`PhaseState`], the power
//! available and the time. The arbiter carries the state from tick to tick the
//! same way it carries the previous setpoints, so a whole afternoon of passing
//! clouds is a unit test.
//!
//! # What "available" means
//!
//! The power the charge point could actually use if it were in the right mode:
//! what it wants, bounded by what the guard will allow it. It deliberately does
//! **not** include the mode's own minimum — that is what is being decided.

use core::fmt;

use hems_core::prelude::{AssetId, Evse, PhaseMode, Power};
use time::{Duration, OffsetDateTime};

/// How eagerly a charge point changes its mode.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PhaseSwitchConfig {
    /// The shortest time a mode is kept before another switch is allowed.
    ///
    /// A vehicle that has just re-negotiated its session should be left to
    /// charge. Five minutes is the value evcc and openWB converged on.
    #[cfg_attr(
        feature = "serde",
        serde(default = "default_dwell", with = "crate::guard::duration_secs")
    )]
    pub dwell: Duration,
    /// How long the surplus must stay too small for three phases before
    /// dropping to one.
    ///
    /// Short, because not switching down means not charging at all: the charge
    /// point is above its three-phase minimum and the surplus is not. A minute
    /// of the wrong mode is cheaper than ten minutes of nothing.
    #[cfg_attr(
        feature = "serde",
        serde(default = "default_confirm_down", with = "crate::guard::duration_secs")
    )]
    pub confirm_down: Duration,
    /// How long the surplus must stay large enough for three phases before
    /// going back up.
    ///
    /// Longer, because the downside of getting it wrong is small — the car
    /// charges at up to 4,1 kW instead of up to 11 — and the downside of getting
    /// it wrong *often* is a contactor.
    #[cfg_attr(
        feature = "serde",
        serde(default = "default_confirm_up", with = "crate::guard::duration_secs")
    )]
    pub confirm_up: Duration,
    /// How far above the three-phase minimum the available power must be before
    /// switching up.
    ///
    /// Without it, switching up at exactly the minimum means the very next tick
    /// is a candidate for switching back down.
    #[cfg_attr(feature = "serde", serde(default = "default_hysteresis"))]
    pub hysteresis: Power,
    /// Whether phase switching is allowed at all.
    ///
    /// Some installations forbid it — an older vehicle that does not tolerate
    /// the interruption, or a network operator's condition. Off means the charge
    /// point stays in the mode its wiring starts in.
    #[cfg_attr(feature = "serde", serde(default = "default_enabled"))]
    pub enabled: bool,
}

fn default_dwell() -> Duration {
    Duration::minutes(5)
}

fn default_confirm_down() -> Duration {
    Duration::minutes(1)
}

fn default_confirm_up() -> Duration {
    Duration::minutes(3)
}

fn default_hysteresis() -> Power {
    Power::new_const(500.0)
}

const fn default_enabled() -> bool {
    true
}

impl Default for PhaseSwitchConfig {
    fn default() -> Self {
        Self {
            dwell: default_dwell(),
            confirm_down: default_confirm_down(),
            confirm_up: default_confirm_up(),
            hysteresis: default_hysteresis(),
            enabled: default_enabled(),
        }
    }
}

/// What the arbiter is doing about one charge point's conductors.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PhaseState {
    /// The mode currently commanded.
    pub mode: PhaseMode,
    /// When that mode was commanded.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub since: OffsetDateTime,
    /// A different mode the conditions have been arguing for, and since when.
    ///
    /// Cleared the moment the conditions stop arguing for it, which is what
    /// makes a passing cloud cost nothing.
    #[cfg_attr(feature = "serde", serde(default))]
    pub wanted: Option<Wanted>,
}

/// A mode the conditions favour, and how long they have favoured it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Wanted {
    /// The mode.
    pub mode: PhaseMode,
    /// When the conditions first favoured it.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub since: OffsetDateTime,
}

impl PhaseState {
    /// A charge point that has just come up in `mode`.
    #[must_use]
    pub const fn new(mode: PhaseMode, now: OffsetDateTime) -> Self {
        Self {
            mode,
            since: now,
            wanted: None,
        }
    }

    /// Whether a switch is being waited out.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.wanted.is_some()
    }
}

impl fmt::Display for PhaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.wanted {
            Some(w) => write!(f, "{} → {} pending", self.mode, w.mode),
            None => write!(f, "{}", self.mode),
        }
    }
}

/// The mode the conditions favour, before any timing.
///
/// The rule is a threshold at the three-phase minimum, because that is where the
/// modes actually cross over. Below it, three phases deliver **nothing** — a
/// charge point under 6 A per conductor is idle, not slow — while one conductor
/// delivers everything asked for up to its own maximum. Above it, three phases
/// deliver everything and one conductor is capped.
///
/// # The hysteresis is relative to the mode, not to the threshold
///
/// Applying it symmetrically — "three phases need the minimum *plus* a margin,
/// whichever mode you are in" — creates a band in which a charge point already
/// running on three phases is told to drop to one, where it delivers less. A
/// plan asking for precisely the three-phase minimum, which is an ordinary thing
/// for a semi-continuous variable to land on, falls into that band.
///
/// So the margin applies **on the way up only**:
///
/// | Currently | Switches to three phases at | Because |
/// |---|---|---|
/// | one conductor | minimum + margin | waiting costs at most the margin, and churn costs a contactor |
/// | three conductors | the minimum exactly | dropping below it costs the whole session |
///
/// The band between the two is where whichever mode is running stays running,
/// which is what a hysteresis is for.
#[must_use]
pub fn favoured_mode(
    evse: &Evse,
    current: PhaseMode,
    available: Power,
    hysteresis: Power,
) -> PhaseMode {
    // A hardware threshold must not turn on the last bit of a float. A plan that
    // lands exactly on the three-phase minimum — the ordinary outcome for a
    // semi-continuous variable — comes back from a solver a nanowatt short of
    // it, and without this tolerance that nanowatt drops the charge point onto
    // one conductor for the rest of the session, where it delivers 3,68 kW at
    // 85 % instead of 4,14 kW at 92 %. A milliwatt is far below anything a
    // charge point can resolve and far above anything a solver rounds by.
    const EPS: Power = Power::new_const(1e-3);
    let supports = |m: PhaseMode| evse.meta.phases.supports(m);
    let threshold = evse.min_power(PhaseMode::Three)
        + match current {
            PhaseMode::Single => hysteresis.max(Power::ZERO),
            PhaseMode::Three => Power::ZERO,
        };
    let three_ok = supports(PhaseMode::Three) && available >= threshold - EPS;
    let one_ok =
        supports(PhaseMode::Single) && available >= evse.min_power(PhaseMode::Single) - EPS;

    if three_ok {
        PhaseMode::Three
    } else if one_ok {
        PhaseMode::Single
    } else {
        // Below both minimums the charge point is idle in either mode, so a
        // switch buys nothing and costs a contactor operation. It keeps whatever
        // it last had, which is also the mode it will start the next session in.
        current
    }
}

/// Advance one charge point's phase policy.
///
/// Returns the state for the next tick; `state.mode` is what should be
/// commanded. A returned mode that differs from the one passed in is a switch,
/// and the arbiter turns it into a [`hems_core::setpoint::Command::PhaseCount`]
/// ahead of the charging current.
///
/// The order of the guards matters. A mode that cannot be held (the wiring does
/// not support it) is corrected immediately, because it is not a preference but
/// a mistake. Everything else waits: first for the conditions to persist, then
/// for the current mode to have had its turn.
#[must_use]
pub fn decide(
    evse: &Evse,
    state: PhaseState,
    available: Power,
    now: OffsetDateTime,
    config: &PhaseSwitchConfig,
) -> PhaseState {
    // A mode the wiring cannot hold is not a decision, it is a correction.
    let held = evse.meta.phases.clamp_mode(state.mode);
    if held != state.mode {
        return PhaseState::new(held, now);
    }
    if !config.enabled || !evse.meta.phases.is_switchable() {
        return PhaseState {
            wanted: None,
            ..state
        };
    }

    let favoured = favoured_mode(evse, state.mode, available, config.hysteresis);
    if favoured == state.mode {
        // The conditions agree with where we are; forget any argument they were
        // making a moment ago.
        return PhaseState {
            wanted: None,
            ..state
        };
    }

    // The conditions want something else. Have they wanted it long enough, and
    // has the current mode had its turn?
    let wanted = match state.wanted {
        Some(w) if w.mode == favoured => w,
        _ => Wanted {
            mode: favoured,
            since: now,
        },
    };
    let window = match favoured {
        PhaseMode::Single => config.confirm_down,
        PhaseMode::Three => config.confirm_up,
    };
    let confirmed = now - wanted.since >= window;
    let settled = now - state.since >= config.dwell;
    if confirmed && settled {
        PhaseState::new(favoured, now)
    } else {
        PhaseState {
            wanted: Some(wanted),
            ..state
        }
    }
}

/// The next tick's phase policy for every switchable charge point on a site.
///
/// `available` answers "how much could this asset use, if it were in the right
/// mode" — what it wants, bounded by what the guard allows.
pub fn decide_all<'a>(
    assets: impl IntoIterator<Item = (&'a AssetId, &'a Evse, Power)>,
    previous: &std::collections::BTreeMap<AssetId, PhaseState>,
    now: OffsetDateTime,
    config: &PhaseSwitchConfig,
) -> std::collections::BTreeMap<AssetId, PhaseState> {
    assets
        .into_iter()
        .map(|(id, evse, available)| {
            let state = previous
                .get(id)
                .copied()
                .unwrap_or_else(|| PhaseState::new(evse.meta.phases.default_mode(), now));
            (id.clone(), decide(evse, state, available, now, config))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{AssetMeta, Capabilities, Evse};
    use hems_core::prelude::{CircuitId, Current, Phase, PhaseConnection};
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-06-21 09:00:00 UTC);

    fn evse(connection: PhaseConnection) -> Evse {
        Evse {
            meta: AssetMeta::new(
                AssetId::new("wallbox").unwrap(),
                CircuitId::new("main").unwrap(),
                connection,
                Power::from_kw(11.0),
            )
            .with_capabilities(Capabilities::MEASURE | Capabilities::SET_POWER),
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
        }
    }

    fn switchable() -> Evse {
        evse(PhaseConnection::Switchable { phase: Phase::L1 })
    }

    /// Run `available` for `minutes`, one tick a minute, and return the state.
    fn run(
        evse: &Evse,
        mut state: PhaseState,
        available: f64,
        minutes: i64,
        start: OffsetDateTime,
        config: &PhaseSwitchConfig,
    ) -> (PhaseState, OffsetDateTime) {
        let mut now = start;
        for _ in 0..minutes {
            now += Duration::minutes(1);
            state = decide(evse, state, Power::from_kw(available), now, config);
        }
        (state, now)
    }

    #[test]
    fn a_plan_landing_exactly_on_the_three_phase_minimum_stays_on_three() {
        // A semi-continuous charge point pinned to its own minimum is the
        // ordinary outcome of a solve, and a solver returns that bound a
        // nanowatt short of itself. Read literally it says "below the
        // three-phase minimum" and costs the session 8 % of its efficiency for
        // the rest of the evening.
        let evse = switchable();
        let minimum = evse.min_power(PhaseMode::Three);
        let solver_answer = Power::new(minimum.get() - 1e-10);
        assert_eq!(
            favoured_mode(
                &evse,
                PhaseMode::Three,
                solver_answer,
                Power::new_const(500.0)
            ),
            PhaseMode::Three
        );
    }

    #[test]
    fn two_kilowatts_of_surplus_charges_a_car_on_one_conductor_and_not_on_three() {
        // The whole argument in one assertion. 6 A three-phase is 4,14 kW; on one
        // conductor it is 1,38 kW. A household with 2 kW of surplus is on
        // opposite sides of that line depending on a contactor.
        let e = switchable();
        assert!(e.min_power(PhaseMode::Three) > Power::from_kw(2.0));
        assert!(e.min_power(PhaseMode::Single) < Power::from_kw(2.0));
        assert_eq!(
            favoured_mode(&e, PhaseMode::Three, Power::from_kw(2.0), Power::ZERO),
            PhaseMode::Single
        );
    }

    #[test]
    fn a_switch_waits_for_the_conditions_to_persist_and_for_the_dwell_to_pass() {
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let state = PhaseState::new(PhaseMode::Three, T0);

        // Half a minute of 2 kW is not enough to confirm anything…
        let after_30s = decide(
            &e,
            state,
            Power::from_kw(2.0),
            T0 + Duration::seconds(30),
            &cfg,
        );
        assert_eq!(after_30s.mode, PhaseMode::Three);
        assert!(after_30s.is_pending());

        // …and neither is a confirmed minute while the mode is still fresh:
        // the dwell is five.
        let (after_2min, _) = run(&e, state, 2.0, 2, T0, &cfg);
        assert_eq!(after_2min.mode, PhaseMode::Three, "dwell not yet served");

        // Five minutes serves both, and the switch is stamped when it happened.
        let (after_5min, _) = run(&e, state, 2.0, 5, T0, &cfg);
        assert_eq!(after_5min.mode, PhaseMode::Single);
        assert_eq!(after_5min.since, T0 + Duration::minutes(5));
        assert!(!after_5min.is_pending());
    }

    #[test]
    fn dropping_to_one_conductor_is_quicker_than_climbing_back() {
        // The asymmetry. Not switching down means not charging; not switching up
        // means charging more slowly. The expensive mistake is made quickly.
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let long_ago = T0 - Duration::hours(1);

        let (down, _) = run(
            &e,
            PhaseState::new(PhaseMode::Three, long_ago),
            2.0,
            2,
            T0,
            &cfg,
        );
        assert_eq!(
            down.mode,
            PhaseMode::Single,
            "two minutes is enough to drop"
        );

        let (up_2, _) = run(
            &e,
            PhaseState::new(PhaseMode::Single, long_ago),
            9.0,
            2,
            T0,
            &cfg,
        );
        assert_eq!(up_2.mode, PhaseMode::Single, "but not enough to climb");
        let (up_4, _) = run(
            &e,
            PhaseState::new(PhaseMode::Single, long_ago),
            9.0,
            4,
            T0,
            &cfg,
        );
        assert_eq!(up_4.mode, PhaseMode::Three);
    }

    #[test]
    fn a_passing_cloud_costs_nothing() {
        // Three phases, one minute of cloud, sun again. The confirmation window
        // has not closed, so the contactor never moves — which is the entire
        // reason there is a confirmation window.
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let state = PhaseState::new(PhaseMode::Three, T0 - Duration::hours(1));

        let dim = decide(
            &e,
            state,
            Power::from_kw(2.0),
            T0 + Duration::minutes(1),
            &cfg,
        );
        assert_eq!(dim.mode, PhaseMode::Three);
        assert!(dim.is_pending(), "it is thinking about it");

        let back = decide(
            &e,
            dim,
            Power::from_kw(8.0),
            T0 + Duration::minutes(2),
            &cfg,
        );
        assert_eq!(back.mode, PhaseMode::Three);
        assert!(!back.is_pending(), "and it has stopped thinking about it");
    }

    #[test]
    fn the_hysteresis_is_relative_to_the_mode_and_never_costs_a_session() {
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let three_min = e.min_power(PhaseMode::Three);

        // From one conductor, the bare minimum is not enough to move up: waiting
        // costs at most the margin, and churn costs a contactor.
        assert_eq!(
            favoured_mode(&e, PhaseMode::Single, three_min, cfg.hysteresis),
            PhaseMode::Single
        );
        assert_eq!(
            favoured_mode(
                &e,
                PhaseMode::Single,
                three_min + cfg.hysteresis,
                cfg.hysteresis
            ),
            PhaseMode::Three
        );

        // From three, the bare minimum *is* enough to stay: dropping below it
        // would deliver 3,7 kW where 4,14 was asked for, and a plan landing
        // exactly on the three-phase minimum is an ordinary thing for a
        // semi-continuous variable to do.
        assert_eq!(
            favoured_mode(&e, PhaseMode::Three, three_min, cfg.hysteresis),
            PhaseMode::Three
        );
        // A hair below, and one conductor is strictly better: three deliver
        // nothing at all there.
        assert_eq!(
            favoured_mode(
                &e,
                PhaseMode::Three,
                three_min - Power::new(1.0),
                cfg.hysteresis
            ),
            PhaseMode::Single
        );
    }

    #[test]
    fn the_chosen_mode_always_delivers_at_least_as_much_as_the_other_would() {
        // The property the threshold exists for, checked across the whole range.
        // The only exception is the hysteresis band on the way up, where staying
        // on one conductor is a deliberate, bounded loss.
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let deliver = |mode: PhaseMode, want: Power| {
            if want >= e.min_power(mode) {
                want.min(e.max_power(mode))
            } else {
                Power::ZERO
            }
        };
        for milliwatts in (0..12_000).step_by(37) {
            let want = Power::new(f64::from(milliwatts));
            for current in [PhaseMode::Single, PhaseMode::Three] {
                let chosen = favoured_mode(&e, current, want, cfg.hysteresis);
                let other = chosen.other();
                let lost = deliver(other, want) - deliver(chosen, want);
                assert!(
                    lost <= cfg.hysteresis + e.min_power(PhaseMode::Three)
                        - e.max_power(PhaseMode::Single),
                    "at {want} from {current}: {chosen} loses {lost} against {other}"
                );
            }
        }
    }

    #[test]
    fn an_idle_charge_point_is_left_where_it_is() {
        // Below both minimums the car is not charging in either mode, so a
        // switch buys nothing and costs a contactor operation.
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        for mode in [PhaseMode::Single, PhaseMode::Three] {
            let (state, _) = run(&e, PhaseState::new(mode, T0), 0.2, 30, T0, &cfg);
            assert_eq!(state.mode, mode, "from {mode}");
        }
    }

    #[test]
    fn a_charge_point_that_cannot_switch_never_does() {
        let cfg = PhaseSwitchConfig::default();
        for connection in [
            PhaseConnection::Three,
            PhaseConnection::Single { phase: Phase::L2 },
        ] {
            let e = evse(connection);
            let start = connection.default_mode();
            let (low, _) = run(&e, PhaseState::new(start, T0), 2.0, 60, T0, &cfg);
            assert_eq!(low.mode, start, "{connection:?} at 2 kW");
            let (high, _) = run(&e, PhaseState::new(start, T0), 11.0, 60, T0, &cfg);
            assert_eq!(high.mode, start, "{connection:?} at 11 kW");
        }
    }

    #[test]
    fn switching_off_leaves_the_charge_point_in_its_wiring_default() {
        let e = switchable();
        let cfg = PhaseSwitchConfig {
            enabled: false,
            ..PhaseSwitchConfig::default()
        };
        let (state, _) = run(&e, PhaseState::new(PhaseMode::Three, T0), 2.0, 60, T0, &cfg);
        assert_eq!(state.mode, PhaseMode::Three);
        assert!(!state.is_pending());
    }

    #[test]
    fn a_mode_the_wiring_cannot_hold_is_corrected_at_once() {
        // A site reconfigured from a switchable charge point to a fixed
        // three-phase one, with the old single-phase state still in the store.
        let e = evse(PhaseConnection::Three);
        let state = decide(
            &e,
            PhaseState::new(PhaseMode::Single, T0 - Duration::days(1)),
            Power::from_kw(2.0),
            T0,
            &PhaseSwitchConfig::default(),
        );
        assert_eq!(state.mode, PhaseMode::Three);
        assert_eq!(state.since, T0);
    }

    #[test]
    fn a_full_day_of_a_german_roof_follows_the_envelope_and_not_the_noise() {
        // The property that matters operationally. A bell-shaped day crossed
        // with minute-scale noise of ±35 % — far harsher than real irradiance,
        // which is strongly autocorrelated — and the mode should track the
        // envelope: one conductor at the ends of the day, three across the
        // middle. The envelope alone would switch three times; across seeds the
        // noise takes it to between three and nine, and the bound below is
        // deliberately generous because the point is that it is not hundreds.
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let mut state = PhaseState::new(PhaseMode::Three, T0);
        let mut now = T0;
        let mut switches = 0;
        let mut modes = Vec::new();
        let mut rng: u64 = 0x5EED;
        let mut next = |modulo: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % modulo
        };

        for minute in 0..(16 * 60) {
            now += Duration::minutes(1);
            let hours = f64::from(minute) / 60.0;
            let envelope = 9.0 * (-((hours - 8.0) / 3.5).powi(2)).exp();
            #[allow(clippy::cast_precision_loss)]
            let noise = 1.0 - 0.35 * (next(100) as f64 / 100.0);
            let before = state.mode;
            state = decide(&e, state, Power::from_kw(envelope * noise), now, &cfg);
            if state.mode != before {
                switches += 1;
            }
            modes.push(state.mode);
        }

        assert!(
            (2..=12).contains(&switches),
            "a sixteen-hour day should switch a handful of times, not {switches}"
        );
        // The shoulders of the day sit in the single-phase band (1,38 kW to
        // 4,64 kW of surplus); the middle is above it. Before the first shoulder
        // the surplus is below *both* minimums, the charge point is idle either
        // way, and the mode is deliberately left where it was.
        assert!(
            modes[..3 * 60].iter().all(|m| *m == PhaseMode::Three),
            "an idle charge point is not switched at dawn"
        );
        assert_eq!(
            modes[4 * 60 + 30],
            PhaseMode::Single,
            "the morning shoulder"
        );
        assert_eq!(modes[8 * 60], PhaseMode::Three, "the peak of the day");
        assert_eq!(modes[12 * 60], PhaseMode::Single, "the evening shoulder");
    }

    #[test]
    fn a_switch_is_never_undone_before_the_dwell_has_run() {
        // The one hard guarantee: whatever the surplus does, a mode holds for at
        // least the dwell. Without it a contactor can be asked to operate every
        // tick, and a wallbox that switches every minute charges nothing at all.
        let e = switchable();
        let cfg = PhaseSwitchConfig::default();
        let mut state = PhaseState::new(PhaseMode::Three, T0);
        let mut now = T0;
        let mut last_switch = T0;
        let mut rng: u64 = 0x00C0_FFEE;
        let mut next = |modulo: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % modulo
        };

        for _ in 0..2000 {
            now += Duration::seconds(30);
            #[allow(clippy::cast_precision_loss)]
            let available = Power::from_kw(next(1200) as f64 / 100.0);
            let before = state.mode;
            state = decide(&e, state, available, now, &cfg);
            if state.mode != before {
                assert!(
                    now - last_switch >= cfg.dwell,
                    "switched after only {}",
                    now - last_switch
                );
                last_switch = now;
            }
        }
    }
}
