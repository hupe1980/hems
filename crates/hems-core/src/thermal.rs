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
    /// Effective solar aperture, m² — the area of perfectly transmitting
    /// glazing this building is equivalent to.
    ///
    /// Multiply by the irradiance on the building's principal glazed plane and
    /// you have the heat the sun puts into it. It is the glazed area times the
    /// glass's `g` value times whatever the frames, the curtains and the tree in
    /// front take away, and it is one number rather than four because no
    /// installer knows the other three and the fit does not need them separated.
    ///
    /// **The plane is vertical**, at the façade the windows are mostly in — not
    /// the horizontal. Driving the aperture with a horizontal irradiance would
    /// make it a different constant in December from the one it is in June, and
    /// a house identified in one season would then be wrong in the other by the
    /// ratio of the two, which at 52° north is about three.
    ///
    /// Zero is a building with no glazing modelled, which is a *worse* model rather
    /// than a safer one — see [`Rc2::free_heat_kw`] for which way it is wrong.
    pub solar_aperture_m2: f64,
    /// Heat from people, cooking, and everything electric that ends up as heat,
    /// kW.
    ///
    /// A constant, deliberately. The schedules in DIN V 18599-10 describe an
    /// average dwelling over a year and say nothing about whether *this*
    /// household is at home on a Tuesday; a box that pretended to know would be
    /// inventing a diurnal shape to sit beside the two it has actually measured.
    /// Three watts per square metre of the archetype's floor area, which is the
    /// residential middle of EN ISO 13790 Annex G.
    pub internal_gain_kw: f64,
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

/// What the compressor has been doing when the plan is made.
///
/// # Why a minimum runtime needs a memory
///
/// A heat pump's minimum on-time and minimum off-time are
/// stated *inside* a plan: on at slot `k` and off at `k − 1` forces on at
/// `k + 1`. That is the textbook unit-commitment formulation and it is right —
/// but a receding-horizon controller commits only the **first** slot and then
/// throws the rest away, and the first slot is the one with no `k − 1` in the
/// model to be constrained against.
///
/// So without this the constraint binds only on slots that are never executed.
/// A plan may start the compressor at 08:00, the box commits that quarter hour,
/// re-plans at 08:15 against a model with no memory at all, and stops it again —
/// for ever, at every re-plan, and each plan is individually feasible. The
/// executed trajectory short-cycles exactly as though `min_on_slots` had never
/// been written, and nothing anywhere reports a violation, because within each
/// plan there is none.
///
/// Carrying the two facts a compressor has — whether it is running, and for how
/// long — closes the horizon boundary: the planner turns them into the slots at
/// the start of the plan that are not decisions any more, and pins those rather
/// than branching on them.
///
/// It lives here rather than in the planner because the **simulator** needs the
/// same two facts to answer one, and a house whose compressor obeyed a different
/// rule from the plan's would make every cycling figure meaningless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CompressorState {
    /// Whether the compressor is running as the horizon opens.
    pub running: bool,
    /// How many whole slots it has been in that state.
    ///
    /// Saturating: a unit that has been off since yesterday and one that has
    /// been off for an hour are equally free, so a caller may count as far as it
    /// likes and does not have to clamp.
    pub slots_in_state: usize,
}

impl CompressorState {
    /// A compressor that has been in its state long enough to be free.
    #[must_use]
    pub const fn settled(running: bool) -> Self {
        Self {
            running,
            slots_in_state: usize::MAX,
        }
    }

    /// The state after a slot in which the unit was `running`.
    #[must_use]
    pub const fn after(self, running: bool) -> Self {
        Self {
            running,
            slots_in_state: if self.running == running {
                self.slots_in_state.saturating_add(1)
            } else {
                1
            },
        }
    }
}

/// A building archetype, for a box that has not identified its own house yet.
///
/// # Why the default is not good enough
///
/// The fabric capacity is what decides whether pre-heating into a cheap hour
/// pays at all, and between a 1970s solid-wall house and a new timber frame it
/// differs by a factor of five. The envelope loss differs by four. A box that
/// planned every household against one set of numbers would over-heat most of
/// them and pay the comfort slack for the overshoot — so the archetype an
/// installer picks in the cellar is the **prior**, and the box replaces it with
/// a building identified from the household's own thermometer
/// (`hems_forecast::building`) once it has watched a few excited days.
///
/// # Where the numbers come from
///
/// Each is a 150 m² dwelling, and the two capacities are the effective thermal
/// capacity classes of DIN EN ISO 13790 (light 80 kJ/m²K ≈ 22 Wh/m²K, heavy
/// 260, very heavy 370) times that floor area. `R_air_out` is `ΔT / Q` at the
/// archetype's specific heat load, taken at the German design pair of 21 °C
/// indoors and −20 °C outdoors — so a class quoted at *q* W/m² has
/// `R = 41 K / (q · 150 W)`.
///
/// `R_air_mass = 0,4 K/kW` is shared: with an air capacity near 0,6 kWh/K it is
/// an air–fabric time constant of about a quarter of an hour, which is the
/// number the exact discretisation exists for (see the module header) and is a
/// property of how a room exchanges heat with its own walls rather than of how
/// well the outside wall is insulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum BuildingClass {
    /// The average German single-family house of roughly 150 m² — 1990s
    /// masonry, or an older one partly retrofitted. About 45 W/m², which is
    /// 6,8 kW of design heat load.
    ///
    /// The default, and the house every reference day in this workspace is
    /// measured on.
    #[default]
    Average,
    /// GEG-era new build or a deep retrofit: about 27 W/m², 4,1 kW.
    NewBuild,
    /// Unretrofitted pre-1979 solid-wall masonry: about 110 W/m², 16,5 kW, and
    /// a very heavy fabric that takes a day to move.
    SolidWall,
    /// A flat inside a heated block: roughly 80 m², losing heat on two sides
    /// rather than six.
    ///
    /// Light on **envelope** and not on **mass**, which is the distinction that
    /// decides whether pre-heating pays. Its outside walls are somebody else's
    /// problem, so `R_air_out` is the highest of the four; but the concrete slabs
    /// above and below it are its own thermal capacity — ISO 13790 counts an
    /// internal element to half its depth — so 5,0 kWh/K over 80 m² is about
    /// 63 Wh/m²K, which is medium rather than light. A flat is a small store
    /// behind a good coat, not a tent.
    Apartment,
}

impl BuildingClass {
    /// The two-mass model this archetype stands for.
    #[must_use]
    pub const fn rc2(self) -> Rc2 {
        match self {
            Self::Average => Rc2::house(),
            Self::NewBuild => Rc2 {
                air_capacity_kwh_per_k: 0.6,
                // Timber frame or a screed floor over insulation: light to
                // medium, about 40 Wh/m²K.
                mass_capacity_kwh_per_k: 6.0,
                // 41 K / 4,1 kW.
                r_air_out_k_per_kw: 10.0,
                r_air_mass_k_per_kw: 0.4,
                // More glazing than the average house and worse glass for it:
                // 30 m² at a triple-glazed g of 0,5. The two changes nearly
                // cancel, which is why a new build is not the sunniest of the
                // four.
                solar_aperture_m2: 5.0,
                internal_gain_kw: 0.45,
            },
            Self::SolidWall => Rc2 {
                air_capacity_kwh_per_k: 0.6,
                // Very heavy, ≈ 100 Wh/m²K over 150 m².
                mass_capacity_kwh_per_k: 15.0,
                // 41 K / 16,5 kW.
                r_air_out_k_per_kw: 2.5,
                r_air_mass_k_per_kw: 0.4,
                // Small windows in thick walls, and the reveal shades them.
                solar_aperture_m2: 3.5,
                internal_gain_kw: 0.45,
            },
            Self::Apartment => Rc2 {
                // 80 m² rather than 150.
                air_capacity_kwh_per_k: 0.35,
                mass_capacity_kwh_per_k: 5.0,
                // 41 K / 2,7 kW: a flat with heated neighbours above, below and
                // to one side loses heat through far less envelope than its
                // floor area suggests.
                r_air_out_k_per_kw: 15.0,
                r_air_mass_k_per_kw: 0.4,
                // Windows on one or two sides rather than four, over half the
                // floor area.
                solar_aperture_m2: 2.0,
                internal_gain_kw: 0.25,
            },
        }
    }
}

impl Rc2 {
    /// The average German single-family house of roughly 150 m².
    ///
    /// `R_air_out = 6 K/kW` is a design heat load of 6,8 kW at −20 °C outdoors
    /// and 21 °C indoors — 45 W/m², which is 1990s masonry or an older house
    /// partly retrofitted rather than a new build. The fabric holds about
    /// 12 kWh/K, which is what makes pre-heating worth planning at all.
    ///
    /// It is [`BuildingClass::Average`], and it is the prior a box starts from
    /// when nobody has said which house this is. See [`BuildingClass`] for the
    /// span the others cover and why the choice matters.
    #[must_use]
    pub const fn house() -> Self {
        Self {
            air_capacity_kwh_per_k: 0.6,
            mass_capacity_kwh_per_k: 12.0,
            r_air_out_k_per_kw: 6.0,
            r_air_mass_k_per_kw: 0.4,
            // About 25 m² of glazing, a little under half of it facing the sun,
            // double-glazed at g ≈ 0,6, with frames and curtains taking a third.
            solar_aperture_m2: 4.5,
            // 150 m² at 3 W/m².
            internal_gain_kw: 0.45,
        }
    }

    /// Heat this building gets for nothing, kW.
    ///
    /// `irradiance_w_per_m2` is on the **vertical** plane the glazing is mostly
    /// in — see [`Rc2::solar_aperture_m2`]. It enters the model exactly where
    /// the heat pump's kilowatts do, which is why the discretisation needs no
    /// new term and the planner stays a linear program: the state equation is
    /// linear in the heat input, and free heat is a *known constant* in each
    /// slot rather than a decision.
    ///
    /// # Why a model without this is not merely less precise
    ///
    /// The German single-family archetype loses `(21 − T_out) / 6` kW. At 5 °C
    /// that is 2,7 kW; a clear March noon on a vertical south plane is about
    /// 550 W/m², which through a 4,5 m² aperture is 2,5 kW, and the internal
    /// gains are another 0,45. **The free heat exceeds the demand.** Even at
    /// −2 °C in January it is about half of it.
    ///
    /// A planner that does not model it therefore believes the house needs
    /// heating all day on exactly the days it does not, runs the compressor into
    /// a room the sun is already warming, and pays the comfort slack for the
    /// overshoot at the top of the band — which is the *opposite* of the error a
    /// conservative omission would make, so leaving it out is not the safe
    /// choice. It is also the error that makes an identified fabric wrong: a fit
    /// with no aperture has nowhere to put the sun but `R_air_out`, so it
    /// reports a better-insulated house than the one it watched, and reports a
    /// different one in June from the one it reported in December.
    #[must_use]
    pub fn free_heat_kw(&self, irradiance_w_per_m2: f64) -> f64 {
        if !irradiance_w_per_m2.is_finite() {
            return self.internal_gain_kw.max(0.0);
        }
        self.internal_gain_kw.max(0.0)
            + self.solar_aperture_m2.max(0.0) * irradiance_w_per_m2.max(0.0) / 1000.0
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
            // The two gain parameters are *not* in the list above: they take no
            // part in the discretisation, so a nonsensical one cannot make the
            // step diverge and must not cost a house its whole thermal model.
            // Zero is a meaningful value for both, and `free_heat_kw` floors
            // them, so this only has to refuse a NaN.
            && [self.solar_aperture_m2, self.internal_gain_kw]
                .iter()
                .all(|v| v.is_finite() && *v >= 0.0)
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

    /// The same unit running **backwards**, as an energy efficiency ratio.
    ///
    /// The slope is **negative**, which is the whole difference and is the thing
    /// a heating curve reused unchanged would get exactly wrong: a heat pump
    /// heats better when it is warm outside and cools *worse*, because in both
    /// cases what it is fighting is the gap between indoors and out.
    ///
    /// Anchored at the two points a datasheet quotes: about 4,5 at 25 °C and
    /// about 3,0 at 35 °C, which is an ordinary reversible air-to-water unit at a
    /// cooling flow temperature underfloor heating can carry. The clamp
    /// [`CopCurve::at`] already applies keeps it inside 1–6, so the extrapolation
    /// below about 15 °C — where nobody cools anyway — cannot invent free cold.
    #[must_use]
    pub const fn air_source_cooling() -> Self {
        Self {
            at_zero: 8.25,
            slope_per_k: -0.15,
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
