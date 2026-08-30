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
//!
//! subject to, in every slot,
//!
//! ```text
//! g_in − g_out = load + b_ch − b_dis + ev − (pv − curtail)      (energy balance)
//! g_out ≤ pv − curtail + b_dis                                  (no invented export)
//! e[k] = e[k−1] + Δ(η_ch·b_ch − b_dis/η_dis)                    (battery state)
//! ev_e[k] = ev_e[k−1] + Δ·η·ev                                  (car state)
//! b_ch + ev ≤ ceiling + max(0, pv − load)                       (§ 14a)
//! g_out ≤ feed-in ceiling                                       (§ 9 EEG, LPP)
//! curtail ≤ pv
//! ```
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
    /// Electrical power drawn by the heat pump.
    pub heat_pump: Power,
    /// Electrical power drawn by the hot-water heater.
    pub dhw: Power,
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
    /// heat, summed over the horizon.
    ///
    /// The same shape as [`Solved::unmet_charge`] and for the same reason: a
    /// cold shower is expensive, not impossible, and a plan that says how short
    /// it fell is worth more than no plan at all.
    pub unmet_hot_water: hems_core::prelude::Energy,
}

/// Solve `problem` and turn the result into a [`Plan`].
///
/// `now` stamps the plan so the arbiter can tell how old it is.
///
/// # Errors
/// [`SolveError`] when the horizon is empty, the constraints conflict, or the
/// solver fails.
pub fn solve(
    problem: &Problem<'_>,
    names: &AssetNames,
    now: OffsetDateTime,
) -> Result<Solved, SolveError> {
    let n = problem.horizon.len;
    if n == 0 {
        return Err(SolveError::EmptyHorizon);
    }

    // The solver takes the problem by value, so the variable *handles* are kept
    // separately: they stay valid across the hand-over and the read-back.
    let (problem_vars, declared) = build_variables(problem, None);
    let v = declared.borrow();
    let objective = build_objective(problem, &v);

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

    let mut rows = Rows::default();
    model = add_constraints(model, problem, &v, &mut rows);

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
        shadow_prices(problem, &solution, &v)
    } else {
        Vec::new()
    };

    Ok(read_back(problem, names, now, &solution, &v, &shadows))
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
    };

    let (problem_vars, declared) = build_variables(problem, Some(&pins));
    let v = declared.borrow();
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
    let objective = build_objective(problem, &v) * DUAL_OBJECTIVE_SCALE;
    let mut rows = Rows::default();
    let mut lp_model = problem_vars.minimise(objective).using(good_lp::clarabel);
    lp_model.settings().verbose(false);
    let model = add_constraints(lp_model, problem, &v, &mut rows);
    let Ok(mut lp) = model.solve() else {
        return Vec::new();
    };
    let duals = lp.compute_dual();

    let at = |refs: &[good_lp::constraint::ConstraintReference], k: usize| {
        refs.get(k)
            .map(|r| duals.dual(r.clone()) / DUAL_OBJECTIVE_SCALE)
    };
    (0..n)
        .map(|k| {
            crate::shadow::RawDuals {
                balance: at(&rows.balance, k).unwrap_or(0.0),
                battery: at(&rows.battery, k),
                ev: at(&rows.ev, k),
                dhw: at(&rows.dhw, k),
                air: at(&rows.air, k),
                mass: at(&rows.mass, k),
                steuve: rows
                    .steuve
                    .get(k)
                    .and_then(Option::as_ref)
                    .map(|r| duals.dual(r.clone()) / DUAL_OBJECTIVE_SCALE),
            }
            .into_shadow(problem)
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
}

/// Declare the variables and their bounds.
///
/// `pins`, when given, replaces every binary with a constant — which is what
/// turns the mixed-integer model into the linear program whose duals are the
/// shadow prices of [`crate::shadow`].
fn build_variables(problem: &Problem<'_>, pins: Option<&Pins>) -> (ProblemVariables, Variables) {
    let n = problem.horizon.len;
    let mut vars = ProblemVariables::new();
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
    };

    let import_ceiling = problem
        .limits
        .import_ceiling
        .map_or(f64::INFINITY, Power::get);

    for k in 0..n {
        let (pv, _load) = problem.forecasts_at(k);
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

        charge_point_variables(problem, &mut vars, &mut v, k, pins);

        let (dhw_max, dhw_capacity) = problem
            .dhw
            .map_or((0.0, 0.0), |d| (d.heater.get(), d.capacity.get()));
        v.dhw.push(vars.add(variable().min(0.0).max(dhw_max)));
        v.dhw_e
            .push(vars.add(variable().min(0.0).max(dhw_capacity)));
        v.dhw_short
            .push(vars.add(variable().min(0.0).max(problem.dhw_draw_at(k))));

        match problem.thermal {
            Some(t) => {
                v.hp.push(vars.add(variable().min(0.0).max(t.heat_pump.max_electrical.get())));
                // A binary only where the unit really is on/off. For a
                // modulating unit it is pinned to one and never branches, so the
                // problem stays a linear program — which is what keeps a
                // 192-slot horizon solvable on a gateway box.
                v.hp_on.push(match (t.heat_pump.modulating, pins) {
                    (true, _) => vars.add(variable().min(1.0).max(1.0)),
                    (false, Some(p)) => {
                        let on = p.hp_on.get(k).copied().unwrap_or(0.0);
                        vars.add(variable().min(on).max(on))
                    }
                    (false, None) => vars.add(variable().binary()),
                });
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
                    &mut v.hp_on,
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

    (vars, v)
}

/// The charge point's variables for one slot.
///
/// Split out because it is where most of the model's integrality lives, and
/// because the rule that keeps the model fast is easiest to see in one place: a
/// binary is declared **only where it can decide something**. There is a car, it
/// has a floor to respect, and the slot is one it could still be charging in.
/// Every other slot is pinned, so branch and bound never opens a node for a car
/// that has already left — which on a 192-slot horizon is most of them, and the
/// difference between a plan in a second and a plan in a minute.
fn charge_point_variables(
    problem: &Problem<'_>,
    vars: &mut ProblemVariables,
    v: &mut Variables,
    k: usize,
    pins: Option<&Pins>,
) {
    let (ev_max, ev_capacity) = problem
        .ev
        .map_or((0.0, 0.0), |e| (e.max_charge.get(), e.capacity.get()));
    // Absent, because it has not arrived yet or has already left. Either way
    // the charge point can do nothing in this slot, so it gets no binary: on a
    // 192-slot horizon that is most of them, and it is the difference between a
    // plan in a second and a plan in a minute.
    let absent = problem
        .ev
        .zip(problem.horizon.get(k))
        .is_some_and(|(e, slot)| !e.present_in(slot));

    v.ev.push(vars.add(variable().min(0.0).max(ev_max)));
    v.ev_on.push(match (problem.ev, pins) {
        (Some(e), Some(p)) if e.min_charge > Power::ZERO && !absent => {
            let on = p.ev_on.get(k).copied().unwrap_or(0.0);
            vars.add(variable().min(on).max(on))
        }
        (Some(e), None) if e.min_charge > Power::ZERO && !absent => vars.add(variable().binary()),
        (Some(_), _) if absent => vars.add(variable().min(0.0).max(0.0)),
        _ => vars.add(variable().min(1.0).max(1.0)),
    });
    v.ev_e.push(vars.add(variable().min(0.0).max(ev_capacity)));
    // How much of the promise the plan had to give up. Priced high enough to be
    // lexicographic in practice, but *finite* — a deadline that cannot be met
    // should produce the best achievable schedule, not no schedule at all.
    v.ev_short
        .push(vars.add(variable().min(0.0).max(ev_capacity)));
}

/// What the plan is trying to minimise, in euros over the horizon.
fn build_objective(problem: &Problem<'_>, vars: &Vars<'_>) -> Expression {
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
    vars: &Vars<'_>,
    rows: &mut Rows,
) -> M {
    for k in 0..problem.horizon.len {
        model = balance(model, problem, vars, k, rows);
        model = storage(model, problem, vars, k, rows);
        model = charging(model, problem, vars, k, rows);
        model = grid_rules(model, problem, vars, k, rows);
        model = building(model, problem, vars, k, rows);
        model = hot_water(model, problem, vars, k, rows);
    }
    model
}

/// What goes in equals what comes out, and neither direction can be invented.
fn balance<M: SolverModel>(
    mut model: M,
    problem: &Problem<'_>,
    vars: &Vars<'_>,
    k: usize,
    rows: &mut Rows,
) -> M {
    let (pv, load) = problem.forecasts_at(k);

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
            - vars.curtail[k]
            == load - pv
    )));

    // Power cannot be imported and exported at the same instant. Stating it as a
    // physical bound rather than a binary keeps the model a pure linear program —
    // which matters on a gateway box — and says something true: exported power
    // has to come from the roof or the battery.
    //
    // Without it the model is *unbounded* wherever export earns more than import
    // costs, and it will happily invent an infinite round trip through a meter.
    //
    // # There used to be a second one, and removing it is what made the duals
    // # mean anything
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
        let (pv, _) = problem.forecasts_at(k);
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
    // A charge point is semi-continuous: off, or between the 6 A of IEC 61851
    // and its maximum. Without this the cheapest way to meet a modest target is
    // to trickle a few hundred watts across many hours — which delivers nothing
    // at all, and the model has no way to find that out.
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
    let (pv, load) = problem.forecasts_at(k);
    let row = if pv <= load {
        // No surplus to lift the ceiling with, so the rule is the bare one — and
        // nothing that is *not* a steuerbare Verbrauchseinrichtung belongs on
        // the left. A hot-water tank counts here for what it *spends* of the
        // surplus, never against the ceiling itself.
        model.add_constraint(constraint!(
            vars.b_ch[k] + vars.ev[k] + vars.hp[k] - vars.b_dis[k] <= ceiling.get()
        ))
    } else {
        // With a surplus, everything that consumes it reduces the headroom it
        // buys: the curtailed production that never happened, and the tank that
        // took some. Writing them on the left is the linear way to say
        // `Σ SteuVE ≤ ceiling + max(0, pv − load − curtail − dhw)` — exact
        // wherever the surplus survives them, and conservative by at most the
        // tank's own rating where it does not, which is the safe direction for a
        // plan the guard has to be able to carry out.
        model.add_constraint(constraint!(
            vars.b_ch[k] + vars.ev[k] + vars.hp[k] - vars.b_dis[k] + vars.curtail[k] + vars.dhw[k]
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

    // Minimum runtime. A compressor is damaged by short cycling far faster than
    // by running, so a plan that switches every slot is cheaper on paper and
    // more expensive in fact.
    if k >= 1 {
        let n = problem.horizon.len;
        for j in (k + 1)..(k + t.heat_pump.min_on_slots).min(n) {
            model = model.with(constraint!(
                vars.hp_on[j] >= vars.hp_on[k] - vars.hp_on[k - 1]
            ));
        }
        for j in (k + 1)..(k + t.heat_pump.min_off_slots).min(n) {
            model = model.with(constraint!(
                vars.hp_on[j] <= 1.0 - vars.hp_on[k - 1] + vars.hp_on[k]
            ));
        }
    }
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
fn baseline_cost(problem: &Problem<'_>) -> hems_core::prelude::CostBreakdown {
    let mut cost = hems_core::prelude::CostBreakdown::default();
    let mut car_remaining = problem.ev.map_or(0.0, |e| {
        (e.energy_target.get() - e.energy_now.get()).max(0.0)
    });
    let mut thermostat_on = false;
    let mut dhw_stored = problem.dhw.map_or(0.0, |d| d.stored_now.get());
    let mut thermal_state = problem
        .thermal
        .map_or(hems_core::prelude::ThermalState::default(), |t| t.state);
    let thermal_step = problem.thermal_step();

    for k in 0..problem.horizon.len {
        let (pv, load) = problem.forecasts_at(k);

        // An unmanaged charge point runs flat out from the moment the cable
        // goes in until the car is full.
        let slot_k = problem.horizon.get(k);
        let ev = match problem.ev {
            Some(e) if car_remaining > 0.0 && slot_k.is_some_and(|s| e.present_in(s)) => {
                let p = e.max_charge.get();
                car_remaining = (car_remaining - p * e.efficiency * DT_HOURS).max(0.0);
                p
            }
            _ => 0.0,
        };

        // An ordinary thermostat with half a kelvin of hysteresis. It knows
        // nothing about the price or the weather, which is the whole of the
        // difference being measured.
        let hp = match problem.thermal {
            Some(t) => {
                if thermal_state.indoor_c < t.comfort_min_c {
                    thermostat_on = true;
                } else if thermal_state.indoor_c > t.comfort_min_c + 0.5 {
                    thermostat_on = false;
                }
                let p = if thermostat_on {
                    t.heat_pump.max_electrical.get()
                } else {
                    0.0
                };
                let outdoor = problem.outdoor_at(k);
                thermal_state = thermal_step.step(
                    thermal_state,
                    p * t.heat_pump.cop(outdoor) / 1000.0,
                    outdoor,
                );
                p
            }
            None => 0.0,
        };

        // An unmanaged tank reheats the moment it drops below its set point and
        // stops when it is full. It knows nothing about the price or the roof,
        // which is the whole of the difference being measured.
        let dhw = match problem.dhw {
            Some(d) => {
                let draw = problem.dhw_draw_at(k) + d.standing_loss.get() * DT_HOURS;
                dhw_stored = (dhw_stored - draw).max(0.0);
                let missing = d.capacity.get() * DHW_THERMOSTAT_SET - dhw_stored;
                let p = if missing > 0.0 {
                    d.heater
                        .get()
                        .min(missing / (d.cop * DT_HOURS).max(f64::EPSILON))
                } else {
                    0.0
                };
                dhw_stored = (dhw_stored + p * d.cop * DT_HOURS).min(d.capacity.get());
                p
            }
            None => 0.0,
        };

        let net = load + ev + hp + dhw - pv;
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

        // A thermostat is not free of discomfort either: it reheats only after
        // the house has already fallen through the band, and in a cold snap it
        // never catches up. Leaving that out of the baseline would credit the
        // planner with comfort it did not actually have to buy.
        if let Some(t) = problem.thermal {
            let outside = (t.comfort_min_c - thermal_state.indoor_c)
                .max(thermal_state.indoor_c - t.comfort_max_c)
                .max(0.0);
            cost.discomfort_eur += outside * t.discomfort_eur_per_kelvin_hour * DT_HOURS;
        }
    }
    cost
}

/// How much has to be in the car by the end of slot `k`, if anything.
///
/// Three cases, and the middle one is the one an implementation forgets:
///
/// * the departure is **inside** the horizon — the full target applies in that
///   slot, and nothing before it;
/// * the departure is **beyond** the horizon — no deadline falls inside it, so
///   an unconstrained model would simply not charge, wait for the next re-plan,
///   and repeat that until the deadline finally came into view and the car could
///   no longer be filled in time. A pro-rata floor at the end of the horizon
///   keeps the plan moving at the constant rate that would just make it;
/// * the departure is **behind** the horizon — the car should already be full,
///   so the target applies from the first slot and the solve is infeasible if it
///   cannot be met, which is the honest answer.
fn charging_target(problem: &Problem<'_>, ev: EvSession, k: usize) -> Option<f64> {
    let last = problem.horizon.len.saturating_sub(1);
    let first = problem.horizon.first;
    if problem.horizon.index_of(ev.departure).is_some() {
        return (problem.horizon.get(k) == Some(ev.departure)).then(|| ev.energy_target.get());
    }
    if ev.departure < first {
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

/// Turn a solved model into a plan.
fn read_back(
    problem: &Problem<'_>,
    names: &AssetNames,
    now: OffsetDateTime,
    solution: &impl Solution,
    vars: &Vars<'_>,
    shadows: &[crate::shadow::Shadow],
) -> Solved {
    let n = problem.horizon.len;
    // ── Read the answer back ────────────────────────────────────────────────
    let mut flows = Vec::with_capacity(n);
    let mut slots = Vec::with_capacity(n);
    let mut cost = hems_core::prelude::CostBreakdown::default();

    for k in 0..n {
        // A solver returns a basis, not a decision. Two roundings, and the
        // second one is not cosmetic.
        //
        // A variable that is logically zero comes back as 1e-13, and it travels
        // from here into plan targets, JSON and tests that compare against zero.
        // No device in a household resolves a milliwatt, so neither does this.
        //
        // And a variable that landed exactly **on** a bound comes back a
        // nanowatt short of it: a semi-continuous charge point pinned to its own
        // 4 140 W minimum returns 4 139,999 999 999 896. Downstream that is not
        // a rounding error, it is a different decision — the arbiter's conductor
        // policy reads it as "below the three-phase minimum", drops the wallbox
        // to one conductor, and the car charges at 3,68 kW and 85 % instead of
        // 4,14 kW and 92 % for the rest of the session. Snapping to the
        // milliwatt costs nothing and removes a whole class of that.
        let value = |v: Variable| {
            let w = solution.value(v).max(0.0);
            Power::new(if w < 1e-3 {
                0.0
            } else {
                (w * 1e3).round() / 1e3
            })
        };
        let f = Flows {
            grid_import: value(vars.g_in[k]),
            grid_export: value(vars.g_out[k]),
            battery_charge: value(vars.b_ch[k]),
            battery_discharge: value(vars.b_dis[k]),
            battery_energy: hems_core::prelude::Energy::new(solution.value(vars.b_e[k]).max(0.0)),
            ev_charge: value(vars.ev[k]),
            curtailed: value(vars.curtail[k]),
            heat_pump: value(vars.hp[k]),
            dhw: value(vars.dhw[k]),
            dhw_stored: hems_core::prelude::Energy::new(solution.value(vars.dhw_e[k]).max(0.0)),
            indoor_c: solution.value(vars.t_in[k]),
            discomfort_k: solution.value(vars.cold[k]) + solution.value(vars.warm[k]),
        };

        // The same four terms the objective minimises, so the reported saving
        // cannot flatter itself by leaving out what the plan actually spent.
        // Only the *preference* weights of the objective are dropped here: a
        // carbon price and an autarky premium are what a household is willing to
        // pay, not what it is charged, and adding them to a reported bill would
        // invent an invoice nobody sends.
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

        // The marginal value of a kilowatt-hour in this slot. Where the dual
        // pass ran it is the **shadow price** of the energy balance — what the
        // next kilowatt-hour would actually cost this plan, which above a
        // binding limit is not the tariff at all. Where it did not, it falls
        // back to the price the plan faces, which is what the first four
        // versions used and is enough to rank slots.
        let shadow = shadows.get(k).copied();
        let site_marginal = shadow.map_or_else(
            || {
                if f.grid_import > f.grid_export {
                    import_eur
                } else {
                    export_eur
                }
            },
            |s| s.site,
        );
        let targets = slot_targets(problem, names, &f, k, shadow.as_ref());

        slots.push(SlotPlan {
            slot: problem.horizon.get(k).expect("k < horizon length"),
            targets,
            marginal_eur_per_kwh: Some(site_marginal),
            flexibility_eur_per_kwh: shadow.and_then(|s| s.flexibility),
        });
        flows.push(f);
    }

    let baseline = baseline_cost(problem);
    let unmet_charge = hems_core::prelude::Energy::new(
        (0..n)
            .map(|k| solution.value(vars.ev_short[k]))
            .fold(0.0_f64, f64::max)
            .max(0.0),
    );

    let unmet_hot_water = hems_core::prelude::Energy::new(
        (0..n)
            .map(|k| solution.value(vars.dhw_short[k]).max(0.0))
            .sum::<f64>(),
    );

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
    }
}
