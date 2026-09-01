//! What the planner is asked to decide, and what it is allowed to trade off.

use hems_core::prelude::{
    CompressorState, CopCurve, Energy, Horizon, Power, Programme, Rc2, Slot, Soc, ThermalState,
};
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

    /// How much of the session's own capacity the promise uses up, in `[0, 1]`.
    ///
    /// The energy still owed, over the energy the charge point could deliver if
    /// it ran flat out for every quarter hour the cable is in. Zero is a car
    /// that is already full; one is a session with no slack at all.
    ///
    /// # What it is for
    ///
    /// It is the cheap answer to "is this household at risk today", and the
    /// reason it exists is a measurement. Planning against three futures instead
    /// of one median is worth €0,35 a day on the evening a car arrives *as* a
    /// § 14a reduction starts — and it removes the charge the median plan leaves
    /// undelivered — while costing €0,95 a day and seven times the solve on an
    /// ordinary winter evening where nothing is at stake. Something has to
    /// decide which day it is.
    ///
    /// The obvious candidate — ask the plan whether it expects a shortfall —
    /// **does not work**, and the way it fails is worth stating: a plan that has
    /// looked only at the median cannot know it is at risk. On the reference
    /// evening it predicts it will make the target, the weather disappoints, and
    /// the trigger never fires. Asking it is asking the wrong oracle. A property
    /// of the *session*, which needs no solve at all, is not blind in that way.
    #[must_use]
    pub fn tightness(&self, now: Slot) -> f64 {
        let owed = (self.energy_target - self.energy_now)
            .max(Energy::ZERO)
            .get();
        if owed <= 0.0 {
            return 0.0;
        }
        let from = self.arrival.unwrap_or(now).max(now);
        let slots = from.distance_to(self.departure).max(0) as f64;
        let deliverable = self.max_charge.get() * self.efficiency.clamp(0.0, 1.0) * slots * 0.25;
        if deliverable <= 0.0 {
            return 1.0;
        }
        (owed / deliverable).clamp(0.0, 1.0)
    }
}

/// An appliance that runs a fixed programme, placed in a window.
///
/// The washing machine, the dishwasher, the tumble dryer — S2 calls the pattern
/// `PPBC`, and it is the one piece of household flexibility a household can
/// actually see happening. It is also the only flexible thing in the model that
/// is **atomic**: a battery can be charged a little, a heat pump turned down a
/// little, and a dishwasher cannot be run a little. Either the whole programme
/// goes in somewhere, or it does not go in.
///
/// # Why this is a start time and not a power
///
/// The obvious model — a load that may be moved between slots — is wrong in the
/// way that matters. A dishwasher's programme is *shaped*: two kilowatts while
/// it heats, two hundred watts while it washes, two kilowatts again to dry. A
/// planner allowed to smear that over six hours will schedule 400 W of
/// dishwasher into every sunny slot, which no dishwasher will do, and the
/// household's day arrives with the machine still full. So the decision is a
/// single binary per feasible start, the programme follows it exactly, and what
/// the model reports is a schedule the appliance can carry out.
///
/// One binary per **feasible** start, not per slot: a two-hour programme that
/// must finish by six leaves a few dozen, which costs the solver nothing next to
/// the charge point's own integrality.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ShiftableRun {
    /// The shape it draws once started.
    pub programme: Programme,
    /// The first slot the household will let it start in.
    ///
    /// `None` means "as soon as the horizon does" — which is what a machine
    /// loaded and switched to `Auto` is.
    #[cfg_attr(feature = "serde", serde(default))]
    pub earliest: Option<Slot>,
    /// The first slot it must already be **finished** in — half-open, like
    /// [`EvSession::departure`] and for the same reason.
    ///
    /// `None` means the end of the horizon, which for a two-day horizon is a
    /// household saying "sometime". Anybody who means "before we get up" says
    /// so here; the plan is otherwise entitled to find the cheapest two hours in
    /// forty-eight and it will.
    #[cfg_attr(feature = "serde", serde(default))]
    pub deadline: Option<Slot>,
    /// What leaving the programme unrun is worth avoiding, €.
    ///
    /// **Soft, like every other deadline in this model.** A window too tight for
    /// the programme, or a household that asked for a wash between two and three
    /// on a day with a § 14a reduction across it, must produce a plan that says
    /// "not this one" rather than no plan at all. Set it well above what the run
    /// costs in electricity and it is lexicographic in practice; set it to zero
    /// and the machine will simply never run, which is a legitimate way to say
    /// "only if it is free".
    pub unserved_eur: f64,
}

impl ShiftableRun {
    /// A run of `programme` that must be finished before `deadline`.
    #[must_use]
    pub fn before(programme: Programme, deadline: Slot) -> Self {
        Self {
            programme,
            earliest: None,
            deadline: Some(deadline),
            unserved_eur: 2.0,
        }
    }

    /// Not before `earliest`.
    #[must_use]
    pub fn not_before(mut self, earliest: Slot) -> Self {
        self.earliest = Some(earliest);
        self
    }

    /// What leaving it unrun is worth avoiding.
    #[must_use]
    pub fn worth(mut self, eur: f64) -> Self {
        self.unserved_eur = eur;
        self
    }

    /// Whether a run started in slot index `k` of `horizon` fits the window.
    ///
    /// Three conditions, and the third is the one an implementation forgets: the
    /// programme has to finish **inside the horizon**, or the plan commits to
    /// slots it cannot see and reports an energy it never accounted for.
    #[must_use]
    pub fn can_start_at(&self, horizon: Horizon, k: usize) -> bool {
        let len = self.programme.slots();
        if len == 0 || k + len > horizon.len {
            return false;
        }
        let Some(start) = horizon.get(k) else {
            return false;
        };
        if self.earliest.is_some_and(|e| start < e) {
            return false;
        }
        // The last slot the programme occupies is `k + len − 1`; it must end
        // before the deadline slot begins.
        self.deadline
            .is_none_or(|d| horizon.get(k + len - 1).is_some_and(|last| last < d))
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
    /// # It bounds an on/off unit and not a modulating one, and that is physics
    ///
    /// A compressor that cannot go below 30 % of its rating still delivers a
    /// *quarter-hour average* below that, by running for four minutes of the
    /// fifteen — which is exactly what an on/off unit does all winter. So at the
    /// resolution this model works in, a modulation floor is not a constraint on
    /// the slot average, and imposing one would forbid a plan the hardware can
    /// carry out.
    ///
    /// What the floor does constrain is a unit whose *cycling* is being
    /// scheduled, and that is [`HeatPumpModel::modulating`] `== false`: there the
    /// binary says the unit is on for the whole slot, so the floor applies to it
    /// and [`HeatPumpModel::min_on_slots`] prices the cycling. Reading this field
    /// for a modulating unit would be modelling a machine nobody sells.
    ///
    /// The cost of a modulating unit spending a slot below its floor is
    /// therefore compressor starts rather than infeasibility, and it is priced
    /// nowhere — an honest gap, and a small one: a modulating unit under an MPC
    /// spends most of the day at a steady output, which is the reason to buy
    /// one.
    pub min_electrical: Power,
    /// Whether the unit modulates.
    ///
    /// A modulating unit is a linear program — fast, and exactly right, because
    /// the quantity the model decides is a quarter-hour average and a
    /// modulating unit can deliver any average up to its rating. An on/off unit
    /// needs a binary per slot plus minimum-runtime constraints, which is a
    /// genuine mixed-integer problem and markedly slower on the pure-Rust
    /// solver. Most heat pumps sold in Germany today modulate.
    ///
    /// It also decides whether [`HeatPumpModel::min_electrical`] binds anything;
    /// see the note there.
    pub modulating: bool,
    /// How the coefficient of performance moves with the weather.
    #[cfg_attr(feature = "serde", serde(default))]
    pub cop: CopCurve,
    /// The fewest consecutive slots the unit must stay on once started.
    /// Ignored when `modulating`.
    pub min_on_slots: usize,
    /// The fewest consecutive slots it must stay off once stopped.
    pub min_off_slots: usize,
    /// What the compressor is doing as the horizon opens.
    ///
    /// Ignored when `modulating`, and the whole of what makes
    /// [`HeatPumpModel::min_on_slots`] mean anything on a **receding** horizon.
    /// See [`CompressorState`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub compressor: CompressorState,
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
            compressor: CompressorState::default(),
        }
    }

    /// An on/off air-source unit of `max_electrical`, idle.
    ///
    /// The unit a minimum runtime is *for*: it has one output and the plan
    /// decides when it runs, so cycling is a decision the planner makes rather
    /// than one the unit's own controller hides.
    ///
    /// The binary commits the **slot**, and
    /// [`HeatPumpModel::min_electrical`] is the lowest average the unit can hold
    /// across one by short-cycling inside it — which is what a single-speed
    /// compressor's own controller does and why the floor is a third of the
    /// rating rather than all of it. Pinning the floor *to* the rating was
    /// tried: it is arguably more literal, and it turns a comfort band into a
    /// packing problem in five-kilowatt pulses that did not solve a 48-slot day
    /// in twelve minutes. The unit hardware actually ships is the one modelled
    /// here.
    #[must_use]
    pub fn on_off(max_electrical: Power) -> Self {
        Self {
            modulating: false,
            ..Self::modulating(max_electrical)
        }
    }

    /// The same unit, with the compressor in the state `compressor` describes.
    #[must_use]
    pub const fn with_compressor(mut self, compressor: CompressorState) -> Self {
        self.compressor = compressor;
        self
    }

    /// The coefficient of performance at an outdoor temperature.
    #[must_use]
    pub fn cop(&self, outdoor_c: f64) -> f64 {
        self.cop.at(outdoor_c)
    }

    /// How many slots at the start of the horizon the compressor's own history
    /// has already decided, and what it decided.
    ///
    /// `None` for a modulating unit and for one that has been in its current
    /// state long enough to be free.
    #[must_use]
    pub fn committed(&self) -> Option<(bool, usize)> {
        if self.modulating {
            return None;
        }
        let required = if self.compressor.running {
            self.min_on_slots
        } else {
            self.min_off_slots
        };
        let left = required.saturating_sub(self.compressor.slots_in_state);
        (left > 0).then_some((self.compressor.running, left))
    }
}

/// What a plan committed its discrete decisions to, ready to start the next one.
///
/// # Why a receding horizon should hand one to itself
///
/// A box re-plans every quarter of an hour over a horizon of a day. Consecutive
/// plans therefore differ by **one slot of new information** and agree about
/// almost everything else — and a branch-and-bound solver, handed the second
/// problem cold, rediscovers the whole schedule from nothing every time.
///
/// A mixed-integer solver given a feasible incumbent can prune against its
/// objective from the first node instead. The previous plan, shifted forward by
/// the slots that have elapsed, is exactly such an incumbent and costs nothing
/// to produce: the plan was solved anyway.
///
/// Only the **discrete** decisions are carried. The continuous ones are cheap
/// for a simplex to recover once the integers are decided, and a stale
/// continuous value is a worse starting point than none — the weather has moved.
///
/// # It cannot make a plan wrong
///
/// An initial solution is a *hint*. It is checked by the solver and discarded if
/// it is not feasible, so a commitment that has gone stale — the car left early,
/// the network operator sent a limit — costs one feasibility check and nothing
/// else. Nothing here relaxes a constraint or changes an objective, which is why
/// a warm-started plan and a cold one are the same plan.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Commitment {
    /// Whether the charge point was running, one entry per slot.
    pub ev_on: Vec<f64>,
    /// Whether an on/off heat pump was running.
    pub hp_on: Vec<f64>,
    /// Where each appliance's programme was placed, indexed `[i][k]`.
    pub shiftable: Vec<Vec<f64>>,
}

impl Commitment {
    /// The same commitment read `by` slots later — what the next plan inherits.
    ///
    /// The decisions that have already happened drop off the front and the tail
    /// is padded with zeros, which is the honest answer for slots the previous
    /// horizon never reached: "no reason to think anything runs here", not a
    /// guess. A solver improves on a zero tail in the first few nodes; it would
    /// have to *undo* an invented one.
    #[must_use]
    pub fn shifted(&self, by: usize) -> Self {
        fn slide(v: &[f64], by: usize) -> Vec<f64> {
            let mut out: Vec<f64> = v.iter().skip(by).copied().collect();
            out.resize(v.len(), 0.0);
            out
        }
        Self {
            ev_on: slide(&self.ev_on, by),
            hp_on: slide(&self.hp_on, by),
            shiftable: self.shiftable.iter().map(|s| slide(s, by)).collect(),
        }
    }

    /// Whether there is anything here to start from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ev_on.is_empty() && self.hp_on.is_empty() && self.shiftable.is_empty()
    }

    /// The hint for one slot of a vector, if this commitment reaches that far.
    pub(crate) fn hint(values: &[f64], k: usize) -> Option<f64> {
        values.get(k).copied().filter(|v| v.is_finite())
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
    /// Appliances waiting to run a programme, in the order
    /// [`crate::solve::AssetNames::shiftable`] names them.
    ///
    /// Empty on most solves. Each one is a handful of binaries, and they are the
    /// cheapest flexibility a household owns after the tank: nothing is stored,
    /// nothing degrades, and the only cost of moving one is that somebody has to
    /// unload it later.
    pub shiftable: Vec<ShiftableRun>,
    /// Hot water drawn in each slot, in watt-hours of heat.
    ///
    /// A forecast like any other — the morning shower and the evening washing-up
    /// are the most predictable events in a household's day. A short slice is
    /// read as "no draw after this", which is the reading that stops a missing
    /// forecast from inventing demand.
    pub dhw_draw: &'a [f64],
    /// How much of the community's generation this member may be allocated in
    /// each slot, in watts — the § 42c share.
    ///
    /// The community's own generation times this member's Aufteilungsschlüssel
    /// (§ 42c Abs. 3 Nr. 2), which `hems_grid::sharing` settles exactly after the
    /// fact and which is a **forecast** here like any other. A short slice is
    /// read as "nothing to share after this", which is the reading that stops a
    /// missing forecast from inventing a neighbour's roof.
    ///
    /// The allocation is capped at what the member actually draws, so it can only
    /// ever discount **grid import** — the electricity reaches it over the public
    /// grid, and a kilowatt-hour the household made on its own roof was never
    /// allocated to it. What the planner does with that is the whole behavioural
    /// point of joining a community: move the flexible load into the quarter
    /// hours where the share is, because that is where a kilowatt-hour is
    /// cheaper.
    ///
    /// The price of an allocated kilowatt-hour is a *price* and lives in the
    /// tariff ([`hems_tariff::SlotPrice::shared_import_ct`]).
    pub community_share: &'a [f64],
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
    /// How the plan is asked to treat the fact that the forecast is wrong.
    ///
    /// [`Risk::deterministic`] by default — one future, the median of both
    /// forecasts. Not because it is the best plan: `hemsd risk` measures three
    /// futures as worth about €0,35 a day where a service is at risk and as
    /// costing about €0,95 a day where none is, and three futures cost seven
    /// times the solve. That is a trade a household's box should make
    /// deliberately rather than inherit. See [`Risk`].
    pub risk: Risk,
    /// What a kilowatt-hour still in the battery at the end of the horizon is
    /// worth, as a multiple of the mean import price over the horizon.
    ///
    /// Without this the plan empties the battery into the last few slots —
    /// stored energy has no value after the horizon ends, so selling it at any
    /// price beats keeping it. That is an artefact of where the horizon happens
    /// to stop, not a decision anybody wants, and in a receding-horizon
    /// controller it repeats at every re-plan.
    ///
    /// `1.0` values the remaining charge at what it would cost to buy back.
    /// Below 1 makes the plan slightly keener to sell, above 1 keener to hold.
    pub terminal_value_factor: f64,
    /// How close to optimal is close enough, as a fraction of the objective.
    ///
    /// The model has binaries — a charge point below 6 A is idle, an on/off heat
    /// pump is on or off — so proving optimality can cost far more time than the
    /// last fraction of a percent is worth. A household plan is re-made every
    /// quarter of an hour against forecasts that are wrong by more than this;
    /// spending a minute to close a 0,2 % gap is spending it on nothing.
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
    /// The previous plan's discrete decisions, shifted onto this horizon.
    ///
    /// A **hint**, never a constraint: the solver checks it, uses it as an
    /// incumbent if it is feasible, and discards it if it is not. See
    /// [`Commitment`] for why a receding-horizon controller should always have
    /// one, and [`Problem::with_warm_start`] for how it is set.
    ///
    /// Honoured by the HiGHS and `microlp` backends. The dual pass runs on
    /// Clarabel with the binaries already pinned, so it neither needs nor reads
    /// one.
    pub warm_start: Option<&'a Commitment>,
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
    /// How finely the compressor's on/off decision is made across the horizon.
    ///
    /// Only reads for a **non-modulating** heat pump; a modulating one has no
    /// binary to coarsen. See [`CommitmentHorizon`].
    pub commitment_horizon: CommitmentHorizon,
}

/// How far ahead a compressor is committed slot by slot, and how coarsely after
/// that.
///
/// # The problem it solves
///
/// A single-speed heat pump is one binary per slot, and a two-day horizon at
/// quarter-hour resolution is ninety-six of them — in every one of ninety-six
/// re-plans. That is a genuine mixed-integer problem and it took **ten minutes
/// and fifty-two seconds** of solver time to plan one simulated day, against
/// nine seconds for the same day on a modulating unit. Three numerical
/// approaches were measured and none of them closed the gap: the tighter
/// Rajan–Takriti rows (D66), the warm start (D71) and pinning the slots the
/// compressor's own history has already decided (D65).
///
/// # Why coarsening the tail is exact where it matters
///
/// A receding-horizon controller executes the **first** slot of a plan and
/// throws the rest away. The tail exists to price the consequences of the first
/// slot — the house it leaves behind, the store it leaves full — and a
/// consequence measured to the hour is the same consequence measured to the
/// quarter hour, because the building's slow mass has a time constant of days
/// and its fast one of thirteen minutes. What the tail does *not* need is the
/// power to say which quarter of an hour, thirty hours from now, a compressor
/// starts in: that decision will be re-made a hundred and twenty times before it
/// is executed.
///
/// So the decision is made slot by slot over [`CommitmentHorizon::fine_slots`]
/// and per block of [`CommitmentHorizon::block_slots`] after that — the same
/// construction utility-scale unit commitment uses, where the day ahead is
/// hourly and the week after it is not. The **continuous** power stays per slot
/// throughout, so a blocked hour is still free to modulate between the unit's
/// floor and its rating; what a block fixes is only whether the compressor is
/// running at all.
///
/// The head stays fine, which is where every property that has to hold holds:
/// the minimum runtime across the re-plan boundary (D65), the § 14a ceiling that
/// arrives at teatime, the comfort band this afternoon.
///
/// # It cannot make a plan unlawful
///
/// Blocking only ever *removes* schedules from the feasible set — it is a
/// restriction, never a relaxation — so no constraint the model states can be
/// escaped by coarsening. The cost is optimality in the tail, and it is
/// measured on the reference day rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CommitmentHorizon {
    /// How many slots at the head of the horizon are decided one by one.
    ///
    /// Must cover everything the head has to be able to say: the slots the
    /// compressor's own history has already committed, and enough of the
    /// afternoon for a minimum runtime to be scheduled rather than inherited.
    /// [`CommitmentHorizon::fine_for`] raises it where a unit's own minimum
    /// needs more.
    pub fine_slots: usize,
    /// How many slots share one decision after that. `1` is no blocking at all:
    /// every slot is its own decision, which is [`CommitmentHorizon::fine`].
    pub block_slots: usize,
}

impl Default for CommitmentHorizon {
    /// Two hours slot by slot, then one decision per hour.
    ///
    /// Eight fine slots is longer than any household compressor's minimum
    /// runtime and longer than the twenty minutes a plan survives
    /// (`ArbiterConfig::max_plan_age`), so every slot that will actually be
    /// executed under this plan is a fine one. Four-slot blocks are the hour the
    /// day-ahead auction is still quoted in, and they are at least as long as
    /// the minimum on- and off-times, so a block can never ask a compressor for
    /// a run it is not allowed to make.
    fn default() -> Self {
        Self {
            fine_slots: 8,
            block_slots: 4,
        }
    }
}

impl CommitmentHorizon {
    /// Every slot decided on its own — the model as it was before blocking
    /// existed, and what a reproducibility run or a benchmark asks for.
    #[must_use]
    pub const fn fine() -> Self {
        Self {
            fine_slots: usize::MAX,
            block_slots: 1,
        }
    }

    /// The fine head this configuration needs for `unit`.
    ///
    /// A compressor whose minimum runtime is longer than the configured head
    /// gets a head long enough to hold it, so the slots
    /// [`HeatPumpModel::committed`] pins are never inside a block — pinning a
    /// block start would pin the whole block, which would be a plan constrained
    /// by an accident of arithmetic.
    #[must_use]
    pub fn fine_for(&self, unit: &HeatPumpModel) -> usize {
        self.fine_slots
            .max(unit.min_on_slots)
            .max(unit.min_off_slots)
            .max(unit.committed().map_or(0, |(_, left)| left))
    }

    /// Which slot each slot of `horizon` shares its compressor decision with.
    ///
    /// The identity over the fine head, and the start of each block after it —
    /// so a caller declares one variable where `blocks[k] == k` and reuses that
    /// variable everywhere else.
    ///
    /// # The blocks are aligned to the clock, not to the plan
    ///
    /// A block that started wherever the fine head happened to end would slide
    /// by one slot at every re-plan, and would therefore straddle the hour the
    /// tariff steps at three times out of four. Anchoring them to the local
    /// quarter-hour index instead puts every boundary on the boundary the price
    /// already has, which is where the decision the block is coarsening actually
    /// lives: on the reference days the day-ahead curve is constant within the
    /// hour, so an hourly block throws nothing away at all.
    ///
    /// It also makes consecutive plans agree about *where* the blocks are, which
    /// is what lets the previous plan's commitment warm-start the next one
    /// ([`Commitment`]) instead of being a hint about a partition that has
    /// moved.
    #[must_use]
    pub fn blocks(&self, horizon: Horizon, unit: &HeatPumpModel) -> Vec<usize> {
        let mut out: Vec<usize> = (0..horizon.len).collect();
        let block = self.block_slots.max(1);
        if block == 1 || unit.modulating {
            return out;
        }
        let fine = self.fine_for(unit);
        let mut start = fine;
        for (k, representative) in out.iter_mut().enumerate().skip(fine) {
            if horizon
                .get(k)
                .is_some_and(|s| (s.index_in_local_day() as usize).is_multiple_of(block))
            {
                start = k;
            }
            *representative = start;
        }
        out
    }
}

/// How the plan is asked to treat the fact that the forecast is wrong.
///
/// # Why a scenario set and not a quantile
///
/// Planning against one quantile is a *robustness knob*: it says "assume the
/// weather disappoints and act on that", and the plan it produces is optimal
/// against a world nobody expects. The household then pays the hedge on every
/// ordinary day and gets no credit for it on the bad one, because the model
/// never priced the bad one — it only ever saw a single, pessimistic future.
///
/// A **scenario set** says the honest thing instead: here are three futures and
/// what each is worth. The plan minimises a weighted sum of the mean and the
/// **tail**, so the hedge is bought exactly where the tail is expensive — a car
/// that will leave short, a tank that will be cold at seven — and not where it
/// is merely inconvenient.
///
/// # What is decided once and what is decided later
///
/// Only the **first slot's controllable decisions** are shared across scenarios
/// (non-anticipativity): they are what the arbiter is about to commit, and a plan
/// that gave three different answers for the next fifteen minutes would not be a
/// plan. Everything after it is *recourse* — the plan is allowed to say "and if
/// the afternoon is dull I do this instead", which is what makes the hedge cheap.
///
/// The **discrete** commitments are shared over the whole horizon: whether the
/// charge point runs at all in a slot, whether an on/off heat pump is on, when
/// the dishwasher starts. Those are things a household does once, and letting
/// them differ per scenario would triple the integrality — the expensive part —
/// to buy a schedule nobody can carry out.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Risk {
    /// Which futures the plan is asked to survive.
    pub scenarios: ScenarioSet,
    /// The confidence level of the conditional value at risk, in `(0, 1)`.
    ///
    /// `CVaR_α` is the mean cost of the worst `1 − α` of outcomes. With
    /// [`ScenarioSet::Swanson`]'s three futures and their 0,3 / 0,4 / 0,3
    /// weights, `α = 0,7` makes the tail exactly the pessimistic scenario, which
    /// is the reading a household would give it and the reason it is the
    /// default.
    pub cvar_alpha: f64,
    /// How much of the objective is the tail rather than the mean, in `[0, 1]`.
    ///
    /// Zero is expected cost — the risk-neutral plan, and still better than a
    /// single quantile because it prices all three futures. One is
    /// worst-case-only, which buys a hedge on every sunny day. A third is a
    /// household that would rather not be caught out.
    pub cvar_weight: f64,
}

impl Default for Risk {
    /// **The median, and nothing else.**
    ///
    /// Not because it is the best plan — it is not, and `hemsd risk` measures by
    /// how much — but because it is the plan a caller who has said nothing about
    /// uncertainty is entitled to: the cheapest to solve, and the one every
    /// figure in this workspace is calibrated against. Three futures cost
    /// **seven times the solve** on the reference winter day, which is a
    /// decision a household's box should make deliberately rather than inherit.
    fn default() -> Self {
        Self::deterministic()
    }
}

impl Risk {
    /// One scenario: the median of both forecasts, priced as though it were
    /// certain. What every deterministic MPC does, and the comparison the
    /// scenario plan has to beat.
    #[must_use]
    pub const fn deterministic() -> Self {
        Self {
            scenarios: ScenarioSet::Median,
            cvar_alpha: 0.7,
            cvar_weight: 0.0,
        }
    }

    /// Three futures, a third of the objective on the tail.
    #[must_use]
    pub const fn hedged() -> Self {
        Self {
            scenarios: ScenarioSet::Swanson,
            cvar_alpha: 0.7,
            cvar_weight: 1.0 / 3.0,
        }
    }

    /// One scenario at a chosen quantile of each forecast — the old robustness
    /// knob, kept because a household with a battery small against its array
    /// sometimes wants exactly that and nothing more expensive.
    #[must_use]
    pub const fn at_quantile(pv: Quantile, load: Quantile) -> Self {
        Self {
            scenarios: ScenarioSet::Quantile { pv, load },
            cvar_alpha: 0.7,
            cvar_weight: 0.0,
        }
    }

    /// How much weight the tail carries, clamped to something meaningful.
    #[must_use]
    pub fn tail_weight(&self) -> f64 {
        if self.cvar_weight.is_finite() {
            self.cvar_weight.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// `1 / (1 − α)`, the factor the tail excesses are scaled by, clamped so a
    /// nonsense `α` cannot make the objective explode.
    #[must_use]
    pub fn tail_scale(&self) -> f64 {
        let alpha = if self.cvar_alpha.is_finite() {
            self.cvar_alpha.clamp(0.0, 0.99)
        } else {
            0.7
        };
        1.0 / (1.0 - alpha)
    }

    /// The futures the plan is asked to survive, with their probabilities.
    #[must_use]
    pub fn realisations(&self) -> Vec<Realisation> {
        self.scenarios.realisations()
    }
}

/// Which futures a plan is priced against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum ScenarioSet {
    /// The median of both forecasts, and nothing else.
    Median,
    /// One chosen quantile of each forecast.
    Quantile {
        /// Which quantile of production.
        pv: Quantile,
        /// Which quantile of load.
        load: Quantile,
    },
    /// Three futures from the three quantiles the forecast already carries,
    /// weighted by **Swanson's rule** — 0,3 / 0,4 / 0,3 on the 10th, 50th and
    /// 90th percentiles.
    ///
    /// Swanson's rule is the standard three-point discretisation of a continuous
    /// distribution from its P10/P50/P90, and using it means the scenario set
    /// costs *nothing to produce*: it is the band `hems-forecast` already
    /// publishes, read as three paths rather than as three numbers per slot.
    ///
    /// The pairing is **comonotone in the household's misfortune**: the
    /// pessimistic future is low production *and* high load together, the
    /// optimistic one is the reverse. Sampling the two independently would put
    /// most of the probability on the bland middle and never generate the day the
    /// hedge exists for — and a household's bad day is precisely the correlated
    /// one, because a dull cold afternoon is both at once.
    ///
    /// Three, not thirty: the error is correlated across a day (a front three
    /// hours late, not a coin flip per quarter hour), so the useful variation is
    /// between *paths* rather than within them, and three paths already contain
    /// the decision — hedge or do not.
    #[default]
    Swanson,
}

/// One future the plan is priced against.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Realisation {
    /// Which quantile of production this future takes.
    pub pv: Quantile,
    /// Which quantile of load.
    pub load: Quantile,
    /// How likely it is, summing to one across the set.
    pub probability: f64,
}

impl ScenarioSet {
    /// The futures, with their probabilities.
    #[must_use]
    pub fn realisations(self) -> Vec<Realisation> {
        let one = |pv, load| {
            vec![Realisation {
                pv,
                load,
                probability: 1.0,
            }]
        };
        match self {
            ScenarioSet::Median => one(Quantile::P50, Quantile::P50),
            ScenarioSet::Quantile { pv, load } => one(pv, load),
            // Swanson's rule on a P10/P50/P90 band.
            ScenarioSet::Swanson => vec![
                Realisation {
                    pv: Quantile::P10,
                    load: Quantile::P90,
                    probability: 0.3,
                },
                Realisation {
                    pv: Quantile::P50,
                    load: Quantile::P50,
                    probability: 0.4,
                },
                Realisation {
                    pv: Quantile::P90,
                    load: Quantile::P10,
                    probability: 0.3,
                },
            ],
        }
    }
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
            shiftable: Vec::new(),
            dhw_draw: &[],
            community_share: &[],
            outdoor_c: &[],
            limits: PlanningLimits::default(),
            objective: Objective::cost(),
            curtailment_penalty_eur_per_kwh: 0.01,
            unmet_charge_eur_per_kwh: 5.0,
            risk: Risk::default(),
            terminal_value_factor: 1.0,
            mip_gap: 0.005,
            shadow_prices: true,
            warm_start: None,
            solve_budget_s: 10.0,
            commitment_horizon: CommitmentHorizon::default(),
        }
    }

    /// Decide the compressor on this grid rather than the default one.
    #[must_use]
    pub const fn with_commitment_horizon(mut self, horizon: CommitmentHorizon) -> Self {
        self.commitment_horizon = horizon;
        self
    }

    /// Start the search from what the previous plan committed.
    ///
    /// The commitment must already be **aligned to this horizon** — use
    /// [`Commitment::shifted`] with the number of slots that have elapsed since
    /// it was made. A misaligned one is not unsafe, merely useless: the solver
    /// finds it infeasible and throws it away.
    #[must_use]
    pub const fn with_warm_start(mut self, commitment: &'a Commitment) -> Self {
        self.warm_start = Some(commitment);
        self
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

    /// Plan inside a § 42c energy-sharing community, with the share it offers.
    ///
    /// The prices come from the [`PriceStack`], which is where a price belongs;
    /// this is the *quantity*.
    #[must_use]
    pub fn in_community(mut self, share_w: &'a [f64]) -> Self {
        self.community_share = share_w;
        self
    }

    /// The § 42c share available in slot `k`, watts.
    #[must_use]
    pub fn community_share_at(&self, k: usize) -> f64 {
        self.community_share.get(k).copied().unwrap_or(0.0).max(0.0)
    }

    /// Whether any slot of this horizon has a community share worth modelling.
    ///
    /// A household that is not in a community, and one whose community has a
    /// dark day, both get the model exactly as it was before § 42c existed —
    /// no column, no row.
    #[must_use]
    pub fn has_community_share(&self) -> bool {
        (0..self.horizon.len).any(|k| {
            self.community_share_at(k) > 0.0
                && self
                    .prices
                    .slots
                    .get(k)
                    .is_some_and(|p| p.sharing_discount_f64() > 0.0)
        })
    }

    /// Add an appliance waiting to run a programme.
    #[must_use]
    pub fn with_shiftable(mut self, run: ShiftableRun) -> Self {
        self.shiftable.push(run);
        self
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

    /// The futures this problem is priced against.
    ///
    /// Never empty: a nonsense risk configuration falls back to the median,
    /// because a plan against no future at all is not a safer answer than a plan
    /// against one.
    #[must_use]
    pub fn realisations(&self) -> Vec<Realisation> {
        let mut out = self.risk.realisations();
        if out.is_empty() {
            out = ScenarioSet::Median.realisations();
        }
        out
    }

    /// The forecast values for slot `k` under one future, in watts: production,
    /// then load.
    #[must_use]
    pub fn forecasts_in(&self, realisation: Realisation, k: usize) -> (f64, f64) {
        let slot = self.horizon.get(k);
        let pv = slot
            .and_then(|s| self.pv.at(s))
            .map_or(0.0, |b| realisation.pv.of(b).max(0.0));
        let load = slot
            .and_then(|s| self.load.at(s))
            .map_or(0.0, |b| realisation.load.of(b).max(0.0));
        (pv, load)
    }

    /// The forecast values for slot `k` in the **central** future — the median
    /// of both — which is what a report shows a household.
    #[must_use]
    pub fn forecasts_at(&self, k: usize) -> (f64, f64) {
        self.forecasts_in(
            Realisation {
                pv: Quantile::P50,
                load: Quantile::P50,
                probability: 1.0,
            },
            k,
        )
    }

    /// Set how the plan treats forecast error.
    #[must_use]
    pub const fn with_risk(mut self, risk: Risk) -> Self {
        self.risk = risk;
        self
    }
}
