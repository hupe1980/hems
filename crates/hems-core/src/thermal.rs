//! The building as thermal storage: a two-mass RC model, discretised exactly.
//!
//! A house does not cool down the moment the heating stops and cannot be
//! reheated instantly either. That inertia is **storage** — usually several
//! times larger than the household battery, and free — and it is the reason a
//! heat pump can be moved into a cheap or sunny hour at all. The smallest model
//! that reproduces it is two capacities (the indoor air, the building fabric)
//! and two resistances (air ↔ outdoors, air ↔ fabric):
//!
//! ```text
//! C_air · dT_air/dt  = (T_out − T_air)/R_air_out + (T_mass − T_air)/R_air_mass + Q
//! C_mass · dT_mass/dt = (T_air − T_mass)/R_air_mass
//! ```
//!
//! # Why the discretisation is the interesting part
//!
//! The planner works in quarter hours; the air–fabric coupling of an ordinary
//! house has a time constant of about thirteen minutes. Stepping the equations
//! above with **explicit Euler** at Δt = 15 min is therefore not a small
//! approximation, and two things go wrong at once.
//!
//! *The fast eigenvalue comes out with the wrong sign.* For the house below the
//! exact pair is `{0,9969; 0,3135}`; explicit Euler gives `{0,9969; −0,1601}`.
//! The slow mode — the fabric, which is what makes pre-heating worth planning —
//! is reproduced almost perfectly. The fast mode is not merely inaccurate, it
//! alternates, so the planned air temperature **rings** after every change of
//! heat input: 20,16 → 19,88 → 19,75 exactly, against 19,67 → 19,80 → 19,72 in
//! the Euler model. Those are the slots in which an on/off heat pump's minimum
//! runtime is decided and in which the comfort slack is priced.
//!
//! *And the input gain is 64 % too large.* One kilowatt held for a slot raises
//! the air by 0,254 K, not by `Δt/C_air` = 0,417 K, because the air is already
//! shedding heat into the fabric while it warms — and the fabric takes 0,008 K
//! of it, where explicit Euler gives it exactly none. A planner that believes
//! heating works two thirds better than it does under-heats.
//!
//! Explicit Euler here is also only *conditionally* stable, and nothing checks
//! the condition. The house below sits just inside it; drop the air capacity to
//! 0,3 kWh/K — a flat rather than a house — and the fast eigenvalue passes −1,27
//! and the planned temperature diverges.
//!
//! So the model is discretised **exactly** instead, by a zero-order hold: the
//! heat input and the outdoor temperature are constant across a slot, which is
//! precisely what a quarter-hour plan asserts, and under that assumption
//!
//! ```text
//! x[k+1] = A_d · x[k] + b_heat · Q[k] + b_out · T_out[k]
//! ```
//!
//! holds with **no discretisation error at all**. `A_d` is the matrix
//! exponential of the continuous system over the step, so its eigenvalues are
//! `e^{λ Δt} ∈ (0, 1)` for any physically valid parameters — the scheme cannot
//! ring and cannot diverge, at any step size. It is still **linear in `Q`**,
//! which is what keeps the planner a linear program.
//!
//! The same [`Rc2Discrete`] serves the planner (Δt = 15 min), the rule-based
//! baseline it compares itself against, and the simulator that answers it
//! (Δt = 1 min). One model, one set of coefficients per step size, and no way
//! for the plan and the house to disagree about physics for numerical reasons.

use time::Duration;

/// A two-capacity, two-resistance building.
///
/// All quantities in kilowatts, kelvin, hours: `C` in kWh/K, `R` in K/kW.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rc2 {
    /// Heat capacity of the indoor air and the furniture that follows it, kWh/K.
    pub air_capacity_kwh_per_k: f64,
    /// Heat capacity of the building fabric, kWh/K. The large, slow one.
    pub mass_capacity_kwh_per_k: f64,
    /// Thermal resistance from the indoor air to outdoors, K/kW.
    pub r_air_out_k_per_kw: f64,
    /// Thermal resistance from the indoor air to the fabric, K/kW.
    pub r_air_mass_k_per_kw: f64,
}

/// The state of the two masses, °C.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ThermalState {
    /// Indoor air temperature, °C.
    pub indoor_c: f64,
    /// Building fabric temperature, °C.
    pub mass_c: f64,
}

impl ThermalState {
    /// Both masses at the same temperature — a house that has been left alone.
    #[must_use]
    pub const fn uniform(temperature_c: f64) -> Self {
        Self {
            indoor_c: temperature_c,
            mass_c: temperature_c,
        }
    }
}

impl Rc2 {
    /// A well-insulated German single-family house of roughly 150 m².
    ///
    /// `R_air_out = 6 K/kW` is a heat loss of about 3,5 kW at −20 °C outdoors
    /// and 21 °C indoors; the fabric holds about 12 kWh/K, which is what makes
    /// pre-heating worth planning at all.
    #[must_use]
    pub const fn house() -> Self {
        Self {
            air_capacity_kwh_per_k: 0.6,
            mass_capacity_kwh_per_k: 12.0,
            r_air_out_k_per_kw: 6.0,
            r_air_mass_k_per_kw: 0.4,
        }
    }

    /// Whether every parameter is finite and strictly positive — the condition
    /// under which the system matrix has real, non-positive eigenvalues and the
    /// exact discretisation below is a contraction.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        [
            self.air_capacity_kwh_per_k,
            self.mass_capacity_kwh_per_k,
            self.r_air_out_k_per_kw,
            self.r_air_mass_k_per_kw,
        ]
        .iter()
        .all(|v| v.is_finite() && *v > 0.0)
    }

    /// Steady-state heat input needed to hold `indoor_c` against `outdoor_c`, kW.
    ///
    /// The fabric contributes nothing in steady state — it is at the air
    /// temperature — so this is the envelope loss alone. It is the number that
    /// says whether a heat pump is big enough for the house at all.
    #[must_use]
    pub fn steady_state_heat_kw(&self, indoor_c: f64, outdoor_c: f64) -> f64 {
        (indoor_c - outdoor_c) / self.r_air_out_k_per_kw
    }

    /// The energy stored in both masses relative to `reference_c`, kWh.
    ///
    /// What the planner is allowed to bank when electricity is cheap, and what
    /// the terminal value of the horizon is computed from.
    #[must_use]
    pub fn stored_kwh(&self, state: ThermalState, reference_c: f64) -> f64 {
        (state.indoor_c - reference_c) * self.air_capacity_kwh_per_k
            + (state.mass_c - reference_c) * self.mass_capacity_kwh_per_k
    }

    /// The exact discrete-time model for a step of `dt`.
    ///
    /// Zero-order hold on the heat input and the outdoor temperature: both are
    /// taken as constant across the step, which is exactly what a quarter-hour
    /// plan asserts about them.
    ///
    /// # Panics
    /// Never: invalid parameters (see [`Rc2::is_valid`]) fall back to an
    /// adiabatic model — a house that neither gains nor loses heat — rather than
    /// producing infinities that would silently poison a plan.
    #[must_use]
    pub fn discretise(&self, dt: Duration) -> Rc2Discrete {
        let hours = dt.as_seconds_f64() / 3600.0;
        if !(self.is_valid() && hours.is_finite() && hours > 0.0) {
            return Rc2Discrete::HOLD;
        }
        // Conductances per unit capacity, in the Festlegung-free notation the
        // building-physics literature uses: `to_out` and `to_mass` are what the
        // air node loses to each neighbour, `from_air` what the fabric gains.
        let to_out = 1.0 / (self.r_air_out_k_per_kw * self.air_capacity_kwh_per_k);
        let to_mass = 1.0 / (self.r_air_mass_k_per_kw * self.air_capacity_kwh_per_k);
        let from_air = 1.0 / (self.r_air_mass_k_per_kw * self.mass_capacity_kwh_per_k);
        let inv_c_air = 1.0 / self.air_capacity_kwh_per_k;

        // The augmented system whose states are (T_air, T_mass, Q, T_out) with
        // the last two held constant. Its matrix exponential *is* the
        // zero-order-hold discretisation — state transition and input matrices
        // in one exponential, with no separate integral to approximate.
        //
        //   d/dt [T_air ]   [ −(a+b)   b   1/C_air   a ] [T_air ]
        //        [T_mass] = [   c     −c      0      0 ] [T_mass]
        //        [  Q   ]   [   0      0      0      0 ] [  Q   ]
        //        [ T_out]   [   0      0      0      0 ] [ T_out]
        // with a = to_out, b = to_mass, c = from_air.
        let mut m = [[0.0_f64; 4]; 4];
        m[0] = [-(to_out + to_mass), to_mass, inv_c_air, to_out];
        m[1] = [from_air, -from_air, 0.0, 0.0];
        let e = expm4(&m, hours);

        Rc2Discrete {
            a: [[e[0][0], e[0][1]], [e[1][0], e[1][1]]],
            b_heat: [e[0][2], e[1][2]],
            b_outdoor: [e[0][3], e[1][3]],
        }
    }

    /// One exact step, for callers that do not keep the coefficients around.
    ///
    /// A simulator stepping at a fixed cadence should call [`Rc2::discretise`]
    /// once and reuse the result; this is the convenient form.
    #[must_use]
    pub fn step(
        &self,
        state: ThermalState,
        heat_kw: f64,
        outdoor_c: f64,
        dt: Duration,
    ) -> ThermalState {
        self.discretise(dt).step(state, heat_kw, outdoor_c)
    }
}

/// How a heat pump's coefficient of performance moves with the weather.
///
/// Linear in the outdoor temperature: close enough for a household, and — far
/// more importantly — it makes the coefficient a **constant within a slot**,
/// computed from the weather forecast before the solver ever sees it. A
/// coefficient that depended on the decision would make the planner non-linear
/// for a second-order effect.
///
/// The slope is positive: a heat pump is *better* when it is warmer, which is
/// why pre-heating in the afternoon beats waiting for the coldest hour of the
/// night even at the same price.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CopCurve {
    /// Coefficient of performance at 0 °C outdoors.
    pub at_zero: f64,
    /// Change in the coefficient per kelvin of outdoor temperature.
    pub slope_per_k: f64,
}

impl Default for CopCurve {
    fn default() -> Self {
        Self::air_source()
    }
}

impl CopCurve {
    /// A modern air-source heat pump at a low flow temperature.
    #[must_use]
    pub const fn air_source() -> Self {
        Self {
            at_zero: 3.2,
            slope_per_k: 0.06,
        }
    }

    /// The coefficient at an outdoor temperature, clamped to a physically
    /// possible range so a nonsense forecast cannot invent free heat.
    #[must_use]
    pub fn at(&self, outdoor_c: f64) -> f64 {
        if !outdoor_c.is_finite() {
            return self.at_zero.clamp(1.0, 6.0);
        }
        (self.at_zero + outdoor_c * self.slope_per_k).clamp(1.0, 6.0)
    }
}

/// The exact discrete-time model of an [`Rc2`] for one step size.
///
/// ```text
/// indoor'  = a[0][0]·indoor + a[0][1]·mass + b_heat[0]·Q + b_outdoor[0]·T_out
/// mass'    = a[1][0]·indoor + a[1][1]·mass + b_heat[1]·Q + b_outdoor[1]·T_out
/// ```
///
/// Every coefficient is a constant, so the two lines above are **linear
/// constraints** and can be handed to a linear program unchanged. That is the
/// whole reason the coefficient of performance is modelled as a function of the
/// *forecast* outdoor temperature rather than of the decision: it collapses into
/// `Q = COP · P_electrical` with a constant `COP` per slot.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rc2Discrete {
    /// State transition, row-major: `a[to][from]`.
    pub a: [[f64; 2]; 2],
    /// Response to one kilowatt of heat held for the step, K.
    pub b_heat: [f64; 2],
    /// Response to the outdoor temperature over the step, dimensionless.
    pub b_outdoor: [f64; 2],
}

impl Rc2Discrete {
    /// A house that neither gains nor loses heat — the fallback for parameters
    /// that are not physical. It is wrong, but it is *bounded* and obvious,
    /// which an infinity is not.
    pub const HOLD: Self = Self {
        a: [[1.0, 0.0], [0.0, 1.0]],
        b_heat: [0.0, 0.0],
        b_outdoor: [0.0, 0.0],
    };

    /// Advance one step.
    #[must_use]
    pub fn step(&self, state: ThermalState, heat_kw: f64, outdoor_c: f64) -> ThermalState {
        ThermalState {
            indoor_c: self.a[0][0] * state.indoor_c
                + self.a[0][1] * state.mass_c
                + self.b_heat[0] * heat_kw
                + self.b_outdoor[0] * outdoor_c,
            mass_c: self.a[1][0] * state.indoor_c
                + self.a[1][1] * state.mass_c
                + self.b_heat[1] * heat_kw
                + self.b_outdoor[1] * outdoor_c,
        }
    }

    /// Whether the step is a contraction — the property explicit Euler loses at
    /// a quarter-hour step and this construction cannot.
    ///
    /// Checked through the spectral radius of a 2×2 matrix, which for real
    /// eigenvalues is `max|λ|`. A discretisation that failed this would grow a
    /// temperature without any heat being put into the house.
    #[must_use]
    pub fn is_contraction(&self) -> bool {
        let [[a, b], [c, d]] = self.a;
        let trace = a + d;
        let det = a * d - b * c;
        let disc = trace * trace - 4.0 * det;
        let radius = if disc >= 0.0 {
            let root = disc.sqrt();
            f64::midpoint(trace, root)
                .abs()
                .max(f64::midpoint(trace, -root).abs())
        } else {
            det.abs().sqrt()
        };
        radius < 1.0
    }
}

/// `exp(m · t)` for a 4×4 matrix, by scaling and squaring with a Taylor series.
///
/// Small, fixed size, no dependency, and accurate to machine precision for the
/// well-conditioned matrices a building model produces. The alternative — an
/// eigendecomposition — needs a case analysis for repeated and zero eigenvalues,
/// both of which occur for perfectly ordinary parameters (an infinitely
/// insulated wall gives a zero eigenvalue), and each case is a place to be
/// wrong.
fn expm4(m: &[[f64; 4]; 4], t: f64) -> [[f64; 4]; 4] {
    let mut scaled = [[0.0_f64; 4]; 4];
    let mut norm = 0.0_f64;
    for i in 0..4 {
        let mut row = 0.0;
        for j in 0..4 {
            scaled[i][j] = m[i][j] * t;
            row += scaled[i][j].abs();
        }
        norm = norm.max(row);
    }

    // Halve until the series converges quickly, then square back up.
    let squarings = if norm > 0.5 {
        (norm / 0.5).log2().ceil().clamp(0.0, 60.0) as u32
    } else {
        0
    };
    let shrink = 2.0_f64.powi(-(squarings as i32));
    for row in &mut scaled {
        for v in row.iter_mut() {
            *v *= shrink;
        }
    }

    // exp(X) = Σ X^k / k!. Eighteen terms is far more than needed once ‖X‖ ≤ ½.
    let mut result = identity4();
    let mut term = identity4();
    for k in 1..=18 {
        term = mul4(&term, &scaled);
        let inv = 1.0 / f64::from(k);
        for row in &mut term {
            for v in row.iter_mut() {
                *v *= inv;
            }
        }
        for i in 0..4 {
            for j in 0..4 {
                result[i][j] += term[i][j];
            }
        }
    }

    for _ in 0..squarings {
        result = mul4(&result, &result);
    }
    result
}

const fn identity4() -> [[f64; 4]; 4] {
    let mut m = [[0.0; 4]; 4];
    m[0][0] = 1.0;
    m[1][1] = 1.0;
    m[2][2] = 1.0;
    m[3][3] = 1.0;
    m
}

fn mul4(a: &[[f64; 4]; 4], b: &[[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut out = [[0.0_f64; 4]; 4];
    for i in 0..4 {
        for k in 0..4 {
            let aik = a[i][k];
            if aik == 0.0 {
                continue;
            }
            for j in 0..4 {
                out[i][j] += aik * b[k][j];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUARTER: Duration = Duration::minutes(15);

    /// The scheme the exact discretisation replaces, kept only to demonstrate
    /// what it was doing.
    fn explicit_euler(
        rc: &Rc2,
        state: ThermalState,
        heat_kw: f64,
        outdoor_c: f64,
        hours: f64,
    ) -> ThermalState {
        let air_gain = hours / rc.air_capacity_kwh_per_k;
        let mass_gain = hours / rc.mass_capacity_kwh_per_k;
        ThermalState {
            indoor_c: state.indoor_c + heat_kw * air_gain
                - (state.indoor_c - outdoor_c) * (air_gain / rc.r_air_out_k_per_kw)
                - (state.indoor_c - state.mass_c) * (air_gain / rc.r_air_mass_k_per_kw),
            mass_c: state.mass_c
                + (state.indoor_c - state.mass_c) * (mass_gain / rc.r_air_mass_k_per_kw),
        }
    }

    #[test]
    fn explicit_euler_rings_where_the_exact_step_decays() {
        // The reason this module exists. Explicit Euler at a quarter-hour step
        // gives the fast (air) mode a *negative* eigenvalue, so the planned
        // temperature alternates about its trajectory after every change of heat
        // input instead of settling towards it.
        let rc = Rc2::house();
        let exact = rc.discretise(QUARTER);

        let mut euler = ThermalState::uniform(21.0);
        let mut settled = ThermalState::uniform(21.0);
        // Four slots of heating, then the heat pump stops — the transition an
        // on/off unit's minimum-runtime decision is made across.
        let mut euler_track = Vec::new();
        let mut exact_track = Vec::new();
        for k in 0..12 {
            let heat = if k < 4 { 6.0 } else { 0.0 };
            euler = explicit_euler(&rc, euler, heat, 2.0, 0.25);
            settled = exact.step(settled, heat, 2.0);
            euler_track.push(euler.indoor_c);
            exact_track.push(settled.indoor_c);
        }

        // After the heat stops, the exact trajectory falls monotonically.
        let tail = &exact_track[4..];
        assert!(
            tail.windows(2).all(|w| w[1] < w[0]),
            "the exact model should cool monotonically: {tail:?}"
        );
        // Explicit Euler does not: it overshoots downwards and bounces back.
        let euler_tail = &euler_track[4..];
        assert!(
            euler_tail.windows(2).any(|w| w[1] > w[0]),
            "explicit Euler should ring here: {euler_tail:?}"
        );
    }

    #[test]
    fn explicit_euler_overstates_the_heat_gain_by_two_thirds() {
        // One kilowatt held for a quarter hour raises the air by 0,254 K, not by
        // Δt/C_air = 0,417 K: the air is shedding heat into the fabric while it
        // warms, and the fabric takes a little of it. A planner that believes
        // the larger number under-heats the house.
        let rc = Rc2::house();
        let d = rc.discretise(QUARTER);
        assert!((d.b_heat[0] - 0.2538).abs() < 5e-4, "{:?}", d.b_heat);
        assert!(d.b_heat[1] > 0.0, "the fabric takes some: {:?}", d.b_heat);

        let naive = 0.25 / rc.air_capacity_kwh_per_k;
        assert!((naive / d.b_heat[0] - 1.64).abs() < 0.02);
    }

    #[test]
    fn explicit_euler_diverges_outright_for_a_flat() {
        // The scheme is only *conditionally* stable and nothing checks the
        // condition. The reference house sits just inside it; a smaller air
        // capacity does not.
        let flat = Rc2 {
            air_capacity_kwh_per_k: 0.3,
            ..Rc2::house()
        };
        let mut state = ThermalState::uniform(21.0);
        for _ in 0..40 {
            state = explicit_euler(&flat, state, 0.0, 5.0, 0.25);
        }
        assert!(
            !(-30.0..=60.0).contains(&state.indoor_c),
            "explicit Euler should diverge here, ended at {} °C",
            state.indoor_c
        );
        // The exact step is a contraction for the same house.
        assert!(flat.discretise(QUARTER).is_contraction());
    }

    #[test]
    fn the_exact_step_is_a_contraction_at_every_step_size() {
        let rc = Rc2::house();
        for minutes in [1_i64, 5, 15, 60, 240, 1440] {
            let d = rc.discretise(Duration::minutes(minutes));
            assert!(
                d.is_contraction(),
                "unstable at a {minutes}-minute step: {:?}",
                d.a
            );
        }
    }

    #[test]
    fn a_quarter_hour_step_matches_a_thousand_small_ones() {
        // The claim "exact": one 15-minute zero-order-hold step equals the
        // continuous solution, so it must agree with a finely integrated one.
        let rc = Rc2::house();
        let start = ThermalState {
            indoor_c: 19.0,
            mass_c: 21.5,
        };
        let coarse = rc.step(start, 3.0, -2.0, QUARTER);

        let fine_step = rc.discretise(Duration::milliseconds(900));
        let mut fine = start;
        for _ in 0..1000 {
            fine = fine_step.step(fine, 3.0, -2.0);
        }
        assert!(
            (coarse.indoor_c - fine.indoor_c).abs() < 1e-9,
            "{} vs {}",
            coarse.indoor_c,
            fine.indoor_c
        );
        assert!((coarse.mass_c - fine.mass_c).abs() < 1e-9);
    }

    #[test]
    fn with_no_heat_the_house_relaxes_towards_outdoors_and_stops_there() {
        let rc = Rc2::house();
        let step = rc.discretise(Duration::hours(1));
        let mut state = ThermalState::uniform(21.0);
        // The fabric's time constant is about C_mass · R_air_out = 72 h, so
        // "eventually" is measured in weeks, not hours.
        for _ in 0..2000 {
            state = step.step(state, 0.0, 3.0);
        }
        assert!((state.indoor_c - 3.0).abs() < 1e-6, "{state:?}");
        assert!((state.mass_c - 3.0).abs() < 1e-6, "{state:?}");
    }

    #[test]
    fn the_steady_state_heat_holds_the_house_exactly() {
        // Feed in precisely the envelope loss and nothing moves — the property
        // that makes `steady_state_heat_kw` usable for sizing.
        let rc = Rc2::house();
        let step = rc.discretise(QUARTER);
        let heat = rc.steady_state_heat_kw(21.0, -5.0);
        let mut state = ThermalState::uniform(21.0);
        for _ in 0..96 {
            state = step.step(state, heat, -5.0);
        }
        assert!((state.indoor_c - 21.0).abs() < 1e-9, "{state:?}");
        assert!((state.mass_c - 21.0).abs() < 1e-9, "{state:?}");
    }

    #[test]
    fn the_fabric_is_the_storage_and_it_is_slower_than_the_air() {
        // Heat the house hard for an hour: the air moves several times as far as
        // the fabric. That separation of time constants is the entire reason the
        // second mass is modelled at all.
        let rc = Rc2::house();
        let step = rc.discretise(QUARTER);
        let mut state = ThermalState::uniform(20.0);
        for _ in 0..4 {
            state = step.step(state, 6.0, 0.0);
        }
        let air_rise = state.indoor_c - 20.0;
        let mass_rise = state.mass_c - 20.0;
        assert!(air_rise > mass_rise, "{state:?}");
        assert!(
            mass_rise > 0.0,
            "the fabric must take some of it: {state:?}"
        );
    }

    #[test]
    fn the_rows_of_the_transition_sum_to_one_when_outdoors_is_ignored() {
        // A house at a uniform temperature with no heat and outdoors at the same
        // temperature must stay exactly where it is, whatever the step size.
        let rc = Rc2::house();
        for minutes in [1_i64, 15, 180] {
            let d = rc.discretise(Duration::minutes(minutes));
            let held = d.step(ThermalState::uniform(21.0), 0.0, 21.0);
            assert!((held.indoor_c - 21.0).abs() < 1e-9, "{minutes} min");
            assert!((held.mass_c - 21.0).abs() < 1e-9, "{minutes} min");
        }
    }

    #[test]
    fn stored_energy_counts_both_masses() {
        let rc = Rc2::house();
        let state = ThermalState {
            indoor_c: 22.0,
            mass_c: 21.0,
        };
        // 1 K of air (0,6 kWh/K) plus 0 K of fabric, relative to 21 °C.
        assert!((rc.stored_kwh(state, 21.0) - 0.6).abs() < 1e-12);
    }

    #[test]
    fn nonsense_parameters_hold_the_temperature_instead_of_exploding() {
        let broken = Rc2 {
            air_capacity_kwh_per_k: 0.0,
            ..Rc2::house()
        };
        let d = broken.discretise(QUARTER);
        assert_eq!(d, Rc2Discrete::HOLD);
        let state = d.step(ThermalState::uniform(21.0), 5.0, -10.0);
        assert_eq!(state, ThermalState::uniform(21.0));
    }

    #[test]
    fn a_zero_length_step_changes_nothing() {
        let d = Rc2::house().discretise(Duration::ZERO);
        assert_eq!(d, Rc2Discrete::HOLD);
    }
}
