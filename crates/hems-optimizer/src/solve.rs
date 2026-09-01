//! The mixed-integer linear program, and what comes out of it.
//!
//! # The formulation
//!
//! One set of variables per quarter-hour slot `k` of the horizon, all in watts
//! and all non-negative:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `g_in[k]`, `g_out[k]` | import from and export to the grid |
//! | `b_ch[k]`, `b_dis[k]` | battery charging and discharging |
//! | `e[k]` | energy in the battery at the *end* of slot `k`, Wh |
//! | `ev[k]` | charging power at the charge point |
//! | `ev_e[k]` | energy in the car at the end of slot `k`, Wh |
//! | `curtail[k]` | production thrown away |
//! | `sh_start[i][k]` | binary: appliance `i` starts its programme in slot `k` |
//!
//! subject to, in every slot,
//!
//! ```text
//! g_in − g_out = load + b_ch − b_dis + ev + hp + dhw + a − (pv − curtail)
//!                                                               (energy balance)
//! g_out ≤ pv − curtail + b_dis                                  (no invented export)
//! e[k] = e[k−1] + Δ(η_ch·b_ch − b_dis/η_dis)                    (battery state)
//! ev_e[k] = ev_e[k−1] + Δ·η·ev                                  (car state)
//! b_ch + ev + hp − b_dis + curtail + dhw + a ≤ ceiling + (pv − load)
//!                                                               (§ 14a, with a surplus)
//! b_ch + ev + hp − b_dis ≤ ceiling                              (§ 14a, without one)
//! g_out ≤ feed-in ceiling                                       (§ 9 EEG, LPP)
//! curtail ≤ pv
//! ```
//!
//! `a` is what the shiftable appliances draw. Four details of the § 14a rows are
//! each a decision rather than an accident, and `grid_rules` gives the argument
//! for every one: curtailment is on the **left** because throwing energy away
//! cannot buy headroom; the tank and the appliances are on the left because they
//! spend the surplus without being steuerbare Verbrauchseinrichtungen
//! themselves; the battery's **discharge** is on the left with a minus, because
//! `[A1 2.3]` measures what crosses the connection point; and the `max(0, ·)`
//! around the surplus is dropped, which is exact wherever the surplus survives
//! and conservative where it does not.
//!
//! and minimising
//!
//! ```text
//! Σ Δ·(price_in·g_in − price_out·g_out + wear·(b_ch + b_dis) + penalty·curtail)
//! ```
//!
//! # Two things this does that most do not
//!
//! **Battery wear is in the objective.** Without it a cost-minimising plan will
//! cycle a battery for a spread that does not cover the damage — measured at up
//! to ten times the saving in `specs/arxiv/arxiv-2606.16051.pdf`. With it, the
//! plan only moves energy when the spread actually pays.
//!
//! **The § 14a constraint is on the netzwirksamer Leistungsbezug**, not on
//! consumption, so the surplus a roof is producing raises the ceiling exactly as
//! `[A1 2.3]` says it does. The same arithmetic runs in the guard at one-second
//! resolution against measurements; here it runs against the forecast.

use good_lp::constraint::ConstraintReference;
use good_lp::{
    Expression, ProblemVariables, Solution, SolverModel, Variable, constraint, variable,
};
use hems_core::prelude::{AssetId, AssetTarget, Envelope, Plan, PlanId, Power, SlotPlan};
use thiserror::Error;
use time::OffsetDateTime;

use crate::model::{EvSession, Problem};

/// Hours in one slot — the factor between power in watts and energy in watt-hours.
const DT_HOURS: f64 = 0.25;

/// What to assume a kilowatt-hour costs where the price stack does not reach.
///
/// A horizon can run past the last published day-ahead auction. Refusing to plan
/// would be worse than planning against a plausible number, and a plausible
/// number that is *flat* is the right shape: it makes the plan indifferent about
/// when to act out there, which is exactly the state of knowledge.
const DEFAULT_IMPORT_EUR_PER_KWH: f64 = 0.30;

/// The same, for what feeding in earns.
const DEFAULT_EXPORT_EUR_PER_KWH: f64 = 0.08;

/// The German grid's carbon intensity where no source provides one, g/kWh.
const DEFAULT_CO2_G_PER_KWH: f64 = 400.0;

/// Where an unmanaged hot-water thermostat holds the tank, as a fraction of its
/// usable heat.
///
/// A tank on a plain thermostat sits at its set point all day, which is what
/// makes it a *baseline*: it never uses the store as a store.
const DHW_THERMOSTAT_SET: f64 = 0.85;

/// What the objective is multiplied by before the dual pass.
///
/// Large enough to bring a €/W coefficient of order 10⁻⁵ up to order 1, which is
/// where an interior-point solver's tolerances live. See [`shadow_prices`].
const DUAL_OBJECTIVE_SCALE: f64 = 1e5;

/// Kilowatt-hours moved in one slot per watt of power.
///
/// Every price in the workspace is €/kWh and every variable here is in watts, so
/// this is the factor that makes the objective come out in euros. Getting it
/// wrong does not make the model infeasible — it silently rescales one term
/// against another, and the plan quietly stops caring about whichever term lost.
const SLOT_KWH_PER_W: f64 = DT_HOURS / 1000.0;

/// Why a plan could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SolveError {
    /// The constraints cannot all be met — most often a charging target that
    /// cannot be reached before departure.
    #[error("the problem has no feasible solution: {0}")]
    Infeasible(String),
    /// The solver failed for a reason of its own.
    #[error("the solver failed: {0}")]
    Solver(String),
    /// The horizon has no slots.
    #[error("the horizon is empty")]
    EmptyHorizon,
    /// A forecast does not reach the end of the horizon.
    ///
    /// Refused rather than filled in, because the only honest filler is one
    /// nobody has: a slot with no band was previously planned as **zero
    /// production and zero load**, which is a confident lie in both directions.
    /// It makes the roof dark and the house empty, so the plan defers every
    /// flexible kilowatt-hour into hours it believes are free, and the day
    /// arrives to find them ordinary.
    #[error(
        "the {series} forecast covers {covered} of the horizon's {slots} slots; \
         planning the rest would assume no sun and no household at all"
    )]
    ForecastTooShort {
        /// Which of the two — `"production"` or `"load"`.
        series: &'static str,
        /// How many of the horizon's slots the forecast has a band for.
        covered: usize,
        /// How many the horizon has.
        slots: usize,
    },
    /// The price stack is indexed by position, and its `slot` says it belongs
    /// somewhere else.
    ///
    /// A stack **shorter** than the horizon is fine and deliberate — a horizon
    /// can run past the last published auction, and a flat default price is a
    /// plausible answer out there. A stack that is *the wrong hours* is not: the
    /// plan is then optimised against somebody else's day, and nothing about the
    /// result looks wrong.
    #[error("the price stack's slot {position} is {found}, but the horizon's is {expected}")]
    PricesMisaligned {
        /// Where in the stack the two first disagree.
        position: usize,
        /// The slot the stack carries there, as an RFC 3339 instant.
        found: String,
        /// The slot the horizon has there.
        expected: String,
    },
}

/// The names the planner gives the assets it plans for.
///
/// The optimiser works on a model, not on a site, so the caller says which asset
/// each part of the model stands for.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetNames {
    /// The stationary battery.
    pub battery: Option<AssetId>,
    /// The charge point.
    pub evse: Option<AssetId>,
    /// The photovoltaic array, for curtailment targets.
    pub pv: Option<AssetId>,
    /// The heat pump.
    pub heat_pump: Option<AssetId>,
    /// The hot-water tank.
    pub dhw: Option<AssetId>,
    /// The appliances waiting to run a programme, in the same order as
    /// [`crate::model::Problem::shiftable`].
    pub shiftable: Vec<AssetId>,
}

impl AssetNames {
    /// No named assets — a plan that computes flows but commands nothing.
    #[must_use]
    pub fn none() -> Self {
        Self {
            battery: None,
            evse: None,
            pv: None,
            heat_pump: None,
            dhw: None,
            shiftable: Vec::new(),
        }
    }
}

/// One slot's worth of decided flows, in watts.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Flows {
    /// Import from the grid.
    pub grid_import: Power,
    /// Export to the grid.
    pub grid_export: Power,
    /// Battery charging power.
    pub battery_charge: Power,
    /// Battery discharging power.
    pub battery_discharge: Power,
    /// Energy in the battery at the end of the slot.
    pub battery_energy: hems_core::prelude::Energy,
    /// Charging power at the charge point.
    pub ev_charge: Power,
    /// Production thrown away.
    pub curtailed: Power,
    /// The part of `grid_import` a § 42c community allocated to this member.
    pub shared_import: Power,
    /// Electrical power drawn by the heat pump.
    pub heat_pump: Power,
    /// Electrical power drawn by the hot-water heater.
    pub dhw: Power,
    /// Electrical power drawn by the shiftable appliances together.
    pub shiftable: Power,
    /// Heat in the tank at the end of the slot, above its lowest acceptable
    /// temperature.
    pub dhw_stored: hems_core::prelude::Energy,
    /// Indoor air temperature at the end of the slot, °C.
    pub indoor_c: f64,
    /// Kelvin outside the comfort band in this slot — what the plan decided the
    /// household could put up with, and the number to show them before it does.
    pub discomfort_k: f64,
}

/// A solved plan, with the flows behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct Solved {
    /// The plan the arbiter follows.
    pub plan: Plan,
    /// The decided flows, one entry per slot.
    pub flows: Vec<Flows>,
    /// Energy the car was promised that the plan could not deliver by its
    /// deadline.
    ///
    /// Zero on any ordinary day. Above zero it is the one thing the household
    /// has to be told *before* the morning: the schedule that came back is the
    /// best achievable, and it is short. A hard deadline would have returned no
    /// plan at all, which is a worse answer to the same question.
    pub unmet_charge: hems_core::prelude::Energy,
    /// Hot water the household asked for that the tank could not deliver, as
    /// heat, summed over the horizon (see [`Solved::unmet_charge`] for why both
    /// are expectations).
    ///
    /// The same shape as [`Solved::unmet_charge`] and for the same reason: a
    /// cold shower is expensive, not impossible, and a plan that says how short
    /// it fell is worth more than no plan at all.
    pub unmet_hot_water: hems_core::prelude::Energy,
    /// What this plan decided discretely, for the **next** re-plan to start
    /// from.
    ///
    /// Shift it by the slots that elapse before the next solve
    /// ([`Commitment::shifted`]) and hand it back through
    /// [`Problem::with_warm_start`]. It is a hint and cannot change what a plan
    /// says; see [`Commitment`].
    ///
    /// [`Commitment`]: crate::model::Commitment
    /// [`Commitment::shifted`]: crate::model::Commitment::shifted
    /// [`Problem::with_warm_start`]: crate::model::Problem::with_warm_start
    pub commitment: crate::model::Commitment,
}

// There is deliberately no `Solved::service_is_at_risk` here, and the absence is
// a decision rather than an omission.
//
// It existed, and it read the plan's own predicted shortfall to decide whether
// the household was worth planning against three futures. Measured, it **never
// fires**: on the reference evening the median plan expects to make the target,
// the weather disappoints, and by then it is too late. A plan that has looked
// only at the median cannot know it is at risk — asking it is asking the wrong
// oracle. `EvSession::tightness` is the replacement, because it is a property of
// the *session* and needs no solve at all.
//
// Keeping the method would leave a public API whose documentation recommended
// the thing this project measured and rejected. `Solved::unmet_charge` and
// `Solved::unmet_hot_water` are still there for a caller that wants to *report*
// a shortfall, which is the honest use.

/// Solve `problem` and turn the result into a [`Plan`].
///
/// `now` stamps the plan so the arbiter can tell how old it is.
///
/// # Errors
/// [`SolveError`] when the horizon is empty, its inputs do not line up with it
/// ([`check_inputs`]), the constraints conflict, or the solver fails.
pub fn solve(
    problem: &Problem<'_>,
    names: &AssetNames,
    now: OffsetDateTime,
) -> Result<Solved, SolveError> {
    check_inputs(problem)?;

    // The solver takes the problem by value, so the variable *handles* are kept
    // separately: they stay valid across the hand-over and the read-back.
    let (problem_vars, declared, risk, shared) = build_variables(problem, None);
    let objective = build_objective(problem, &declared, risk.as_ref());

    // The backend is chosen here rather than through `good_lp::default_solver`,
    // because Cargo unifies features across a workspace build: a crate that
    // enables `microlp` for its own tests would otherwise silently change which
    // solver the daemon runs. Naming it makes the choice deterministic —
    // HiGHS wins wherever it is available, and it is what a production box
    // should be built with.
    #[cfg(feature = "highs")]
    let mut model = {
        let m = problem_vars.minimise(objective).using(good_lp::highs);
        // A wall-clock limit makes the answer depend on how busy the machine
        // was, so a caller that needs the same inputs to give the same plan —
        // a replay, a regression suite, a saving figure somebody can check —
        // asks for no limit and waits.
        let m = if problem.solve_budget_s.is_finite() && problem.solve_budget_s > 0.0 {
            m.set_time_limit(problem.solve_budget_s)
        } else {
            m
        };
        // Clamped first, so the only error the setter can return is impossible
        // here. Proving the last fraction of a percent of a mixed-integer plan
        // costs more time than a household forecast is accurate to.
        #[allow(clippy::cast_possible_truncation)]
        let gap = problem.mip_gap.clamp(0.0, 1.0) as f32;
        m.set_mip_rel_gap(gap)
            .expect("a finite, non-negative gap is always accepted")
    };
    #[cfg(all(feature = "microlp", not(feature = "highs")))]
    let mut model = problem_vars.minimise(objective).using(good_lp::microlp);

    let mut rows = Vec::new();
    model = add_constraints(model, problem, &declared, &shared, risk.as_ref(), &mut rows);

    let solution = model.solve().map_err(|e| match e {
        good_lp::ResolutionError::Infeasible => SolveError::Infeasible(
            "no schedule satisfies every constraint — most often a charging target that cannot be reached before departure".into(),
        ),
        other => SolveError::Solver(other.to_string()),
    })?;

    // ── The dual pass ───────────────────────────────────────────────────────
    //
    // A mixed-integer program has no duals, so the discrete decisions are pinned
    // at what the solve chose and the linear program that remains is solved
    // again — on Clarabel, whichever backend did the first solve, so a box built
    // with `microlp` and one built with HiGHS agree about what a kilowatt-hour
    // is worth. See `crate::shadow`.
    let shadows = if problem.shadow_prices {
        shadow_prices(problem, &solution, &declared[0].borrow())
    } else {
        Vec::new()
    };

    // The report shows the **central** future — the one with the largest
    // probability, and the one a household would recognise as "the plan". Its
    // first slot is shared with every other future by construction
    // (`non_anticipativity`), so the only decision the arbiter is about to
    // commit is unambiguous whatever the weather does; the slots after it are
    // advisory, and the plan is re-made long before they arrive.
    let central = central_future(problem);
    Ok(read_back(
        problem, names, now, &solution, &declared, central, &shadows,
    ))
}

/// Whether the problem's inputs actually describe the horizon it plans over.
///
/// An input that does not reach far enough has to be refused rather than filled
/// in, because a plan built on a filler is not obviously wrong, is not logged
/// and fails no test — it is simply optimal against a day nobody is going to
/// have. Two fillers are lies:
///
/// * a **forecast** with no band for a slot reads as zero, which says the roof
///   is dark *and* the house is empty — wrong in both directions at once, so the
///   plan defers flexible load into hours it believes cost nothing;
/// * a **price stack** whose entry `k` is not the horizon's slot `k` prices the
///   day against somebody else's hours. The stack carries its own [`Slot`], so
///   this costs one comparison per slot.
///
/// A price stack that merely *stops short* is allowed: a horizon can run past
/// the last published auction, and a flat default out there makes the plan
/// indifferent about when to act, which is the state of knowledge. So do the
/// outdoor temperature (the last value given, then 10 °C) and the hot-water draw
/// (no draw). See D67.
///
/// [`Slot`]: hems_core::prelude::Slot
///
/// # Errors
/// [`SolveError::EmptyHorizon`], [`SolveError::ForecastTooShort`] or
/// [`SolveError::PricesMisaligned`].
pub fn check_inputs(problem: &Problem<'_>) -> Result<(), SolveError> {
    let slots = problem.horizon.len;
    if slots == 0 {
        return Err(SolveError::EmptyHorizon);
    }

    for (series, forecast) in [("production", problem.pv), ("load", problem.load)] {
        let covered = problem
            .horizon
            .slots()
            .take_while(|s| forecast.at(*s).is_some())
            .count();
        if covered < slots {
            return Err(SolveError::ForecastTooShort {
                series,
                covered,
                slots,
            });
        }
    }

    for (position, price) in problem.prices.slots.iter().enumerate().take(slots) {
        let expected = problem.horizon.get(position);
        if expected != Some(price.slot) {
            return Err(SolveError::PricesMisaligned {
                position,
                found: format!("{}", price.slot.start()),
                expected: expected.map_or_else(
                    || "past the end of the horizon".into(),
                    |s| format!("{}", s.start()),
                ),
            });
        }
    }

    Ok(())
}

/// Which future the report shows: the most likely one.
fn central_future(problem: &Problem<'_>) -> usize {
    problem
        .realisations()
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.probability.total_cmp(&b.probability))
        .map_or(0, |(i, _)| i)
}

/// Re-solve with the binaries pinned and read the duals of the state equations.
///
/// Returns an empty vector where the linear program cannot be solved. A missing
/// shadow price is not an error: it degrades the guard's allocation weights back
/// to the slot price they were before, which is exactly what the previous four
/// versions ran on, and refusing to produce a plan because a *diagnostic* failed
/// would be a worse trade than any it could inform.
fn shadow_prices(
    problem: &Problem<'_>,
    solution: &impl Solution,
    solved: &Vars<'_>,
) -> Vec<crate::shadow::Shadow> {
    use good_lp::solvers::{DualValues, SolutionWithDual};

    let n = problem.horizon.len;
    let pins = Pins {
        ev_on: (0..n)
            .map(|k| solution.value(solved.ev_on[k]).round())
            .collect(),
        hp_on: (0..n)
            .map(|k| solution.value(solved.hp_on[k]).round())
            .collect(),
        sh_start: solved
            .sh_start
            .iter()
            .map(|starts| starts.iter().map(|v| solution.value(*v).round()).collect())
            .collect(),
    };

    // ── The duals are prices, so the objective they come from is a price ─────
    //
    // The plan may have been made under a risk preference: `(1 − λ)·mean +
    // λ·CVaR`. Its duals are marginal values *of that objective*, which is a
    // mixture of money and a household's appetite for being caught out — and
    // "what is a kilowatt-hour worth here" is a question about money. Worse, the
    // state equations appear inside `cost_s`, which appears in both the
    // objective and the tail rows, so under a mixture the dual of a battery's
    // own state equation is a combination nobody can put a unit on.
    //
    // So the dual pass re-solves the same futures, with the same discrete
    // decisions pinned, against the **expectation**. The risk preference decided
    // the plan; it does not get to decide what the plan says a kilowatt-hour
    // costs.
    let priced = Problem {
        risk: crate::model::Risk {
            cvar_weight: 0.0,
            ..problem.risk
        },
        ..problem.clone()
    };
    let problem = &priced;
    let (problem_vars, declared, risk, shared) = build_variables(problem, Some(&pins));
    // The objective is scaled before the dual pass, and this is not a tuning
    // knob — it is the difference between duals and noise.
    //
    // The model is written in **watts** and **euros**: a variable is order 10³
    // and its objective coefficient is order 10⁻⁵ (a quarter hour of a
    // 30 ct/kWh kilowatt-hour, per watt). That is a condition number around
    // 10⁸, which a simplex method walks through without noticing — it only ever
    // compares coefficients with each other — and which an interior-point method
    // does not: against Clarabel's default tolerances the objective is nearly
    // flat, and the duals it returns are the arithmetic of that flatness rather
    // than the marginal values of anything. Unscaled, every slot of an eight-slot
    // household came back at 5 986 €/kWh.
    //
    // Multiplying the objective by a positive constant leaves the optimum
    // exactly where it was and scales every dual by the same constant, so
    // dividing them back out afterwards is not an approximation.
    let objective = build_objective(problem, &declared, risk.as_ref()) * DUAL_OBJECTIVE_SCALE;
    let mut rows: Vec<Rows> = Vec::new();
    let mut lp_model = problem_vars.minimise(objective).using(good_lp::clarabel);
    lp_model.settings().verbose(false);
    let model = add_constraints(
        lp_model,
        problem,
        &declared,
        &shared,
        risk.as_ref(),
        &mut rows,
    );
    let Ok(mut lp) = model.solve() else {
        return Vec::new();
    };
    let duals = lp.compute_dual();

    // ── Summed across the futures, not read from one ─────────────────────────
    //
    // Each future has its own copy of every state equation, and the objective
    // already carries that future's probability — so the dual of one of them is
    // already `p_s ×` the marginal value *in that future*. What a household
    // wants to know is what a kilowatt-hour in the battery is worth **whatever
    // happens**, which is a kilowatt-hour appearing in every future at once, and
    // the marginal value of that is the plain sum. Reading the central future
    // alone would silently report the median day's price as though the dull one
    // could not happen, which is the belief the scenario set exists to remove.
    let sum = |pick: &dyn Fn(&Rows) -> Option<good_lp::constraint::ConstraintReference>| {
        let mut total = None;
        for r in &rows {
            if let Some(reference) = pick(r) {
                *total.get_or_insert(0.0) += duals.dual(reference) / DUAL_OBJECTIVE_SCALE;
            }
        }
        total
    };
    (0..n)
        .map(|k| {
            crate::shadow::RawDuals {
                balance: sum(&|r| r.balance.get(k).cloned()).unwrap_or(0.0),
                battery: sum(&|r| r.battery.get(k).cloned()),
                ev: sum(&|r| r.ev.get(k).cloned()),
                dhw: sum(&|r| r.dhw.get(k).cloned()),
                air: sum(&|r| r.air.get(k).cloned()),
                mass: sum(&|r| r.mass.get(k).cloned()),
                steuve: sum(&|r| r.steuve.get(k).and_then(Clone::clone)),
            }
            .into_shadow(problem, k)
        })
        .collect()
}

/// The variable vectors, so the read-back can be a function of its own.
struct Vars<'a> {
    g_in: &'a [Variable],
    g_out: &'a [Variable],
    b_ch: &'a [Variable],
    b_dis: &'a [Variable],
    b_e: &'a [Variable],
    ev: &'a [Variable],
    /// `1` while the charge point is delivering on every conductor it has. A
    /// charge point below 6 A is not charging slowly, it is idle, so its power is
    /// semi-continuous.
    ev_on: &'a [Variable],
    ev_e: &'a [Variable],
    /// Energy the car was promised and the plan could not deliver, Wh.
    ev_short: &'a [Variable],
    curtail: &'a [Variable],
    /// Heat-pump electrical power.
    hp: &'a [Variable],
    /// `1` while the heat pump runs. Only meaningful for a non-modulating unit.
    hp_on: &'a [Variable],
    /// Indoor air temperature at the end of the slot, °C.
    t_in: &'a [Variable],
    /// Fabric temperature at the end of the slot, °C.
    t_mass: &'a [Variable],
    /// Kelvin below the comfort band.
    cold: &'a [Variable],
    /// Kelvin above it.
    warm: &'a [Variable],
    /// Electrical power of the hot-water heater.
    dhw: &'a [Variable],
    /// Heat in the tank at the end of the slot, Wh above the lowest acceptable
    /// temperature.
    dhw_e: &'a [Variable],
    /// Hot water the household asked for and the tank could not give, Wh.
    dhw_short: &'a [Variable],
    /// `1` where appliance `i` starts its programme in slot `k`, indexed
    /// `[i][k]`. Zero-pinned wherever a start would not fit the window.
    sh_start: &'a [Vec<Variable>],
    /// `1` where appliance `i` was not run at all — the soft half of its
    /// deadline. Integral for free: it is one minus a sum of binaries.
    sh_short: &'a [Variable],
    /// Import that a § 42c community allocated to this member, watts.
    shared: &'a [Variable],
}

/// Every decision variable of the model, owned.
///
/// One vector per quantity, one entry per slot. An absent battery, car or heat
/// pump still gets its variables — pinned to zero — so that the constraints
/// below have one shape rather than a branch in every line.
struct Variables {
    g_in: Vec<Variable>,
    g_out: Vec<Variable>,
    b_ch: Vec<Variable>,
    b_dis: Vec<Variable>,
    b_e: Vec<Variable>,
    ev: Vec<Variable>,
    ev_on: Vec<Variable>,
    ev_e: Vec<Variable>,
    ev_short: Vec<Variable>,
    curtail: Vec<Variable>,
    hp: Vec<Variable>,
    hp_on: Vec<Variable>,
    t_in: Vec<Variable>,
    t_mass: Vec<Variable>,
    cold: Vec<Variable>,
    warm: Vec<Variable>,
    dhw: Vec<Variable>,
    dhw_e: Vec<Variable>,
    dhw_short: Vec<Variable>,
    sh_start: Vec<Vec<Variable>>,
    sh_short: Vec<Variable>,
    shared: Vec<Variable>,
}

impl Variables {
    /// Borrow the vectors for the constraint and objective builders.
    fn borrow(&self) -> Vars<'_> {
        Vars {
            g_in: &self.g_in,
            g_out: &self.g_out,
            b_ch: &self.b_ch,
            b_dis: &self.b_dis,
            b_e: &self.b_e,
            ev: &self.ev,
            ev_on: &self.ev_on,
            ev_e: &self.ev_e,
            ev_short: &self.ev_short,
            curtail: &self.curtail,
            hp: &self.hp,
            hp_on: &self.hp_on,
            t_in: &self.t_in,
            t_mass: &self.t_mass,
            cold: &self.cold,
            warm: &self.warm,
            dhw: &self.dhw,
            dhw_e: &self.dhw_e,
            dhw_short: &self.dhw_short,
            sh_start: &self.sh_start,
            sh_short: &self.sh_short,
            shared: &self.shared,
        }
    }
}

/// The integer decisions the mixed-integer solve made, so the dual pass can pin
/// them.
///
/// A mixed-integer program has no duals; the standard construction is to fix the
/// discrete decisions and re-solve the linear program that remains. These are
/// those decisions, read back and rounded — a solver returns `0,999 999 999` for
/// a binary and a bound of `0,999 999 999` is not a pin.
#[derive(Debug, Default)]
struct Pins {
    ev_on: Vec<f64>,
    hp_on: Vec<f64>,
    /// Where each appliance's programme was placed, `[i][k]`.
    sh_start: Vec<Vec<f64>>,
}

/// Declare the variables and their bounds.
///
/// `pins`, when given, replaces every binary with a constant — which is what
/// turns the mixed-integer model into the linear program whose duals are the
/// shadow prices of [`crate::shadow`].
fn build_variables(
    problem: &Problem<'_>,
    pins: Option<&Pins>,
) -> (
    ProblemVariables,
    Vec<Variables>,
    Option<RiskVars>,
    SharedBinaries,
) {
    let mut vars = ProblemVariables::new();
    let futures = problem.realisations();

    // The discrete commitments, created **once** and shared by every future.
    // See `Risk`: whether the charge point runs at all in a slot, whether an
    // on/off heat pump is on, when the dishwasher starts — those are things a
    // household does once. Letting them differ per future would triple the
    // integrality, which is the expensive part, to buy a schedule nobody can
    // carry out.
    let shared = shared_binaries(problem, &mut vars, pins);

    let per: Vec<Variables> = futures
        .iter()
        .map(|realisation| recourse_variables(problem, &mut vars, &shared, *realisation))
        .collect();

    // ζ and one tail excess per future — the Rockafellar–Uryasev linearisation
    // of the conditional value at risk. ζ is free: at the optimum it is the
    // value at risk itself, which can be any sign.
    //
    // Declared **only** where the tail carries weight. A risk-neutral solve then
    // hands the backend exactly the model it had before any of this existed —
    // no free unbounded column, no dead rows — which is what makes
    // `Risk::deterministic` a comparison against the old planner rather than
    // against a differently-conditioned one.
    let risk = (problem.risk.tail_weight() > 0.0).then(|| RiskVars {
        zeta: vars.add(variable()),
        tail: futures
            .iter()
            .map(|_| vars.add(variable().min(0.0)))
            .collect(),
    });

    (vars, per, risk, shared)
}

/// The decisions every future shares.
struct SharedBinaries {
    ev_on: Vec<Variable>,
    hp_on: Vec<Variable>,
    sh_start: Vec<Vec<Variable>>,
    sh_short: Vec<Variable>,
}

/// ζ and the tail excesses of the conditional value at risk.
struct RiskVars {
    /// The value at risk. Free: at the optimum it is the `α`-quantile of the
    /// scenario costs, and a household's cost can be negative on a June day.
    zeta: Variable,
    /// `max(0, cost_s − ζ)` for each future, one row each.
    tail: Vec<Variable>,
}

/// The discrete commitments, shared by every future.
fn shared_binaries(
    problem: &Problem<'_>,
    vars: &mut ProblemVariables,
    pins: Option<&Pins>,
) -> SharedBinaries {
    let n = problem.horizon.len;
    let mut shared = SharedBinaries {
        ev_on: Vec::with_capacity(n),
        hp_on: Vec::with_capacity(n),
        sh_start: Vec::with_capacity(problem.shiftable.len()),
        sh_short: Vec::with_capacity(problem.shiftable.len()),
    };
    // How far ahead the compressor is decided slot by slot. Beyond it, a block
    // of slots shares **one** variable rather than being tied together by
    // equalities: an equality row still leaves branch and bound a column to open
    // a node on, and the whole point is that there is nothing there to branch.
    let blocks = problem.thermal.map(|t| {
        problem
            .commitment_horizon
            .blocks(problem.horizon, &t.heat_pump)
    });
    for k in 0..n {
        shared
            .ev_on
            .push(charge_point_binary(problem, vars, k, pins));
        let representative = blocks.as_ref().map_or(k, |b| b[k]);
        shared.hp_on.push(if representative == k {
            heat_pump_binary(problem, vars, k, pins)
        } else {
            shared.hp_on[representative]
        });
    }
    shiftable_variables(problem, vars, &mut shared, pins);
    shared
}

/// The continuous decisions of one future.
fn recourse_variables(
    problem: &Problem<'_>,
    vars: &mut ProblemVariables,
    shared: &SharedBinaries,
    realisation: crate::model::Realisation,
) -> Variables {
    let n = problem.horizon.len;
    let mut v = Variables {
        g_in: Vec::with_capacity(n),
        g_out: Vec::with_capacity(n),
        b_ch: Vec::with_capacity(n),
        b_dis: Vec::with_capacity(n),
        b_e: Vec::with_capacity(n),
        ev: Vec::with_capacity(n),
        ev_on: Vec::with_capacity(n),
        ev_e: Vec::with_capacity(n),
        ev_short: Vec::with_capacity(n),
        curtail: Vec::with_capacity(n),
        hp: Vec::with_capacity(n),
        hp_on: Vec::with_capacity(n),
        t_in: Vec::with_capacity(n),
        t_mass: Vec::with_capacity(n),
        cold: Vec::with_capacity(n),
        warm: Vec::with_capacity(n),
        dhw: Vec::with_capacity(n),
        dhw_e: Vec::with_capacity(n),
        dhw_short: Vec::with_capacity(n),
        sh_start: Vec::with_capacity(problem.shiftable.len()),
        sh_short: Vec::with_capacity(problem.shiftable.len()),
        shared: Vec::with_capacity(n),
    };

    let import_ceiling = problem
        .limits
        .import_ceiling
        .map_or(f64::INFINITY, Power::get);
    let has_sharing = problem.has_community_share();

    for k in 0..n {
        let (pv, _load) = problem.forecasts_in(realisation, k);
        // The feed-in ceiling is per slot: § 9 EEG does not lapse, an LPP
        // session does, and a horizon that spans the end of one has to know
        // which of its slots the limit reaches.
        let export_ceiling = problem
            .horizon
            .get(k)
            .and_then(|s| problem.limits.feed_in_at(s))
            .map_or(f64::INFINITY, Power::get);
        v.g_in
            .push(vars.add(variable().min(0.0).max(import_ceiling)));
        // The § 42c cheap block: import the community allocated to this member.
        // Bounded by the share above and by the household's own import below
        // (`sharing`), so it is exactly `min(share, g_in)` wherever the community
        // is the cheaper of the two — which is the only case a linear program can
        // represent, and the only case anybody signs a contract for.
        //
        // **Not declared at all** where there is no community, rather than
        // declared and pinned to zero. A pinned column is not free: it changes
        // the order the backend sees the model in, and the reference winter day
        // came back a cent different for it. A household outside a community
        // gets the model byte for byte as it was before § 42c existed, which is
        // what makes every figure measured before this still a figure about the
        // same planner.
        if has_sharing {
            v.shared
                .push(vars.add(variable().min(0.0).max(problem.community_share_at(k))));
        }
        v.g_out
            .push(vars.add(variable().min(0.0).max(export_ceiling)));
        v.curtail.push(vars.add(variable().min(0.0).max(pv)));

        let (charge_max, discharge_max, floor, ceiling) =
            problem.battery.map_or((0.0, 0.0, 0.0, 0.0), |b| {
                (
                    b.max_charge.get(),
                    b.max_discharge.get(),
                    b.floor_energy().get(),
                    b.ceiling_energy().get(),
                )
            });
        v.b_ch.push(vars.add(variable().min(0.0).max(charge_max)));
        v.b_dis
            .push(vars.add(variable().min(0.0).max(discharge_max)));
        v.b_e.push(vars.add(variable().min(floor).max(ceiling)));

        charge_point_variables(problem, vars, &mut v);
        v.ev_on.push(shared.ev_on[k]);

        let (dhw_max, dhw_capacity) = problem
            .dhw
            .map_or((0.0, 0.0), |d| (d.heater.get(), d.capacity.get()));
        v.dhw.push(vars.add(variable().min(0.0).max(dhw_max)));
        v.dhw_e
            .push(vars.add(variable().min(0.0).max(dhw_capacity)));
        // The shortfall covers the standing loss as well as the draw. `dhw_e ≥ 0`
        // is the lowest temperature the household accepts, not the ambient one,
        // so a tank on that floor is still losing heat — and an equality that can
        // only be closed by heating forces the heater to `loss/cop` for ever, and
        // is infeasible wherever a heater cannot out-run its own cylinder.
        let dhw_deficit = problem
            .dhw
            .map_or(0.0, |d| d.standing_loss.get() * DT_HOURS)
            + problem.dhw_draw_at(k);
        v.dhw_short
            .push(vars.add(variable().min(0.0).max(dhw_deficit)));

        v.hp_on.push(shared.hp_on[k]);
        match problem.thermal {
            Some(t) => {
                v.hp.push(vars.add(variable().min(0.0).max(t.heat_pump.max_electrical.get())));
                // Wide bounds on the temperatures: the comfort band is a *soft*
                // constraint, and pinning the state variable to it would make a
                // cold snap infeasible instead of merely uncomfortable.
                v.t_in.push(vars.add(variable().min(-20.0).max(45.0)));
                v.t_mass.push(vars.add(variable().min(-20.0).max(45.0)));
                v.cold.push(vars.add(variable().min(0.0)));
                v.warm.push(vars.add(variable().min(0.0)));
            }
            None => {
                for target in [
                    &mut v.hp,
                    &mut v.t_in,
                    &mut v.t_mass,
                    &mut v.cold,
                    &mut v.warm,
                ] {
                    target.push(vars.add(variable().min(0.0).max(0.0)));
                }
            }
        }
    }

    v.sh_start.clone_from(&shared.sh_start);
    v.sh_short.clone_from(&shared.sh_short);
    v
}

/// The charge point's on/off binary for one slot.
///
/// A binary is declared **only where it can decide something**: there is a car,
/// it has a floor to respect, and the slot is one it could still be charging in.
/// Every other slot is pinned, so branch and bound never opens a node for a car
/// that has already left — which on a 192-slot horizon is most of them, and the
/// difference between a plan in a second and a plan in a minute.
fn charge_point_binary(
    problem: &Problem<'_>,
    vars: &mut ProblemVariables,
    k: usize,
    pins: Option<&Pins>,
) -> Variable {
    let absent = problem
        .ev
        .zip(problem.horizon.get(k))
        .is_some_and(|(e, slot)| !e.present_in(slot));
    match (problem.ev, pins) {
        (Some(e), Some(p)) if e.min_charge > Power::ZERO && !absent => {
            let on = p.ev_on.get(k).copied().unwrap_or(0.0);
            vars.add(variable().min(on).max(on))
        }
        (Some(e), None) if e.min_charge > Power::ZERO && !absent => vars.add(variable().binary()),
        (Some(_), _) if absent => vars.add(variable().min(0.0).max(0.0)),
        _ => vars.add(variable().min(1.0).max(1.0)),
    }
}

/// The heat pump's on/off binary for one slot.
///
/// A binary only where the unit really is on/off. For a modulating unit it is
/// pinned to one and never branches, so the problem stays a linear program —
/// which is what keeps a long horizon solvable on a gateway box.
///
/// It is also pinned over the slots the compressor's **own history** has already
/// decided. A unit that started four minutes ago owes its minimum runtime
/// whatever this plan would prefer, and the model's own `min_on_slots` rows
/// cannot say so: they are written against a `k − 1` that, for the first slot of
/// a horizon, does not exist. Pinning is the same mechanism the charge point's
/// absent slots use — the bound enforces it, so branch and bound never opens a
/// node for a decision that was made before the plan started. See
/// [`CompressorState`].
///
/// [`CompressorState`]: crate::model::CompressorState
fn heat_pump_binary(
    problem: &Problem<'_>,
    vars: &mut ProblemVariables,
    k: usize,
    pins: Option<&Pins>,
) -> Variable {
    let Some(t) = problem.thermal else {
        return vars.add(variable().min(0.0).max(0.0));
    };
    if t.heat_pump.modulating {
        return vars.add(variable().min(1.0).max(1.0));
    }
    // What the compressor is already committed to, before this plan decides
    // anything.
    if let Some((running, left)) = t.heat_pump.committed()
        && k < left
    {
        let on = f64::from(u8::from(running));
        return vars.add(variable().min(on).max(on));
    }
    match pins {
        Some(p) => {
            let on = p.hp_on.get(k).copied().unwrap_or(0.0);
            vars.add(variable().min(on).max(on))
        }
        None => vars.add(warm(variable().binary(), problem, |c| {
            crate::model::Commitment::hint(&c.hp_on, k)
        })),
    }
}

/// Attach the previous plan's value to a variable, where there is one.
///
/// An initial value is a hint the backend may use as an incumbent and may
/// ignore; it changes no bound, no row and no objective coefficient, which is
/// what makes a warm-started plan the same plan as a cold one. Clarabel — the
/// dual pass — does not read it, and does not need to: its binaries are pinned.
fn warm(
    v: good_lp::variable::VariableDefinition,
    problem: &Problem<'_>,
    pick: impl Fn(&crate::model::Commitment) -> Option<f64>,
) -> good_lp::variable::VariableDefinition {
    match problem.warm_start.and_then(pick) {
        Some(value) => v.initial(value),
        None => v,
    }
}

/// One binary per **feasible** start for every appliance, and the soft
/// alternative of not running it.
///
/// Declaring a binary for every slot would be the obvious loop and it is what
/// makes a 192-slot horizon slow for nothing: a two-hour programme that must
/// finish by six has a few dozen places it can go, and branch and bound has no
/// business opening a node for the other hundred and fifty. Every infeasible
/// start is pinned to zero here instead, so the window is enforced by the
/// variable's own bounds rather than by a constraint the solver has to discover.
fn shiftable_variables(
    problem: &Problem<'_>,
    vars: &mut ProblemVariables,
    v: &mut SharedBinaries,
    pins: Option<&Pins>,
) {
    for (i, run) in problem.shiftable.iter().enumerate() {
        let starts: Vec<Variable> = (0..problem.horizon.len)
            .map(|k| {
                if !run.can_start_at(problem.horizon, k) {
                    return vars.add(variable().min(0.0).max(0.0));
                }
                match pins {
                    // The dual pass re-solves with the placement fixed, so the
                    // duals it reads are the marginal values *given* where the
                    // machine went — which is the conditioning the arbiter is
                    // living under too.
                    Some(p) => {
                        let on = p
                            .sh_start
                            .get(i)
                            .and_then(|r| r.get(k))
                            .copied()
                            .unwrap_or(0.0);
                        vars.add(variable().min(on).max(on))
                    }
                    None => vars.add(variable().binary()),
                }
            })
            .collect();
        v.sh_start.push(starts);
        // Integral without being declared so: one minus a sum of binaries.
        v.sh_short.push(vars.add(variable().min(0.0).max(1.0)));
    }
}

/// The charge point's variables for one slot.
///
/// Split out because it is where most of the model's integrality lives, and
/// because the charge point's *binary* is shared across the futures
/// ([`charge_point_binary`]) while its power, its stored energy and its
/// shortfall are recourse — a car may charge harder in a sunny future and the
/// plan is allowed to say so.
fn charge_point_variables(problem: &Problem<'_>, vars: &mut ProblemVariables, v: &mut Variables) {
    let (ev_max, ev_capacity) = problem
        .ev
        .map_or((0.0, 0.0), |e| (e.max_charge.get(), e.capacity.get()));
    v.ev.push(vars.add(variable().min(0.0).max(ev_max)));
    v.ev_e.push(vars.add(variable().min(0.0).max(ev_capacity)));
    // How much of the promise the plan had to give up. Priced high enough to be
    // lexicographic in practice, but *finite* — a deadline that cannot be met
    // should produce the best achievable schedule, not no schedule at all.
    v.ev_short
        .push(vars.add(variable().min(0.0).max(ev_capacity)));
}

/// The power appliance `i` draws in slot `k`, as a linear expression.
///
/// A start in slot `j` puts `programme[k − j]` into slot `k`, so the power in a
/// slot is the sum over every start that would still be running. It is an
/// expression rather than a variable on purpose: there is nothing to decide
/// beyond the start, and a variable tied to one by an equality is a row the
/// solver has to carry for every slot of every appliance.
fn shiftable_power(problem: &Problem<'_>, vars: &Vars<'_>, i: usize, k: usize) -> Expression {
    let run = &problem.shiftable[i];
    let first = k.saturating_sub(run.programme.slots().saturating_sub(1));
    vars.sh_start[i][first..=k].iter().enumerate().fold(
        Expression::from(0.0),
        |acc, (offset, start)| {
            // `offset` counts forward from the earliest start still running in
            // this slot, so the step it is in counts backward from `k`.
            let watts = run.programme.power_at(k - (first + offset)).get();
            if watts > 0.0 {
                acc + *start * watts
            } else {
                acc
            }
        },
    )
}

/// Everything the shiftable appliances draw in slot `k`.
fn shiftable_total(problem: &Problem<'_>, vars: &Vars<'_>, k: usize) -> Expression {
    (0..problem.shiftable.len()).fold(Expression::from(0.0), |acc, i| {
        acc + shiftable_power(problem, vars, i, k)
    })
}

/// What the plan is trying to minimise: a weighted sum of the **mean** cost over
/// the futures and the **tail** of them.
///
/// ```text
/// (1 − λ)·Σ_s p_s·cost_s  +  λ·CVaR_α
/// CVaR_α = ζ + 1/(1 − α)·Σ_s p_s·tail_s ,  tail_s ≥ cost_s − ζ ,  tail_s ≥ 0
/// ```
///
/// The second line is Rockafellar and Uryasev's linearisation: minimising over
/// `ζ` makes it exactly the mean cost of the worst `1 − α` of outcomes, at the
/// price of one free variable and one row per future. With
/// [`ScenarioSet::Swanson`]'s three futures and `α = 0,7` the tail is the
/// pessimistic one, so the objective reads "mostly the average day, partly the
/// dull cold one".
///
/// `λ = 0` collapses it to expected cost, and a single-future set collapses the
/// whole construction to the deterministic objective — the same expression the
/// planner minimised before any of this existed, which is what makes
/// [`Risk::deterministic`] an honest comparison rather than a different model.
///
/// [`ScenarioSet::Swanson`]: crate::model::ScenarioSet::Swanson
/// [`Risk::deterministic`]: crate::model::Risk::deterministic
fn build_objective(
    problem: &Problem<'_>,
    per: &[Variables],
    risk: Option<&RiskVars>,
) -> Expression {
    let futures = problem.realisations();
    let mut mean = Expression::from(0.0);
    for (s, realisation) in futures.iter().enumerate() {
        mean += scenario_cost(problem, &per[s].borrow(), *realisation) * realisation.probability;
    }
    let Some(risk) = risk else {
        return mean;
    };

    let lambda = problem.risk.tail_weight();
    let mut tail = Expression::from(risk.zeta);
    for (s, realisation) in futures.iter().enumerate() {
        tail += risk.tail[s] * (realisation.probability * problem.risk.tail_scale());
    }
    mean * (1.0 - lambda) + tail * lambda
}

/// What one future costs, in euros over the horizon.
fn scenario_cost(
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    realisation: crate::model::Realisation,
) -> Expression {
    let mut objective = Expression::from(0.0);
    for k in 0..problem.horizon.len {
        let price = problem.prices.slots.get(k);
        // Everything is euros. A carbon price and an autarky premium are what
        // the household is willing to pay to avoid a kilogram of CO₂ and a
        // kilowatt-hour of import, so they add to the energy price instead of
        // replacing it — which is what keeps them comparable with battery wear,
        // curtailment and discomfort.
        let carbon_eur = price
            .and_then(|p| p.co2_g_per_kwh)
            .unwrap_or(DEFAULT_CO2_G_PER_KWH)
            / 1000.0
            * problem.objective.co2_eur_per_kg;
        let import_eur = price.map_or(
            DEFAULT_IMPORT_EUR_PER_KWH,
            hems_tariff::SlotPrice::import_f64,
        ) + carbon_eur
            + problem.objective.autarky_eur_per_kwh;
        let export_eur = price.map_or(
            DEFAULT_EXPORT_EUR_PER_KWH,
            hems_tariff::SlotPrice::export_f64,
        );
        objective += (vars.g_in[k] * import_eur - vars.g_out[k] * export_eur) * SLOT_KWH_PER_W;
        // § 42c: the part of that import the community allocated to this member
        // is billed at the community's price instead of the supplier's, so it
        // comes back off as a discount. Never negative — see
        // `SlotPrice::sharing_discount_f64`, which is what keeps the cheap block
        // convex and therefore linear.
        let sharing_eur = price.map_or(0.0, hems_tariff::SlotPrice::sharing_discount_f64);
        if let Some(shared) = vars.shared.get(k).filter(|_| sharing_eur > 0.0) {
            objective -= *shared * sharing_eur * SLOT_KWH_PER_W;
        }
        objective += vars.curtail[k] * problem.curtailment_penalty_eur_per_kwh * SLOT_KWH_PER_W;
        // Per watt-hour, not per watt: this is stored energy, not a flow.
        objective += vars.ev_short[k] * (problem.unmet_charge_eur_per_kwh / 1000.0);
        if let Some(b) = problem.battery {
            // Wear is charged on throughput, half on each leg, so a full cycle
            // pays it once.
            let wear = b.degradation_eur_per_kwh / 2.0;
            objective += (vars.b_ch[k] + vars.b_dis[k]) * wear * SLOT_KWH_PER_W;
        }
        if let Some(t) = problem.thermal {
            // Kelvin-hours outside the comfort band, priced.
            objective +=
                (vars.cold[k] + vars.warm[k]) * t.discomfort_eur_per_kelvin_hour * DT_HOURS;
        }
        if let Some(d) = problem.dhw {
            // Hot water asked for and not delivered. Per watt-hour, because it
            // is stored heat rather than a flow.
            objective += vars.dhw_short[k] * (d.shortfall_eur_per_kwh / 1000.0);
        }
    }

    // A programme the plan decided not to run at all. Priced once, not per slot:
    // it is a decision rather than a flow, and it is the term that stops the
    // cheapest schedule being the one where nothing ever happens.
    //
    // It is the same shared variable in every future, so it adds the same
    // constant to each — which shifts ζ and leaves the tail where it was. It is
    // carried inside the scenario cost anyway, so that `cost_s` is the whole of
    // what a future costs and the report and the objective say the same thing.
    for (i, run) in problem.shiftable.iter().enumerate() {
        objective += vars.sh_short[i] * run.unserved_eur.max(0.0);
    }

    // What is left in the tank is worth what it would cost to put back — the
    // same argument as the battery's terminal value, and without it a
    // receding-horizon plan lets the water go cold on the last evening of every
    // horizon it ever computes.
    if let Some(d) = problem.dhw
        && d.cop > 0.0
    {
        let value_per_wh = mean_import_price(problem) / d.cop / 1000.0;
        objective -= vars.dhw_e[problem.horizon.len - 1] * value_per_wh;
    }

    // The heat the building is holding at the end of the horizon is worth what
    // it would cost to put back — the same argument as the battery's terminal
    // value, and the reason a plan does not let the house coast cold into its
    // own last slot. Valuing it at the replacement cost keeps it neutral: the
    // plan pre-heats when that is genuinely cheaper, and not otherwise.
    let _ = realisation;
    if let Some(t) = problem.thermal {
        let mean_outdoor = if problem.outdoor_c.is_empty() {
            10.0
        } else {
            problem.outdoor_c.iter().sum::<f64>() / problem.outdoor_c.len() as f64
        };
        let mean_import = mean_import_price(problem);
        let value_per_kelvin = mean_import / t.heat_pump.cop(mean_outdoor);
        let last = problem.horizon.len - 1;
        objective -= vars.t_in[last] * (value_per_kelvin * t.building.air_capacity_kwh_per_k);
        objective -= vars.t_mass[last] * (value_per_kelvin * t.building.mass_capacity_kwh_per_k);
    }

    // What is left in the battery when the horizon ends is worth something: a
    // receding-horizon controller re-plans long before it gets there, and a plan
    // that empties the store into its own last slot is an artefact of where the
    // horizon stops rather than a decision.
    if let Some(b) = problem.battery
        && problem.terminal_value_factor > 0.0
    {
        // Per watt-hour, discounted by what it costs to get the energy back out.
        let value_per_wh =
            mean_import_price(problem) * problem.terminal_value_factor * b.efficiency_discharge
                / 1000.0;
        objective -= vars.b_e[problem.horizon.len - 1] * value_per_wh;
    }

    objective
}

/// The mean import price over the horizon, €/kWh — the number both terminal
/// values are expressed in.
fn mean_import_price(problem: &Problem<'_>) -> f64 {
    if problem.prices.slots.is_empty() {
        DEFAULT_IMPORT_EUR_PER_KWH
    } else {
        problem
            .prices
            .slots
            .iter()
            .map(hems_tariff::SlotPrice::import_f64)
            .sum::<f64>()
            / problem.prices.slots.len() as f64
    }
}

/// Where each state equation ended up in the model, so its dual can be read.
///
/// One entry per slot per store. Only the **equalities that define a state**
/// are recorded: those are the rows whose duals are marginal values of a
/// *quantity the household owns*, which is what [`crate::shadow`] is after. The
/// inequalities — the grid ceilings, the fuse — have duals too, and they are the
/// price of a *limit* rather than of a store; they are not what weights an
/// allocation and are deliberately not collected.
#[derive(Debug, Default)]
struct Rows {
    /// The energy balance: one more watt of load in this slot.
    balance: Vec<ConstraintReference>,
    /// The battery's state of charge at the end of the slot.
    battery: Vec<ConstraintReference>,
    /// The car's.
    ev: Vec<ConstraintReference>,
    /// The heat in the hot-water tank.
    dhw: Vec<ConstraintReference>,
    /// The indoor air temperature, and the fabric behind it.
    air: Vec<ConstraintReference>,
    mass: Vec<ConstraintReference>,
    /// The § 14a ceiling, where one binds in this slot.
    ///
    /// The one *inequality* whose dual is collected, and it is collected for a
    /// different purpose from the rest: it is not what a store holds, it is what
    /// a **kilowatt of relief from the network operator's limit** is worth to
    /// this household. That is the number a § 41e offer or an OpenADR bid is
    /// made of, and until now the design's answer to "what is your flexibility
    /// worth" was "assume 30 % of nominal" (§ 24.3).
    steuve: Vec<Option<ConstraintReference>>,
}

/// The physical and regulatory constraints, slot by slot.
///
/// Takes `rows` so the same builder serves both passes: the mixed-integer solve
/// throws the references away, and the dual pass keeps them. One enumeration of
/// the model, not two — a second copy for the dual pass would be a second thing
/// that can disagree with the first about what the plan meant.
fn add_constraints<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    per: &[Variables],
    shared: &SharedBinaries,
    risk: Option<&RiskVars>,
    rows: &mut Vec<Rows>,
) -> M {
    let futures = problem.realisations();
    for (s, realisation) in futures.iter().enumerate() {
        let vars = per[s].borrow();
        let mut scenario_rows = Rows::default();
        for k in 0..problem.horizon.len {
            model = balance(model, problem, &vars, *realisation, k, &mut scenario_rows);
            model = storage(model, problem, &vars, *realisation, k, &mut scenario_rows);
            model = charging(model, problem, &vars, k, &mut scenario_rows);
            model = grid_rules(model, problem, &vars, *realisation, k, &mut scenario_rows);
            model = building(model, problem, &vars, k, &mut scenario_rows);
            model = hot_water(model, problem, &vars, k, &mut scenario_rows);
        }
        rows.push(scenario_rows);
    }
    // Shared by every future, so stated once.
    model = shiftable_runs(model, problem, &per[0].borrow());
    model = minimum_runtime(model, problem, shared);
    model = non_anticipativity(model, problem, per);
    conditional_value_at_risk(model, problem, per, risk)
}

/// The first slot is decided **once**, whatever the weather turns out to be.
///
/// Non-anticipativity, and it is what makes the plan a plan. The arbiter is
/// about to commit the next fifteen minutes; a schedule that gave three
/// different answers for it — one per future — would be three schedules and a
/// coin. Everything after the first slot is *recourse*: the plan may say "and if
/// the afternoon is dull I do this instead", which is exactly what makes hedging
/// affordable rather than a tax on every sunny day.
///
/// Only the **controllable** decisions are tied. The grid, the curtailment and
/// every state variable are left free, and that is not an oversight: production
/// differs between the futures by kilowatts in the first slot, so an import that
/// had to be identical in all three would be infeasible the moment the band was
/// wider than nothing.
fn non_anticipativity<M: SolverModel>(mut model: M, problem: &Problem<'_>, per: &[Variables]) -> M {
    if problem.horizon.len == 0 {
        return model;
    }
    let first = per[0].borrow();
    for other in per.iter().skip(1) {
        let o = other.borrow();
        for (a, b) in [
            (first.b_ch[0], o.b_ch[0]),
            (first.b_dis[0], o.b_dis[0]),
            (first.ev[0], o.ev[0]),
            (first.hp[0], o.hp[0]),
            (first.dhw[0], o.dhw[0]),
        ] {
            model = model.with(constraint!(a - b == 0.0));
        }
    }
    model
}

/// The two rows per future that turn a tail into a linear program.
///
/// `tail_s ≥ cost_s − ζ` with `tail_s ≥ 0` (a bound, already declared). At the
/// optimum of [`build_objective`] `ζ` is the value at risk and `tail_s` the
/// excess above it, so the weighted sum of the excesses is the conditional value
/// at risk. Nothing here is an approximation: it is exact for any finite
/// scenario set.
fn conditional_value_at_risk<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    per: &[Variables],
    risk: Option<&RiskVars>,
) -> M {
    // With no weight on the tail there are no such variables to constrain, and a
    // risk-neutral solve is byte for byte the model it always was.
    let Some(risk) = risk else {
        return model;
    };
    for (s, realisation) in problem.realisations().iter().enumerate() {
        let cost = scenario_cost(problem, &per[s].borrow(), *realisation);
        // `tail_s + ζ − cost_s ≥ 0`, built as one expression rather than through
        // the comparison macro: `cost_s` is the whole of a future's objective —
        // several hundred terms — and a macro that has to decide which side of a
        // relation each of them belongs on is not the place to find that out.
        //
        // Getting the sign wrong is not subtle and does not fail quietly: ζ
        // appears in the objective with a positive coefficient, so with the row
        // reversed nothing stops it going to minus infinity and every solve
        // comes back `Unbounded`.
        model = model.with((risk.tail[s] + risk.zeta - cost).geq(0.0));
    }
    model
}

/// What goes in equals what comes out, and neither direction can be invented.
fn balance<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    realisation: crate::model::Realisation,
    k: usize,
    rows: &mut Rows,
) -> M {
    let (pv, load) = problem.forecasts_in(realisation, k);

    // Energy balance:
    //     import − export = load + charging + car − (production − curtailed)
    // rearranged so the constants sit on the right. Curtailment *removes*
    // production, so it pushes the balance towards importing — the sign that is
    // easiest to get backwards and hardest to notice, because it only shows up
    // once a feed-in limit forces curtailment at all.
    rows.balance.push(model.add_constraint(constraint!(
        vars.g_in[k] - vars.g_out[k] - vars.b_ch[k] + vars.b_dis[k]
            - vars.ev[k]
            - vars.hp[k]
            - vars.dhw[k]
            - shiftable_total(problem, vars, k)
            - vars.curtail[k]
            == load - pv
    )));

    // § 42c: a member can only be allocated electricity it actually drew. The
    // other end of the cheap block is the variable's own upper bound — the
    // community's generation times this member's Aufteilungsschlüssel — so
    // between them the two say `shared = min(share, g_in)` at the optimum,
    // without a binary and without a `min`.
    //
    // Stated only where a community offers something, so a household without one
    // carries no row.
    if let Some(shared) = vars.shared.get(k) {
        model = model.with(constraint!(*shared <= vars.g_in[k]));
    }

    // Power cannot be imported and exported at the same instant. Stating it as a
    // physical bound rather than a binary keeps the model a pure linear program —
    // which matters on a gateway box — and says something true: exported power
    // has to come from the roof or the battery.
    //
    // Without it the model is *unbounded* wherever export earns more than import
    // costs, and it will happily invent an infinite round trip through a meter.
    //
    // # Its mirror is implied, and stating it destroys every shadow price
    //
    // The mirror statement — `g_in ≤ load + b_ch + ev + hp + dhw`, imported power
    // has to go into the house or a store — reads like the other half of the
    // same fact and is **implied by this one and the balance**. Substituting the
    // balance into it gives it back exactly, so it constrained nothing.
    //
    // A redundant constraint is usually harmless. This one was not, because it
    // is *always active*: whenever the household is not exporting, the import
    // equals the consumption and the row is tight. An always-tight redundant row
    // has a free non-negative dual, and it absorbs the energy balance's dual
    // along with it — so the shadow price of a kilowatt-hour became
    // indeterminate, and an interior-point solver, asked for the analytic centre
    // of a dual face that is a whole ray, returned 5 986 €/kWh for an ordinary
    // 20 ct hour. See [`shadow_prices`].
    model.with(constraint!(
        vars.g_out[k] <= pv - vars.curtail[k] + vars.b_dis[k]
    ))
}

/// The battery: where its charge comes from and where it may go.
fn storage<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    realisation: crate::model::Realisation,
    k: usize,
    rows: &mut Rows,
) -> M {
    let Some(b) = problem.battery else {
        return model;
    };
    let previous: Expression = if k == 0 {
        Expression::from(b.energy_now().get())
    } else {
        vars.b_e[k - 1].into()
    };
    rows.battery.push(model.add_constraint(constraint!(
        vars.b_e[k]
            == previous + vars.b_ch[k] * (b.efficiency_charge * DT_HOURS)
                - vars.b_dis[k] * (DT_HOURS / b.efficiency_discharge)
    )));
    if !b.grid_charging_allowed {
        // The battery may only take what the roof is producing — the setting
        // that keeps a storage system outside MiSpeL's flow bookkeeping
        // altogether, because none of its energy is ever grey.
        let (pv, _) = problem.forecasts_in(realisation, k);
        model = model.with(constraint!(vars.b_ch[k] <= pv));
    }
    model
}

/// The car: its charge, its floor, and its deadline.
fn charging<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    k: usize,
    rows: &mut Rows,
) -> M {
    let Some(e) = problem.ev else {
        return model;
    };
    let previous: Expression = if k == 0 {
        Expression::from(e.energy_now.get())
    } else {
        vars.ev_e[k - 1].into()
    };
    rows.ev.push(model.add_constraint(constraint!(
        vars.ev_e[k] == previous + vars.ev[k] * (e.efficiency * DT_HOURS)
    )));

    // A charge point is semi-continuous: off, or between the 6 A of IEC 61851
    // and its maximum. Without this the cheapest way to meet a modest target is
    // to trickle a few hundred watts across many hours — which delivers nothing
    // at all, and the model has no way to find that out.
    //
    // A charge point that can drop to one conductor has *two* such ranges, and
    // they are mutually exclusive. Modelling only the wider one makes the plan
    // pessimistic about hardware the household owns: under a limit that leaves
    // it 3 kW it refuses to charge, while the arbiter would have switched and
    // charged.
    if e.min_charge > Power::ZERO {
        model = model.with(constraint!(
            vars.ev[k] <= vars.ev_on[k] * e.max_charge.get()
        ));
        model = model.with(constraint!(
            vars.ev[k] >= vars.ev_on[k] * e.min_charge.get()
        ));
    }

    // Nothing goes into a car that is not plugged in — before it arrives as
    // well as after it leaves.
    if problem.horizon.get(k).is_some_and(|s| !e.present_in(s)) {
        model = model.with(constraint!(vars.ev[k] == 0.0));
    }
    if let Some(target) = charging_target(problem, e, k) {
        model = model.with(constraint!(vars.ev_e[k] + vars.ev_short[k] >= target));
    } else {
        model = model.with(constraint!(vars.ev_short[k] <= 0.0));
    }
    model
}

/// A compressor's minimum on- and off-times.
///
/// ```text
/// on[j] ≥ on[k] − on[k−1]      for j ∈ (k, k + L)     (once started, stay on)
/// on[j] ≤ 1 − on[k−1] + on[k]  for j ∈ (k, k + ℓ)     (once stopped, stay off)
/// ```
///
/// Both rows constrain a **transition**, so each needs the slot before it — and
/// the first slot of the horizon is the one a receding-horizon controller
/// actually executes. Written for `k ≥ 1` only, they leave it unconstrained: the
/// box starts the compressor, commits that quarter hour, re-plans against a
/// model with no memory and stops it again, every plan feasible and the day
/// short-cycling as though the minimum runtime had never been written.
///
/// So `on[−1]` is a constant here — the compressor's own
/// [`CompressorState`](hems_core::prelude::CompressorState) — and
/// `heat_pump_binary` pins the opening slots its minimum still owes. Anchoring
/// that first transition also shrinks the search: a 96-slot heating day solves
/// in about 1,0 s against 5,0 s, because a free first transition branches the
/// length of the horizon (D65).
///
/// The rows are the pairwise ones rather than Rajan and Takriti's convex hull,
/// which was built and measured at 6,9 s against 4,9 s here (D66).
///
/// `hp_on` is shared by every scenario, so these are stated once rather than
/// per future.
fn minimum_runtime<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    shared: &SharedBinaries,
) -> M {
    let Some(t) = problem.thermal.filter(|t| !t.heat_pump.modulating) else {
        return model;
    };
    let n = problem.horizon.len;
    let on = &shared.hp_on;
    // A minimum of one slot is no minimum: the windows below are empty and the
    // rows are simply not built.
    let min_on = t.heat_pump.min_on_slots.max(1);
    let min_off = t.heat_pump.min_off_slots.max(1);
    let was_on = f64::from(u8::from(t.heat_pump.compressor.running));

    for k in 0..n {
        // A slot that shares its decision with the one before it has no
        // transition to constrain: substituting `on[k − 1] = on[k]` turns the
        // minimum-on row into `on[j] ≥ 0` and the minimum-off row into
        // `on[j] ≤ 1`, both of which are already bounds. Skipping them is exact
        // and it is most of the rows once the tail is committed in blocks.
        if k > 0 && on[k - 1] == on[k] {
            continue;
        }
        let previous: Expression = if k == 0 {
            Expression::from(was_on)
        } else {
            on[k - 1].into()
        };
        // …and the same substitution empties a row whose *target* shares the
        // decision being made, which is every slot inside `k`'s own block.
        for j in (k + 1)..(k + min_on).min(n) {
            if on[j] != on[k] {
                model = model.with(constraint!(on[j] >= on[k] - previous.clone()));
            }
        }
        for j in (k + 1)..(k + min_off).min(n) {
            if on[j] != on[k] {
                model = model.with(constraint!(on[j] <= 1.0 - previous.clone() + on[k]));
            }
        }
    }
    model
}

/// Every appliance runs its programme exactly once, or says it did not.
///
/// `Σ start + short = 1`. The sum is over binaries, so `short` comes out
/// integral without being declared so, and the row is the only one an appliance
/// contributes — everything else about it is arithmetic on the starts.
fn shiftable_runs<M: SolverModel>(mut model: M, problem: &Problem<'_>, vars: &Vars<'_>) -> M {
    for i in 0..problem.shiftable.len() {
        let placed = vars.sh_start[i]
            .iter()
            .fold(Expression::from(0.0), |acc, v| acc + *v);
        model = model.with(constraint!(placed + vars.sh_short[i] == 1.0));
    }
    model
}

/// § 14a: the controllable devices, net of the surplus that covers them.
///
/// A heat pump is Fallgruppe b `[A1 2.4.1.b]`, so it counts here too. The
/// surplus is what the roof *delivers*, so curtailed production does not raise
/// the ceiling: throwing energy away cannot buy headroom.
///
/// The battery's **discharge** is on the left with a minus, because `[A1 2.3]`
/// measures what the controllable devices draw *from the grid* and energy that
/// came out of a store never crossed the connection point. Two notes on why this
/// is safe here where it needed care in the guard: the plan decides both sides
/// of it in the same solve, so there is no risk of the discharge being reversed
/// by a later decision; and charging and discharging appear with opposite signs,
/// so no round trip through the battery can manufacture headroom. It is what
/// stops a plan refusing to charge a car under a teatime reduction while a full
/// battery sits behind the meter — and it is the same arithmetic
/// `hems_realtime::Guard::lend_generation` runs against measurements a second at
/// a time.
///
/// The ceiling is read **per slot**: a ninety-minute reduction is not a
/// forty-eight-hour one, and an anticipated window is not in force yet.
fn grid_rules<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    realisation: crate::model::Realisation,
    k: usize,
    rows: &mut Rows,
) -> M {
    let Some(ceiling) = problem
        .horizon
        .get(k)
        .and_then(|s| problem.limits.steuve_at(s))
    else {
        rows.steuve.push(None);
        return model;
    };
    let (pv, load) = problem.forecasts_in(realisation, k);
    let row = if pv <= load {
        // No surplus to lift the ceiling with, so the rule is the bare one — and
        // nothing that is *not* a steuerbare Verbrauchseinrichtung belongs on
        // the left. A hot-water tank and a dishwasher count here for what they
        // *spend* of the surplus, never against the ceiling itself; with no
        // surplus to spend they do not appear at all, which makes this branch
        // exact.
        model.add_constraint(constraint!(
            vars.b_ch[k] + vars.ev[k] + vars.hp[k] - vars.b_dis[k] <= ceiling.get()
        ))
    } else {
        // With a surplus, everything that consumes it reduces the headroom it
        // buys: the curtailed production that never happened, the tank that took
        // some, and the appliance somebody set running. Writing them on the left
        // is the linear way to say
        // `Σ SteuVE − b_dis ≤ ceiling + max(0, pv − load − curtail − dhw − shiftable)`.
        //
        // # The `max` is dropped, and that is a decision rather than an oversight
        //
        // Exact wherever the surplus survives the three of them; where it does
        // not, the right-hand side falls *below* the ceiling, so the plan asks
        // less of the connection than the Festlegung allows. The safe direction —
        // the guard runs the exact formula against measurements a second at a
        // time (`hems_grid::steuve_budget`) and would refuse the difference
        // anyway.
        //
        // Restoring the `max` exactly is a disjunction and needs a binary per
        // slot: substituting the energy balance turns the constraint into
        // `min(Σ SteuVE − b_dis, g_in − g_out) ≤ ceiling`, which is a union of
        // two half-spaces and therefore not convex. A § 14a limit sent without a
        // duration covers the whole horizon, so that is up to sixty more
        // binaries on a summer day, to buy at most the tank's and the
        // appliance's own rating of headroom in the slots where a surplus is
        // small *and* a reduction is in force. Measured on the reference days it
        // is worth nothing at all, and it was not built.
        model.add_constraint(constraint!(
            vars.b_ch[k] + vars.ev[k] + vars.hp[k] - vars.b_dis[k]
                + vars.curtail[k]
                + vars.dhw[k]
                + shiftable_total(problem, vars, k)
                <= ceiling.get() + (pv - load)
        ))
    };
    rows.steuve.push(Some(row));
    model
}

/// The hot-water tank: a linear store with a leak and a deadline every morning.
///
/// The cheapest flexibility in most German houses, and the one the first four
/// versions of this planner did not have. Three hundred litres between 45 and
/// 60 °C hold about five kilowatt-hours of heat; a hot-water heat pump puts it
/// there at a coefficient of performance near three, so the whole store is worth
/// under two kilowatt-hours of electricity — small, but free to move, and it
/// moves into exactly the hours the roof is producing.
///
/// The draw is a **soft** constraint for the same reason the charging deadline
/// is: a household that starts the day with a cold tank cannot have a hot shower
/// at seven whatever the plan says, and answering "no schedule exists" is worse
/// than answering "this one, and it is two kilowatt-hours short".
fn hot_water<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    k: usize,
    rows: &mut Rows,
) -> M {
    let Some(d) = problem.dhw else {
        return model;
    };
    let previous: Expression = if k == 0 {
        Expression::from(d.stored_now.get())
    } else {
        vars.dhw_e[k - 1].into()
    };
    rows.dhw.push(model.add_constraint(constraint!(
        vars.dhw_e[k]
            == previous + vars.dhw[k] * (d.cop * DT_HOURS)
                - (d.standing_loss.get() * DT_HOURS + problem.dhw_draw_at(k))
                + vars.dhw_short[k]
    )));
    model
}

/// The building: two thermal masses, discretised exactly over the slot.
///
/// The coefficients come from [`hems_core::thermal::Rc2::discretise`], which is
/// a zero-order hold — heat input and outdoor temperature constant across the
/// slot, which is exactly what a quarter-hour plan asserts about them — so the
/// step carries **no discretisation error**. It is still linear in the heat
/// input, because the coefficient of performance is a constant computed from the
/// *forecast* outdoor temperature rather than from the decision.
///
/// The building's own inertia is usually several times the household battery,
/// and free. Getting its dynamics wrong is therefore not a rounding error; it is
/// mis-sizing the largest store in the house.
fn building<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    k: usize,
    rows: &mut Rows,
) -> M {
    let Some(t) = problem.thermal else {
        return model;
    };
    let outdoor = problem.outdoor_at(k);
    let cop = t.heat_pump.cop(outdoor);
    let d = problem.thermal_step();

    let (prev_in, prev_mass): (Expression, Expression) = if k == 0 {
        (t.state.indoor_c.into(), t.state.mass_c.into())
    } else {
        (vars.t_in[k - 1].into(), vars.t_mass[k - 1].into())
    };

    // Watts in, kilowatts of heat out: `b_heat` is kelvin per kilowatt held for
    // one step, so the factor is COP / 1000.
    rows.air.push(model.add_constraint(constraint!(
        vars.t_in[k]
            == prev_in.clone() * d.a[0][0]
                + prev_mass.clone() * d.a[0][1]
                + vars.hp[k] * (d.b_heat[0] * cop / 1000.0)
                + d.b_outdoor[0] * outdoor
    )));
    rows.mass.push(model.add_constraint(constraint!(
        vars.t_mass[k]
            == prev_in * d.a[1][0]
                + prev_mass * d.a[1][1]
                + vars.hp[k] * (d.b_heat[1] * cop / 1000.0)
                + d.b_outdoor[1] * outdoor
    )));

    // The comfort band is soft: a cold snap should make the house uncomfortable
    // and expensive, not the problem infeasible.
    model = model.with(constraint!(vars.t_in[k] + vars.cold[k] >= t.comfort_min_c));
    model = model.with(constraint!(vars.t_in[k] - vars.warm[k] <= t.comfort_max_c));

    if t.heat_pump.modulating {
        return model;
    }
    let max = t.heat_pump.max_electrical.get();
    let min = t.heat_pump.min_electrical.get();
    model = model.with(constraint!(vars.hp[k] <= vars.hp_on[k] * max));
    model = model.with(constraint!(vars.hp[k] >= vars.hp_on[k] * min));

    // The minimum runtime is **not** here. It ties slots to each other rather
    // than describing one, and `hp_on` is shared by every future, so stating it
    // per scenario per slot would add the same rows three times over. See
    // `minimum_runtime`.
    model
}

/// What the same horizon would cost without an energy manager.
///
/// The comparison has to deliver the **same service** or it is not a comparison:
/// the car still reaches its target and the house is still warm. What the
/// baseline lacks is the *decisions* — no battery, a charge point that starts the
/// moment the car is plugged in, and a heat pump on an ordinary thermostat.
///
/// A baseline that priced a household simply consuming its own load, with no car
/// and no heating at all, produces a saving figure flattering enough to be
/// useless: it credits the optimiser for energy it never had to buy.
///
/// # It faces the same law, and pays for the same failures
///
/// The baseline lives under the **same grid rules**. A household with no energy
/// manager cannot be addressed as one `[A1 4.4.b]`, so its Steuerbox turns each
/// device down on its own `[A1 4.4.a]` — that is
/// [`PlanningLimits::direct_control_ceiling`] — and its roof is capped by § 9 EEG
/// exactly like the managed one, curtailing what it cannot export. Measuring a
/// saving against a household that ignored the network operator prices a
/// counterfactual nobody is allowed to buy.
///
/// And it pays for **service it fails to deliver**, on the same terms the plan
/// does. A car plugged in an hour before it leaves is short whatever anybody
/// does; a tank that starts the day cold cannot fill a bath at seven. Charging
/// the plan for that and not the baseline would be the same asymmetry pointing
/// the other way.
///
/// [`PlanningLimits::direct_control_ceiling`]: crate::model::PlanningLimits::direct_control_ceiling
fn baseline_cost(problem: &Problem<'_>) -> hems_core::prelude::CostBreakdown {
    let mut cost = hems_core::prelude::CostBreakdown::default();
    let mut house = Unmanaged::new(problem);

    for k in 0..problem.horizon.len {
        let (pv, load) = problem.forecasts_at(k);
        let slot_k = problem.horizon.get(k);

        // The ceiling one device faces while a reduction is in force. The plan
        // is given the sum for everything behind it; a household with no energy
        // manager is not, so each of its devices is turned down on its own.
        let device_ceiling = slot_k
            .and_then(|s| problem.limits.steuve_at(s))
            .and(problem.limits.direct_control_ceiling)
            .map_or(f64::INFINITY, Power::get);

        let ev = house.charge_point(problem, k, device_ceiling, &mut cost);
        let hp = house.heat_pump(problem, k, device_ceiling);
        let dhw = house.hot_water(problem, k, &mut cost);
        let appliances: f64 = house.appliances(problem, k);

        // § 9 EEG and an LPP session bound what leaves the connection point, and
        // they do not ask whether there is an energy manager behind it. What the
        // baseline cannot export it throws away, at the same price the plan pays
        // for doing so.
        let mut net = load + ev + hp + dhw + appliances - pv;
        if net < 0.0
            && let Some(ceiling) = slot_k.and_then(|s| problem.limits.feed_in_at(s))
        {
            let curtailed = (-net - ceiling.get()).max(0.0);
            net += curtailed;
            cost.curtailment_eur +=
                curtailed * problem.curtailment_penalty_eur_per_kwh * SLOT_KWH_PER_W;
        }

        let price = problem.prices.slots.get(k);
        cost.energy_eur += if net >= 0.0 {
            net * price.map_or(
                DEFAULT_IMPORT_EUR_PER_KWH,
                hems_tariff::SlotPrice::import_f64,
            )
        } else {
            net * price.map_or(
                DEFAULT_EXPORT_EUR_PER_KWH,
                hems_tariff::SlotPrice::export_f64,
            )
        } * SLOT_KWH_PER_W;
        // The baseline is in the **same community**. A household joins one and
        // then does nothing about it — the Aufteilungsschlüssel still allocates
        // it whatever its unmanaged draw happens to overlap. Leaving that out
        // would credit the plan with the membership rather than with the
        // shifting, which is the same asymmetry as measuring a saving against a
        // household that ignored the network operator.
        if net > 0.0 {
            cost.sharing_eur -= net.min(problem.community_share_at(k))
                * price.map_or(0.0, hems_tariff::SlotPrice::sharing_discount_f64)
                * SLOT_KWH_PER_W;
        }

        // A thermostat is not free of discomfort either: it reheats only after
        // the house has already fallen through the band, and in a cold snap it
        // never catches up. Leaving that out of the baseline would credit the
        // planner with comfort it did not actually have to buy.
        if let Some(t) = problem.thermal {
            let outside = (t.comfort_min_c - house.thermal_state.indoor_c)
                .max(house.thermal_state.indoor_c - t.comfort_max_c)
                .max(0.0);
            cost.discomfort_eur += outside * t.discomfort_eur_per_kelvin_hour * DT_HOURS;
        }
    }
    // A departure outside the horizon is not a missed deadline, so the baseline
    // is held to exactly what the plan is held to out there: the pro-rata floor
    // of `charging_target`. Charging it the whole outstanding balance instead
    // would invent a failure on a car that has not left yet — the same
    // asymmetry, pointing the other way.
    cost.unserved_eur += problem
        .shiftable
        .iter()
        .zip(&house.appliance_starts)
        .filter(|(_, start)| start.is_none())
        .map(|(run, _)| run.unserved_eur.max(0.0))
        .sum::<f64>();
    if let Some(e) = problem.ev
        && problem.horizon.index_of(e.deadline()).is_none()
    {
        let owed = (0..problem.horizon.len)
            .filter_map(|k| charging_target(problem, e, k))
            .fold(0.0_f64, f64::max);
        let in_car = e.energy_target.get() - house.car_remaining;
        cost.unserved_eur += (owed - in_car).max(0.0) * (problem.unmet_charge_eur_per_kwh / 1000.0);
    }
    cost
}

/// The state an unmanaged household carries from slot to slot.
///
/// Split out of [`baseline_cost`] so each appliance's "what would it do with no
/// energy manager" rule reads on its own — they are three independent claims and
/// each is a place the comparison can be made unfair.
struct Unmanaged {
    car_remaining: f64,
    thermostat_on: bool,
    dhw_stored: f64,
    thermal_state: hems_core::prelude::ThermalState,
    thermal_step: hems_core::prelude::Rc2Discrete,
    /// Where each programme runs with nobody deciding: the first slot the
    /// household is allowed to press start in.
    appliance_starts: Vec<Option<usize>>,
}

impl Unmanaged {
    fn new(problem: &Problem<'_>) -> Self {
        Self {
            car_remaining: problem.ev.map_or(0.0, |e| {
                (e.energy_target.get() - e.energy_now.get()).max(0.0)
            }),
            thermostat_on: false,
            dhw_stored: problem.dhw.map_or(0.0, |d| d.stored_now.get()),
            thermal_state: problem
                .thermal
                .map_or(hems_core::prelude::ThermalState::default(), |t| t.state),
            thermal_step: problem.thermal_step(),
            appliance_starts: problem
                .shiftable
                .iter()
                .map(|run| (0..problem.horizon.len).find(|k| run.can_start_at(problem.horizon, *k)))
                .collect(),
        }
    }

    /// What the shiftable appliances draw with nobody scheduling them.
    ///
    /// A household with no energy manager presses start when it loads the
    /// machine, which is the first slot its own window allows — and pays the
    /// same price as the plan for a programme that has nowhere to go at all. The
    /// whole difference being measured is *when*, so the baseline must run the
    /// same programme rather than not run one.
    fn appliances(&self, problem: &Problem<'_>, k: usize) -> f64 {
        problem
            .shiftable
            .iter()
            .zip(&self.appliance_starts)
            .map(|(run, start)| {
                start
                    .filter(|s| k >= *s)
                    .map_or(0.0, |s| run.programme.power_at(k - s).get())
            })
            .sum()
    }

    /// An unmanaged charge point runs flat out from the moment the cable goes in
    /// until the car is full, and pays for whatever it could not deliver by the
    /// departure.
    fn charge_point(
        &mut self,
        problem: &Problem<'_>,
        k: usize,
        device_ceiling: f64,
        cost: &mut hems_core::prelude::CostBreakdown,
    ) -> f64 {
        let slot_k = problem.horizon.get(k);
        let ev = match problem.ev {
            Some(e) if self.car_remaining > 0.0 && slot_k.is_some_and(|s| e.present_in(s)) => {
                let p = e.max_charge.get().min(device_ceiling);
                self.car_remaining = (self.car_remaining - p * e.efficiency * DT_HOURS).max(0.0);
                p
            }
            _ => 0.0,
        };
        if let Some(e) = problem.ev
            && slot_k == Some(e.deadline())
        {
            cost.unserved_eur += self.car_remaining * (problem.unmet_charge_eur_per_kwh / 1000.0);
            self.car_remaining = 0.0;
        }
        ev
    }

    /// An ordinary thermostat with half a kelvin of hysteresis. It knows nothing
    /// about the price or the weather, which is the whole of the difference
    /// being measured.
    fn heat_pump(&mut self, problem: &Problem<'_>, k: usize, device_ceiling: f64) -> f64 {
        let Some(t) = problem.thermal else {
            return 0.0;
        };
        if self.thermal_state.indoor_c < t.comfort_min_c {
            self.thermostat_on = true;
        } else if self.thermal_state.indoor_c > t.comfort_min_c + 0.5 {
            self.thermostat_on = false;
        }
        let p = if self.thermostat_on {
            t.heat_pump.max_electrical.get().min(device_ceiling)
        } else {
            0.0
        };
        let outdoor = problem.outdoor_at(k);
        self.thermal_state = self.thermal_step.step(
            self.thermal_state,
            p * t.heat_pump.cop(outdoor) / 1000.0,
            outdoor,
        );
        p
    }

    /// An unmanaged tank reheats the moment it drops below its set point and
    /// stops when it is full — and is not immune to a cold shower either: it
    /// starts each morning where the evening left it.
    fn hot_water(
        &mut self,
        problem: &Problem<'_>,
        k: usize,
        cost: &mut hems_core::prelude::CostBreakdown,
    ) -> f64 {
        let Some(d) = problem.dhw else {
            return 0.0;
        };
        let draw = problem.dhw_draw_at(k) + d.standing_loss.get() * DT_HOURS;
        cost.unserved_eur += (draw - self.dhw_stored).max(0.0) * (d.shortfall_eur_per_kwh / 1000.0);
        self.dhw_stored = (self.dhw_stored - draw).max(0.0);
        let missing = d.capacity.get() * DHW_THERMOSTAT_SET - self.dhw_stored;
        let p = if missing > 0.0 {
            d.heater
                .get()
                .min(missing / (d.cop * DT_HOURS).max(f64::EPSILON))
        } else {
            0.0
        };
        self.dhw_stored = (self.dhw_stored + p * d.cop * DT_HOURS).min(d.capacity.get());
        p
    }
}

/// How much has to be in the car by the end of slot `k`, if anything.
///
/// Three cases, and the middle one is the one an implementation forgets:
///
/// * the deadline — the slot before the departure — is **inside** the horizon:
///   the full target applies in that slot, and nothing before it;
/// * the departure is **beyond** the horizon — no deadline falls inside it, so
///   an unconstrained model would simply not charge, wait for the next re-plan,
///   and repeat that until the deadline finally came into view and the car could
///   no longer be filled in time. A pro-rata floor at the end of the horizon
///   keeps the plan moving at the constant rate that would just make it;
/// * the deadline is **behind** the horizon — the car should already be full,
///   so the target applies from the first slot and the shortfall says how far
///   off it is.
fn charging_target(problem: &Problem<'_>, ev: EvSession, k: usize) -> Option<f64> {
    let last = problem.horizon.len.saturating_sub(1);
    let first = problem.horizon.first;
    // The last slot the car can charge in is the one *before* it leaves.
    let deadline = ev.deadline();
    if problem.horizon.index_of(deadline).is_some() {
        return (problem.horizon.get(k) == Some(deadline)).then(|| ev.energy_target.get());
    }
    if deadline < first {
        return (k == 0).then(|| ev.energy_target.get());
    }
    if k != last {
        return None;
    }
    // Measured from the moment the car is actually there: a plan whose horizon
    // ends before a car that arrives at teatime has arrived owes it nothing yet.
    let plugged_from = ev.arrival.unwrap_or(first).max(first);
    let end = problem.horizon.get(last)?;
    if end < plugged_from {
        return None;
    }
    let available = plugged_from.distance_to(ev.departure).max(1) as f64;
    let inside = (plugged_from.distance_to(end) + 1).max(0) as f64;
    let fraction = (inside / available).clamp(0.0, 1.0);
    let needed = (ev.energy_target.get() - ev.energy_now.get()).max(0.0);
    Some(ev.energy_now.get() + needed * fraction)
}

/// What each appliance draws in slot `k`, given where its programme was placed.
fn shiftable_at(
    problem: &Problem<'_>,
    placements: &[Option<usize>],
    k: usize,
) -> Vec<hems_core::prelude::Power> {
    problem
        .shiftable
        .iter()
        .zip(placements)
        .map(|(run, start)| {
            start
                .filter(|s| k >= *s)
                .map_or(Power::ZERO, |s| run.programme.power_at(k - s))
        })
        .collect()
}

/// The per-asset targets a solved slot implies.
///
/// Each carries an envelope as well as a value, and the two say different
/// things. The **value** is the average power the plan intends, which through
/// [`AssetTarget::energy`] is the quarter hour's energy commitment. The
/// **envelope** is the freedom the plan gives away: the direction it committed
/// to, out to what the hardware can do. The arbiter spends that freedom
/// delivering the energy despite whatever the slot turns out to hold, which is
/// the difference between following a plan and replaying it.
fn slot_targets(
    problem: &Problem<'_>,
    names: &AssetNames,
    f: &Flows,
    k: usize,
    shadow: Option<&crate::shadow::Shadow>,
    appliances: &[Power],
) -> Vec<AssetTarget> {
    let mut targets = Vec::new();
    let value_of = |asset: &AssetId| shadow.map(|s| s.for_asset(names, asset));
    if let Some(id) = &names.battery {
        let net = f.battery_charge - f.battery_discharge;
        let (max_charge, max_discharge) = problem.battery.map_or((Power::ZERO, Power::ZERO), |b| {
            (b.max_charge, b.max_discharge)
        });
        targets.push(AssetTarget {
            marginal_eur_per_kwh: value_of(id),
            asset: id.clone(),
            power: net,
            // The plan commits to a *direction* and an *amount of energy*; the
            // envelope is the direction, and it runs all the way to what the
            // hardware can do so that the arbiter can still deliver the energy
            // after a cloud, a guard reduction or a late start has cost it part
            // of the slot. An envelope pinned at the planned power would let the
            // arbiter fall behind and never catch up.
            envelope: if net >= Power::ZERO {
                Envelope::new(Power::ZERO, max_charge)
            } else {
                Envelope::new(-max_discharge, Power::ZERO)
            },
        });
    }
    if let Some(id) = &names.evse {
        let max = problem.ev.map_or(f.ev_charge, |e| e.max_charge);
        targets.push(AssetTarget {
            marginal_eur_per_kwh: value_of(id),
            asset: id.clone(),
            power: f.ev_charge,
            envelope: Envelope::new(Power::ZERO, max),
        });
    }
    if let Some(id) = &names.heat_pump
        && let Some(t) = problem.thermal
    {
        targets.push(AssetTarget {
            marginal_eur_per_kwh: value_of(id),
            asset: id.clone(),
            power: f.heat_pump,
            envelope: Envelope::new(Power::ZERO, t.heat_pump.max_electrical),
        });
    }
    if let Some(id) = &names.dhw
        && let Some(d) = problem.dhw
    {
        targets.push(AssetTarget {
            marginal_eur_per_kwh: value_of(id),
            asset: id.clone(),
            power: f.dhw,
            envelope: Envelope::new(Power::ZERO, d.heater),
        });
    }
    // A programme is *placed*, not modulated: once a dishwasher starts it is
    // household load, and the envelope says so. What the target carries is the
    // one thing a driver, a household and a user interface need — the quarter
    // hour it is running in.
    for (id, power) in names.shiftable.iter().zip(appliances) {
        targets.push(AssetTarget {
            marginal_eur_per_kwh: value_of(id),
            asset: id.clone(),
            power: *power,
            envelope: Envelope::exactly(*power),
        });
    }
    if let Some(id) = &names.pv
        && f.curtailed > Power::ZERO
    {
        let (pv, _) = problem.forecasts_at(k);
        let allowed = Power::new(pv) - f.curtailed;
        targets.push(AssetTarget {
            marginal_eur_per_kwh: value_of(id),
            asset: id.clone(),
            power: -allowed,
            envelope: Envelope::new(-Power::new(pv), Power::ZERO),
        });
    }
    targets
}

/// One slot's flows, read back from the solution.
///
/// A solver returns a basis, not a decision. Two roundings happen here, and the
/// second one is not cosmetic.
///
/// A variable that is logically zero comes back as 1e-13, and it travels from
/// here into plan targets, JSON and tests that compare against zero. No device
/// in a household resolves a milliwatt, so neither does this.
///
/// And a variable that landed exactly **on** a bound comes back a nanowatt short
/// of it: a semi-continuous charge point pinned to its own 4 140 W minimum
/// returns 4 139,999 999 999 896. Downstream that is not a rounding error, it is
/// a different decision — the arbiter's conductor policy reads it as "below the
/// three-phase minimum", drops the wallbox to one conductor, and the car charges
/// at 3,68 kW and 85 % instead of 4,14 kW and 92 % for the rest of the session.
/// Snapping to the milliwatt costs nothing and removes a whole class of that.
fn read_flows(solution: &impl Solution, vars: &Vars<'_>, appliances: &[Power], k: usize) -> Flows {
    let value = |v: Variable| {
        let w = solution.value(v).max(0.0);
        Power::new(if w < 1e-3 {
            0.0
        } else {
            (w * 1e3).round() / 1e3
        })
    };
    Flows {
        grid_import: value(vars.g_in[k]),
        grid_export: value(vars.g_out[k]),
        battery_charge: value(vars.b_ch[k]),
        battery_discharge: value(vars.b_dis[k]),
        battery_energy: hems_core::prelude::Energy::new(solution.value(vars.b_e[k]).max(0.0)),
        ev_charge: value(vars.ev[k]),
        curtailed: value(vars.curtail[k]),
        shared_import: vars.shared.get(k).map_or(Power::ZERO, |v| value(*v)),
        heat_pump: value(vars.hp[k]),
        dhw: value(vars.dhw[k]),
        shiftable: appliances.iter().copied().sum(),
        dhw_stored: hems_core::prelude::Energy::new(solution.value(vars.dhw_e[k]).max(0.0)),
        indoor_c: solution.value(vars.t_in[k]),
        discomfort_k: solution.value(vars.cold[k]) + solution.value(vars.warm[k]),
    }
}

/// Add one slot's costs to the report.
///
/// The same terms the objective minimises, so the reported saving cannot flatter
/// itself by leaving out what the plan actually spent. Only the *preference*
/// weights of the objective are dropped: a carbon price and an autarky premium
/// are what a household is willing to pay, not what it is charged, and adding
/// them to a reported bill would invent an invoice nobody sends.
fn charge_the_report(
    problem: &Problem<'_>,
    f: &Flows,
    k: usize,
    cost: &mut hems_core::prelude::CostBreakdown,
) {
    let price = problem.prices.slots.get(k);
    let import_eur = price.map_or(
        DEFAULT_IMPORT_EUR_PER_KWH,
        hems_tariff::SlotPrice::import_f64,
    );
    let export_eur = price.map_or(
        DEFAULT_EXPORT_EUR_PER_KWH,
        hems_tariff::SlotPrice::export_f64,
    );
    cost.energy_eur +=
        (f.grid_import.get() * import_eur - f.grid_export.get() * export_eur) * SLOT_KWH_PER_W;
    // …less what the community's own generation paid for. Every term of the
    // objective is a term of the report, and this one is a term the household
    // is genuinely invoiced differently for — on a line of its own, because
    // "what did belonging to the community buy" is a question the energy line
    // cannot answer.
    cost.sharing_eur -= f.shared_import.get()
        * price.map_or(0.0, hems_tariff::SlotPrice::sharing_discount_f64)
        * SLOT_KWH_PER_W;
    if let Some(b) = problem.battery {
        cost.wear_eur += (f.battery_charge.get() + f.battery_discharge.get())
            * (b.degradation_eur_per_kwh / 2.0)
            * SLOT_KWH_PER_W;
    }
    cost.curtailment_eur +=
        f.curtailed.get() * problem.curtailment_penalty_eur_per_kwh * SLOT_KWH_PER_W;
    if let Some(t) = problem.thermal {
        cost.discomfort_eur += f.discomfort_k * t.discomfort_eur_per_kelvin_hour * DT_HOURS;
    }
}

/// The marginal value of a kilowatt-hour where the dual pass did not run.
///
/// The price the plan faces in this slot — what the first four versions used,
/// and enough to rank slots even though it cannot rank devices.
fn fallback_marginal(problem: &Problem<'_>, f: &Flows, k: usize) -> f64 {
    let price = problem.prices.slots.get(k);
    if f.grid_import > f.grid_export {
        price.map_or(
            DEFAULT_IMPORT_EUR_PER_KWH,
            hems_tariff::SlotPrice::import_f64,
        )
    } else {
        price.map_or(
            DEFAULT_EXPORT_EUR_PER_KWH,
            hems_tariff::SlotPrice::export_f64,
        )
    }
}

/// Turn a solved model into a plan.
fn read_back(
    problem: &Problem<'_>,
    names: &AssetNames,
    now: OffsetDateTime,
    solution: &impl Solution,
    per: &[Variables],
    central: usize,
    shadows: &[crate::shadow::Shadow],
) -> Solved {
    let n = problem.horizon.len;
    let futures = problem.realisations();
    let vars = &per[central].borrow();
    let central_realisation = futures[central];
    // Where each appliance's programme was placed, as a slot index. Read once:
    // it is a single decision per appliance and re-deriving it inside the slot
    // loop would make a quadratic scan out of a lookup.
    let placements: Vec<Option<usize>> = (0..problem.shiftable.len())
        .map(|i| (0..n).find(|k| solution.value(vars.sh_start[i][*k]) > 0.5))
        .collect();

    // ── Read the answer back ────────────────────────────────────────────────
    let mut flows = Vec::with_capacity(n);
    let mut slots = Vec::with_capacity(n);
    let mut cost = hems_core::prelude::CostBreakdown::default();

    for k in 0..n {
        let appliances = shiftable_at(problem, &placements, k);
        let f = read_flows(solution, vars, &appliances, k);
        charge_the_report(problem, &f, k, &mut cost);
        let _ = central_realisation;

        // The marginal value of a kilowatt-hour in this slot. Where the dual
        // pass ran it is the **shadow price** of the energy balance — what the
        // next kilowatt-hour would actually cost this plan, which above a
        // binding limit is not the tariff at all. Where it did not, it falls
        // back to the price the plan faces, which is what the first four
        // versions used and is enough to rank slots.
        let shadow = shadows.get(k).copied();
        let site_marginal = shadow.map_or_else(|| fallback_marginal(problem, &f, k), |s| s.site);

        slots.push(SlotPlan {
            slot: problem.horizon.get(k).expect("k < horizon length"),
            targets: slot_targets(problem, names, &f, k, shadow.as_ref(), &appliances),
            marginal_eur_per_kwh: Some(site_marginal),
            flexibility_eur_per_kwh: shadow.and_then(|s| s.flexibility),
        });
        flows.push(f);
    }

    let baseline = baseline_cost(problem);

    // ── The soft terms, in **expectation** ───────────────────────────────────
    //
    // Each future has its own shortfall, and the objective priced each at that
    // future's probability. Reporting the central one would tell a household the
    // median day's answer and leave the dull one out of the number the plan was
    // actually built around; reporting the worst would say the plan expects a bad
    // day. The expectation is what the objective minimised, so it is what the
    // report carries.
    let expectation = |pick: &dyn Fn(&Vars<'_>, usize) -> f64| -> f64 {
        futures
            .iter()
            .enumerate()
            .map(|(s, realisation)| {
                let v = per[s].borrow();
                realisation.probability * (0..n).map(|k| pick(&v, k)).sum::<f64>()
            })
            .sum()
    };
    // The charging shortfall is a *level*, not a flow: at most one slot carries a
    // target, so summing over the horizon and taking the largest come to the same
    // thing, and summing composes with the expectation above.
    let unmet_charge = hems_core::prelude::Energy::new(
        expectation(&|v, k| solution.value(v.ev_short[k]).max(0.0)).max(0.0),
    );
    let unmet_hot_water = hems_core::prelude::Energy::new(
        expectation(&|v, k| solution.value(v.dhw_short[k]).max(0.0)).max(0.0),
    );

    // The two soft terms of the objective, reported. Uncharged, they let the
    // saving treat a service the household did not get as one it did not have to
    // pay for.
    cost.unserved_eur = unmet_charge.get() * (problem.unmet_charge_eur_per_kwh / 1000.0)
        + unmet_hot_water.get()
            * problem
                .dhw
                .map_or(0.0, |d| d.shortfall_eur_per_kwh / 1000.0)
        // …and a programme the plan decided not to run. Same argument as the
        // other two: a wash nobody got is a wash nobody paid for, and leaving it
        // off the report would let the cheapest plan be the one where the
        // machine stays full.
        + problem
            .shiftable
            .iter()
            .zip(&placements)
            .filter(|(_, placed)| placed.is_none())
            .map(|(run, _)| run.unserved_eur.max(0.0))
            .sum::<f64>();

    Solved {
        plan: Plan {
            id: PlanId::new(),
            created_at: now,
            horizon: problem.horizon,
            slots,
            expected_cost: Some(cost),
            baseline_cost: Some(baseline),
        },
        flows,
        unmet_charge,
        unmet_hot_water,
        // What the next re-plan starts from. Rounded, because a solver returns
        // 0,999 999 999 for a binary and a hint is only useful if it is the
        // decision rather than a number near it.
        commitment: crate::model::Commitment {
            ev_on: (0..n)
                .map(|k| solution.value(vars.ev_on[k]).round())
                .collect(),
            hp_on: (0..n)
                .map(|k| solution.value(vars.hp_on[k]).round())
                .collect(),
            shiftable: vars
                .sh_start
                .iter()
                .map(|starts| starts.iter().map(|v| solution.value(*v).round()).collect())
                .collect(),
        },
    }
}
