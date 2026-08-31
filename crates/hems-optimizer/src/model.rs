//! What the planner is asked to decide, and what it is allowed to trade off.

use hems_core::prelude::{CopCurve, Energy, Horizon, Power, Rc2, Slot, Soc, ThermalState};
use hems_forecast::Forecast;
use hems_tariff::PriceStack;

/// A battery as the planner sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BatteryModel {
    /// Usable capacity.
    pub capacity: Energy,
    /// Charge at the start of the horizon.
    pub soc_now: Soc,
    /// Maximum charging power.
    pub max_charge: Power,
    /// Maximum discharging power, as a positive magnitude.
    pub max_discharge: Power,
    /// One-way charging efficiency.
    pub efficiency_charge: f64,
    /// One-way discharging efficiency.
    pub efficiency_discharge: f64,
    /// The lowest state of charge normal operation may reach.
    pub soc_min: Soc,
    /// The highest state of charge normal operation may reach.
    pub soc_max: Soc,
    /// Held back for a power cut. The planner never plans below it, so a house
    /// that optimises hard for price still has something left when the street
    /// goes dark.
    pub reserve_soc: Soc,
    /// What one kilowatt-hour of throughput costs in battery life, €/kWh.
    ///
    /// The term most home energy managers leave out. A study of cost-only
    /// receding-horizon control found the degradation it provoked could exceed
    /// the energy savings it produced by up to an order of magnitude
    /// (`specs/arxiv/arxiv-2606.16051.pdf`); leaving it at zero reproduces that
    /// behaviour exactly.
    ///
    /// A reasonable value is the cell price divided by the warranted throughput:
    /// a €4 000 pack warranted for 2,4 MWh per kWh of capacity and 10 kWh of
    /// capacity gives about €0,08/kWh (charge and discharge together, so half
    /// that on each leg).
    pub degradation_eur_per_kwh: f64,
    /// Whether the battery may be charged from the grid at all.
    pub grid_charging_allowed: bool,
}

impl BatteryModel {
    /// The energy stored now.
    #[must_use]
    pub fn energy_now(&self) -> Energy {
        self.soc_now.energy_in(self.capacity)
    }

    /// The lowest energy the planner may leave in the battery.
    ///
    /// The larger of the operating floor and the backup reserve: a reserve is a
    /// promise to the household, not a preference.
    #[must_use]
    pub fn floor_energy(&self) -> Energy {
        self.soc_min
            .energy_in(self.capacity)
            .max(self.reserve_soc.energy_in(self.capacity))
    }

    /// The highest energy the planner may store.
    #[must_use]
    pub fn ceiling_energy(&self) -> Energy {
        self.soc_max.energy_in(self.capacity)
    }
}

/// A charging session the planner has to finish in time.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EvSession {
    /// Energy already in the car at the start of the horizon.
    pub energy_now: Energy,
    /// Energy the car should hold when it leaves.
    pub energy_target: Energy,
    /// Usable battery capacity.
    pub capacity: Energy,
    /// Maximum charging power on every conductor the charge point has.
    pub max_charge: Power,
    /// The least power at which charging happens at all — 6 A per phase, by
    /// IEC 61851. Below it a charge point is not charging slowly, it is idle.
    ///
    /// Enforced as a semi-continuous variable: in each slot the charge point is
    /// either off or somewhere between this and [`EvSession::max_charge`]. It is
    /// the one binary the model needs for a car, and leaving it out produces a
    /// plan that trickles 500 W into a wallbox for six hours and is surprised
    /// when the car is empty in the morning.
    pub min_charge: Power,
    // There is deliberately no single-phase range here, and the absence is a
    // decision rather than an omission.
    //
    // A charge point that can drop to one conductor has two operating ranges —
    // 4,14 to 11 kW on three, 1,38 to 3,7 kW on one — and modelling both needs a
    // second binary per slot plus a variable per mode to carry the different
    // efficiencies. It was built, measured, and removed: on the reference winter
    // day it was worth **one cent** and cost **four times the solve time**
    // (10 s → 40 s for a simulated day), because the extra integrality lands
    // exactly in the slots a § 14a event makes hard.
    //
    // The conductor count is the arbiter's decision. It sees the measured
    // surplus rather than a forecast, it decides in a pure function that costs
    // nothing, and it can change its mind inside a slot — see
    // `hems_realtime::phases`. What the planner loses is the ability to schedule
    // a session under a ceiling too tight for three conductors, which is a real
    // case and is on the roadmap as a reason to soften the charging deadline
    // rather than to make every solve four times slower.
    /// Charging efficiency.
    pub efficiency: f64,
    /// The first slot the car is plugged in for.
    ///
    /// `None` means it is plugged in already, which is the ordinary case: a
    /// session is created when the cable goes in. It matters when it is not —
    /// a learned arrival distribution (§ 10), a household that says "we are
    /// back at half past four", a plan made in the morning for the evening.
    /// Without it the model happily charges a car that is not there, and the
    /// resulting plan is not merely optimistic: it is a schedule the arbiter
    /// spends the afternoon failing to follow, so the energy has to be found
    /// again in whatever hours are left, which are the expensive ones.
    #[cfg_attr(feature = "serde", serde(default))]
    pub arrival: Option<Slot>,
    /// The first slot the car is **gone**.
    ///
    /// Half-open, like [`TimedLimit::until`], and that is not a presentation
    /// choice. Read the other way — "the last slot it can charge in" — a car
    /// leaving at eight is planned as though it could still be charging at
    /// 08:14, and a plan with a loose enough ceiling to defer will happily put
    /// the last quarter hour of a session into a slot the cable is out for. At
    /// 11 kW that is 2,75 kWh the car never receives, and it is invisible
    /// wherever a limit was tight enough to force the charging earlier: the
    /// reference evening lost **more** of its charge with no reduction at all
    /// than under one.
    ///
    /// The target must therefore be met by the end of the slot **before** this
    /// one, which is [`EvSession::deadline`].
    pub departure: Slot,
}

impl EvSession {
    /// Whether the car is plugged in during `slot`.
    #[must_use]
    pub fn present_in(&self, slot: Slot) -> bool {
        self.arrival.is_none_or(|a| slot >= a) && slot < self.departure
    }

    /// The last slot the car can charge in — the one before it leaves.
    #[must_use]
    pub fn deadline(&self) -> Slot {
        self.departure.prev()
    }
}

/// A heat pump, as the planner sees it.
///
/// The coefficient of performance comes from [`CopCurve`]: linear in the
/// *forecast* outdoor temperature, so it is a constant per slot and the model
/// stays a linear program.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HeatPumpModel {
    /// Electrical power at full output.
    pub max_electrical: Power,
    /// The lowest electrical power at which the unit runs at all.
    ///
    /// A modulating heat pump turns down to perhaps 30 % of its rating; below
    /// that it cycles instead, which is where compressors die.
    pub min_electrical: Power,
    /// Whether the unit modulates.
    ///
    /// A modulating unit is a linear program — fast, and exactly right. An
    /// on/off unit needs a binary per slot plus minimum-runtime constraints,
    /// which is a genuine mixed-integer problem and markedly slower on the
    /// pure-Rust solver. Most heat pumps sold in Germany today modulate.
    pub modulating: bool,
    /// How the coefficient of performance moves with the weather.
    #[cfg_attr(feature = "serde", serde(default))]
    pub cop: CopCurve,
    /// The fewest consecutive slots the unit must stay on once started.
    /// Ignored when `modulating`.
    pub min_on_slots: usize,
    /// The fewest consecutive slots it must stay off once stopped.
    pub min_off_slots: usize,
}

impl HeatPumpModel {
    /// A modulating air-source unit of `max_electrical`.
    #[must_use]
    pub fn modulating(max_electrical: Power) -> Self {
        Self {
            max_electrical,
            min_electrical: max_electrical * 0.3,
            modulating: true,
            cop: CopCurve::air_source(),
            min_on_slots: 2,
            min_off_slots: 2,
        }
    }

    /// The coefficient of performance at an outdoor temperature.
    #[must_use]
    pub fn cop(&self, outdoor_c: f64) -> f64 {
        self.cop.at(outdoor_c)
    }
}

/// The building the heat pump is heating, and the comfort it owes.
///
/// The physics lives in [`Rc2`] (`hems-core`), discretised exactly, and is
/// shared with the rule-based baseline and with the simulator that answers the
/// plan — so the three can never disagree about the house for numerical
/// reasons. What this type adds is the part that is a *preference* rather than a
/// physical fact: where the comfort band sits and what leaving it is worth.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ThermalModel {
    /// Where the two masses are now.
    pub state: ThermalState,
    /// The building.
    pub building: Rc2,
    /// The bottom of the comfort band, °C.
    pub comfort_min_c: f64,
    /// The top of the comfort band, °C.
    pub comfort_max_c: f64,
    /// What a kelvin-hour outside the comfort band is worth avoiding, €.
    ///
    /// This is the price of the trade the whole heating plan turns on. Set it
    /// too low and the house is cold whenever electricity is dear; too high and
    /// the heat pump ignores the price entirely. A euro or two per kelvin-hour
    /// puts a degree of discomfort on a par with a few kilowatt-hours.
    pub discomfort_eur_per_kelvin_hour: f64,
    /// The unit doing the heating.
    pub heat_pump: HeatPumpModel,
}

impl ThermalModel {
    /// A well-insulated single-family house at `indoor_c`.
    #[must_use]
    pub fn house(indoor_c: f64, heat_pump: HeatPumpModel) -> Self {
        Self {
            state: ThermalState::uniform(indoor_c),
            building: Rc2::house(),
            comfort_min_c: 20.0,
            comfort_max_c: 23.0,
            discomfort_eur_per_kelvin_hour: 1.5,
            heat_pump,
        }
    }

    /// The thermal energy stored in the building relative to `reference_c`, kWh.
    ///
    /// What the planner is allowed to bank when electricity is cheap.
    #[must_use]
    pub fn stored_kwh(&self, state: ThermalState, reference_c: f64) -> f64 {
        self.building.stored_kwh(state, reference_c)
    }
}

/// A hot-water tank, as the planner sees it.
///
/// A linear store, and deliberately not a second [`Rc2`]. What a household cares
/// about is whether there is hot water, not the temperature profile inside the
/// cylinder — and the S2 standard's own view of a tank is the same one: a fill
/// level, a fill rate and a leakage (`FRBC`). Modelling it as heat above the
/// lowest acceptable temperature gives exactly that, stays linear, and maps onto
/// `hems-flex` without translation.
///
/// The temperature view is a presentation detail; [`hems_core::asset::DhwTank`]
/// converts back.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DhwModel {
    /// Heat the tank holds between its lowest acceptable and its highest safe
    /// temperature.
    pub capacity: Energy,
    /// Heat stored now, above the lowest acceptable temperature.
    pub stored_now: Energy,
    /// Electrical power of the heater.
    pub heater: Power,
    /// Thermal kilowatt-hours per electrical kilowatt-hour — one for an
    /// immersion heater, around three for a hot-water heat pump.
    pub cop: f64,
    /// Standing loss.
    pub standing_loss: Power,
    /// What a kilowatt-hour of hot water the household asked for and did not get
    /// is worth avoiding, €/kWh.
    ///
    /// A cold shower is not infinitely expensive and must not be modelled as
    /// though it were: a hard constraint on the draw makes an ordinary morning
    /// infeasible whenever the tank started the day cold, and an infeasible
    /// solve is a *worse* answer than a plan that says how much it fell short.
    /// The same argument as the charging deadline, for the same reason.
    pub shortfall_eur_per_kwh: f64,
}

impl DhwModel {
    /// A three-hundred-litre tank on a hot-water heat pump, half charged.
    #[must_use]
    pub fn tank(capacity: Energy, heater: Power) -> Self {
        Self {
            capacity,
            stored_now: capacity * 0.5,
            heater,
            cop: 3.0,
            standing_loss: Power::new(45.0),
            shortfall_eur_per_kwh: 3.0,
        }
    }
}

/// What the plan should be good at, priced so the terms can be added up.
///
/// # Why this is not an enum of goals
///
/// An enum of goals — `Cost`, `Carbon`, `SelfSufficiency` — is a unit error
/// wearing a design. Battery wear, curtailment and discomfort are all in euros;
/// switching the objective to "carbon" replaces the energy price with grams of
/// CO₂ and leaves the other three alone, so the plan minimises a sum of euros
/// and grams. It behaves, because 400 g/kWh and €0,30/kWh are numbers of a
/// similar size. That is not a reason.
///
/// So every preference is expressed as **what the household is willing to pay to
/// avoid one unit of it**, and the objective stays in one currency. Setting a
/// weight to zero switches a concern off; setting it high makes it lexicographic
/// in practice without the machinery of a lexicographic solve.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Objective {
    /// What a kilogram of carbon dioxide is worth avoiding, €/kg.
    ///
    /// Zero ignores the carbon intensity of the grid. The German CO₂ price for
    /// heating and transport fuels is the obvious anchor; 55 €/t is 0,055.
    #[cfg_attr(feature = "serde", serde(default))]
    pub co2_eur_per_kg: f64,
    /// What a kilowatt-hour taken from the grid is worth avoiding *beyond* what
    /// it costs, €/kWh.
    ///
    /// The self-sufficiency dial. Zero leaves the plan purely economic; a value
    /// near the price spread makes it prefer its own roof even when importing
    /// would be marginally cheaper — which is what people who bought a battery
    /// for independence actually want, and it is honest about what it costs
    /// them.
    #[cfg_attr(feature = "serde", serde(default))]
    pub autarky_eur_per_kwh: f64,
}

impl Objective {
    /// The plain economic objective: cheapest, wear included, nothing else.
    #[must_use]
    pub fn cost() -> Self {
        Self::default()
    }

    /// Cost plus a price on carbon.
    #[must_use]
    pub fn with_carbon_price(mut self, eur_per_kg: f64) -> Self {
        self.co2_eur_per_kg = eur_per_kg;
        self
    }

    /// Cost plus a premium on every imported kilowatt-hour.
    #[must_use]
    pub fn with_autarky_premium(mut self, eur_per_kwh: f64) -> Self {
        self.autarky_eur_per_kwh = eur_per_kwh;
        self
    }
}

/// A ceiling that applies over part of the horizon.
///
/// # Why a window and not a number
///
/// A § 14a reduction has a **duration** — `[LPC-909]` lets the Energy Guard send
/// one with the limit, and the box knows when its own failsafe releases — and
/// applying today's ninety-minute reduction to a forty-eight-hour plan is a
/// different plan from the one the network operator asked for. It makes the
/// household charge its car at three in the morning under a ceiling that lapsed
/// at half past six the previous evening, and it hides the *opposite* error too:
/// once the event ends, a flat limit has no way to say that another one is
/// expected at teatime tomorrow.
///
/// The same shape is what § 24.16's grid-stress anticipation needs — the
/// operator's monthly list of control actions per postcode `[A1 8.4]` is a set
/// of windows — so the planner learns to read one now rather than later.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TimedLimit {
    /// The ceiling, as a positive magnitude.
    pub ceiling: Power,
    /// The first slot it applies in. `None` means "from the start of the
    /// horizon" — which is what a limit already in force is.
    #[cfg_attr(feature = "serde", serde(default))]
    pub from: Option<Slot>,
    /// The first slot it no longer applies in. `None` means "until further
    /// notice", which is what § 9 EEG is and what a § 14a limit sent without a
    /// duration has to be treated as.
    #[cfg_attr(feature = "serde", serde(default))]
    pub until: Option<Slot>,
}

impl TimedLimit {
    /// A ceiling that applies to the whole horizon.
    #[must_use]
    pub const fn always(ceiling: Power) -> Self {
        Self {
            ceiling,
            from: None,
            until: None,
        }
    }

    /// A ceiling in force now that lapses before `until`.
    #[must_use]
    pub const fn until(ceiling: Power, until: Slot) -> Self {
        Self {
            ceiling,
            from: None,
            until: Some(until),
        }
    }

    /// A ceiling expected between two slots — an anticipated reduction.
    #[must_use]
    pub const fn between(ceiling: Power, from: Slot, until: Slot) -> Self {
        Self {
            ceiling,
            from: Some(from),
            until: Some(until),
        }
    }

    /// Whether this limit binds in `slot`.
    #[must_use]
    pub fn applies_in(&self, slot: Slot) -> bool {
        self.from.is_none_or(|f| slot >= f) && self.until.is_none_or(|u| slot < u)
    }
}

/// The limits the plan must respect, from `hems-grid`.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanningLimits {
    /// Ceilings on the netzwirksamer Leistungsbezug of the controllable devices,
    /// each with the window it applies in. Where several overlap the tightest
    /// wins, which is the only reading that respects all of them.
    #[cfg_attr(feature = "serde", serde(default))]
    pub steuve: Vec<TimedLimit>,
    /// Ceilings on feed-in, as positive magnitudes — § 9 EEG (which does not
    /// lapse) or an LPP session (which does).
    #[cfg_attr(feature = "serde", serde(default))]
    pub feed_in: Vec<TimedLimit>,
    /// The largest import the connection allows. A fuse has no schedule.
    #[cfg_attr(feature = "serde", serde(default))]
    pub import_ceiling: Option<Power>,
    /// The ceiling **one** controllable device faces while a `steuve` limit is
    /// in force, under direct control `[A1 4.4.a]`.
    ///
    /// This is not a constraint on the plan — the plan is addressed as an energy
    /// management system `[A1 4.4.b]` and gets one number for everything behind
    /// it. It is what the **baseline** lives under. Leaving it out measures the
    /// saving against a household that ignored the network operator, which is
    /// not a lawful household and not one anybody can buy: a house with no
    /// energy manager cannot be addressed as one, so its Steuerbox turns each
    /// device down on its own and may not go below the minimum of
    /// `[A1 4.5.1]`.
    ///
    /// `None` leaves the baseline unlimited, which is right only where no
    /// § 14a limit applies at all. `hems-grid` owns the number
    /// (`para14a::MINDESTLEISTUNG`); this crate does not restate a regulation it
    /// does not depend on.
    #[cfg_attr(feature = "serde", serde(default))]
    pub direct_control_ceiling: Option<Power>,
}

impl PlanningLimits {
    /// Add a § 14a ceiling.
    #[must_use]
    pub fn with_steuve(mut self, limit: TimedLimit) -> Self {
        self.steuve.push(limit);
        self
    }

    /// Add a ceiling on feed-in.
    #[must_use]
    pub fn with_feed_in(mut self, limit: TimedLimit) -> Self {
        self.feed_in.push(limit);
        self
    }

    /// Set the connection's import ceiling.
    #[must_use]
    pub const fn with_import_ceiling(mut self, ceiling: Power) -> Self {
        self.import_ceiling = Some(ceiling);
        self
    }

    /// Set the per-device ceiling the baseline household faces under direct
    /// control `[A1 4.4.a]`.
    #[must_use]
    pub const fn with_direct_control_ceiling(mut self, ceiling: Power) -> Self {
        self.direct_control_ceiling = Some(ceiling);
        self
    }

    /// The tightest § 14a ceiling binding in `slot`, if any.
    #[must_use]
    pub fn steuve_at(&self, slot: Slot) -> Option<Power> {
        tightest(&self.steuve, slot)
    }

    /// The tightest feed-in ceiling binding in `slot`, if any.
    #[must_use]
    pub fn feed_in_at(&self, slot: Slot) -> Option<Power> {
        tightest(&self.feed_in, slot)
    }
}

fn tightest(limits: &[TimedLimit], slot: Slot) -> Option<Power> {
    limits
        .iter()
        .filter(|l| l.applies_in(slot))
        .map(|l| l.ceiling)
        .reduce(Power::min)
}

/// Everything the planner needs.
#[derive(Debug, Clone)]
pub struct Problem<'a> {
    /// The slots to plan over.
    pub horizon: Horizon,
    /// What energy costs and earns in each of them.
    pub prices: &'a PriceStack,
    /// Expected production, as positive magnitudes in watts.
    pub pv: &'a Forecast,
    /// Expected uncontrollable household load, watts.
    pub load: &'a Forecast,
    /// The stationary battery, if there is one.
    pub battery: Option<BatteryModel>,
    /// The car, if one is plugged in.
    pub ev: Option<EvSession>,
    /// The building and its heat pump, if there is one to plan for.
    pub thermal: Option<ThermalModel>,
    /// The hot-water tank, if there is one.
    pub dhw: Option<DhwModel>,
    /// Hot water drawn in each slot, in watt-hours of heat.
    ///
    /// A forecast like any other — the morning shower and the evening washing-up
    /// are the most predictable events in a household's day. A short slice is
    /// read as "no draw after this", which is the reading that stops a missing
    /// forecast from inventing demand.
    pub dhw_draw: &'a [f64],
    /// Outdoor temperature in each slot, °C.
    ///
    /// Drives the heat loss and the coefficient of performance. A short slice is
    /// extended with its last value rather than refused: a forecast that runs
    /// out is a reason to plan less well, not to stop planning.
    pub outdoor_c: &'a [f64],
    /// The grid's limits.
    pub limits: PlanningLimits,
    /// What the plan is willing to pay to avoid things other than money.
    pub objective: Objective,
    /// What failing to deliver a kilowatt-hour the car was promised is worth
    /// avoiding, €/kWh.
    ///
    /// A **hard** deadline returns no plan at all when the target cannot be
    /// met, which leaves the arbiter on self-consumption and may charge the car
    /// less than the best achievable schedule would have. "I could not do all
    /// of it" is a better answer than "I could not do any of it".
    ///
    /// The default is far above any electricity price, so the deadline is
    /// lexicographic in practice: the plan gives up a kilowatt-hour of charge
    /// only when there is genuinely no way to deliver it. Setting it lower makes
    /// the target a preference the plan may trade; setting it to zero makes it
    /// no target at all.
    pub unmet_charge_eur_per_kwh: f64,
    /// What curtailing a kilowatt-hour of production is worth avoiding, €/kWh.
    ///
    /// Not zero even when feed-in earns nothing: throwing away energy that could
    /// have heated water is a real loss, and a plan that is indifferent to it
    /// will curtail whenever that is a rounding error cheaper.
    pub curtailment_penalty_eur_per_kwh: f64,
    /// Which quantile of the forecasts to plan against.
    ///
    /// The median is the honest central case. Planning production against the
    /// pessimistic quantile and load against the optimistic one produces a plan
    /// that holds up when the weather disappoints — worth doing when the battery
    /// is small relative to the array.
    pub pv_quantile: Quantile,
    /// Which quantile of the load forecast to plan against.
    pub load_quantile: Quantile,
    /// What a kilowatt-hour still in the battery at the end of the horizon is
    /// worth, as a multiple of the mean import price over the horizon.
    ///
    /// Without this the plan empties the battery into the last few slots —
    /// stored energy has no value after the horizon ends, so selling it at any
    /// price beats keeping it. That is an artefact of where the horizon happens
    /// to stop, not a decision anybody wants, and in a receding-horizon
    /// controller it repeats every five minutes.
    ///
    /// `1.0` values the remaining charge at what it would cost to buy back.
    /// Below 1 makes the plan slightly keener to sell, above 1 keener to hold.
    pub terminal_value_factor: f64,
    /// How close to optimal is close enough, as a fraction of the objective.
    ///
    /// The model has binaries — a charge point below 6 A is idle, an on/off heat
    /// pump is on or off — so proving optimality can cost far more time than the
    /// last fraction of a percent is worth. A household plan is re-made every
    /// five minutes against forecasts that are wrong by more than this; spending
    /// a minute to close a 0,2 % gap is spending it on nothing.
    ///
    /// Honoured by the HiGHS backend. `microlp` has no equivalent knob and
    /// always solves to optimality.
    pub mip_gap: f64,
    /// Whether to compute the per-asset shadow prices ([`crate::shadow`]).
    ///
    /// On by default, because the guard's allocation weights are the reason they
    /// exist and a plan without them hands every device the same one. It costs a
    /// second solve of the same model with the binaries pinned — a pure linear
    /// program, so on the reference days it is roughly a third of the first
    /// solve — and it is worth turning off only for a caller that computes flows
    /// and commands nothing.
    pub shadow_prices: bool,
    /// The longest the solver may take, in seconds. Zero or infinite means no
    /// limit.
    ///
    /// When it runs out the best plan found so far is used. A late plan is worse
    /// than a slightly suboptimal one: the arbiter falls back to self-consumption
    /// while it waits.
    ///
    /// # It costs reproducibility, and that is not free
    ///
    /// A **wall-clock** budget makes the answer depend on how busy the machine
    /// was. Two runs of the same day on the same inputs can then differ, which
    /// is exactly what "replay the day and compare" needs not to happen — and
    /// which is how this was found: the determinism test passed alone and failed
    /// under a parallel test run.
    ///
    /// So a box in the field keeps the budget, because a plan that arrives late
    /// is worse than one that is half a per cent off; and anything that has to be
    /// *reproducible* — a replay, a regression suite, a saving figure a customer
    /// can check — sets it to zero and waits. The relative gap of
    /// [`Problem::mip_gap`] is not affected: it is a property of the search, not
    /// of the clock.
    pub solve_budget_s: f64,
}

/// Which quantile of a forecast to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Quantile {
    /// The pessimistic end for production, the optimistic end for load.
    P10,
    /// The central case.
    #[default]
    P50,
    /// The optimistic end for production, the pessimistic end for load.
    P90,
}

impl Quantile {
    /// Pick this quantile out of a band.
    #[must_use]
    pub fn of(self, band: hems_forecast::Band) -> f64 {
        match self {
            Quantile::P10 => band.p10,
            Quantile::P50 => band.p50,
            Quantile::P90 => band.p90,
        }
    }
}

impl<'a> Problem<'a> {
    /// A cost-minimising problem with sensible penalties.
    #[must_use]
    pub fn new(
        horizon: Horizon,
        prices: &'a PriceStack,
        pv: &'a Forecast,
        load: &'a Forecast,
    ) -> Self {
        Self {
            horizon,
            prices,
            pv,
            load,
            battery: None,
            ev: None,
            thermal: None,
            dhw: None,
            dhw_draw: &[],
            outdoor_c: &[],
            limits: PlanningLimits::default(),
            objective: Objective::cost(),
            curtailment_penalty_eur_per_kwh: 0.01,
            unmet_charge_eur_per_kwh: 5.0,
            pv_quantile: Quantile::P50,
            load_quantile: Quantile::P50,
            terminal_value_factor: 1.0,
            mip_gap: 0.005,
            shadow_prices: true,
            solve_budget_s: 10.0,
        }
    }

    /// Add a battery.
    #[must_use]
    pub fn with_battery(mut self, battery: BatteryModel) -> Self {
        self.battery = Some(battery);
        self
    }

    /// Add a charging session.
    #[must_use]
    pub fn with_ev(mut self, ev: EvSession) -> Self {
        self.ev = Some(ev);
        self
    }

    /// Add a building and its heat pump, with the weather it faces.
    #[must_use]
    pub fn with_thermal(mut self, thermal: ThermalModel, outdoor_c: &'a [f64]) -> Self {
        self.thermal = Some(thermal);
        self.outdoor_c = outdoor_c;
        self
    }

    /// Add a hot-water tank and the heat the household will draw from it.
    #[must_use]
    pub fn with_dhw(mut self, dhw: DhwModel, draw_wh: &'a [f64]) -> Self {
        self.dhw = Some(dhw);
        self.dhw_draw = draw_wh;
        self
    }

    /// Heat drawn from the tank in slot `k`, watt-hours.
    #[must_use]
    pub fn dhw_draw_at(&self, k: usize) -> f64 {
        self.dhw_draw.get(k).copied().unwrap_or(0.0).max(0.0)
    }

    /// Set the grid's limits.
    #[must_use]
    pub fn with_limits(mut self, limits: PlanningLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The exact discrete-time building model for one quarter-hour slot.
    ///
    /// Computed once per solve and shared by the constraints, the baseline and
    /// anything else that has to step the house, so the plan and the comparison
    /// it is judged against are heated by the same building.
    #[must_use]
    pub fn thermal_step(&self) -> hems_core::prelude::Rc2Discrete {
        self.thermal
            .map_or(hems_core::prelude::Rc2Discrete::HOLD, |t| {
                t.building.discretise(hems_core::prelude::SLOT)
            })
    }

    /// The outdoor temperature in slot `k`, °C.
    ///
    /// Falls back to the last value given, then to 10 °C — mild enough that a
    /// plan made without weather is neither wildly optimistic about the
    /// coefficient of performance nor panicked about the heat loss.
    #[must_use]
    pub fn outdoor_at(&self, k: usize) -> f64 {
        self.outdoor_c
            .get(k)
            .or_else(|| self.outdoor_c.last())
            .copied()
            .unwrap_or(10.0)
    }

    /// The forecast values for slot `k`, in watts: production, then load.
    #[must_use]
    pub fn forecasts_at(&self, k: usize) -> (f64, f64) {
        let slot = self.horizon.get(k);
        let pv = slot
            .and_then(|s| self.pv.at(s))
            .map_or(0.0, |b| self.pv_quantile.of(b).max(0.0));
        let load = slot
            .and_then(|s| self.load.at(s))
            .map_or(0.0, |b| self.load_quantile.of(b).max(0.0));
        (pv, load)
    }
}
