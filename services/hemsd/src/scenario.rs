//! A whole day of a household, at 1000×.
//!
//! This is where the crates stop being libraries and become a system: real
//! solar geometry drives a real photovoltaic model, a real optimiser plans
//! against real prices, a real guard applies a real § 14a limit from a simulated
//! Steuerbox, and a real arbiter turns all of it into setpoints that simulated
//! hardware answers the way hardware does.
//!
//! It runs in milliseconds and it is deterministic, so "what happens when the
//! network operator reduces us at teatime on a January Thursday" is a question
//! with a repeatable answer.

use std::collections::BTreeMap;

use hems_core::prelude::*;
use hems_forecast::{ArrayModel, Band, Calibration};
use hems_grid::evidence::{EvidenceRecorder, Observation};
use hems_grid::mispel::QuarterHour as MispelQuarterHour;
use hems_grid::para14a::ControlMode;
use hems_optimizer::model::{BatteryModel, DhwModel, HeatPumpModel, Problem, ThermalModel};
use hems_optimizer::solve::solve;
use hems_realtime::arbiter::{Arbiter, ArbiterConfig, Tick};
use hems_realtime::guard::{GridLimits, SiteState};
use hems_sim::{BatterySim, BuildingSim, EvseSim, PvSim, SteuerboxSim, TankSim, VehicleSim};
use hems_tariff::PriceStack;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use time::{Duration, OffsetDateTime};

use crate::forecasting::{Weather, WeatherSpec};
use crate::site::{Household, HouseholdConfig};

/// What a kilowatt-hour of hot water the household wanted and did not get is
/// worth avoiding.
///
/// Well above any electricity price, so the tank is filled in preference to
/// almost anything — and finite, so a household that starts the day with a cold
/// tank gets the best schedule available rather than no schedule at all.
pub const HOT_WATER_SHORTFALL_EUR_PER_KWH: f64 = 3.0;

/// What the household is willing to pay to avoid a kelvin-hour outside its
/// comfort band. One number, used by the planner, by the day's own accounting
/// and by the baseline — three places that would otherwise drift apart.
pub const DISCOMFORT_EUR_PER_KELVIN_HOUR: f64 = 1.5;

/// What a kilowatt-hour the car was promised and did not get is worth avoiding.
///
/// Far above any electricity price, so the deadline is lexicographic in
/// practice — and finite, so a session that cannot be finished returns the best
/// achievable schedule rather than no schedule at all.
///
/// It is a **price**, not only a weight, and that is the point: the same number
/// prices the shortfall in the objective and charges it on the report, for the
/// plan and for the baseline alike. A term the plan may spend and is not charged
/// for is a discount it can help itself to.
pub const UNMET_CHARGE_EUR_PER_KWH: f64 = 5.0;

/// How little slack a charging session must have left before the day is worth
/// planning against more than one future.
///
/// A third. Below it the car will be filled whatever the weather does and there
/// is nothing to insure; above it, the evening the reference `deadline` day is
/// built around — a car arriving *as* a § 14a reduction starts with three hours
/// to take thirteen kilowatt-hours — and three futures beat the median on the
/// mean there, €2,96 against €2,81 over twenty weathers.
///
/// A threshold rather than a formula because the measurement behind it is four
/// weathers on two days, which supports a sign and not a curve.
pub const TIGHT_SESSION: f64 = 1.0 / 3.0;

/// What leaving the dishwasher unrun inside its window is worth avoiding.
///
/// Above what the wash costs in electricity at any hour of the day, so the plan
/// gives up on it only when the window genuinely has nowhere to put it — and
/// finite, so a household that asked for an impossible window gets the rest of
/// its plan rather than none of it. The same number is charged on the report,
/// for the plan and for the baseline alike.
pub const UNRUN_PROGRAMME_EUR: f64 = 2.0;

/// What throwing away a kilowatt-hour the roof produced is worth avoiding.
///
/// Not zero even where feeding in earns nothing: energy that could have heated
/// water is a real loss, and a plan indifferent to it curtails whenever that is
/// a rounding error cheaper. Used by the planner, by the day and by the
/// baseline, which is capped by § 9 EEG exactly as the managed house is.
pub const CURTAILMENT_EUR_PER_KWH: f64 = 0.01;

/// What reaches the battery when charging on three conductors.
const THREE_PHASE_EFFICIENCY: f64 = 0.92;

/// How often the control loop runs in a simulated day.
///
/// A real box ticks once a second; a day at that rate is 86 400 solves of the
/// same arithmetic and adds nothing a minute does not already show. The guard is
/// told the period, because a bound on a state is only a bound on a rate once
/// you know how long the rate is held for.
/// How often a simulated day re-derives.
///
/// A minute, against the one **second** a box on a wall runs at. That ratio is
/// not a detail: every bound a reactive controller has scales with it, so the
/// day's own § 9 EEG excursions are sixty times a box's, and the assertions that
/// bound them say so rather than quoting a number that would be meaningless at
/// the other cadence. See [`DayResult::worst_uncommanded_export_step_w`].
pub const CONTROL_PERIOD: Duration = Duration::minutes(1);

/// Which way the reference household's glazing faces, degrees clockwise from
/// north.
///
/// Due south, like its roof. It is the plane the building's solar aperture is
/// driven with (`hems_core::thermal::Rc2::solar_aperture_m2`), and it is a
/// constant here for the same reason the roof's tilt is read from the site model
/// rather than repeated: the simulator and the plan must not be able to disagree
/// about which way the house is pointing.
const FACADE_AZIMUTH_DEG: f64 = 180.0;

/// How often the planner re-solves.
///
/// It has to be **shorter than [`ArbiterConfig::max_plan_age`]**, or the box
/// spends part of every cycle with no plan it is willing to follow. This ran at
/// thirty minutes against a twenty-minute tolerance for a long time, which is a
/// ten-minute hole every half hour in which the arbiter quietly fell back to
/// surplus tracking — invisible in the totals, and the reason a wallbox
/// apparently switched its conductors twenty-five times on a sunny day.
/// [`DayResult::minutes_without_a_plan`] is what makes it visible now, and a
/// test pins it at zero.
const REPLAN_PERIOD: Duration = Duration::minutes(15);

/// The compressor a non-modulating heat pump runs, with the minimums a real one
/// carries.
///
/// Half an hour each way, which is [`HeatPumpModel::min_on_slots`]'s default of
/// two slots — the planner and the house have to describe **one** machine, or
/// the day is measuring the gap between two models rather than the controller.
fn compressor_sim() -> hems_sim::CompressorSim {
    hems_sim::CompressorSim::new(Duration::minutes(30), Duration::minutes(30))
}

/// What reaches the battery when charging on one conductor.
///
/// Against 0,92 on three. The onboard charger's standing overhead is the whole
/// of the difference: noise at 11 kW, a tenth of the throughput at 1,4. A
/// simulator that used one figure for both would let a controller switch
/// conductors for free and report a saving nobody would ever see on a bill.
const SINGLE_PHASE_EFFICIENCY: f64 = 0.85;

/// How the day should go.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The household.
    pub config: HouseholdConfig,
    /// The day to simulate, starting at local midnight.
    pub date: time::Date,
    /// The day's **mean** outdoor temperature, °C — the middle of the diurnal
    /// swing, not a constant. What the house actually feels is that swing plus
    /// the day's own forecast error ([`Scenario::weather`]).
    pub outdoor_c: f64,
    /// The day's **mean** cloudiness, `0.0` for a clear sky and `1.0` for no
    /// production at all. What the roof actually sees moves around it.
    pub cloudiness: f64,
    /// How far the day that happens strays from the day that was forecast.
    ///
    /// [`WeatherSpec::with_perfect_forecast`] hands the planner the exact series the
    /// simulator is about to run, and the difference between the two is what
    /// forecast error costs. `hemsd simulate --perfect-foresight` is that
    /// comparison, and having to ask for it by name is deliberate.
    pub weather: WeatherSpec,
    /// A § 14a reduction: from, until, and the ceiling.
    pub grid_event: Option<(Duration, Duration, Power)>,
    /// A window in which the Steuerbox says nothing.
    pub steuerbox_outage: Option<(Duration, Duration)>,
    /// The day-ahead price in each of the 96 slots, ct/kWh.
    pub prices_ct: Vec<i64>,
    /// A charging session: how much the car needs and when it leaves.
    pub ev: Option<EvPlan>,
    /// The window the household will let the dishwasher run in: not before the
    /// first, finished by the second, both measured from local midnight.
    ///
    /// `None` leaves the machine unloaded. The window is the household's, not
    /// the planner's: "after breakfast, done before we go to bed" is the whole
    /// of what anybody actually says about a dishwasher, and it is what makes
    /// the eight hours in between worth optimising over.
    pub dishwasher: Option<(Duration, Duration)>,
    /// How the planner treats the fact that its forecasts are wrong.
    ///
    /// A named comparison, like `per_asset_weights` and for the same reason: the
    /// difference between planning against one median and planning against the
    /// three futures the forecast already carries is a *result*, and a result
    /// nobody can reproduce by flipping one field is a claim.
    pub risk: hems_optimizer::Risk,
    /// Whether to plan against three futures on the days the household has
    /// something at stake.
    ///
    /// Measured on the reference days, and the measurement is the whole
    /// argument. Over twenty seeded weathers on each of two days: planning
    /// against three futures costs **€1,04 a day** on an ordinary winter day —
    /// where the household has a battery, a tank, slack everywhere and nothing
    /// whatever to insure — and five to seven times the solve. On the evening a car
    /// arrives *as* a § 14a reduction starts it **beats** the median on the mean,
    /// €2,96 against €2,81, and takes the undelivered charge from €0,07 to
    /// €0,01. Something has to decide which day it is.
    ///
    /// The trigger is [`EvSession::tightness`] — a property of the session that
    /// needs no solve. The obvious alternative, asking the *plan* whether it
    /// expects a shortfall, was built and measured and does not work: a plan
    /// that has looked only at the median cannot know it is at risk, so on the
    /// reference evening it never fired at all.
    ///
    /// **Off by default, on the evidence there is.** `hemsd risk` is four
    /// weathers on two days; that supports a sign and it does not support
    /// changing what a household's box does. Turning it on where a household's
    /// car is tight is a decision somebody can make from the table it prints,
    /// and a longer sweep is what would make it a default.
    ///
    /// [`EvSession::tightness`]: hems_optimizer::EvSession::tightness
    pub adaptive_risk: bool,
    /// How many days of metering the box has behind it when the day opens.
    ///
    /// [`crate::forecasting::WARM_UP_DAYS`] on every ordinary reference day: six
    /// weeks, which is what it takes before a Sunday cell of the load profile has
    /// enough observations to be worth planning against.
    ///
    /// **Zero is a box on its first evening**, and it is a different question
    /// from the saving rather than a smaller answer to it. Hour one of every
    /// real installation runs this code and nothing exercised it: no load
    /// profile, no correction the roof has earned, no charging session predicted
    /// from previous weeks. What a cold box owes a household is not a good plan —
    /// it cannot have one — but a *lawful* one, a delivered one, and an honest
    /// account of how little it knows (D187).
    pub warm_up_days: usize,
    /// Whether the planner prices each asset separately.
    ///
    /// `false` gives every device the slot's marginal value instead, so the
    /// guard's *weighted* max-min allocator is handed the same weight for each
    /// and weights nothing. A named comparison, because the difference is what
    /// the mechanism is worth.
    pub per_asset_weights: bool,
    /// What this household will pay to avoid something other than money.
    ///
    /// [`hems_optimizer::model::Objective::cost`] on every reference day, so
    /// every figure in this
    /// workspace is the plain economic plan. It is here so that a carbon price
    /// and an autarky premium can be **measured** rather than merely settable:
    /// both are terms the solver has always read and nothing could switch on, so
    /// nobody knew what either was worth or what it cost.
    pub objective: hems_optimizer::model::Objective,
    /// The § 42c energy-sharing community this household belongs to, if any.
    ///
    /// A named comparison, like `risk` and `per_asset_weights`: the difference
    /// between a household in a community and the same household outside one is
    /// a *result*, and the only way to see it is to run the day both ways.
    pub community: Option<CommunityMembership>,
    /// Whether the planner runs at all.
    ///
    /// `false` is the degraded mode: no forecast, no prices, no solver —
    /// the box on its own, doing what every home battery has always done. It is
    /// also the only mode in which switching a charge point's conductors earns
    /// anything, because surplus is an instantaneous quantity that cannot be
    /// duty-cycled the way a planner duty-cycles a quarter hour.
    pub planner: bool,
}

/// What a § 42c energy-sharing community offers this household.
///
/// Since **01.06.2026** final customers inside one Bilanzierungsgebiet may use
/// renewable electricity together over the public grid (§ 42c Abs. 1). What
/// is shared is an **allocation**, not physics: each quarter hour the community's
/// generation is divided among its consumers by an Aufteilungsschlüssel agreed in
/// writing (§ 42c Abs. 3 Nr. 2), and each member's share is billed at the
/// community's own price (Nr. 3) instead of at their supplier's.
///
/// The neighbours' roofs are modelled as one array under the **same sky** as
/// this household's — same location, same tilt, same azimuth, a different
/// rating. That is a stand-in: a real box is told the community's generation
/// forecast by whoever operates the community, because the shade on somebody
/// else's roof is not something this box can learn. What it is *not* is the
/// household's own residual correction applied to a stranger's array, which
/// would be learning about the wrong tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CommunityMembership {
    /// The community's installed generation — the neighbours' roofs together.
    pub kwp: Power,
    /// This member's share of the Aufteilungsschlüssel, in `[0, 1]`,
    /// § 42c Abs. 3 Nr. 2.
    pub key: f64,
    /// The community's own energy price, ct/kWh net, § 42c Abs. 3 Nr. 3.
    ///
    /// Net, and only the *energy* component: the electricity reaches the member
    /// over the public grid, so its network charge, its levies and its value
    /// added tax are unchanged. On the reference winter day 12 ct/kWh net still
    /// arrives at the meter at 32,5 ct against the supplier's 47,9 — a third
    /// off, which is worth having and is not the ninety per cent somebody
    /// imagining free electricity from the neighbours would price.
    pub price_ct: Decimal,
}

impl CommunityMembership {
    /// A Mehrfamilienhaus community: three roofs' worth of array, an equal
    /// third of the key, and electricity at 12 ct/kWh net.
    ///
    /// The shape § 42c was written for, and the numbers are deliberately
    /// ordinary — a community that priced itself at nothing would make every
    /// figure below a statement about the number rather than about the
    /// mechanism.
    #[must_use]
    pub fn mehrfamilienhaus(kwp: Power) -> Self {
        Self {
            kwp,
            key: 1.0 / 3.0,
            price_ct: Decimal::new(12, 0),
        }
    }
}

/// The window a household will let a dishwasher run in: loaded after breakfast,
/// unloaded before bed.
///
/// Eight hours of freedom over a ninety-minute programme, which is a realistic
/// amount of latitude and enough for the price curve to have an opinion.
const fn evening_wash() -> (Duration, Duration) {
    (Duration::hours(9), Duration::hours(23))
}

/// A car and the window it is plugged in for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvPlan {
    /// Energy in the car when it arrives.
    pub energy_now: Energy,
    /// Energy it should hold when it leaves.
    pub energy_target: Energy,
    /// When the cable goes in, as an offset into the day.
    ///
    /// Zero for a car that is there when the day starts, and not a detail: a car
    /// plugged in from midnight can always be charged in the cheap hours of the
    /// night, so no scenario without a positive arrival exercises the case that
    /// matters — a § 14a reduction the car has to charge *through*.
    pub arrival: Duration,
    /// When it leaves, as an offset into the day.
    pub departure: Duration,
}

impl EvPlan {
    /// A car that is already plugged in when the day begins.
    #[must_use]
    pub const fn overnight(energy_now: Energy, energy_target: Energy, departure: Duration) -> Self {
        Self {
            energy_now,
            energy_target,
            arrival: Duration::ZERO,
            departure,
        }
    }
}

/// The tightest ceiling a network operator may lawfully command this household,
/// `[A1 4.5.2]`.
///
/// Under an energy management system the minimum is **one number for everything
/// behind it**, and it grows with the number of controllable devices:
/// `4,2 kW + (n − 1) · GZF(n) · 4,2 kW`. A household with a wallbox, a heat pump
/// and a battery is owed **10,5 kW**; with no battery, 7,56 kW.
///
/// A flat 4,2 kW is the *base* of that formula rather than the whole of it, so
/// commanding it is an instruction no operator may send. Deriving the ceiling
/// keeps a reduction lawful when the household changes — and an operator asking
/// for the most relief the Festlegung allows commands exactly this, which is
/// also the sharpest honest test of the household's response.
fn lawful_minimum(config: &HouseholdConfig, date: time::Date) -> Power {
    Household::build(config).map_or(hems_grid::para14a::MINDESTLEISTUNG, |h| {
        hems_grid::para14a::minimum_power(
            &hems_grid::classify_on(&h.site.assets, date),
            hems_grid::para14a::ControlMode::Ems,
        )
    })
}

impl Scenario {
    /// A January day with a teatime reduction — the case § 14a exists for.
    #[must_use]
    pub fn winter_with_grid_event(config: HouseholdConfig) -> Self {
        let date = time::macros::date!(2026 - 01 - 15);
        let ceiling = lawful_minimum(&config, date);
        Self {
            config,
            date,
            outdoor_c: 2.0,
            cloudiness: 0.55,
            // A January day in Germany: broken cloud, and a forecast that is
            // routinely a couple of kelvin and a good deal of irradiance out.
            weather: WeatherSpec::broken(0x_1501_2026),
            grid_event: Some((
                Duration::hours(17),
                Duration::minutes(18 * 60 + 30),
                ceiling,
            )),
            steuerbox_outage: None,
            planner: true,
            per_asset_weights: true,
            // Every reference figure in this workspace is the plain economic
            // plan; a preference is a comparison to ask for by name.
            objective: hems_optimizer::model::Objective::cost(),
            prices_ct: winter_prices(),
            // Home at teatime, gone by seven in the morning with 20 kWh more in
            // it than it arrived with — so the plan has to find the cheap hours
            // of the night *and* work around the § 14a reduction on the way.
            risk: hems_optimizer::Risk::default(),
            community: None,
            adaptive_risk: false,
            warm_up_days: crate::forecasting::WARM_UP_DAYS,
            dishwasher: Some(evening_wash()),
            ev: Some(EvPlan::overnight(
                Energy::from_kwh(18.0),
                Energy::from_kwh(38.0),
                Duration::hours(7),
            )),
        }
    }

    /// The same June day with the planner switched off.
    ///
    /// What the box does on its own: cover the house from the roof and the
    /// store, absorb what is left into the car, and export the rest. It is the
    /// degraded mode the box promises, and it is the one place a switchable charge
    /// point pays for itself — surplus arrives as an instantaneous quantity, and
    /// below 4,14 kW three conductors can do nothing with it at all.
    ///
    /// **The dishwasher stays unloaded here**, and for the same reason the
    /// autumn day leaves it unloaded: this day and the one below it are
    /// controlled experiments on the *surplus* — what a contactor is worth, and
    /// what the household's Ladelimit stops the fallback doing with it — and an
    /// appliance that eats a kilowatt-hour of exactly that surplus makes the day
    /// measure two mechanisms and attribute the sum to one. The no-plan fallback
    /// for an appliance is covered by a test that loads one deliberately.
    #[must_use]
    pub fn summer_without_a_planner(config: HouseholdConfig) -> Self {
        Self {
            planner: false,
            risk: hems_optimizer::Risk::default(),
            community: None,
            adaptive_risk: false,
            warm_up_days: crate::forecasting::WARM_UP_DAYS,
            dishwasher: None,
            ..Self::summer_surplus(config)
        }
    }

    /// A **September** day with the planner off and broken cloud — the day a
    /// switchable charge point is the whole difference.
    ///
    /// Midsummer is the wrong test for it. A 9,8 kWp roof under high pressure
    /// spends the middle of the day above the 4,14 kW a three-phase session needs
    /// to start, so the car fills either way and the contactor earns nothing. The
    /// German shoulder season is the other nine months: under broken cloud the
    /// surplus sits in the **1,4 – 4,1 kW band** for hours, where three
    /// conductors can do nothing with it at all and one can take all of it.
    ///
    /// Measured: **6,0 kWh** into the car against **0,2** with the wallbox wired
    /// to three fixed conductors, for **one** contactor operation and 180 minutes
    /// on a single conductor — and, because the car has somewhere to be at eight,
    /// the difference between a household that got what it asked for and one
    /// that is 4,8 kWh short and €24 out of pocket for it.
    ///
    /// The car's target is deliberately inside what the day can give: a box with
    /// no planner surplus-charges and never imports for a deadline, so a
    /// target beyond the roof measures the absence of a planner rather than the
    /// presence of a contactor.
    ///
    /// And for the same reason **the dishwasher stays unloaded on this day**. It
    /// eats 1,1 kWh of exactly the surplus the contactor is being measured on,
    /// which is enough to put the car short of a target calibrated against the
    /// roof — so the day would report the two mechanisms added together and
    /// attribute the sum to the one it is named after. It is a controlled
    /// experiment; the winter day is where the appliance is measured.
    #[must_use]
    pub fn autumn_without_a_planner(config: HouseholdConfig) -> Self {
        Self {
            risk: hems_optimizer::Risk::default(),
            community: None,
            adaptive_risk: false,
            warm_up_days: crate::forecasting::WARM_UP_DAYS,
            dishwasher: None,
            date: time::macros::date!(2026 - 09 - 20),
            outdoor_c: 14.0,
            cloudiness: 0.5,
            weather: WeatherSpec::settled(0x_2009_2026),
            ev: Some(EvPlan::overnight(
                Energy::from_kwh(20.0),
                Energy::from_kwh(25.0),
                Duration::hours(20),
            )),
            ..Self::summer_without_a_planner(config)
        }
    }

    /// A January evening with a § 14a reduction and a car that cannot wait for
    /// the cheap hours.
    ///
    /// The car arrives at teatime and has to leave at eight, so the plan cannot
    /// move the charging into the night — it has to charge *through* the network
    /// operator's reduction. The § 14a ceiling is 4,2 kW for the whole house, and
    /// after the heat pump and the household take their share the wallbox is left
    /// with something between one and three kilowatts.
    ///
    /// What it demonstrates is the **store lending its discharge** to the § 14a
    /// budget (`[A1 2.3]`): 5,4 kWh of headroom the connection point never
    /// saw, and a car that leaves full.
    ///
    /// It is *not* a phase-switching day, and the KPI says so — **zero
    /// switches**. A planner duty-cycles a quarter hour and reaches the same
    /// average; a contactor pays where
    /// there is no planner, which is the `autumn` day.
    #[must_use]
    pub fn winter_evening_deadline(config: HouseholdConfig) -> Self {
        let ceiling = lawful_minimum(&config, time::macros::date!(2026 - 01 - 15));
        Self {
            grid_event: Some((
                Duration::hours(17),
                Duration::minutes(19 * 60 + 30),
                ceiling,
            )),
            // Home as the reduction starts, gone at eight, and 12 kWh short.
            // The arrival is what makes this scenario the one it says it is:
            // with the car on the cable from midnight the plan simply charges it
            // in the cheap hours of the night and never meets the reduction.
            risk: hems_optimizer::Risk::default(),
            community: None,
            adaptive_risk: false,
            warm_up_days: crate::forecasting::WARM_UP_DAYS,
            dishwasher: Some(evening_wash()),
            ev: Some(EvPlan {
                energy_now: Energy::from_kwh(26.0),
                energy_target: Energy::from_kwh(38.0),
                arrival: Duration::hours(17),
                departure: Duration::hours(20),
            }),
            ..Self::winter_with_grid_event(config)
        }
    }

    /// A January evening reduction on a household with **no store** — the case
    /// § 14a actually bites in.
    ///
    /// Millions of German households have a heat pump and a wallbox and no
    /// battery, and they are the ones a 4,2 kW ceiling is hard on: there is no
    /// discharge to lend the controllable devices headroom (`[A1 2.3]`), so
    /// the reduction has to be *shared* and somebody gets less than they wanted.
    ///
    /// That makes it the day the allocation weights decide something. On the
    /// reference household with a 10 kWh battery they decide almost nothing —
    /// the store lends enough that the budget is rarely short, which is a good
    /// outcome and a poor test. Here the car is three hours from a departure it
    /// needs 12 kWh for and the heat pump is warming a house that is already
    /// inside its comfort band, and the two want 16 kW between them under a
    /// ceiling of 4,2.
    #[must_use]
    pub fn winter_evening_no_store(config: &HouseholdConfig) -> Self {
        let household = HouseholdConfig {
            // Not absent: a household with no battery at all has no asset to
            // model, and what is being tested is the *sharing*, not the absence.
            // Below the 4,2 kW of `[A1 2.4.1]` it is not a steuerbare
            // Verbrauchseinrichtung either, so the household is owed the
            // two-device minimum of 7,56 kW rather than the three-device 10,5.
            battery: config.battery.map(|b| crate::site::BatteryConfig {
                kwh: Energy::from_kwh(1.0),
                power: Power::from_kw(0.5),
                ..b
            }),
            ..config.clone()
        };
        let ceiling = lawful_minimum(&household, time::macros::date!(2026 - 01 - 15));
        Self {
            // **Seven minutes past the hour**, and that is the whole scenario.
            //
            // A network operator does not send its reduction on the household's
            // re-planning grid: `[A1 4.2]` presumes it goes out within five
            // minutes of the Netzzustandsermittlung, and nothing aligns that to
            // a quarter hour. So between the command arriving and the next
            // re-plan there is a window — up to `REPLAN_PERIOD` — in which the
            // plan in force was made **without the ceiling** and its targets do
            // not fit under it.
            //
            // That window is the only time the guard's allocator actually
            // *decides* anything: the rest of the time the planner has already
            // solved the split under the same ceiling and the arbiter follows
            // it. It is the case the guard exists for — an optimiser can be infeasible,
            // a guard cannot.
            grid_event: Some((
                Duration::minutes(17 * 60 + 7),
                Duration::minutes(19 * 60 + 30),
                ceiling,
            )),
            ..Self::winter_evening_deadline(household)
        }
    }

    /// A June day: much more production than the house can use.
    #[must_use]
    pub fn summer_surplus(config: HouseholdConfig) -> Self {
        Self {
            config,
            date: time::macros::date!(2026 - 06 - 21),
            outdoor_c: 24.0,
            cloudiness: 0.1,
            // Midsummer under high pressure: a thin haze that comes and goes.
            weather: WeatherSpec::settled(0x_2106_2026),
            grid_event: None,
            steuerbox_outage: None,
            planner: true,
            per_asset_weights: true,
            // Every reference figure in this workspace is the plain economic
            // plan; a preference is a comparison to ask for by name.
            objective: hems_optimizer::model::Objective::cost(),
            prices_ct: summer_prices(),
            risk: hems_optimizer::Risk::default(),
            community: None,
            adaptive_risk: false,
            warm_up_days: crate::forecasting::WARM_UP_DAYS,
            dishwasher: Some(evening_wash()),
            ev: Some(EvPlan::overnight(
                Energy::from_kwh(20.0),
                Energy::from_kwh(45.0),
                Duration::hours(18),
            )),
        }
    }

    /// The same June day with nobody home: no car on the cable, and a roof the
    /// house cannot use.
    ///
    /// This is the day § 9 EEG was written for. A 9,8 kWp system commissioned
    /// after 25.02.2025 without an intelligent metering system and a control
    /// device may feed in 60 % of its installed direct-current power — 5,88 kW —
    /// and on a clear June day around noon it produces more than that with a
    /// full battery and no car to put it in. What is left has to be thrown away,
    /// and the number this day prints is what the Solarspitzengesetz costs a
    /// household that has not been given its Steuerbox yet.
    ///
    /// Set the roof's [`crate::site::PvConfig::para9`] relief to [`CapRelief::ImsysWithControl`] to
    /// run the same day with the cap lifted, which is the comparison worth
    /// seeing.
    #[must_use]
    pub fn summer_capped(config: &HouseholdConfig) -> Self {
        Self {
            risk: hems_optimizer::Risk::default(),
            community: None,
            adaptive_risk: false,
            warm_up_days: crate::forecasting::WARM_UP_DAYS,
            dishwasher: Some(evening_wash()),
            ev: None,
            // **May, not June.** The cap is a fraction of installed
            // direct-current power, and what a roof delivers against that
            // fraction is decided by cell temperature: at 24 degrees ambient a
            // module runs near 54 and loses about a ninth of its output, so a
            // midsummer peak is barely above the 60 % line at all. A clear day
            // in the middle of May is cooler, nearly as bright, and is when
            // German feed-in peaks and negative prices cluster. Run in June it
            // understates what the Solarspitzengesetz costs by most of it.
            date: time::macros::date!(2026 - 05 - 15),
            outdoor_c: 14.0,
            // A cloudless day, because the cap is a limit on *peak* feed-in and
            // a tenth of cloud is enough to keep a 9,8 kWp roof under it all
            // afternoon. The Solarspitzengesetz exists for the other days.
            cloudiness: 0.0,
            // …and a *settled* one, so that the peak this day is written around
            // actually happens. The soiling stays: a real roof delivers about
            // 92 % of its datasheet, which is worth knowing before anybody
            // quotes a curtailment figure, and it is part of why the 9,8 kWp
            // default in this workspace never sees the cap at all.
            weather: WeatherSpec {
                cloud_amplitude: 0.05,
                temperature_error_k: 1.0,
                ..WeatherSpec::settled(0x_2106_2026)
            },
            ..Self::summer_surplus(HouseholdConfig {
                // A small store, because the size of the store is what decides
                // whether the cap ever bites at all. A household that can absorb
                // the peak into a battery, a car or a tank never curtails a
                // watt — which is the optimiser working, and the reason the
                // 9,8 kWp default in this workspace shows a curtailment figure
                // of zero on every day it runs.
                battery: config.battery.map(|b| crate::site::BatteryConfig {
                    kwh: Energy::from_kwh(1.0),
                    power: Power::from_kw(0.5),
                    ..b
                }),
                // Twenty kilowatts peak on a full south roof, with an
                // inverter sized one to one against the modules. Both halves
                // matter. The cap is a fraction of the **direct-current** power
                // while the limit it meets is the alternating-current peak, so
                // an undersized inverter clips below the cap on its own — which
                // is why the 9,8 kWp default in this workspace barely sees it,
                // and is worth knowing before anybody quotes a curtailment
                // figure from it.
                pv: config.pv.map(|p| crate::site::PvConfig {
                    kwp: Power::from_kw(20.0),
                    ac_nominal: Power::from_kw(20.0),
                    ..p
                }),
                ..config.clone()
            })
        }
    }

    /// The instant the day starts, in UTC.
    #[must_use]
    pub fn start(&self) -> OffsetDateTime {
        metering::calendar::day_start_utc(self.date)
    }

    /// The same day, moved forward in whole weeks until § 42c allocation is a
    /// duty a network operator already has.
    ///
    /// **Whole weeks**, so the weekday survives: the load profile is kept by day
    /// type (workday / Saturday / Sunday-and-holiday), and a January Thursday
    /// moved to a January Friday would be a different household's day wearing
    /// the same name. Fifty-two weeks is 364 days, which also leaves the solar
    /// declination within half a degree of where it was — the comparison stays
    /// the *same* day in every sense that decides a number.
    ///
    /// It exists because a § 42c demonstration on a date the rule does not reach
    /// demonstrates nothing: § 42c Abs. 4 Nr. 1 obliges a network operator to
    /// make sharing possible from 1 June 2026, and before that a household may
    /// agree whatever it likes and will not be allocated a kilowatt-hour. Two
    /// reference days settled one anyway, in January and May 2026, and reported
    /// the credit (D179).
    #[must_use]
    pub fn on_a_day_sharing_reaches(mut self) -> Self {
        while !hems_grid::sharing::applies_on(self.date) {
            self.date += time::Duration::weeks(52);
        }
        self
    }
}

/// One command from a simulated Steuerbox, applied to the machine.
///
/// The whole of what the simulator owes the state machine, and the reason the
/// two can be separate crates at all: `hems-sim` simulates the *operator's* box
/// and says what it put on the wire; what a Controllable System makes of a write
/// is the Controllable System's business. `hems-drv`'s own
/// `steuerbox_against_the_machine.rs` makes the same translation, because it
/// drives the same pair (D186).
fn apply_to_the_machine(
    lpc: &mut hems_drv::eebus::Lpc,
    command: hems_sim::Command,
    now: OffsetDateTime,
) {
    use hems_drv::eebus::LimitWrite;
    match command {
        hems_sim::Command::Heartbeat => lpc.on_heartbeat(now),
        hems_sim::Command::Limit { value, duration } => {
            let write = match duration.and_then(|d| std::time::Duration::try_from(d).ok()) {
                Some(d) => LimitWrite::active_for(value.get(), d),
                None => LimitWrite::active(value.get()),
            };
            lpc.on_limit(&write, now);
        }
        hems_sim::Command::Release => {
            lpc.on_limit(&LimitWrite::deactivated(), now);
        }
    }
}

/// A plausible winter price curve: cheap at night, dear morning and evening.
fn winter_prices() -> Vec<i64> {
    (0..96)
        .map(|i| {
            let hour = i / 4;
            match hour {
                0..=5 => 12,
                6..=8 => 32,
                9..=15 => 22,
                16..=20 => 38,
                _ => 18,
            }
        })
        .collect()
}

/// A summer curve with a midday collapse — and **twelve** negative quarter
/// hours, three whole hours of them, which is what § 51 EEG is about.
///
/// The count was documented as four until `DayResult::para51_hours` started
/// reporting it, which is the argument for the KPI in one line: a rule applied
/// per slot inside the price stack, a figure quoted in two documents, and
/// nothing that compared the two.
fn summer_prices() -> Vec<i64> {
    (0..96)
        .map(|i| {
            let hour = i / 4;
            match hour {
                0..=5 => 8,
                6..=10 => 14,
                11..=13 => -3,
                14..=16 => 4,
                17..=20 => 28,
                _ => 15,
            }
        })
        .collect()
}

/// What one day came to.
///
/// A flat record of measured facts, which is what a report is. The four booleans
/// answer four unrelated questions — was the planner shown the answer, did the
/// failsafe go below the § 14a minimum, was a device commanded below it, was the
/// operator's limit respected — and folding unrelated yes/no facts into a state
/// machine would make each of them harder to read and none of them clearer.
#[expect(
    clippy::struct_excessive_bools,
    reason = "a report is a flat record of facts; these four answer unrelated questions"
)]
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DayResult {
    /// Energy drawn from the grid, kWh.
    pub imported_kwh: f64,
    /// Energy fed into the grid, kWh.
    pub exported_kwh: f64,
    /// Quarter hours in which § 51 EEG took the remuneration to zero.
    ///
    /// The negative-price hours — 573 of them in 2025 — and the number the
    /// § 51 rule owes a day it is applied on (R20). The summer reference day is
    /// *written around* four of them, and until this existed nothing counted
    /// them: the rule was applied per slot inside the price stack and no day
    /// reported whether it had ever bound.
    ///
    /// It is a property of the **tariff** rather than of the plan — no
    /// controller can move a negative auction — so it is context for the
    /// day's decisions rather than a result of them, which is exactly why it
    /// belongs beside them.
    pub para51_hours: usize,
    /// Carbon dioxide behind what this household **imported**, kg.
    ///
    /// Accumulated a control period at a time against the grid's own intensity
    /// in that quarter hour, so it is the emissions of the electricity the
    /// household actually drew rather than the day's average times its total.
    /// The two differ by exactly the thing a carbon price is for: moving load
    /// from a dirty hour to a clean one changes the first and leaves the second
    /// alone.
    ///
    /// It is the KPI the objective's carbon term owes the reference day (R20).
    /// A term that can be priced and whose effect nothing reports is a term that
    /// could stop being applied without any day noticing.
    pub imported_co2_kg: f64,
    /// Production, kWh.
    pub produced_kwh: f64,
    /// What the plan made at midnight expected the day's **bill** to be, euros.
    ///
    /// The opening plan's horizon is exactly the ninety-six slots of the day, so
    /// this and [`DayResult::cost`]'s own `billed_eur` are the same question
    /// asked of a forecast and of a meter. `None` where no plan was made at all.
    ///
    /// # What the gap is made of, which is more than the weather
    ///
    /// It is tempting to read it as "what forecast error cost", and it is not:
    /// it is the distance between one plan and the day that happened, and four
    /// things move it.
    ///
    /// The **weather** the opening plan did not know. The **car**, which at
    /// midnight is not plugged in, so the plan is given only what the box has
    /// learned about when this household usually comes home. The **ninety-five
    /// re-plans** after it, each of which sees a horizon running into the next
    /// day that the opening plan could not — a receding-horizon controller is
    /// entitled to beat its own opening projection and routinely does. And the
    /// **arbiter**, which quantises a charge point to six amperes and a tank to
    /// on or off.
    ///
    /// `--perfect-foresight` is what separates the first from the other three:
    /// on the reference winter day the gap is **+€1,03** honestly and **−€2,46**
    /// with the weather known in advance, so the weather is not even the largest
    /// of the four. That is a number worth having precisely because it is not
    /// the one anybody expects.
    ///
    /// # Why the bill and not the whole cost
    ///
    /// The plan's ledger and the day's ledger agree about the terms that are on
    /// an invoice — energy, and what a § 42c community took off it — and
    /// deliberately do not agree about the two that are not: the day charges what
    /// it *borrowed from the stores* against its own opening state, and credits
    /// charge pushed into the car past what the household asked for. Neither is a
    /// term a plan can predict, because both are statements about where the day
    /// ended relative to where it began. Comparing the billed halves is a
    /// comparison; comparing the totals would be an argument about bookkeeping.
    pub opening_plan_bill_eur: Option<f64>,
    /// Kilowatt-hours a § 42c community allocated to this household over the
    /// day, settled from its own meter registers.
    ///
    /// Zero where the household is not in a community. A **structural** zero
    /// reads exactly like a mechanism that ran and achieved nothing, so the
    /// report prints the line only where there is one to print.
    pub shared_kwh: f64,
    /// The same, for the **unmanaged** household of
    /// [`DayResult::baseline`] — a member of the same community that joined and
    /// then did nothing about it.
    ///
    /// It was computed and thrown away: `baseline_cost` settled the allocation
    /// to get its credit and kept only the euros. That made the value of
    /// *shifting* into a community window measurable only as a difference of two
    /// differences — two savings each the difference of two ~30 € days — where
    /// the quantity itself is right here. On a January day the euro figure comes
    /// out at three cents either way, which is noise; the kilowatt-hours are the
    /// mechanism (D174).
    pub baseline_shared_kwh: f64,
    /// Household consumption excluding the controllable devices, kWh.
    pub consumed_kwh: f64,
    /// Production thrown away because it could be neither used nor exported, kWh.
    pub curtailed_kwh: f64,
    /// What the day cost, term by term. The energy bill is only part of it:
    /// battery life and time outside the comfort band were spent too, and a
    /// saving computed without them is the one § 9.2 of the concept condemns.
    pub cost: CostBreakdown,
    /// What the same day would have cost with no storage and no shifting,
    /// priced with the same terms.
    pub baseline: CostBreakdown,
    /// The day's electricity bill computed the way most published comparisons
    /// compute it: **netted within each quarter hour** before it is priced.
    ///
    /// Not a cost term, and deliberately not in [`CostBreakdown`] — that type
    /// holds what the day *spent*, one entry per term of the objective. This is
    /// the same spending measured a second way, and the gap between the two is a
    /// property of the **measurement** rather than of the household.
    ///
    /// # Why it is worth a line on the report
    ///
    /// A two-register meter does not net. It integrates the instantaneous flow
    /// into the import register or the export register as the sign falls, so a
    /// quarter hour in which the house draws 1 kW for seven minutes and feeds
    /// back 1 kW for seven minutes registers both — and is billed for both, at
    /// two different prices. Netting that quarter hour to zero makes it free.
    ///
    /// An evaluation that nets at its own scheduling step therefore charges a
    /// controller less than the meter will, and it does so **unevenly**: a
    /// controller whose output fluctuates inside the interval is forgiven more
    /// than one whose output is smooth. arXiv:2510.25373 measures what that is
    /// worth on residential battery scheduling and puts it at 37 % of the
    /// reported advantage at a quarter-hour step and 69 % at an hourly one.
    ///
    /// `hemsd` never nets: [`DayResult::cost`] is accumulated tick by tick from
    /// the two directions priced apart. This field is what that decision is
    /// worth on this day, so the claim is a number rather than an assertion —
    /// and so that the day it stops being true, the number moves (R20).
    pub energy_eur_netted: f64,
    /// The **unmanaged** household's bill under the same netting, so the
    /// artefact can be read where it matters: on the *saving*.
    ///
    /// A saving is a difference, and netting does not forgive both households
    /// equally. It forgives whichever one crosses zero more often inside a
    /// quarter hour — and that is the one the paper's own mechanism names: a
    /// thermostat and a plug-in wallbox reverse the connection point far more
    /// than a plan that smooths it. So a comparison evaluated at its scheduling
    /// step credits the *baseline* with reversals the meter would charge it for,
    /// and the managed household's advantage shrinks. Reporting both is what
    /// turns that from an argument into a figure this day can print.
    pub baseline_energy_eur_netted: f64,
    /// The share of consumption covered without importing.
    pub self_sufficiency: f64,
    /// Energy moved through the battery, kWh.
    pub battery_throughput_kwh: f64,
    /// The lowest state of charge the battery reached, as a fraction.
    ///
    /// The number that says whether the backup reserve was a promise or a
    /// setting: a plan can respect it and the arbiter still spend it between
    /// re-plans, which is why the guard enforces it too.
    pub battery_soc_min: f64,
    /// Energy delivered to the car, kWh.
    pub ev_charged_kwh: f64,
    /// Electrical energy used by the heat pump, kWh.
    pub heat_pump_kwh: f64,
    /// Whether the day was run with the planner shown the weather in advance.
    ///
    /// A saving from such a day is an **upper bound**, not a result: the
    /// controller was handed the answer. The whole of this project's case is
    /// that a HEMS quoting a saving without saying which of the two it measured
    /// is quoting the second, and a report that does not carry the flag makes
    /// hems do exactly that to itself — the reference winter day saves €2,09
    /// honestly and €5,25 with foresight, and until this existed both printed
    /// the same way.
    pub foresight_is_perfect: bool,
    /// How many times a single-speed compressor started.
    ///
    /// Zero for a modulating unit, which has nothing to start. For an on/off
    /// one it is the cost the planner's minimum runtime exists to bound: a
    /// compressor is damaged by starting far faster than by running, so a plan
    /// that looks cheap and cycles forty times is not the cheaper plan.
    ///
    /// It is here because the constraint was, for the life of this project,
    /// unobservable — every reference day ran a modulating unit, so nothing ever
    /// cycled and no number ever said so. That is how a minimum runtime came to
    /// be stated in every plan and enforced on no day: the rows constrain a
    /// transition and need a previous slot, and a receding horizon executes only
    /// the first, which has none.
    pub compressor_starts: usize,
    /// How long the compressor stayed on against a command to stop, in minutes.
    ///
    /// The guard may lawfully command zero in the middle of a run — a § 14a
    /// reduction does not wait for a compressor — and the unit's own minimum
    /// runtime then overrides it. So this is not a defect count: it is how much
    /// of the manager's authority the hardware takes back, which is worth a
    /// number because it is the part of a plan that never happens.
    pub compressor_held_minutes: i64,
    /// Electrical energy used by the hot-water tank, kWh.
    pub dhw_kwh: f64,
    /// Electricity the shiftable appliance drew.
    pub appliance_kwh: f64,
    /// How much later than the unmanaged household the appliance was started,
    /// in minutes.
    ///
    /// The whole of what an energy manager decides about a dishwasher, so the
    /// whole of what a day has to report about one. A structural zero here means
    /// the planner never moved it — which is the failure this workspace keeps
    /// finding in itself, and the reason the number is printed rather than
    /// inferred from the bill.
    pub appliance_shift_minutes: i64,
    /// Hot water the household asked for and did not get, as heat, kWh.
    ///
    /// A cold shower, in the unit it is planned in. Zero on any ordinary day;
    /// above zero it is the number the household has to be told, and it is why
    /// the draw is a priced slack rather than a hard constraint.
    pub cold_water_kwh: f64,
    /// The emptiest the tank ever got, as a fraction of its usable heat.
    pub tank_min_fill: f64,
    /// Lowest indoor temperature reached, °C.
    pub indoor_min_c: f64,
    /// Highest indoor temperature reached, °C.
    pub indoor_max_c: f64,
    /// Kelvin-hours spent outside the comfort band.
    pub discomfort_kelvin_hours: f64,
    /// The lowest netzwirksamer Leistungsbezug the operator may reduce this
    /// household to, `[A1 4.5]` — in the mode the site is actually in.
    ///
    /// Under an energy management system `[A1 4.4.b]` it is one number for
    /// everything behind it and it **grows with the number of controllable
    /// devices**: `4,2 kW + (n − 1) · GZF(n) · 4,2 kW`. A household with a
    /// wallbox, a heat pump and a battery is owed 10,5 kW, not 4,2 — the flat
    /// figure is the *base* of the formula, and reading it as the whole of it is
    /// the easiest mistake in the Festlegung to make.
    pub minimum_power_kw: f64,
    /// Whether the box's **own** failsafe value sits below that minimum.
    ///
    /// A different fault from [`DayResult::commanded_below_minimum`] and worth
    /// telling apart: this one is the household's own configuration restraining
    /// it further than any operator may, on nothing more than a lost heartbeat.
    pub failsafe_below_minimum: bool,
    /// Whether a ceiling the operator commanded was **below** that minimum.
    ///
    /// An unlawful instruction, `[A1 4.5]`. The box carries it out anyway — a
    /// guard cannot refuse a grid limit — and records it, because the record of
    /// `[A1 7.2]` is what a customer takes to the operator. It is printed
    /// because a field that is written and never read is the failure mode R20
    /// names: nothing else would say that a reference day was running under an
    /// instruction no operator may send.
    pub commanded_below_minimum: bool,
    /// How long a network operator's § 14a limit was in force, minutes.
    pub limited_minutes: i64,
    /// Whether the netzwirksamer Leistungsbezug stayed inside the ceiling the
    /// whole time.
    pub grid_event_respected: bool,
    /// The largest overshoot of a § 14a ceiling, watts. Should be zero.
    pub worst_overshoot_w: f64,
    /// The largest amount by which the **Einspeiseleistung** crossed the § 9
    /// EEG / LPP ceiling at the connection point, watts.
    ///
    /// A statutory limit with nothing asserting it is a statutory limit nobody
    /// is enforcing. § 14a had `grid_event_respected` from the first day and
    /// § 9 EEG had only [`DayResult::peak_feed_in_kw`] — a number printed beside
    /// its ceiling, which a reader has to compare by eye, and which sat *above*
    /// it for four quarter hours of the `capped` day while the line underneath
    /// said the limit had been respected throughout.
    ///
    /// It is measured against the **instantaneous** ceiling because § 9 Abs. 2
    /// EEG says Leistung. What it counts is what the connection point did, not
    /// what the guard commanded: the two differ by the control period, and it is
    /// the difference this exists to size.
    pub worst_feed_in_overshoot_w: f64,
    /// For how long the connection point was above that ceiling, minutes.
    pub feed_in_over_minutes: i64,
    /// The largest rise in *uncommanded* net export between two control
    /// periods, watts — the roof going up plus the household's own draw going
    /// down, neither of which the manager asked for.
    ///
    /// This is what bounds [`DayResult::worst_feed_in_overshoot_w`], and it is
    /// the reason that number is not zero on a reactive limiter. A guard that
    /// held the connection point exactly at the ceiling last tick is over it by
    /// this much on the next one, whatever it does, because the step happened
    /// between two decisions.
    ///
    /// It is reported rather than reasoned about because the dominant term is
    /// not what it was assumed to be. The bound this replaced was written as
    /// "the household's own uncommanded load steps" and hard-coded at a
    /// kilowatt; measured on the `capped` day, the worst tick has 288 W of
    /// household draw and the step is almost entirely the **roof** — a cloud
    /// edge clearing on a 20 kWp array in sixty seconds. The load-step story
    /// was true for the site it was written on and stopped being true when the
    /// array grew, which is exactly what a hard-coded bound cannot tell you
    /// (D174).
    pub worst_uncommanded_export_step_w: f64,
    /// How long the manager held itself at its failsafe value for want of
    /// contact with an Energy Guard, minutes — the `init` and `failsafe` states
    /// of the EEBUS machine together.
    pub failsafe_minutes: i64,
    /// How many § 14a control events the day produced a record for, `[A1 7]`.
    ///
    /// Only records whose rule is an actual network-operator limit. A record
    /// opened because the manager was holding *itself* at its failsafe value for
    /// want of an Energy Guard is counted in [`DayResult::failsafe_events`]: it
    /// is a real record and worth keeping, but reporting it as a § 14a event
    /// tells a household the operator intervened on a day when nobody did.
    pub control_events: usize,
    /// How many records the day produced for the manager restraining itself
    /// (`init` or `failsafe`, `[LPC-901]`).
    pub failsafe_events: usize,
    /// The slowest a reduction was acted on, seconds — `[A1 4.2 S. 5]` requires
    /// it to be without delay, and presumes five minutes is enough.
    pub worst_latency_s: f64,
    /// Whether the household had to be **commanded** into the reduction, or was
    /// already inside it.
    ///
    /// Both satisfy `[A1 4.2]` and they are different facts. A record that
    /// cannot tell them apart reports a compliant quiet house as one that took
    /// minutes to react.
    pub acted_by_command: Option<bool>,
    /// How many compliance samples the evidence carries.
    pub evidence_samples: usize,
    /// How many times the charge point changed its conductor count.
    ///
    /// Each one interrupts the session while the vehicle re-negotiates, so the
    /// number is a cost as well as a capability: a controller that chases every
    /// cloud spends the afternoon switching instead of charging.
    pub phase_switches: usize,
    /// What the car was **actually** short of its target when it left, kWh.
    ///
    /// Measured on the car at its departure, not read off a plan. The two are
    /// different facts and only one of them is a failure: a plan that admits to
    /// a shortfall at nine in the morning and then makes it up by seven left
    /// nobody short, and a plan that promised everything and delivered less did.
    /// Reporting the plan's own confession as the outcome credits the optimiser
    /// for its pessimism and hides its optimism, which is the wrong way round.
    ///
    /// It is also what [`CostBreakdown::unserved_eur`] charges the day for.
    pub unmet_charge_kwh: f64,
    /// The largest shortfall any *plan* of the day admitted to, kWh.
    ///
    /// The forward-looking half of the same fact: what the household could have
    /// been told before the morning rather than after it. Above zero with
    /// [`DayResult::unmet_charge_kwh`] at zero is a plan that was pessimistic
    /// and recovered — worth knowing, and not a failure.
    pub planned_charge_shortfall_kwh: f64,
    /// Minutes in which the arbiter had no plan it was willing to follow.
    ///
    /// Should be zero for a box whose planner is running: a plan older than
    /// [`hems_realtime::ArbiterConfig::max_plan_age`] is worse than none, so the
    /// arbiter drops it — and if the planner re-solves less often than that
    /// tolerance, the house runs on the fallback for part of every cycle without
    /// anything saying so.
    pub minutes_without_a_plan: i64,
    /// Control ticks in which a device could not hold the command it was given.
    ///
    /// See [`hems_core::report::DayKpis::clipped_ticks`] — the seam between the
    /// arbiter and the hardware, which `hems_device::realisable` closes
    /// correctly and silently.
    pub clipped_ticks: i64,
    /// The energy the hardware refused over those ticks, kWh.
    pub clipped_kwh: f64,
    /// Energy the store lent the controllable devices under a § 14a ceiling, kWh.
    ///
    /// `[A1 2.3]` measures what the controllable devices draw *from the grid*,
    /// so a battery discharging into the wallbox is headroom the household owns
    /// and the Festlegung allows. This is how much of it was used — the number
    /// that says what a battery is worth on the evening of a reduction, and it
    /// is zero on a day with no reduction at all.
    pub lent_kwh: f64,
    /// Minutes the charge point spent **charging** on a single conductor.
    ///
    /// Only while a session is running: an idle wallbox keeps whichever mode it
    /// last used, which costs nothing and says nothing, and counting it would
    /// turn a night with no car into sixteen hours of single-phase charging.
    pub single_phase_minutes: i64,
    /// How the production forecast did, scored against what the roof delivered.
    ///
    /// The number that says whether the saving above means anything. A day with
    /// perfect foresight scores a CRPS of zero, and its saving is an upper bound
    /// no box in a real house can reach.
    pub pv_forecast: Calibration,
    /// The same for the household load forecast.
    pub load_forecast: Calibration,
    /// What the box learned its roof actually delivers against what the
    /// geometric model says, at midday.
    ///
    /// Below one on any real roof: soiling, shading, mismatch and module
    /// tolerance. The simulator applies it and never tells the model, so this is
    /// [`hems_forecast::ResidualModel`] finding it the way it would in the
    /// field — and a figure stuck at exactly 1,00 means the corrector is not
    /// being fed.
    pub roof_correction: f64,
    /// How many days of metering the forecasts rest on.
    pub history_days: usize,
    /// What the monitor makes of the roof after the warm-up.
    ///
    /// A verdict rather than a ratio, because that is what a household is shown
    /// and what would have to move if the monitor stopped working: a cold box
    /// answers `Learning { days: 0 }` and a warmed one `Healthy`, so the figure
    /// is not structurally constant across the reference days (R20, D199).
    pub roof_health: hems_forecast::Health,
    /// The largest **quarter-hour average** power that left the connection
    /// point, kW.
    ///
    /// The quantity § 9 EEG limits, in the resolution it limits it at. A
    /// quarter-hour average and not an instantaneous peak, because the 60 % cap
    /// is a *settlement* limit read off meter registers rather than a control
    /// instruction — and a controller that chased the instantaneous value
    /// would curtail a roof all afternoon to avoid a one-minute transient nobody
    /// meters.
    ///
    /// It is the one number that says whether a cap bound anything at all. A day
    /// that reports a peak below its own ceiling and a curtailment of zero is a
    /// day where the Solarspitzengesetz cost nothing — which is a result, not a
    /// missing feature, and telling the two apart needs this number.
    pub peak_feed_in_kw: f64,
    /// The § 9 EEG / LPP ceiling in force, kW, where there is one.
    pub feed_in_ceiling_kw: Option<f64>,
    /// The most a kilowatt-hour of relief from the § 14a ceiling was worth to
    /// this household, €/kWh — the shadow price of the network operator's own
    /// limit, taken from the plan living under it.
    ///
    /// Zero on a day with no reduction, and *also* zero on a day with one that
    /// did not bind, which is a result rather than a gap: a limit that costs a
    /// household nothing is a limit nobody should be compensated for. It is what
    /// a § 41e Aggregatorvertrag offer should be priced from.
    pub relief_eur_per_kwh: f64,
    /// The widest ratio between the most and least valuable asset in a single
    /// slot of any plan the day made.
    ///
    /// One would mean every device is worth the same to the plan, which is what
    /// the guard's "weighted" max-min allocator was actually being given until
    /// the planner started pricing assets separately — a ranking that ranks
    /// nothing. On a day with a car near its departure under a reduction it is
    /// tens.
    pub widest_asset_value_ratio: f64,
    /// Above what annual consumption Modul 2 beats the household's current
    /// module, kilowatt-hours per year.
    ///
    /// `None` where the working price is already zero, or where the day carries
    /// no consumption to price. This is [`hems_tariff::advisor`] answering the
    /// question a household actually asks and no supplier can answer
    /// neutrally — *is the module we are on the right one?* — as a **threshold**
    /// rather than as a projection. One day is not a year, and a comparison that
    /// multiplied a January Thursday by 365 would tell every household with an
    /// electric car that it is losing four figures a year.
    pub modul2_break_even_kwh_per_year: Option<f64>,
    /// What Modul 2 would have saved or cost on *this day's* energy alone,
    /// euros — the honest half of the comparison, with no annualisation in it.
    pub modul2_delta_today_eur: f64,
    /// How many of the site's assets were described to a Customer Energy
    /// Manager in S2 (EN 50491-12-2) terms, and their control types.
    ///
    /// A number here that stops matching the site's asset count is a device the
    /// S2 layer cannot describe — the first thing a real Resource Manager would
    /// find, and better found here. It is also what keeps the flexibility model
    /// a dependency rather than documentation.
    pub s2_resources: usize,
    /// Assets the S2 mapping gave a control type that this workspace cannot yet
    /// express as a description.
    ///
    /// Reported beside the count rather than folded into it: a gap that says so
    /// is a backlog item, and a gap counted as a success is a lie that survives
    /// four versions.
    pub s2_undescribed: usize,
    /// How many of the day's re-plans were made against three futures because
    /// the charging session had no slack left.
    ///
    /// Zero on a day with slack everywhere, which is what makes the adaptive
    /// policy free there — and the number that says whether the mechanism
    /// decided anything at all.
    pub risk_re_solves: usize,
    /// The day's quarter-hour meter registers, in the form the `MiSpeL` flow
    /// bookkeeping reads them, `[MiSpeL A1 4.2.1]`.
    ///
    /// These are the *registers*, not the control values: non-negative
    /// magnitudes in kilowatt-hours, exact decimals, accumulated as the day
    /// runs. They are what closes the loop between the control stack and the
    /// settlement — a manager that decides when to charge from the grid but
    /// cannot say afterwards how much of its feed-in was grey has done half the
    /// job.
    pub quarter_hours: Vec<MispelQuarterHour>,
    /// What the roof produced in each of them, kWh.
    ///
    /// Beside the registers rather than inside them, exactly as the box records
    /// it: `Z2E¼` is the storage system and the charge point's generation and
    /// not the sun's, and a day report that read one as the other told a
    /// household running off its own roof that it was self-sufficient to no
    /// degree at all (D124).
    ///
    /// **Never serialised**, and not as a convenience: a `Slot` is a struct and
    /// a JSON object's keys are strings, so writing this map is a runtime error
    /// rather than a shape somebody dislikes. It exists to pair the registers
    /// with the roof on the way into the box's own store, and the day's total is
    /// [`DayResult::produced_kwh`].
    #[serde(skip)]
    pub production_kwh_by_slot: BTreeMap<Slot, Decimal>,
    /// The day's § 14a control events, closed, each with its whole
    /// minute-resolution compliance trace — `[A1 7.2]`.
    ///
    /// # Why the record and not only the counts
    ///
    /// Every other field here is a *number about* the evidence: how many events,
    /// how many samples, whether the limit held. Those are what a fleet view and
    /// a KPI table need. What a **network operator** asks for is the record
    /// itself, and `[A1 7.3]` keeps it for two years — so the day has to carry
    /// it out, which is what `--store` writes down.
    pub evidence: Vec<hems_grid::ControlEvent>,
}

impl DayResult {
    /// Whether the shiftable appliance ran at all.
    #[must_use]
    pub fn appliance_ran(&self) -> bool {
        self.appliance_kwh > 0.0
    }

    /// This day as the fleet is told about it.
    ///
    /// The narrow record `obsd` aggregates — a dozen numbers rather than every
    /// field of this struct, because a fleet service coupled to an edge daemon's
    /// internal report is a fleet service that cannot be deployed independently
    /// of it. Both sides share [`hems_core::report::DayKpis`], so a renamed
    /// field is a compile error rather than a dashboard that reads zero for six
    /// weeks.
    #[must_use]
    pub fn kpis(&self, site: &str, date: time::Date) -> hems_core::report::DayKpis {
        hems_core::report::DayKpis {
            site: site.to_owned(),
            date,
            imported_kwh: self.imported_kwh,
            exported_kwh: self.exported_kwh,
            produced_kwh: self.produced_kwh,
            self_sufficiency: self.self_sufficiency,
            shared_kwh: self.shared_kwh,
            economics: Some(hems_core::report::Economics {
                cost: self.cost,
                // The simulator has one because it can re-run the day as an
                // unmanaged house. A box on a wall cannot (D116).
                baseline: self.baseline,
            }),
            respected_the_grid: self.grid_event_respected,
            worst_overshoot_w: self.worst_overshoot_w,
            worst_feed_in_overshoot_w: self.worst_feed_in_overshoot_w,
            minutes_without_a_plan: u32::try_from(self.minutes_without_a_plan).unwrap_or(u32::MAX),
            clipped_ticks: u32::try_from(self.clipped_ticks).unwrap_or(u32::MAX),
            clipped_kwh: self.clipped_kwh,
            control_events: self.control_events,
            below_minimum_commanded: self.commanded_below_minimum,
            forecast: Some(hems_core::report::ForecastScores {
                pv_coverage: self.pv_forecast.coverage,

                pv_crps: self.pv_forecast.crps,

                load_coverage: self.load_forecast.coverage,

                load_crps: self.load_forecast.crps,
            }),
            foresight_was_perfect: self.foresight_is_perfect,
        }
    }

    /// What the optimisation saved against the baseline, euros — every term
    /// counted on both sides.
    #[must_use]
    pub fn saving_eur(&self) -> f64 {
        self.baseline.total() - self.cost.total()
    }

    /// What it saved on the electricity bill alone.
    ///
    /// Always the larger number, which is why it is not the headline.
    #[must_use]
    pub fn bill_saving_eur(&self) -> f64 {
        self.baseline.billed_eur() - self.cost.billed_eur()
    }
}

/// The reference household's own devices, unwrapped once.
///
/// The simulation harness runs the reference household, which has every asset —
/// its simulators, its baseline and its KPI table are built around that house.
/// A configuration that dropped one is a different household than the days were
/// calibrated on, and the harness says so instead of simulating a device that
/// is not there. (`hemsd run` has no such precondition: the running box builds
/// exactly the assets its configuration describes.)
struct Reference {
    /// The roof — a dark one of no size where the household has none, because
    /// a simulator can honestly run a roof that never produces, where it cannot
    /// honestly run a battery that is not there.
    pv: crate::site::PvConfig,
    battery: crate::site::BatteryConfig,
    evse: crate::site::EvseConfig,
    heat_pump: crate::site::HeatPumpConfig,
    dhw: crate::site::DhwConfig,
}

impl Reference {
    fn of(config: &HouseholdConfig) -> anyhow::Result<Self> {
        let missing = |what: &'static str| {
            anyhow::anyhow!(
                "the simulation harness runs the reference household, which has a {what}; \
                 a household without one is a `hemsd run` configuration, not a scenario"
            )
        };
        Ok(Self {
            pv: config.pv.unwrap_or(crate::site::PvConfig {
                kwp: Power::ZERO,
                ac_nominal: Power::ZERO,
                tilt_deg: 0.0,
                azimuth_deg: 180.0,
                para9: Para9Status::default(),
            }),
            battery: config.battery.ok_or_else(|| missing("battery"))?,
            evse: config.evse.ok_or_else(|| missing("charge point"))?,
            heat_pump: config.heat_pump.ok_or_else(|| missing("heat pump"))?,
            dhw: config.dhw.ok_or_else(|| missing("hot-water tank"))?,
        })
    }
}

/// The reference household's asset identifiers, unwrapped once — see
/// [`Reference`].
struct ReferenceIds {
    /// Present even where the site has no roof, for the same reason
    /// [`Reference::pv`] is: a measurement recorded under an identifier no
    /// asset carries is ignored by the guard, which is exactly what a dark
    /// roof of no size amounts to.
    pv: AssetId,
    battery: AssetId,
    evse: AssetId,
    heat_pump: AssetId,
    dhw: AssetId,
}

impl ReferenceIds {
    fn of(household: &Household) -> anyhow::Result<Self> {
        let take = |id: &Option<AssetId>, what: &'static str| {
            id.clone().ok_or_else(|| {
                anyhow::anyhow!("the reference household names its {what}, and this site has none")
            })
        };
        Ok(Self {
            pv: match &household.pv {
                Some(id) => id.clone(),
                None => AssetId::new("pv")?,
            },
            battery: take(&household.battery, "battery")?,
            evse: take(&household.evse, "charge point")?,
            heat_pump: take(&household.heat_pump, "heat pump")?,
            dhw: take(&household.dhw, "hot-water tank")?,
        })
    }
}

/// Run one day and report what happened.
///
/// The control loop runs once a minute and the planner every quarter of an hour,
/// which is the same shape a real box uses — fast enough to track a cloud, slow
/// enough that a solver is affordable.
///
/// # Errors
/// When the household described by the scenario is not a valid site — a
/// duplicate asset name, or an asset on a circuit that does not exist — or when
/// it belongs to a § 42c community on a day the rule does not reach.
///
/// # Panics
/// When a scenario declares a charging session but the simulated charge point
/// has no vehicle. The two are built together in this function, so that is a
/// programming error rather than a runtime condition.
#[allow(clippy::too_many_lines)]
pub fn run(scenario: &Scenario) -> anyhow::Result<DayResult> {
    // The boundary where a § 42c allocation becomes a **claim about money**, and
    // therefore where the date is checked — not inside `allocate_by`, whose
    // arithmetic is defined by the community's own contract and which the capped
    // day asks a pure conservation question of. It is the shape `histd`'s MiSpeL
    // export already uses for the sibling rule.
    //
    // A day dated before a network operator's duty begins would settle an
    // allocation nobody would perform and credit the household for it. Two of
    // these days did, and reported it, because `sharing::applies_on` existed and
    // was called by nothing (D179).
    if scenario.community.is_some() && !hems_grid::sharing::applies_on(scenario.date) {
        anyhow::bail!(
            "this household is in a § 42c community on {}, and a network operator's \
             duty to allocate begins on {} — `Scenario::on_a_day_sharing_reaches` \
             moves the day forward in whole weeks, so the weekday and the season \
             survive",
            scenario.date,
            hems_grid::sharing::SHARING_START,
        );
    }
    let household = Household::build(&scenario.config)?;
    let house = Reference::of(&scenario.config)?;
    let ids = ReferenceIds::of(&household)?;
    let site = &household.site;
    let start = scenario.start();
    let day = Horizon::new(start, 96);

    // ── The physical house ──────────────────────────────────────────────────
    // The roof the *simulator* runs is the roof the site model describes, down to
    // the same two numbers. A second copy of 35°/180° here would be a house that
    // could disagree with the one the box plans against, and the disagreement
    // would present as a forecast error the residual corrector spent the day
    // learning.
    let array = ArrayModel::new(
        house.pv.kwp,
        house.pv.ac_nominal,
        house.pv.tilt_deg,
        house.pv.azimuth_deg,
    );
    let mut building = BuildingSim {
        nominal_electrical: house.heat_pump.power,
        cooling_electrical: house.heat_pump.cooling_electrical.unwrap_or(Power::ZERO),
        // The same floor the planner is given, so the plan and the house agree
        // about what "on" is worth for a unit whose slots are scheduled.
        min_electrical: house.heat_pump.power * 0.3,
        thermostat_set_c: house.heat_pump.comfort_min_c + 0.5,
        thermostat_max_c: house.heat_pump.comfort_max_c,
        // A single-speed compressor where the household has one, so a day can
        // report what the planner's minimum runtime actually bought. Without it
        // the constraint is stated in every plan and observed by nothing.
        compressor: (!house.heat_pump.modulating).then(compressor_sim),
        ..BuildingSim::new(21.0)
    };
    let mut inverter = PvSim::default();
    let stored_at_start = |battery: &BatterySim, tank: &TankSim| -> f64 {
        // Electrical energy in the battery, plus the electricity it would take
        // to put the tank's heat back.
        battery.stored.kwh() + tank.stored.kwh() / tank.cop.max(f64::EPSILON)
    };
    let tank_asset = match site.asset(&ids.dhw) {
        Some(Asset::Dhw(t)) => t.clone(),
        _ => unreachable!("the household always has a tank"),
    };
    let mut tank = TankSim {
        cop: tank_asset.cop,
        standing_loss: tank_asset.standing_loss,
        ..TankSim::new(tank_asset.usable_heat(), tank_asset.heater)
    };
    let mut dishwasher = scenario
        .config
        .dishwasher
        .clone()
        .map(hems_sim::ApplianceSim::new);
    let mut battery = BatterySim::new(house.battery.kwh, house.battery.power);
    // Thirty per cent, or the backup reserve if the household asked for more: a
    // house that boots below its own promise is a real case, but it is not the
    // one these days are about, and starting there makes every reserve figure
    // look like it was broken by a standing loss.
    battery.stored = house.battery.kwh * house.battery.reserve_soc.fraction().max(0.30);
    let mut evse = EvseSim::new();
    if house.evse.switchable {
        evse = evse.switchable();
    }
    let mut evse = evse.with_vehicle(VehicleSim {
        capacity: VEHICLE_CAPACITY,
        stored: scenario.ev.map_or(Energy::from_kwh(18.0), |e| e.energy_now),
        efficiency: 0.92,
        efficiency_single_phase: SINGLE_PHASE_EFFICIENCY,
        max_charge: Power::from_kw(11.0),
    });

    // ── The network operator ────────────────────────────────────────────────
    let mut steuerbox = SteuerboxSim::quiet();
    if let Some((from, until, limit)) = scenario.grid_event {
        steuerbox = steuerbox.with_event(start + from, start + until, limit);
    }
    if let Some((from, until)) = scenario.steuerbox_outage {
        steuerbox = steuerbox.with_outage(start + from, start + until);
    }
    // The value the box holds itself at when it cannot hear an Energy Guard
    // (`[LPC-022]`, `[LPC-901]`). It is the household's own § 14a minimum and
    // not a flat 4,2 kW: a failsafe is a *substitute* for an instruction, and
    // restraining the house below what the operator itself may lawfully command
    // — on nothing more than a lost heartbeat — is a configuration fault dressed
    // up as caution. `[A1 4.5.2]`'s minimum grows with the number of
    // controllable devices, and this household is owed 10,5 kW.
    let failsafe_limit = hems_grid::para14a::minimum_power(
        &hems_grid::classify_at(&site.assets, start),
        hems_grid::para14a::ControlMode::Ems,
    )
    .max(hems_grid::para14a::MINDESTLEISTUNG);
    // **The machine a real box runs**, driven directly rather than through a
    // socket. It was `hems_grid::lpc::LpcMachine` — a second implementation of
    // the same certifiable state machine — so every § 14a figure the reference
    // days produced came from a machine no household has (D186).
    let mut lpc = hems_drv::eebus::Lpc::new(
        household.grid_meter.clone(),
        hems_drv::eebus::Use::Lpc,
        failsafe_limit,
        // The Failsafe Duration Minimum `[LPC-022]` allows, from the crate that
        // enforces the range rather than from a constant restated here.
        *hems_drv::eebus::FAILSAFE_DURATION_RANGE.start(),
        start,
    );

    // ── Prices and forecasts ────────────────────────────────────────────────
    let mut tariff = crate::site::tariff_for(site, &scenario.prices_ct, day);
    // § 42c Abs. 3 Nr. 3: the community's own price replaces the supplier's
    // energy component on the allocated kilowatt-hours, and nothing else.
    if let Some(c) = scenario.community {
        tariff = tariff.in_community(hems_tariff::tariff::SharingTariff::at(c.price_ct));
    }
    let prices = PriceStack::build(&tariff, day);
    // The § 51 EEG hours this day actually contained. A property of the tariff
    // rather than of the plan — nobody can move a negative auction — and the
    // number that says whether the rule bound at all on a day written around
    // it.
    let para51_hours = prices.negative_price_slots().len();

    // The neighbours' roofs, under the same sky as this one — and the meter that
    // watches them. The *same* meter the unmanaged household is given, so the
    // two sides of the comparison cannot disagree about what a quarter hour of
    // community generation was. See [`CommunityMembership`] for why the
    // neighbours are not given this household's own learned correction.
    let mut community = CommunityMeter::for_scenario(scenario);

    // ── What the box knows, and what it is about to find out ────────────────
    //
    // The weather the day *has*, and the three weeks of metering the box has
    // behind it. Nothing here is the series the simulator is about to run: the
    // planner gets the geometric roof model corrected by what this roof has
    // actually been delivering, this household's own load profile, and — until
    // the cable goes in — a charging session predicted from previous weeks.
    //
    // Handing the planner the simulator's own curves instead would make the
    // forecast unable to be wrong, and every saving a perfect-foresight one.
    let weather = Weather::new(scenario.weather, scenario.cloudiness, scenario.outdoor_c);
    let learned = crate::forecasting::warm_up(
        scenario.warm_up_days,
        &weather,
        &array,
        site.location,
        start,
        household_load,
        scenario
            .ev
            .map(|e| (e.arrival, e.departure, e.energy_target - e.energy_now)),
    );

    // Two days of forecast, not one. The planner's horizon is 24 h, so a
    // re-plan at six in the evening runs to six the next evening — and a
    // forecast that stops at midnight told it there would be no sun *and no
    // load* tomorrow, which is a lie in both directions and one the terminal
    // value only partly hides.
    let full = Horizon::new(start, 96 * 2);
    let pv_forecast =
        crate::forecasting::pv_forecast(&learned, &weather, &array, site.location, full);
    // A cold box plans from its own meter, exactly as `runtime::planner` does —
    // one function, so the simulated first evening is the one a household
    // actually gets (D187). The reading is the household's own underlying load
    // at midnight, which is what a box with a connection-point meter sees.
    let load_forecast = crate::forecasting::load_forecast(
        &learned.load,
        full,
        Some(household_load(Slot::containing(start))),
    )
    .ok_or_else(|| anyhow::anyhow!("a box with no history and no meter cannot plan"))?;

    // What the forecasts turn out to have been worth, scored against what
    // happened. A day whose scores are zero is a day with perfect foresight, and
    // its saving is an upper bound rather than a result.
    let mut pv_scored: Vec<(Band, f64)> = Vec::new();
    let mut load_scored: Vec<(Band, f64)> = Vec::new();

    // ── The control loop ────────────────────────────────────────────────────
    // The control loop runs once a minute here, and the guard has to know: a
    // bound on a state is only a bound on a rate once you know how long the rate
    // is held for.
    let arbiter = Arbiter::new(ArbiterConfig {
        guard: hems_realtime::GuardConfig {
            tick_period: CONTROL_PERIOD,
            ..hems_realtime::GuardConfig::default()
        },
        ..ArbiterConfig::default()
    });
    let mut evidence = EvidenceRecorder::new();
    let mut plan: Option<Plan> = None;
    // Whether the plan in force still has a charging session in it.
    let mut ev_in_plan = false;
    let mut previous: BTreeMap<AssetId, Power> = BTreeMap::new();
    // The previous plan's discrete decisions, with the slot they were made in so
    // the next solve can shift them onto its own horizon.
    let mut last_commitment: Option<(Slot, hems_optimizer::Commitment)> = None;
    // Production and load accumulated over the quarter hour in progress, so the
    // forecast can be scored against what a meter would have registered rather
    // than against an instant.
    let mut pv_slot_wh = 0.0_f64;
    let mut load_slot_wh = 0.0_f64;
    let mut slot_covered = Duration::ZERO;
    // Energy each asset has moved since the start of the current quarter hour.
    // The arbiter follows the plan's *energy*, so somebody has to count it, and
    // on a real box that somebody is the driver layer.
    let mut delivered: BTreeMap<AssetId, Energy> = BTreeMap::new();
    let mut delivered_slot = Slot::containing(start);
    let mut registers: std::collections::BTreeMap<Slot, MispelQuarterHour> =
        std::collections::BTreeMap::new();
    // The arbiter's conductor policy, carried from tick to tick the same way the
    // previous setpoints are.
    let mut phase_state: BTreeMap<AssetId, hems_realtime::PhaseState> = BTreeMap::new();
    let overrides = BTreeMap::new();

    let stores_open = stored_at_start(&battery, &tank);
    // How long each state lasted, accumulated as **durations**.
    //
    // These were five `+= 1` counters over a loop whose step is
    // [`CONTROL_PERIOD`], so every one of them reported minutes only for as long
    // as nobody changed that constant — and a constant that cannot be changed is
    // not a parameter, it is a number written in five places. Summing the step
    // makes the period a parameter again, which is what lets a day be run at the
    // cadence a real box actually ticks at (D140).
    let mut single_phase = Duration::ZERO;
    let mut without_a_plan = Duration::ZERO;
    let mut failsafe_for = Duration::ZERO;
    let mut limited_for = Duration::ZERO;
    let mut feed_in_over = Duration::ZERO;
    // The previous period's uncommanded net export, so the step between two
    // guard decisions can be measured rather than argued about.
    let mut previous_uncommanded_export: Option<Power> = None;
    // What the connection point saw last tick. The only thing on this loop that
    // reads it is the offline fallback for a shiftable appliance, which has to
    // decide before the current tick's balance exists.
    let mut last_grid = Power::ZERO;

    let mut result = DayResult {
        grid_event_respected: true,
        para51_hours,
        // Seeded so the first sample sets both ends; a zero start would report a
        // freezing house that never happened.
        indoor_min_c: f64::INFINITY,
        indoor_max_c: f64::NEG_INFINITY,
        battery_soc_min: f64::INFINITY,
        tank_min_fill: f64::INFINITY,
        ..DayResult::default()
    };
    let step = CONTROL_PERIOD;
    let mut now = start;
    // When the ceiling in force now first applied. A reason chain that says
    // "since 17:02" is worth something; one that says "since now" on every tick
    // is worth nothing, and it is the same field either way.
    let mut ceiling_since: Option<OffsetDateTime> = None;
    let mut previous_ceiling: Option<Power> = None;

    while now < start + Duration::days(1) {
        // The Steuerbox speaks.
        for command in steuerbox.poll(now) {
            apply_to_the_machine(&mut lpc, command, now);
        }
        lpc.on_timeout(now);
        let ceiling = lpc.ceiling();
        if ceiling != previous_ceiling {
            ceiling_since = ceiling.map(|_| now);
            previous_ceiling = ceiling;
        }
        // `init` and `failsafe` both hold the device at its own preconfigured
        // value because contact with the Energy Guard is missing — that is the
        // manager restraining itself, not a network operator reducing the house,
        // and the evidence record has to say which of the two happened.
        // § 9 EEG travels with the site rather than with an event: a system in
        // the size class keeps the 60 % cap until an intelligent metering system
        // with a control device is in operation. So it is **not** handed to the
        // guard here — the guard derives it from the site on every tick, which
        // is the only way a real box gets it too, and a simulator that fed it in
        // would be testing a path production does not have.
        result.feed_in_ceiling_kw =
            hems_grid::para9::site_feed_in_ceiling(site, None, None).map(|(p, _)| p.kw());
        let limits = GridLimits {
            steuve_ceiling: ceiling,
            steuve_since: ceiling_since,
            in_failsafe: !lpc.is_controlled(),
            // What an *operator* has asked for, which in this scenario is
            // nothing. The statute is the guard's own business.
            feed_in_ceiling: None,
            feed_in_rule: None,
            mgcp_factor: None,
        };

        // Re-plan on [`REPLAN_PERIOD`] **or when something the plan assumed
        // stops being true**. A plan made while the car still needed 3 kWh keeps
        // asking for them after the car is full, and a quarter of an hour of
        // that is most of a kilowatt-hour of the evening tariff bought for
        // nothing. The design has always said "every 15 min or on event"; the
        // car reaching its target is an event.
        let car_finished = scenario
            .ev
            .zip(evse.vehicle.as_ref())
            .is_some_and(|(ev, v)| v.stored >= ev.energy_target);
        if scenario.planner
            && (now == start
                || (now - start).whole_minutes() % REPLAN_PERIOD.whole_minutes() == 0
                || (car_finished && ev_in_plan))
        {
            let horizon = Horizon::new(now, 96);
            let horizon_prices = PriceStack::build(&tariff, horizon);
            let horizon_draw: Vec<f64> = horizon
                .slots()
                .map(|s| hems_forecast::hotwater::draw(s).get())
                .collect();
            // What the community expects to be able to allocate this member, slot
            // by slot. Zero-length where the household is not in one, which is
            // what leaves the model exactly as it was.
            let horizon_share: Vec<f64> = community.forecast(&weather, site.location, horizon);
            // The temperature the planner plans against: the diurnal shape,
            // without the day's own error, aligned with *this* horizon. Per slot
            // because a heat pump's coefficient of performance is linear in it —
            // a flat number prices the coldest hour of the night and the mildest
            // of the afternoon identically, and pre-heating into the warm part
            // of the day is half of what a heating plan is for.
            let horizon_outdoor: Vec<f64> = horizon
                .slots()
                .map(|s| weather.forecast_outdoor_at(s.start()))
                .collect();
            // And the sun on the windows the *forecast* expects — the mean cloud,
            // not this day's own. The house will get whatever it gets; the plan
            // is made against what a box could have known, which is the whole
            // discipline of D35.
            let horizon_window: Vec<f64> = horizon
                .slots()
                .map(|s| weather.forecast_window_at(site.location, s, FACADE_AZIMUTH_DEG))
                .collect();
            let mut problem = Problem::new(horizon, &horizon_prices, &pv_forecast, &load_forecast)
                .with_battery(BatteryModel {
                    capacity: battery.capacity,
                    soc_now: battery.soc(),
                    max_charge: battery.max_charge,
                    max_discharge: battery.max_discharge,
                    efficiency_charge: battery.efficiency_charge,
                    efficiency_discharge: battery.efficiency_discharge,
                    soc_min: battery.soc_min,
                    soc_max: battery.soc_max,
                    reserve_soc: house.battery.reserve_soc,
                    degradation_eur_per_kwh: house.battery.wear_eur_per_kwh,
                    grid_charging_allowed: true,
                })
                .with_thermal(
                    ThermalModel {
                        state: building.state,
                        building: building.building,
                        comfort_min_c: house.heat_pump.comfort_min_c,
                        comfort_max_c: house.heat_pump.comfort_max_c,
                        discomfort_eur_per_kelvin_hour: DISCOMFORT_EUR_PER_KELVIN_HOUR,
                        heat_pump: HeatPumpModel {
                            modulating: house.heat_pump.modulating,
                            cop: building.cop,
                            // What the compressor is actually doing, not what a
                            // fresh model would assume. A receding horizon
                            // commits its first slot and throws the rest away,
                            // so the minimum-runtime rows — which constrain a
                            // transition and therefore need a previous slot —
                            // never reach the one slot that gets executed. This
                            // is the only thing that closes that boundary.
                            compressor: building
                                .compressor
                                .as_ref()
                                .map(hems_sim::CompressorSim::state)
                                .unwrap_or_default(),
                            max_cooling: house.heat_pump.cooling_electrical.unwrap_or(Power::ZERO),
                            ..HeatPumpModel::modulating(house.heat_pump.power)
                        },
                    },
                    &horizon_outdoor,
                    &horizon_window,
                )
                .with_dhw(
                    DhwModel {
                        capacity: tank_asset.usable_heat(),
                        stored_now: tank.stored,
                        heater: tank_asset.heater,
                        cop: tank_asset.cop,
                        standing_loss: tank_asset.standing_loss,
                        thermostat_set: tank_asset.stored_heat(tank_asset.t_set_c),
                        shortfall_eur_per_kwh: HOT_WATER_SHORTFALL_EUR_PER_KWH,
                    },
                    &horizon_draw,
                )
                .with_limits(crate::site::planning_limits(
                    &limits,
                    lpc.limit_ends_at(),
                    site,
                    now,
                ))
                .with_objective(scenario.objective)
                .in_community(&horizon_share);

            // The dishwasher, while there is still a decision to make about it.
            // Once it is running the decision is spent — a programme cannot be
            // moved after it has started, and a planner that kept re-placing one
            // would be planning a machine that does not exist. Once it has
            // finished there is nothing left to place at all.
            if let Some(sim) = dishwasher
                .as_ref()
                .filter(|d| !d.is_running() && !d.is_finished())
                && let Some((from, until)) = scenario.dishwasher
            {
                let earliest = Slot::containing((start + from).max(now));
                let deadline = Slot::containing(start + until);
                if deadline > earliest {
                    problem = problem.with_shiftable(hems_optimizer::ShiftableRun {
                        programme: sim.programme.clone(),
                        earliest: Some(earliest),
                        deadline: Some(deadline),
                        unserved_eur: UNRUN_PROGRAMME_EUR,
                    });
                }
            }

            // The car. Two regimes, and telling them apart is the difference
            // between a forecast and an oracle:
            //
            // * **before the cable goes in**, the planner gets what the box has
            //   *learned* — when this car usually comes home, how empty, and
            //   when it usually leaves — or nothing at all where three weeks of
            //   history do not support a prediction. It does not get the
            //   household's actual plans for the evening, which the box has no
            //   way of knowing.
            // * **from the moment it is plugged in**, the session is a fact: the
            //   vehicle reports its charge and the household has said when it
            //   needs the car.
            //
            // Collapsing the first case into the second — the exact arrival,
            // departure and target handed to every plan from midnight — would
            // make a scenario built to test a car arriving mid-reduction test a
            // planner that had already been told.
            if let Some(ev) = scenario.ev
                && let Some(Asset::Evse(evse_asset)) = site.asset(&ids.evse)
            {
                let plugged = now >= start + ev.arrival;
                let predicted =
                    crate::forecasting::session_at(&learned, now, start, start + ev.arrival);
                // A predicted session only while the cable is still out; once it
                // is in, the household's own window and the vehicle's own charge
                // are facts and the prediction is not consulted again.
                let (arrival, departure, needed) = match predicted.filter(|_| !plugged) {
                    Some(f) => (f.arrival, f.departure, f.energy),
                    None => (
                        Slot::containing(start + ev.arrival),
                        Slot::containing(start + ev.departure),
                        Energy::ZERO,
                    ),
                };
                let vehicle = evse.vehicle.as_ref().expect("a car is plugged in");
                let energy_now = if plugged {
                    vehicle.stored
                } else {
                    (vehicle.capacity - needed).max(Energy::ZERO)
                };
                let energy_target = if plugged {
                    ev.energy_target
                } else {
                    vehicle.capacity
                };
                let worth_planning = if plugged {
                    vehicle.stored < ev.energy_target
                } else {
                    needed > Energy::ZERO
                };
                if departure > Slot::containing(now) && worth_planning {
                    problem = problem.with_ev(hems_optimizer::model::EvSession {
                        energy_now,
                        energy_target,
                        capacity: vehicle.capacity,
                        max_charge: vehicle
                            .max_charge
                            .min(evse_asset.max_power(PhaseMode::Three)),
                        // The minimum in the mode the plan *assumes*, which is
                        // the wiring's default — deliberately not the mode the
                        // contactor happens to be in. Taking the current one
                        // couples the plan to the contactor and the contactor to
                        // the plan: a plan made in single-phase mode asks for
                        // single-phase amounts, which keeps it in single-phase
                        // mode, which keeps the plan small. Taking the *lowest*
                        // of the two is worse still — it lets the plan dribble
                        // through hours it did not need to use.
                        //
                        // Phase switching is not the planner's lever. It is the
                        // arbiter's, and it earns its keep exactly where the
                        // guard has cut the charge point below what three
                        // conductors can start on: a § 14a reduction, a circuit
                        // limit, or a surplus with no plan behind it.
                        min_charge: evse_asset.min_power(PhaseMode::Three),
                        efficiency: vehicle.efficiency,
                        arrival: (arrival > Slot::containing(now)).then_some(arrival),
                        departure,
                    });
                }
            }

            // No wall-clock budget: a simulated day has to give the same answer
            // twice, and a budget measured against the clock makes the plan
            // depend on how busy the machine was. The relative gap stays — it is
            // a property of the search rather than of the clock.
            problem.solve_budget_s = 0.0;
            problem.shadow_prices = scenario.per_asset_weights;
            problem.risk = scenario.risk;
            // The previous plan, slid forward by the quarter hours that have
            // gone by since it was made. Held in a binding rather than passed
            // inline because the problem borrows it for the length of the solve.
            let warm = last_commitment.as_ref().map(|(made_at, c)| {
                let elapsed = usize::try_from(made_at.distance_to(Slot::containing(now)))
                    .unwrap_or(usize::MAX);
                c.shifted(elapsed)
            });
            if let Some(w) = warm.as_ref() {
                problem = problem.with_warm_start(w);
            }
            ev_in_plan = problem.ev.is_some();
            // A session that needs more than `TIGHT_SESSION` of the capacity it
            // has left is a day worth planning against more than one future.
            if scenario.adaptive_risk
                && problem
                    .ev
                    .is_some_and(|e| e.tightness(Slot::containing(now)) > TIGHT_SESSION)
            {
                problem.risk = hems_optimizer::Risk::hedged();
                result.risk_re_solves += 1;
            }
            match solve(&problem, &household.names, now) {
                Ok(solved) => {
                    // A shortfall is not an error and not a silence: the plan
                    // that came back is the best achievable, and it is short.
                    result.planned_charge_shortfall_kwh = result
                        .planned_charge_shortfall_kwh
                        .max(solved.unmet_charge.kwh());
                    // What a kilowatt-hour of relief from the § 14a ceiling
                    // would have been worth to this household — the shadow price
                    // of the ceiling itself, from the plan that is living under
                    // it. It is the honest answer to "what is your flexibility
                    // worth", and the number a § 41e offer should be priced from
                    // instead of the thirty-per-cent-of-nominal an aggregator
                    // assumes.
                    for slot in &solved.plan.slots {
                        if let Some(v) = slot.flexibility_eur_per_kwh {
                            result.relief_eur_per_kwh = result.relief_eur_per_kwh.max(v);
                        }
                    }
                    // And the spread of per-asset values in the slot being
                    // commanded: the number that says the weighted allocator is
                    // weighting something. One value for every device is a
                    // ranking that ranks nothing.
                    if let Some(sp) = solved.plan.slot_at(now) {
                        let values: Vec<f64> = sp
                            .targets
                            .iter()
                            .filter_map(|t| t.marginal_eur_per_kwh)
                            .collect();
                        if let (Some(lo), Some(hi)) = (
                            values.iter().copied().reduce(f64::min),
                            values.iter().copied().reduce(f64::max),
                        ) && lo > 0.0
                        {
                            result.widest_asset_value_ratio =
                                result.widest_asset_value_ratio.max(hi / lo);
                        }
                    }
                    // What the next re-plan starts from. Consecutive plans
                    // differ by one slot of new information and agree about
                    // almost everything else; handing the solver the previous
                    // schedule gives it an incumbent to prune against from the
                    // first node instead of rediscovering the day.
                    last_commitment = Some((Slot::containing(now), solved.commitment));
                    // What the plan that **opens** the day expects the day's
                    // bill to be. Its horizon is exactly the ninety-six slots of
                    // the day, so the two are the same question asked twice: once
                    // of a forecast and once of a meter. The gap between them is
                    // the seam nothing else in the workspace can see, and it only
                    // became a meaningful number when the plan and the day
                    // stopped being given the same weather.
                    if now == start {
                        result.opening_plan_bill_eur = solved
                            .plan
                            .expected_cost
                            .as_ref()
                            .map(hems_core::prelude::CostBreakdown::billed_eur);
                    }
                    plan = Some(solved.plan);
                }
                Err(_) => plan = None,
            }
        }

        // What the house is actually doing. The roof answers what it was told
        // last tick, because a curtailment command nothing on the other end
        // obeys makes every feed-in limit untestable end to end.
        let slot = Slot::containing(now);
        // What the day is actually doing — the realised cloud, the realised
        // temperature and the soiling the forecast model has never been told
        // about. None of it is what the planner was given.
        let available = weather.production_at(&array, site.location, now);
        let outdoor_now = weather.outdoor_at(now);
        let ceiling = previous
            .get(&ids.pv)
            .copied()
            .map_or(available, Power::outflow);
        let (pv_now, _) = inverter.step(available, ceiling, step);
        let load_now = weather.load_at(now, household_load(slot));

        // The dishwasher. It is *scheduled*, never modulated: the plan names the
        // quarter hour, the machine runs its own programme from there, and the
        // arbiter never sees it — an appliance with no consumption ceiling to
        // give is not something a guard can bargain with, and pretending
        // otherwise would let a reduction count on a kilowatt that kept flowing.
        // Its power reaches the guard the only way it truthfully can: as a
        // measurement, in the household's own load.
        let appliance_now = match dishwasher.as_mut() {
            Some(sim) => {
                let asked = household
                    .dishwasher
                    .as_ref()
                    .zip(scenario.dishwasher)
                    .is_some_and(|(id, (from, until))| {
                        // The household's own window, and only where the whole
                        // programme fits inside it — the same test the
                        // comparison is held to.
                        if from + sim.programme.duration() > until || now < start + from {
                            return false;
                        }
                        match plan.as_ref().and_then(|p| p.slot_at(now)) {
                            Some(sp) => sp.target(id).is_some_and(|t| t.power > Power::ZERO),
                            // ── no plan, and the dishes still have to be
                            // washed ──
                            //
                            // Waiting for a plan that is not coming is what this
                            // did for exactly one afternoon, and the offline day
                            // reported a household that had given up washing up
                            // and a **negative** saving — because the comparison
                            // ran the machine and the box did not. A box with no
                            // planner owes the household the behaviour every
                            // appliance timer has always had: start it when the
                            // sun is out, and no later than the last moment that
                            // still finishes inside the window.
                            None => {
                                last_grid.outflow() >= sim.programme.power_at(0)
                                    || now + sim.programme.duration() >= start + until
                            }
                        }
                    });
                sim.step(asked, now - start, step)
            }
            None => Power::ZERO,
        };
        result.appliance_kwh += appliance_now.kw() * step.as_seconds_f64() / 3600.0;

        let mut state = SiteState::default();
        if let Some(id) = &household.dishwasher {
            state
                .assets
                .insert(id.clone(), Measurement::power(now, appliance_now));
        }
        for (id, power) in [
            (&ids.pv, pv_now),
            (&household.load, load_now),
            (
                &ids.battery,
                previous.get(&ids.battery).copied().unwrap_or(Power::ZERO),
            ),
            (
                &ids.evse,
                previous.get(&ids.evse).copied().unwrap_or(Power::ZERO),
            ),
        ] {
            state
                .assets
                .insert(id.clone(), Measurement::power(now, power));
        }
        // What the roof *could* do, which is the one thing a curtailed inverter
        // cannot be asked for indirectly: read its output and a controller
        // learns only what it already commanded.
        if let Some(m) = state.assets.get_mut(&ids.pv) {
            m.available_power = Some(available);
        }
        // The guard needs the state of charge as well as the power: a full
        // battery must not be told to charge and one at its backup reserve must
        // not be told to discharge, and both happen between re-plans.
        if let Some(m) = state.assets.get_mut(&ids.battery) {
            m.soc = Some(battery.soc());
        }
        // And the *car's*, on the charge point, which is where a vehicle's charge
        // reaches a real box. Without it the surplus fallback cannot know the car
        // has had what it was asked for.
        if let Some(m) = state.assets.get_mut(&ids.evse)
            && let Some(v) = evse.vehicle.as_ref()
            && v.capacity > Energy::ZERO
        {
            m.soc = Soc::new(v.stored.kwh() / v.capacity.kwh()).ok();
        }
        state.assets.insert(
            ids.dhw.clone(),
            Measurement::power(now, previous.get(&ids.dhw).copied().unwrap_or(Power::ZERO)),
        );
        state.assets.insert(
            ids.heat_pump.clone(),
            Measurement::power(
                now,
                previous.get(&ids.heat_pump).copied().unwrap_or(Power::ZERO),
            ),
        );
        // What the charge point's contactor is actually in. The guard bounds a
        // device by what it *is* doing, not by what it was told to do — a
        // single-phase session is a Schieflast and a three-phase one is not.
        state.phases.insert(ids.evse.clone(), evse.mode);
        let grid_now: Power = state.assets.values().filter_map(|m| m.power).sum();
        state.grid = Some(Measurement::power(now, grid_now));

        if slot != delivered_slot {
            // Score the quarter hour that just ended: what the forecasts said
            // against what the house did. This is the number that says whether
            // the saving below means anything.
            score_slot(
                &pv_forecast,
                &load_forecast,
                delivered_slot,
                slot_covered,
                pv_slot_wh,
                load_slot_wh,
                &mut pv_scored,
                &mut load_scored,
            );
            pv_slot_wh = 0.0;
            load_slot_wh = 0.0;
            slot_covered = Duration::ZERO;
            delivered.clear();
            delivered_slot = slot;
        }
        pv_slot_wh += available.get() * step.as_seconds_f64() / 3600.0;
        load_slot_wh += load_now.inflow().get() * step.as_seconds_f64() / 3600.0;
        slot_covered += step;

        let decision = arbiter.tick(Tick {
            now,
            site,
            state: &state,
            limits: &limits,
            plan: plan.as_ref(),
            overrides: &overrides,
            // A simulated day has no Customer Energy Manager on it: the reference
            // days measure *this* box's planner against a household, and a second
            // optimiser in the loop would be measuring two things at once. The
            // arbiter's S2 voice is tested where it is decided, in
            // `hems-realtime`, and end to end in `tests/managed_by_a_cem.rs`.
            cem: &BTreeMap::new(),
            previous: &previous,
            delivered: &delivered,
            phases: &phase_state,
        });

        // The hardware answers. The contactor first: changing the conductor
        // count while current is flowing is what welds one shut.
        if let Some(next) = decision.phases.get(&ids.evse)
            && evse.set_mode(next.mode)
        {
            result.phase_switches += 1;
        }
        phase_state = decision.phases.clone();

        let battery_actual = battery.step(
            decision
                .commanded
                .get(&ids.battery)
                .copied()
                .unwrap_or(Power::ZERO),
            step,
        );
        // A charge point with no car on it takes nothing, whatever it is told.
        // Modelling that is what makes an arrival time mean anything.
        let plugged_in = scenario
            .ev
            .is_none_or(|ev| now >= start + ev.arrival && now < start + ev.departure);
        let evse_actual = evse.step(
            if plugged_in {
                decision
                    .commanded
                    .get(&ids.evse)
                    .copied()
                    .unwrap_or(Power::ZERO)
            } else {
                Power::ZERO
            },
            step,
        );
        // Which way the unit is running comes from the **plan**, because it is
        // the only layer that knows: the arbiter narrows a power and the meter
        // cannot tell heat from cold afterwards. A household whose unit cannot
        // cool never sees `Cool`, because the planner's cooling variable is
        // bounded at zero (D202).
        let commanded_hp = decision
            .commanded
            .get(&ids.heat_pump)
            .copied()
            .unwrap_or(Power::ZERO);
        let window_now = weather.window_at(site.location, now, FACADE_AZIMUTH_DEG);
        // The **arbiter's** direction, not the plan's. They are the same number
        // until they are not: the arbiter drops a plan older than
        // `max_plan_age` and falls back, and a simulator reading the plan
        // directly would go on cooling a house the box had stopped commanding —
        // a harness modelling a path production does not run (D187).
        let cooling = decision
            .thermal
            .get(&ids.heat_pump)
            .copied()
            .unwrap_or_default()
            == hems_core::prelude::ThermalMode::Cool;
        let hp_actual = if cooling {
            building.step_cooling(commanded_hp, outdoor_now, window_now, step)
        } else {
            building.step(commanded_hp, outdoor_now, window_now, step)
        };
        let (dhw_actual, dhw_short) = tank.step(
            decision
                .commanded
                .get(&ids.dhw)
                .copied()
                .unwrap_or(Power::ZERO),
            weather.draw_in(slot, hems_forecast::hotwater::draw(slot)) * (step / SLOT),
            step,
        );
        previous.insert(ids.battery.clone(), battery_actual);
        previous.insert(ids.evse.clone(), evse_actual);
        previous.insert(ids.heat_pump.clone(), hp_actual);
        previous.insert(ids.dhw.clone(), dhw_actual);
        result.dhw_kwh += dhw_actual.kw() * step.as_seconds_f64() / 3600.0;
        result.cold_water_kwh += dhw_short.kwh();
        result.cost.unserved_eur += dhw_short.kwh() * HOT_WATER_SHORTFALL_EUR_PER_KWH;
        result.tank_min_fill = result.tank_min_fill.min(tank.fill());
        result.minimum_power_kw = result
            .minimum_power_kw
            .max(decision.verdict.minimum_power.kw());
        // The car's own deadline, measured on the car rather than on a plan. A
        // plan that admitted to a shortfall and then made it up is not a
        // shortfall; a plan that promised everything and delivered less is one,
        // and only this side of the loop can tell them apart.
        if let Some((ev, vehicle)) = scenario.ev.zip(evse.vehicle.as_ref())
            && now < start + ev.departure
            && now + step >= start + ev.departure
        {
            result.unmet_charge_kwh = (ev.energy_target - vehicle.stored).max(Energy::ZERO).kwh();
            result.cost.unserved_eur += result.unmet_charge_kwh * UNMET_CHARGE_EUR_PER_KWH;
        }
        // The inverter's ceiling for the next tick. Kept with the other
        // commanded values so a stale one behaves the way a stale command to any
        // other device does.
        let pv_allowed = decision
            .commanded
            .get(&ids.pv)
            .copied()
            .unwrap_or(-available)
            .outflow();
        previous.insert(ids.pv.clone(), -pv_allowed);
        // Curtailment is production the *controller refused*, measured against
        // what the weather offered this tick. Taking the difference between the
        // weather and the meter instead would count the inverter's own settling
        // time as a decision, and print a curtailment figure every sunrise on a
        // day when nothing was curtailed at all.
        let curtailed_now =
            (available - pv_allowed).max(Power::ZERO).kw() * step.as_seconds_f64() / 3600.0;
        result.curtailed_kwh += curtailed_now;
        // Priced, because the objective prices it: production thrown away is a
        // real loss whatever feeding it in would have earned.
        result.cost.curtailment_eur += curtailed_now * CURTAILMENT_EUR_PER_KWH;
        for (id, power) in [
            (&ids.battery, battery_actual),
            (&ids.evse, evse_actual),
            (&ids.heat_pump, hp_actual),
            (&ids.dhw, dhw_actual),
        ] {
            *delivered.entry(id.clone()).or_insert(Energy::ZERO) += power.over(step);
        }

        // ── Accounting ──────────────────────────────────────────────────────
        let hours = step.as_seconds_f64() / 3600.0;
        let grid = load_now
            + appliance_now
            + battery_actual
            + evse_actual
            + hp_actual
            + dhw_actual
            + pv_now;
        last_grid = grid;

        // The meter registers MiSpeL reads, accumulated exactly. `Z2` sees the
        // storage and the charge point together, which is Basisfall A3; the
        // charge point here is one-way, so nothing ever comes back out of it.
        {
            let register = registers.entry(slot).or_insert_with(|| MispelQuarterHour {
                anzulegender_wert: prices
                    .at(slot)
                    .filter(|p| !p.negative_price_hour)
                    .map_or(Decimal::ZERO, |p| p.export_ct),
                spot_price: prices.at(slot).map_or(Decimal::ZERO, |p| p.energy_ct),
                ..MispelQuarterHour::empty(slot)
            });
            let kwh = |p: Power| Decimal::try_from(p.kw() * hours).unwrap_or_default();
            register.grid_draw += kwh(grid.inflow());
            community.observe(&weather, site.location, now, hours, slot, grid.inflow());
            register.grid_feed_in += kwh(grid.outflow());
            register.device_consumption += kwh(battery_actual.inflow() + evse_actual.inflow());
            register.device_generation += kwh(battery_actual.outflow() + evse_actual.outflow());
            *result
                .production_kwh_by_slot
                .entry(slot)
                .or_insert(Decimal::ZERO) += kwh(pv_now.outflow());
        }
        result.produced_kwh += pv_now.outflow().kw() * hours;
        result.consumed_kwh += load_now.inflow().kw() * hours;
        result.imported_kwh += grid.inflow().kw() * hours;
        // The grid's own intensity in *this* quarter hour, so that shifting a
        // kilowatt-hour from the evening ramp into the middle of the day shows
        // up. Where the tariff carries no intensity there is nothing honest to
        // accumulate — a flat annual figure would make this the day's import
        // times a constant, which is a number that cannot move.
        if let Some(grams) = prices.at(slot).and_then(|p| p.co2_g_per_kwh) {
            result.imported_co2_kg += grid.inflow().kw() * hours * grams / 1000.0;
        }
        result.exported_kwh += grid.outflow().kw() * hours;
        // § 9 EEG is a limit on a *power*, so it is checked on every tick
        // against what the connection point actually did — the same discipline
        // the § 14a evidence record applies to the netzwirksamer
        // Leistungsbezug. `feed_in_ceiling_kw` is derived from the site above
        // and is `None` where nothing caps this roof.
        if let Some(cap) = result.feed_in_ceiling_kw {
            let over = grid.outflow().kw() - cap;
            if over > 0.0 {
                result.worst_feed_in_overshoot_w =
                    result.worst_feed_in_overshoot_w.max(over * 1000.0);
                feed_in_over += step;
            }
            // What the guard could not have known a period earlier: the roof
            // rising and the household's uninstrumented draw falling. It is the
            // bound on the line above, so it is measured beside it rather than
            // assumed about it.
            let uncommanded = pv_now.outflow() - load_now.inflow();
            if let Some(previous) = previous_uncommanded_export {
                result.worst_uncommanded_export_step_w = result
                    .worst_uncommanded_export_step_w
                    .max((uncommanded - previous).kw() * 1000.0);
            }
            previous_uncommanded_export = Some(uncommanded);
        }
        result.battery_throughput_kwh += battery_actual.abs().kw() * hours;
        if evse.mode == PhaseMode::Single && evse_actual > Power::ZERO {
            single_phase += step;
        }
        if scenario.planner
            && plan
                .as_ref()
                .is_none_or(|p| p.is_stale(now, arbiter.config().max_plan_age))
        {
            without_a_plan += step;
        }
        // What the hardware would not take. Counted here rather than inferred
        // from the meter afterwards, because the meter cannot tell a device that
        // refused a command from one that was never asked.
        if !decision.clipped.is_empty() {
            result.clipped_ticks += 1;
            result.clipped_kwh += decision
                .clipped
                .values()
                .map(|p| p.kw() * hours)
                .sum::<f64>();
        }
        result.battery_soc_min = result.battery_soc_min.min(battery.soc().fraction());
        result.ev_charged_kwh += evse_actual.inflow().kw() * hours;
        result.heat_pump_kwh += hp_actual.inflow().kw() * hours;
        result.indoor_min_c = result.indoor_min_c.min(building.indoor_c());
        result.indoor_max_c = result.indoor_max_c.max(building.indoor_c());
        let outside_band = (house.heat_pump.comfort_min_c - building.indoor_c())
            .max(building.indoor_c() - house.heat_pump.comfort_max_c)
            .max(0.0);
        result.discomfort_kelvin_hours += outside_band * hours;

        if let Some(price) = prices.at(slot) {
            result.cost.energy_eur += grid.inflow().kw() * hours * price.import_f64()
                - grid.outflow().kw() * hours * price.export_f64();
        }
        // Half the wear on each leg, so a full cycle pays it once — the same
        // convention the planner's objective uses.
        result.cost.wear_eur +=
            battery_actual.abs().kw() * hours * (house.battery.wear_eur_per_kwh / 2.0);
        result.cost.discomfort_eur += outside_band * hours * DISCOMFORT_EUR_PER_KELVIN_HOUR;

        // Two different stories, and they were being told as one. A limit from a
        // Steuerbox is a network operator reducing the house; the same number
        // arrived at from `init` or `failsafe` is the manager restraining itself
        // because nothing is talking to it. Counting the second as a § 14a event
        // reports a reduction on a day when the operator said nothing at all.
        if limits.steuve_ceiling.is_some() {
            if limits.in_failsafe {
                failsafe_for += step;
            } else {
                limited_for += step;
            }
        }

        if limits.steuve_ceiling.is_some() {
            result.lent_kwh += decision.verdict.lent_generation.over(step).kwh();
        }

        // The record `[A1 7.2]` asks for, built as the loop runs rather than
        // reconstructed afterwards from logs that were never kept.
        let steuve_ids: Vec<AssetId> = decision
            .verdict
            .steuve
            .iter()
            .flat_map(|s| s.assets.iter().cloned())
            .collect();
        // The three controllable devices, split by whether a network operator
        // may actually reduce them. A device below the threshold of
        // `[A1 2.4.1]` is ordinary load: it spends the surplus like any other
        // and never appears against the ceiling.
        let (steuve_consumption, other_consumption) = [
            (&ids.battery, battery_actual),
            (&ids.evse, evse_actual),
            (&ids.heat_pump, hp_actual),
        ]
        .into_iter()
        .fold(
            (Power::ZERO, Power::ZERO),
            |(steuve, other), (id, actual)| {
                if steuve_ids.contains(id) {
                    (steuve + actual.inflow(), other)
                } else {
                    (steuve, other + actual.inflow())
                }
            },
        );
        evidence.observe(
            Observation {
                ceiling: limits.steuve_ceiling,
                rule: if limits.in_failsafe {
                    GuardRule::Failsafe
                } else {
                    GuardRule::Lpc
                },
                mode: ControlMode::Ems,
                minimum_power: decision.verdict.minimum_power,
                // Every local generator, the store included. `[A1 2.3]`
                // measures what the controllable devices draw *from the grid*,
                // and a battery discharging into the wallbox means kilowatts
                // that never crossed the connection point. Counting only the
                // roof over-reports the Nachweis figure — and once the guard
                // started lending a discharge to the § 14a budget it reported a
                // reduction as breached on a day when it was respected.
                //
                // **Which** of the three is a steuerbare Verbrauchseinrichtung
                // is the classification's answer, not a fixed triple (D122). It used to
                // be a fixed triple, and on the no-store household — whose
                // battery is deliberately below the 4,2 kW of `[A1 2.4.1]` — that
                // charged an ordinary load against a network operator's ceiling
                // in the evidence record. It stayed invisible only because the
                // *planner* was making the same mistake in the other direction
                // (D121), so a record that over-counted was compared against a
                // plan that over-restrained. Two wrongs, one seam.
                //
                // And **every** non-controllable consumer is on the other side,
                // the tank and the appliance included. Leaving them out
                // overstates the surplus available to cover the controllable
                // devices, which understates the netzwirksamer Leistungsbezug —
                // the one direction a compliance record may never be wrong in.
                netzwirksam: hems_grid::netzwirksamer_leistungsbezug(
                    steuve_consumption,
                    load_now.inflow()
                        + appliance_now.inflow()
                        + dhw_actual.inflow()
                        + other_consumption,
                    pv_now.outflow() + battery_actual.outflow() + evse_actual.outflow(),
                ),
                applied: !decision.setpoints.is_empty(),
            },
            &steuve_ids,
            now,
        );

        now += step;
    }

    // Close any record still open when the day ends.
    evidence.observe(
        Observation {
            ceiling: None,
            rule: GuardRule::Lpc,
            mode: ControlMode::Ems,
            minimum_power: Power::ZERO,
            netzwirksam: Power::ZERO,
            applied: false,
        },
        &[],
        now,
    );
    result.control_events = evidence
        .closed()
        .iter()
        .filter(|e| e.rule == GuardRule::Lpc)
        .count();
    result.failsafe_events = evidence.closed().len() - result.control_events;
    result.evidence_samples = evidence.closed().iter().map(|e| e.samples.len()).sum();
    result.worst_latency_s = evidence
        .worst_latency()
        .map_or(0.0, time::SignedDuration::as_seconds_f64);
    result.acted_by_command = evidence
        .closed()
        .iter()
        .filter(|e| e.rule == GuardRule::Lpc)
        .find_map(|e| e.acted)
        .map(|a| a == hems_grid::Action::Commanded);
    // The compliance verdict now comes from the record itself rather than from a
    // separate running check — one source, and it is the one an operator would
    // be asked to produce.
    result.grid_event_respected = evidence.fully_compliant();
    // Only a *network operator's* instruction can be unlawful under `[A1 4.5]`.
    // The same shape arrived at from `init` or `failsafe` is the box restraining
    // itself, which is a configuration fault rather than an operator's, and
    // conflating them would tell a household the operator broke the law on a day
    // nobody sent anything.
    result.commanded_below_minimum = evidence
        .closed()
        .iter()
        .filter(|e| e.rule == GuardRule::Lpc)
        .any(hems_grid::ControlEvent::below_minimum);
    result.failsafe_below_minimum = evidence
        .closed()
        .iter()
        .filter(|e| e.rule != GuardRule::Lpc)
        .any(hems_grid::ControlEvent::below_minimum);
    result.worst_overshoot_w = evidence
        .closed()
        .iter()
        .filter_map(hems_grid::ControlEvent::worst_overshoot)
        .map(Power::get)
        .fold(0.0, f64::max);
    // The record itself, not only the numbers taken off it. `[A1 7.3]` keeps it
    // for two years and `hemsd --store` is what writes it down.
    result.evidence = evidence.closed().to_vec();

    // Close the ledger on the stores. The objective values what it leaves behind
    // (`terminal_value_factor`); a day that counted the purchase and not the
    // thing bought would report a plan that filled its battery and its tank in
    // the last cheap hour of the night as *expensive*.
    let mean_import = prices
        .slots
        .iter()
        .map(hems_tariff::SlotPrice::import_f64)
        .sum::<f64>()
        / prices.slots.len().max(1) as f64;
    result.cost.stored_eur =
        ((stores_open - stored_at_start(&battery, &tank)) * mean_import).max(0.0);
    // …and the car, which is a different entry for a reason. Both households own
    // the same one, so a kilowatt-hour a controller pushed into it *past what
    // the household asked for* is a kilowatt-hour nobody buys later — and
    // refusing to credit it measures a manager that absorbed a sunny afternoon
    // into the car against one that exported the same energy at the feed-in
    // tariff. Only the excess: the charge up to the target is the service, and
    // it is already in the bill.
    result.cost.vehicle_eur = evse
        .vehicle
        .as_ref()
        .zip(scenario.ev)
        .map_or(0.0, |(v, ev)| {
            -(v.stored - ev.energy_target).max(Energy::ZERO).kwh() * mean_import
        });
    (
        result.baseline,
        result.baseline_shared_kwh,
        result.baseline_energy_eur_netted,
    ) = baseline_cost(scenario, &weather, &array, &prices, site, site.location);

    // ── § 42c: what the community actually allocated this member ────────────
    (result.shared_kwh, result.cost.sharing_eur) = community.settle(&prices);

    // The durations the loop accumulated, in the minutes the report speaks.
    result.single_phase_minutes = single_phase.whole_minutes();
    result.minutes_without_a_plan = without_a_plan.whole_minutes();
    result.failsafe_minutes = failsafe_for.whole_minutes();
    result.limited_minutes = limited_for.whole_minutes();
    result.feed_in_over_minutes = feed_in_over.whole_minutes();

    result.quarter_hours = registers.into_values().collect();

    // The same bill, netted per quarter hour before it is priced — what a
    // published comparison that evaluates at its own scheduling step reports.
    // Computed from the registers rather than from a second pass over the day,
    // because the registers already hold each direction accumulated apart: their
    // *difference* is the net the coarser method would price, and their sum is
    // what the meter actually bills.
    result.energy_eur_netted = result
        .quarter_hours
        .iter()
        .filter_map(|q| {
            let price = prices.at(q.slot)?;
            let draw = q.grid_draw.to_f64()?;
            let fed_in = q.grid_feed_in.to_f64()?;
            let net = draw - fed_in;
            Some(if net >= 0.0 {
                net * price.import_f64()
            } else {
                net * price.export_f64()
            })
        })
        .sum();

    // The § 9 EEG quantity, at the resolution § 9 EEG measures it: the largest
    // quarter-hour average that left the connection point, straight off the
    // registers the settlement would be built from.
    result.peak_feed_in_kw = result
        .quarter_hours
        .iter()
        .filter_map(|q| q.grid_feed_in.to_f64())
        .fold(0.0, f64::max)
        * 4.0;

    // ── What the forecasts were worth ───────────────────────────────────────
    //
    // The last quarter hour of the day is flushed here rather than left in the
    // accumulator. The loop scores a slot when the *next* one starts, so
    // without this the 23:45 slot was silently dropped from every day the
    // project has ever run — ninety-five scored quarter hours reported as
    // ninety-six.
    score_slot(
        &pv_forecast,
        &load_forecast,
        delivered_slot,
        slot_covered,
        pv_slot_wh,
        load_slot_wh,
        &mut pv_scored,
        &mut load_scored,
    );
    // Whether the planner was shown the answer. Read from the realisation
    // rather than from a flag the caller sets, so a day cannot be labelled
    // honest by forgetting to say otherwise.
    result.foresight_is_perfect = scenario.weather.is_perfect();
    // What the compressor did, which is the only witness that the planner's
    // minimum runtime survived the re-plan boundary.
    if let Some(c) = building.compressor {
        result.compressor_starts = c.starts;
        result.compressor_held_minutes = c.held_against_command.whole_minutes();
    }
    result.pv_forecast = Calibration::score(pv_scored.iter().copied());
    result.load_forecast = Calibration::score(load_scored.iter().copied());
    result.roof_correction = learned
        .roof
        .ratio_at(Slot::containing(start + Duration::hours(12)));
    result.history_days = learned.days;
    result.roof_health = learned.health.verdict();

    // ── Is the household on the right network-charge module? ────────────────
    //
    // `hems_tariff::advisor` prices the day's own quarter-hour consumption
    // under each § 14a module and annualises it. It had no caller for four
    // versions, which is the failure mode R20 exists for: a module nobody
    // invokes is not a feature.
    let consumption: BTreeMap<Slot, Energy> = result
        .quarter_hours
        .iter()
        .map(|q| {
            (
                q.slot,
                Energy::from_kwh(q.grid_draw.try_into().unwrap_or(0.0)),
            )
        })
        .collect();
    if !consumption.is_empty() {
        let comparisons =
            hems_tariff::compare_moduls(&consumption, &crate::site::modul_choices(&tariff));
        // The **energy** halves, on this day, with nothing annualised: that is
        // what the day is evidence for. The fixed halves are annual by nature,
        // and the two are combined only in the break-even below — a threshold
        // in kilowatt-hours a year, which is a statement a household can check
        // against its own bill rather than a projection from one Thursday.
        let today = |label: &str| {
            comparisons
                .iter()
                .find(|c| c.label == label)
                .map_or(0.0, |c| c.energy_cost_eur.to_f64().unwrap_or(0.0))
        };
        result.modul2_delta_today_eur = today("Modul 2") - today("Modul 1");
        result.modul2_break_even_kwh_per_year = hems_tariff::advisor::modul2_break_even_kwh(
            Decimal::new(1000, 2),
            Decimal::new(4, 1),
            Decimal::new(25, 0),
            Decimal::new(120, 0),
        )
        .and_then(|d| d.to_f64());
    }

    // ── The dishwasher ──────────────────────────────────────────────────────
    //
    // How far the plan moved it, against the household that pressed start as
    // soon as it was allowed to. A structural zero means the mechanism decided
    // nothing, and that is why the number is reported rather than inferred from
    // the bill.
    // A machine nobody loaded is not a machine that failed to run, so the charge
    // hangs off the *window* rather than off the appliance: a household that
    // asked for nothing is owed nothing.
    if let Some((sim, (from, _))) = dishwasher.as_ref().zip(scenario.dishwasher) {
        match sim.started_at() {
            Some(at) => result.appliance_shift_minutes = (at - from).whole_minutes(),
            // Nothing ran. Charged at the same price the baseline pays, so the
            // saving cannot be made of a wash nobody got.
            None => result.cost.unserved_eur += UNRUN_PROGRAMME_EUR,
        }
    }

    // ── The site, in S2 terms ───────────────────────────────────────────────
    //
    // Every asset **actually described** the way EN 50491-12-2 describes it —
    // the messages a Resource Manager on the other end of a WebSocket would be
    // sent, built and counted rather than inferred from the control-type
    // mapping. The difference is the whole point: counting assets whose control
    // type is not `NotControllable` reports a number that goes up when a device
    // is added and never notices that no `describe_*` was ever written for it,
    // which is exactly how a hot-water tank sat in this figure for four versions
    // with nothing to send.
    {
        let modes: BTreeMap<AssetId, hems_core::prelude::PhaseMode> = phase_state
            .iter()
            .map(|(id, p)| (id.clone(), p.mode))
            .collect();
        let mut context = hems_flex::DescribeContext::new(start, start + Duration::days(1), &modes);
        if let Some(vehicle) = evse.vehicle.as_ref().filter(|_| scenario.ev.is_some()) {
            context = context.with_ev_session(hems_flex::EvStorage {
                stored: vehicle.stored,
                capacity: vehicle.capacity,
                efficiency: vehicle.efficiency,
            });
        }
        let described = hems_flex::describe_site(site, &context);
        result.s2_resources = described.described();
        result.s2_undescribed = described.undescribed.len();
    }

    // The **same** arithmetic a box on a wall runs, from the same three metered
    // energies, and not the sum of the loads this simulator happens to be able
    // to see. The two had drifted, and a fleet that averages a simulated day
    // beside a real one was averaging two different questions (D125).
    result.self_sufficiency = hems_core::report::self_sufficiency(
        result.imported_kwh,
        result.produced_kwh,
        result.exported_kwh,
    );
    Ok(result)
}

/// What the same day would have cost without an energy manager.
///
/// The comparison has to deliver the **same service**, or it is not a comparison
/// at all: the car still reaches its target and the house is still warm. What
/// the baseline lacks is the *decisions* — no battery, a wallbox that starts the
/// moment the car is plugged in, and a heat pump on an ordinary thermostat.
/// Anything else flatters the optimiser, and a saving figure nobody can
/// reproduce is worse than no figure at all.
///
/// # Same day, same law, same failures
///
/// Three things make it a comparison rather than an advertisement.
///
/// It faces the **same realisation** — down to the cloud at 12:19. A baseline
/// run against a different draw prices two different Tuesdays.
///
/// It faces the **same law**. A household with no energy manager cannot be
/// addressed as one `[A1 4.4.b]`, so during a § 14a reduction its Steuerbox
/// turns each device down on its own `[A1 4.4.a]`, no further than the minimum
/// of `[A1 4.5.1]`; and its roof is capped by § 9 EEG exactly like the managed
/// one, throwing away what it cannot export. Ignoring either would measure the
/// optimiser against a household nobody is allowed to be.
///
/// And it pays for the **service it fails to deliver**. A wallbox limited to
/// 4,2 kW through teatime may not fill a car by seven; a thermostat that starts
/// the day with a cold tank cannot fill a bath at six. The plan is charged for
/// exactly that ([`CostBreakdown::unserved_eur`]), so the baseline has to be.
/// The charge point the unmanaged household has — the same one, three-phase,
/// because it is the same house.
fn unmanaged_wallbox_power(site: &Site) -> Power {
    site.assets
        .iter()
        .find_map(|a| match a {
            Asset::Evse(e) => Some(e.max_power(PhaseMode::Three)),
            _ => None,
        })
        .unwrap_or(Power::from_kw(11.0))
}

/// An unmanaged shiftable appliance: pressed at the first moment the household's
/// own window allows.
///
/// And only where the whole programme fits inside that window. Both sides are
/// held to the same one: an unmanaged household that overran a deadline the plan
/// was refused would be a comparison with a household living under a different
/// promise.
fn unmanaged_appliance(
    scenario: &Scenario,
    sim: Option<&mut hems_sim::ApplianceSim>,
    now: OffsetDateTime,
    step: Duration,
) -> Power {
    let start = scenario.start();
    match sim {
        Some(sim) => {
            let fits = scenario.dishwasher.is_some_and(|(from, until)| {
                from + sim.programme.duration() <= until && now >= start + from
            });
            sim.step(fits, now - start, step)
        }
        None => Power::ZERO,
    }
}

/// An unmanaged wallbox: it starts as soon as the car is plugged in and runs flat
/// out until the car is full.
///
/// Which means it cannot start *before* the cable goes in either. A baseline that
/// charged a car that was not there would buy the cheap hours of the night the
/// optimiser is being judged on finding.
///
/// It also pays for the car it leaves short at the departure, at the same price
/// the plan is charged — a car plugged in an hour before it leaves is short
/// whatever anybody does, and charging only one side for that would be the same
/// asymmetry pointing the other way.
#[allow(clippy::too_many_arguments)]
fn unmanaged_wallbox(
    scenario: &Scenario,
    evse_power: Power,
    bounded: &impl Fn(Power) -> Power,
    car_remaining: &mut Energy,
    car_stored: &mut Energy,
    cost: &mut CostBreakdown,
    now: OffsetDateTime,
    step: Duration,
) -> Power {
    let start = scenario.start();
    let hours = step.as_seconds_f64() / 3600.0;
    let plugged_in = scenario
        .ev
        .is_none_or(|e| now >= start + e.arrival && now < start + e.departure);
    let ev = if *car_remaining > Energy::ZERO && plugged_in {
        let p = bounded(evse_power);
        let delivered = Energy::new(p.get() * hours * THREE_PHASE_EFFICIENCY).min(*car_remaining);
        *car_remaining -= delivered;
        *car_stored += delivered;
        p
    } else {
        Power::ZERO
    };
    if let Some(e) = scenario.ev
        && now < start + e.departure
        && now + step >= start + e.departure
    {
        cost.unserved_eur += car_remaining.kwh() * UNMET_CHARGE_EUR_PER_KWH;
        *car_remaining = Energy::ZERO;
    }
    ev
}

/// An ordinary thermostat with a half-kelvin hysteresis: on at the bottom of the
/// comfort band, off half a degree above it.
///
/// It knows nothing about the price and nothing about the weather, which is the
/// whole of the difference being measured — and it lives under the same § 14a
/// direct-control ceiling the rest of the unmanaged house does.
fn unmanaged_heat_pump(
    house: &Reference,
    building: &mut BuildingSim,
    thermostat_on: &mut bool,
    bounded: &impl Fn(Power) -> Power,
    outdoor_c: f64,
    // The same sun the managed house gets. A baseline heated by a different
    // physics from the plan's is not a comparison (D37).
    window_w_per_m2: f64,
    step: Duration,
) -> Power {
    let low = house.heat_pump.comfort_min_c;
    let high = house.heat_pump.comfort_max_c;

    // **The same hardware, on its own thermostat.** A household that has bought a
    // reversible unit does not sit in a hot house all July because nobody
    // optimised for it — it sets a cooling set point and the unit runs. Leaving
    // that out would credit the planner with the *cooling function* rather than
    // with deciding when to use it, which is D195's mistake exactly: the
    // comparison household must own what this one owns.
    //
    // The set point is the top of the band, with the same half-kelvin hysteresis
    // the heating side uses, and no anticipation of anything: a thermostat cools
    // when the room is already too warm, where a plan cools into the hours the
    // roof is paying for. That difference is the whole of what is being measured.
    if let Some(cooling) = house.heat_pump.cooling_electrical
        && building.indoor_c() > high
    {
        return building.step_cooling(bounded(cooling), outdoor_c, window_w_per_m2, step);
    }

    if building.indoor_c() < low {
        *thermostat_on = true;
    } else if building.indoor_c() > low + 0.5 {
        *thermostat_on = false;
    }
    building.step(
        if *thermostat_on {
            bounded(house.heat_pump.power)
        } else {
            Power::ZERO
        },
        outdoor_c,
        window_w_per_m2,
        step,
    )
}

/// The two quarter-hour series a § 42c settlement needs: what the neighbours'
/// roofs made, and what this member drew.
///
/// It exists so the managed day and the unmanaged one accumulate them the same
/// way. Two copies of "sum a minute into a quarter hour" is two places for the
/// two sides of a comparison to stop agreeing about what a quarter hour is.
#[derive(Debug, Default)]
struct CommunityMeter {
    membership: Option<CommunityMembership>,
    neighbours: Option<ArrayModel>,
    generation: std::collections::BTreeMap<Slot, Decimal>,
    draw: std::collections::BTreeMap<Slot, Decimal>,
}

impl CommunityMeter {
    /// A meter for the community this scenario's household belongs to, or an
    /// inert one where it belongs to none.
    fn for_scenario(scenario: &Scenario) -> Self {
        Self {
            membership: scenario.community,
            neighbours: scenario
                .community
                .map(|c| ArrayModel::new(c.kwp, c.kwp * 0.8, 35.0, 180.0)),
            ..Self::default()
        }
    }

    /// Add one control period.
    fn observe(
        &mut self,
        weather: &Weather,
        location: GeoPoint,
        now: OffsetDateTime,
        hours: f64,
        slot: Slot,
        drawn: Power,
    ) {
        let Some(neighbours) = self.neighbours.as_ref() else {
            return;
        };
        let kwh = |p: Power| Decimal::try_from(p.kw() * hours).unwrap_or_default();
        *self.generation.entry(slot).or_default() +=
            kwh(weather.production_at(neighbours, location, now));
        *self.draw.entry(slot).or_default() += kwh(drawn);
    }

    /// What the community expects to be able to allocate this member over
    /// `horizon`, slot by slot, in watts — the planner's input.
    ///
    /// Empty where the household is not in a community, which is what leaves the
    /// model exactly as it was before § 42c existed: no column, no row.
    fn forecast(&self, weather: &Weather, location: GeoPoint, horizon: Horizon) -> Vec<f64> {
        let (Some(c), Some(neighbours)) = (self.membership, self.neighbours.as_ref()) else {
            return Vec::new();
        };
        horizon
            .slots()
            .map(|s| {
                weather.modelled_production(neighbours, location, s).get() * c.key.clamp(0.0, 1.0)
            })
            .collect()
    }

    /// The kilowatt-hours allocated and the credit they earned.
    fn settle(&self, prices: &PriceStack) -> (f64, f64) {
        self.membership.map_or((0.0, 0.0), |c| {
            settle_sharing(c, prices, &self.generation, &self.draw)
        })
    }
}

/// What a § 42c community allocated one member over a day, and what it saved.
///
/// Settled from the day's own quarter-hour meter registers through the same
/// arithmetic a Nachweis would use — `hems_grid::sharing`, which is `metering`'s
/// allocation with the § 42c cascade on top — rather than from the plan's idea
/// of it. The plan is a forecast; this is what happened, and the difference
/// between them is a seam worth being able to see.
///
/// The other two members are ordinary households of the same size drawing the
/// day's own load profile, and they matter: under a dynamic Aufteilungsschlüssel
/// what a neighbour cannot use is re-offered (§ 42c Abs. 3 Nr. 2), so a
/// community whose other members were assumed to consume nothing would hand this
/// household the whole roof and overstate the benefit threefold.
///
/// Returns the kilowatt-hours allocated and the credit they earned, which is
/// **negative** because it comes off a bill.
fn settle_sharing(
    membership: CommunityMembership,
    prices: &PriceStack,
    generation: &std::collections::BTreeMap<Slot, Decimal>,
    draw: &std::collections::BTreeMap<Slot, Decimal>,
) -> (f64, f64) {
    let key = Decimal::try_from(membership.key.clamp(0.0, 1.0)).unwrap_or_default();
    let others = ((Decimal::ONE - key) / Decimal::TWO).max(Decimal::ZERO);
    let community = hems_grid::sharing::Community::new(
        "11YDE-VE-------2",
        vec![
            hems_grid::sharing::Member::new("DE0001111111111111111111111111111", key),
            hems_grid::sharing::Member::new("DE0002222222222222222222222222222", others),
            hems_grid::sharing::Member::new("DE0003333333333333333333333333333", others),
        ],
    );
    let (mut kwh, mut credit) = (0.0, 0.0);
    for (slot, drawn) in draw {
        let neighbour = Decimal::try_from(household_load(*slot).kw() * 0.25).unwrap_or_default();
        let Ok(allocation) = hems_grid::sharing::allocate_by(
            &community,
            *slot,
            generation.get(slot).copied().unwrap_or_default(),
            &[*drawn, neighbour, neighbour],
            hems_grid::sharing::Aufteilung::Dynamisch,
        ) else {
            continue;
        };
        let mine = allocation.shares[0].shared.to_f64().unwrap_or(0.0);
        kwh += mine;
        credit -= mine
            * prices
                .at(*slot)
                .map_or(0.0, hems_tariff::SlotPrice::sharing_discount_f64);
    }
    (kwh, credit)
}

#[allow(clippy::too_many_lines)]
fn baseline_cost(
    scenario: &Scenario,
    weather: &Weather,
    array: &ArrayModel,
    prices: &PriceStack,
    site: &Site,
    location: GeoPoint,
) -> (CostBreakdown, f64, f64) {
    let house = Reference::of(&scenario.config).expect("run() has already validated this");
    // The unmanaged household's own quarter-hour registers, kept only so the
    // same day can be priced the netted way for *both* sides. A saving is a
    // difference, so a measurement artefact that forgives one household more
    // than the other moves the saving even when it barely moves either bill.
    let mut netted: std::collections::BTreeMap<Slot, (f64, f64)> =
        std::collections::BTreeMap::new();
    let start = scenario.start();
    // The same § 9 EEG ceiling the managed house lives under. It is a property
    // of the installation, not of who is controlling it.
    let feed_in_ceiling = hems_grid::para9::site_feed_in_ceiling(site, None, None).map(|(p, _)| p);
    let evse_power = unmanaged_wallbox_power(site);
    let step = CONTROL_PERIOD;
    let hours = step.as_seconds_f64() / 3600.0;
    // How much this wallbox will still push. It stops at the **target** the
    // household asked for, and that is a known asymmetry rather than a
    // considered choice: the managed household may charge on past it out of a
    // surplus and is credited for the excess (`vehicle_eur`), and this one never
    // can, so only one side of the comparison can ever earn that entry. The
    // asymmetry is small, and it can only ever *understate* the saving — which is
    // the safe direction, and the reason the obvious repairs are worse than it.
    let mut car_remaining = scenario.ev.map_or(Energy::ZERO, |e| {
        (e.energy_target - e.energy_now).max(Energy::ZERO)
    });
    // Tracked as an absolute rather than derived from `car_remaining`, which is
    // zeroed at the departure so that the shortfall is charged once.
    let mut car_stored = scenario.ev.map_or(Energy::ZERO, |e| e.energy_now);
    let mut building = BuildingSim {
        nominal_electrical: house.heat_pump.power,
        cooling_electrical: house.heat_pump.cooling_electrical.unwrap_or(Power::ZERO),
        // The same floor the planner is given, so the plan and the house agree
        // about what "on" is worth for a unit whose slots are scheduled.
        min_electrical: house.heat_pump.power * 0.3,
        thermostat_set_c: house.heat_pump.comfort_min_c + 0.5,
        thermostat_max_c: house.heat_pump.comfort_max_c,
        // A single-speed compressor where the household has one, so a day can
        // report what the planner's minimum runtime actually bought. Without it
        // the constraint is stated in every plan and observed by nothing.
        compressor: (!house.heat_pump.modulating).then(compressor_sim),
        ..BuildingSim::new(21.0)
    };
    let mut thermostat_on = false;
    let mut cost = CostBreakdown::default();
    // **The same tank the managed household has**, read off the same asset.
    //
    // It was a tank of its own: `litres × c × 15 K`, on `TankSim::new`'s default
    // coefficient of 3,0 and default standing loss of 45 W. For the reference
    // household every one of those happens to equal the configured value — the
    // span really is 60 − 45 — which is what kept it invisible: when the literal
    // equals the configuration, no test can tell one from the other. For any
    // *other* household it is a different appliance, and the difference between
    // two tanks was being reported as a saving (D183, R25).
    let (mut tank, thermostat_set) = match site.assets.iter().find_map(|a| match a {
        Asset::Dhw(tank) => Some(tank),
        _ => None,
    }) {
        Some(asset) => (
            TankSim {
                cop: asset.cop,
                standing_loss: asset.standing_loss,
                ..TankSim::new(asset.usable_heat(), asset.heater)
            },
            asset.stored_heat(asset.t_set_c),
        ),
        _ => (
            TankSim::new(
                Energy::from_kwh(
                    house.dhw.litres * hems_core::asset::WATER_KWH_PER_LITRE_KELVIN * 15.0,
                ),
                house.dhw.heater,
            ),
            Energy::ZERO,
        ),
    };
    let tank_open = tank.stored;
    // The unmanaged household presses start when it loads the machine, which is
    // the first moment its own window allows. That is the whole of the
    // difference being measured — the same programme, the same window, and
    // nobody deciding *when*.
    let mut dishwasher = scenario
        .config
        .dishwasher
        .clone()
        .map(hems_sim::ApplianceSim::new);
    // The unmanaged household is in the **same § 42c community**. It joined and
    // then did nothing about it: the Aufteilungsschlüssel still allocates it
    // whatever its own draw happens to overlap. Leaving that out would credit
    // the plan with the *membership* rather than with the shifting, which is the
    // same asymmetry as measuring a saving against a household that ignored the
    // network operator.
    let mut community = CommunityMeter::for_scenario(scenario);
    // **The same pack the managed household has, on the controller it shipped
    // with.** Built from the same figures and started at the same charge, so the
    // only difference between the two households is who decides when it moves.
    //
    // It had no battery at all until D195, and that made every saving this
    // project quoted the value of *owning* a store rather than of managing one.
    // Nobody removes a battery to go back to an unmanaged house, so the
    // counterfactual that answers a household's actual question is the factory
    // controller — and maximising self-consumption is what every hybrid inverter
    // in this market does out of the box. It is also the baseline the MPC
    // literature reports against, so a figure measured this way is comparable
    // with a published one and a figure measured against an idle store is not.
    let mut baseline_battery = BatterySim::new(house.battery.kwh, house.battery.power);
    baseline_battery.stored = house.battery.kwh * house.battery.reserve_soc.fraction().max(0.30);
    let baseline_battery_open = baseline_battery.stored;
    let baseline_battery_floor = house.battery.kwh
        * house
            .battery
            .reserve_soc
            .fraction()
            .max(baseline_battery.soc_min.fraction());
    let mut now = start;

    while now < start + Duration::days(1) {
        let slot = Slot::containing(now);
        // The **same day**, down to the cloud at 12:19. A baseline run against
        // a different realisation than the optimised day is not a comparison at
        // all — it prices two different Tuesdays and calls the difference a
        // saving.
        let pv = -weather.production_at(array, location, now);
        let load = weather.load_at(now, household_load(slot));

        // The ceiling **one** device faces while the reduction is in force. The
        // managed house is given one number for everything behind it
        // `[A1 4.4.b]`; a house with no energy manager is not, so its Steuerbox
        // addresses each device on its own `[A1 4.4.a]` and may not take any of
        // them below the minimum of `[A1 4.5.1]`.
        let device_ceiling = scenario
            .grid_event
            .filter(|(from, until, _)| now >= start + *from && now < start + *until)
            .map(|_| hems_grid::para14a::MINDESTLEISTUNG);
        let bounded = |p: Power| device_ceiling.map_or(p, |c| p.min(c));

        let ev = unmanaged_wallbox(
            scenario,
            evse_power,
            &bounded,
            &mut car_remaining,
            &mut car_stored,
            &mut cost,
            now,
            step,
        );

        let hp = unmanaged_heat_pump(
            &house,
            &mut building,
            &mut thermostat_on,
            &bounded,
            weather.outdoor_at(now),
            weather.window_at(location, now, FACADE_AZIMUTH_DEG),
            step,
        );

        // An unmanaged tank reheats whenever it is below **its own set point**
        // and stops there. It never uses the store as a store, which is the whole
        // of the difference being measured — and it is not immune to a cold
        // shower either: it starts each morning where the evening left it.
        //
        // The set point rather than the brim, and the distinction is the
        // household's money: `t_max_c` is the highest *safe* temperature, a
        // safety bound the installer sets so nobody is scalded, and heating to it
        // all day is not what an unmanaged household does. This commanded the
        // heater unconditionally, so the baseline held its water at 60 °C where
        // the household asks for 55 — and every kilowatt-hour of the difference
        // was credited to the manager (D183).
        let (dhw, dhw_short) = tank.step(
            if tank.stored < thermostat_set {
                house.dhw.heater
            } else {
                Power::ZERO
            },
            weather.draw_in(slot, hems_forecast::hotwater::draw(slot)) * (step / SLOT),
            step,
        );
        cost.unserved_eur += dhw_short.kwh() * HOT_WATER_SHORTFALL_EUR_PER_KWH;

        let appliance = unmanaged_appliance(scenario, dishwasher.as_mut(), now, step);

        // The battery, deciding for itself. A factory controller looks at the
        // house's net position and does the obvious thing with it: a surplus
        // goes into the store rather than out of the door, a deficit comes out
        // of the store rather than off the grid. It never buys to charge and
        // never sells to discharge — that is the whole of what it knows, and the
        // whole of the difference the plan is being measured for.
        let demand = load + appliance + ev + hp + dhw + pv;
        let wanted = if demand < Power::ZERO {
            // A surplus: soak it up.
            demand.outflow().min(baseline_battery.max_charge)
        } else {
            // A deficit: cover it, but never below the household's own backup
            // reserve.
            //
            // **No § 14a ceiling here, and that is the rule rather than an
            // omission.** What a reduction bounds is the *netzwirksamer
            // Leistungsbezug* — what the household draws through the connection
            // point — and a discharging battery draws nothing; it is the thing
            // that makes the draw smaller. Capping it would have handicapped the
            // baseline in exactly the hour the comparison is about, which is the
            // flattering direction. The charging side needs no ceiling either,
            // for a reason particular to this controller: it only ever charges
            // from surplus, so it never draws through the connection point at
            // all.
            let spare = (baseline_battery.stored - baseline_battery_floor).max(Energy::ZERO);
            let by_charge =
                Power::new(spare.get() / hours.max(1e-9) * baseline_battery.efficiency_discharge);
            -demand.min(baseline_battery.max_discharge).min(by_charge)
        };
        let battery_actual = baseline_battery.step(wanted, step);
        cost.wear_eur += battery_actual.abs().kw() * hours * (house.battery.wear_eur_per_kwh / 2.0);

        // § 9 EEG bounds what leaves the connection point whether or not there
        // is an energy manager behind it, so what the baseline cannot export it
        // throws away — at the same price the plan pays for doing so.
        let mut grid = demand + battery_actual;
        if let Some(ceiling) = feed_in_ceiling {
            let curtailed = (grid.outflow() - ceiling).max(Power::ZERO);
            grid += curtailed;
            cost.curtailment_eur += curtailed.kw() * hours * CURTAILMENT_EUR_PER_KWH;
        }
        if let Some(price) = prices.at(slot) {
            cost.energy_eur += grid.inflow().kw() * hours * price.import_f64()
                - grid.outflow().kw() * hours * price.export_f64();
        }
        {
            let register = netted.entry(slot).or_insert((0.0, 0.0));
            register.0 += grid.inflow().kw() * hours;
            register.1 += grid.outflow().kw() * hours;
        }
        community.observe(weather, location, now, hours, slot, grid.inflow());
        // A thermostat is not free of discomfort: it starts reheating only once
        // the house has already fallen through the band. Leaving that out would
        // credit the planner with comfort the baseline also delivered.
        let outside = (house.heat_pump.comfort_min_c - building.indoor_c())
            .max(building.indoor_c() - house.heat_pump.comfort_max_c)
            .max(0.0);
        cost.discomfort_eur += outside * hours * DISCOMFORT_EUR_PER_KELVIN_HOUR;
        now += step;
    }
    // The same ledger the optimised day closes, so both sides are measured in
    // the same way.
    //
    // It is **not** near zero, and the reason is worth naming: both households'
    // tanks open at half their usable heat whatever the thermostat is set to, so
    // a household whose set point sits below that mark drains toward it over the
    // day and is charged for the difference. On the reference winter day that is
    // €1,19 for a 50 °C household against €0,73 for a 58 °C one — more than the
    // €0,36 of electricity the set point itself costs. The comparison stays
    // **fair**, because both sides open in the same state and it cancels in a
    // saving; what it is not is an instrument for anything about the *tank*,
    // which is why `the_baseline_household_heats_its_water_the_way_this_household_asked`
    // asserts on the meter instead.
    let mean_import = prices
        .slots
        .iter()
        .map(hems_tariff::SlotPrice::import_f64)
        .sum::<f64>()
        / prices.slots.len().max(1) as f64;
    // **The same rule the managed household is charged by**, `.max(0.0)` and
    // all: ending with a fuller tank than it started with is a real credit and
    // neither side takes it, because a credit one side can earn and the other
    // cannot is a comparison between two households rather than between two
    // decisions.
    //
    // Unclamped it would be an arbitrage rather than an accounting entry: the
    // heat is bought in whatever hours a thermostat demands it and credited back
    // at the day's *mean* import price, so a baseline that held its water hotter
    // would come out cheaper for having bought more electricity — R29's car
    // entry, in the tank (D183).
    //
    // The **battery** is in this entry now for the same reason the tank is: a
    // household whose factory controller ended the day emptier than it started
    // has not saved what its meter says it saved, and before D195 the baseline
    // had no store to borrow from so the term could only ever be charged to the
    // managed side.
    //
    // Computed exactly as `stored_at_start` computes it for the managed
    // household — electrical energy in the battery plus the electricity it would
    // take to put the tank's heat back, **one** quantity with **one** clamp, at
    // the plain mean import price. Writing it as two separately clamped terms,
    // or discounting the battery's half by the round-trip efficiency, would both
    // have been defensible in isolation and neither is what the other household
    // is charged — and a ledger that prices the same kilowatt-hour differently
    // depending on which household is holding it is the asymmetry this whole
    // function exists to avoid.
    let stores_open = baseline_battery_open.kwh() + tank_open.kwh() / tank.cop.max(f64::EPSILON);
    let stores_close =
        baseline_battery.stored.kwh() + tank.stored.kwh() / tank.cop.max(f64::EPSILON);
    cost.stored_eur = ((stores_open - stores_close) * mean_import).max(0.0);
    let (baseline_shared_kwh, sharing_eur) = community.settle(prices);
    cost.sharing_eur = sharing_eur;
    // A window with nowhere to put the programme costs the same on both sides.
    // Charging only the plan for a wash nobody got would be the asymmetry this
    // whole function exists to avoid, pointing the other way.
    if scenario.dishwasher.is_some() && dishwasher.is_some_and(|d| !d.is_finished()) {
        cost.unserved_eur += UNRUN_PROGRAMME_EUR;
    }
    // The same entry the managed day closes, and on the same terms: only the
    // charge beyond what the household asked for, because everything up to the
    // target is the service both sides delivered and paid for.
    cost.vehicle_eur = scenario.ev.map_or(0.0, |ev| {
        -(car_stored - ev.energy_target).max(Energy::ZERO).kwh() * mean_import
    });
    let energy_eur_netted = netted
        .iter()
        .filter_map(|(slot, (draw, fed_in))| {
            let price = prices.at(*slot)?;
            let net = draw - fed_in;
            Some(if net >= 0.0 {
                net * price.import_f64()
            } else {
                net * price.export_f64()
            })
        })
        .sum();
    (cost, baseline_shared_kwh, energy_eur_netted)
}

fn household_load(slot: Slot) -> Power {
    let minute = f64::from(slot.local_minute_of_day());
    let hour = minute / 60.0;
    let base = 250.0;
    let morning = 700.0 * (-((hour - 7.5) / 1.2).powi(2)).exp();
    let evening = 1100.0 * (-((hour - 19.0) / 2.0).powi(2)).exp();
    Power::new(base + morning + evening)
}

/// Score one finished quarter hour against what the forecasts said about it.
///
/// A free function because it is called twice — once per slot boundary inside
/// the control loop, and once after it for the quarter hour the loop's own
/// shape would otherwise leave sitting in the accumulator.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one accumulator of the loop this was lifted out of"
)]
fn score_slot(
    pv_forecast: &hems_forecast::Forecast,
    load_forecast: &hems_forecast::Forecast,
    slot: Slot,
    covered: Duration,
    pv_wh: f64,
    load_wh: f64,
    pv_scored: &mut Vec<(Band, f64)>,
    load_scored: &mut Vec<(Band, f64)>,
) {
    // Watt-hours over the part of the slot that was actually simulated, back to
    // an average power. `covered` is a duration rather than a count of ticks
    // because [`CONTROL_PERIOD`] is a parameter: a count is only a duration
    // while nobody changes it, and this file used to hold five such counts.
    let hours = covered.as_seconds_f64() / 3600.0;
    if hours <= 0.0 {
        return;
    }
    if let Some(band) = pv_forecast.at(slot) {
        pv_scored.push((band, pv_wh / hours));
    }
    if let Some(band) = load_forecast.at(slot) {
        load_scored.push((band, load_wh / hours));
    }
}

/// The car both households own.
///
/// One constant rather than two, because every comparison in this file rests on
/// the two sides having the same vehicle: a managed household charging a 60 kWh
/// car against a baseline charging a smaller one is measuring the car.
const VEHICLE_CAPACITY: Energy = Energy::new_const(60_000.0);
