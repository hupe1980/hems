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
use hems_grid::lpc::{LpcConfig, LpcMachine};
use hems_grid::mispel::QuarterHour as MispelQuarterHour;
use hems_grid::para14a::ControlMode;
use hems_optimizer::model::{
    BatteryModel, DhwModel, HeatPumpModel, PlanningLimits, Problem, ThermalModel, TimedLimit,
};
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

/// How often the control loop runs in a simulated day.
///
/// A real box ticks once a second; a day at that rate is 86 400 solves of the
/// same arithmetic and adds nothing a minute does not already show. The guard is
/// told the period, because a bound on a state is only a bound on a rate once
/// you know how long the rate is held for.
const CONTROL_PERIOD: Duration = Duration::minutes(1);

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
    /// [`WeatherSpec::PERFECT`] hands the planner the exact series the
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
    /// Whether the planner prices each asset separately.
    ///
    /// `false` gives every device the slot's marginal value instead, so the
    /// guard's *weighted* max-min allocator is handed the same weight for each
    /// and weights nothing. A named comparison, because the difference is what
    /// the mechanism is worth.
    pub per_asset_weights: bool,
    /// Whether the planner runs at all.
    ///
    /// `false` is the degraded mode of G3: no forecast, no prices, no solver —
    /// the box on its own, doing what every home battery has always done. It is
    /// also the only mode in which switching a charge point's conductors earns
    /// anything, because surplus is an instantaneous quantity that cannot be
    /// duty-cycled the way a planner duty-cycles a quarter hour.
    pub planner: bool,
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

impl Scenario {
    /// A January day with a teatime reduction — the case § 14a exists for.
    #[must_use]
    pub fn winter_with_grid_event(config: HouseholdConfig) -> Self {
        Self {
            config,
            date: time::macros::date!(2026 - 01 - 15),
            outdoor_c: 2.0,
            cloudiness: 0.55,
            // A January day in Germany: broken cloud, and a forecast that is
            // routinely a couple of kelvin and a good deal of irradiance out.
            weather: WeatherSpec::broken(0x_1501_2026),
            grid_event: Some((
                Duration::hours(17),
                Duration::minutes(18 * 60 + 30),
                Power::from_kw(4.2),
            )),
            steuerbox_outage: None,
            planner: true,
            per_asset_weights: true,
            prices_ct: winter_prices(),
            // Home at teatime, gone by seven in the morning with 20 kWh more in
            // it than it arrived with — so the plan has to find the cheap hours
            // of the night *and* work around the § 14a reduction on the way.
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
    /// degraded mode G3 promises, and it is the one place a switchable charge
    /// point pays for itself — surplus arrives as an instantaneous quantity, and
    /// below 4,14 kW three conductors can do nothing with it at all.
    #[must_use]
    pub fn summer_without_a_planner(config: HouseholdConfig) -> Self {
        Self {
            planner: false,
            ..Self::summer_surplus(config)
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
    /// What it actually demonstrates is the **store lending its discharge** to
    /// the § 14a budget (`[A1 2.3]`, D26): 4,6 kWh of headroom the connection
    /// point never saw, and a car 0,6 kWh short of a 12 kWh target instead of far
    /// further.
    ///
    /// This doc comment used to say it was "the case phase switching exists for,
    /// and the only one in which it pays". It is not, and the day's own KPI says
    /// so — **zero switches**. A planner duty-cycles a quarter hour and reaches
    /// the same average, which is exactly the measurement behind D22; switching
    /// pays where there is no planner, which is the `offline` day (178 minutes on
    /// one conductor, three switches, 2,5 kWh). A claim in a comment that the
    /// scenario's own output contradicts is the cheapest kind of wrong to leave
    /// lying around, and this workspace has now found several.
    #[must_use]
    pub fn winter_evening_deadline(config: HouseholdConfig) -> Self {
        Self {
            grid_event: Some((
                Duration::hours(17),
                Duration::minutes(19 * 60 + 30),
                Power::from_kw(4.2),
            )),
            // Home as the reduction starts, gone at eight, and 12 kWh short.
            // The arrival is what makes this scenario the one it says it is:
            // with the car on the cable from midnight the plan simply charges it
            // in the cheap hours of the night and never meets the reduction.
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
    /// discharge to lend the controllable devices headroom (`[A1 2.3]`, D26), so
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
            // it. It is the case D3 exists for — an optimiser can be infeasible,
            // a guard cannot.
            grid_event: Some((
                Duration::minutes(17 * 60 + 7),
                Duration::minutes(19 * 60 + 30),
                Power::from_kw(4.2),
            )),
            ..Self::winter_evening_deadline(HouseholdConfig {
                // Not quite zero: a household with no battery at all has no asset to
                // model, and what is being tested is the *sharing*, not the absence.
                battery_kwh: Energy::from_kwh(1.0),
                battery_power: Power::from_kw(0.5),
                ..config.clone()
            })
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
            prices_ct: summer_prices(),
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
    /// Set [`HouseholdConfig::cap_relief`] to [`CapRelief::ImsysWithControl`] to
    /// run the same day with the cap lifted, which is the comparison worth
    /// seeing.
    #[must_use]
    pub fn summer_capped(config: &HouseholdConfig) -> Self {
        Self {
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
                battery_kwh: Energy::from_kwh(1.0),
                battery_power: Power::from_kw(0.5),
                // Twenty kilowatts peak on a full south roof, with an
                // inverter sized one to one against the modules. Both halves
                // matter. The cap is a fraction of the **direct-current** power
                // while the limit it meets is the alternating-current peak, so
                // an undersized inverter clips below the cap on its own — which
                // is why the 9,8 kWp default in this workspace barely sees it,
                // and is worth knowing before anybody quotes a curtailment
                // figure from it.
                pv_kwp: Power::from_kw(20.0),
                pv_ac_nominal: Power::from_kw(20.0),
                ..config.clone()
            })
        }
    }

    /// The instant the day starts, in UTC.
    #[must_use]
    pub fn start(&self) -> OffsetDateTime {
        metering::calendar::day_start_utc(self.date)
    }
}

/// What the planner is allowed to do, slot by slot.
///
/// The § 14a ceiling carries the **window it applies in**. A reduction has a
/// duration — `[LPC-909]` sends one with the limit, and the failsafe releases
/// after its own minimum `[LPC-022]` — and stretching today's ninety minutes
/// across a forty-eight-hour horizon plans the house under a limit that lapsed
/// before teatime. It costs money in both directions: the plan charges the car
/// at three in the morning as if the network operator were still asking for
/// something, and it never sees the reduction coming when one is announced ahead.
///
/// The feed-in ceiling has no such window: § 9 EEG applies until an intelligent
/// metering system with a control device is in operation, which is a change of
/// installation rather than a change of hour.
fn planning_limits(
    limits: &GridLimits,
    ends_at: Option<OffsetDateTime>,
    site: &Site,
) -> PlanningLimits {
    let mut planning = PlanningLimits::default().with_import_ceiling(site.grid.import_ceiling());
    if let Some(ceiling) = limits.steuve_ceiling {
        planning = planning.with_steuve(match ends_at {
            Some(end) => TimedLimit::until(ceiling, Slot::containing(end)),
            None => TimedLimit::always(ceiling),
        });
    }
    if let Some(ceiling) = limits.feed_in_ceiling {
        planning = planning.with_feed_in(TimedLimit::always(ceiling));
    }
    planning
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

/// A summer curve with a midday collapse — and four negative quarter hours,
/// which is what § 51 EEG is about.
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
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DayResult {
    /// Energy drawn from the grid, kWh.
    pub imported_kwh: f64,
    /// Energy fed into the grid, kWh.
    pub exported_kwh: f64,
    /// Production, kWh.
    pub produced_kwh: f64,
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
    /// Electrical energy used by the hot-water tank, kWh.
    pub dhw_kwh: f64,
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
    /// How long a network operator's § 14a limit was in force, minutes.
    pub limited_minutes: i64,
    /// Whether the netzwirksamer Leistungsbezug stayed inside the ceiling the
    /// whole time.
    pub grid_event_respected: bool,
    /// The largest overshoot of a § 14a ceiling, watts. Should be zero.
    pub worst_overshoot_w: f64,
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
    /// The largest shortfall any plan of the day admitted to on the charging
    /// deadline, kWh.
    ///
    /// Zero on an ordinary day. Above zero it is what the household has to be
    /// told before the morning rather than after it: the schedule was the best
    /// achievable and it could not deliver everything the car was promised.
    pub unmet_charge_kwh: f64,
    /// Minutes in which the arbiter had no plan it was willing to follow.
    ///
    /// Should be zero for a box whose planner is running: a plan older than
    /// [`hems_realtime::ArbiterConfig::max_plan_age`] is worse than none, so the
    /// arbiter drops it — and if the planner re-solves less often than that
    /// tolerance, the house runs on the fallback for part of every cycle without
    /// anything saying so.
    pub minutes_without_a_plan: i64,
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
    /// The largest **quarter-hour average** power that left the connection
    /// point, kW.
    ///
    /// The quantity § 9 EEG limits, in the resolution it limits it at. A
    /// quarter-hour average and not an instantaneous peak, because the 60 % cap
    /// is a *settlement* limit read off meter registers rather than a control
    /// instruction (D27) — and a controller that chased the instantaneous value
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
}

impl DayResult {
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

/// Run one day and report what happened.
///
/// The control loop runs once a minute and the planner every half hour, which is
/// the same shape a real box uses — fast enough to track a cloud, slow enough
/// that a solver is affordable.
///
/// # Errors
/// When the household described by the scenario is not a valid site — a
/// duplicate asset name, or an asset on a circuit that does not exist.
///
/// # Panics
/// When a scenario declares a charging session but the simulated charge point
/// has no vehicle. The two are built together in this function, so that is a
/// programming error rather than a runtime condition.
#[allow(clippy::too_many_lines)]
pub fn run(scenario: &Scenario) -> anyhow::Result<DayResult> {
    let household = Household::build(&scenario.config)?;
    let site = &household.site;
    let start = scenario.start();
    let day = Horizon::new(start, 96);

    // ── The physical house ──────────────────────────────────────────────────
    let array = ArrayModel::new(
        scenario.config.pv_kwp,
        scenario.config.pv_ac_nominal,
        35.0,
        180.0,
    );
    let mut building = BuildingSim {
        nominal_electrical: scenario.config.heat_pump_power,
        thermostat_set_c: scenario.config.comfort_min_c + 0.5,
        thermostat_max_c: scenario.config.comfort_max_c,
        ..BuildingSim::new(21.0)
    };
    let mut inverter = PvSim::default();
    let stored_at_start = |battery: &BatterySim, tank: &TankSim| -> f64 {
        // Electrical energy in the battery, plus the electricity it would take
        // to put the tank's heat back.
        battery.stored.kwh() + tank.stored.kwh() / tank.cop.max(f64::EPSILON)
    };
    let tank_asset = match site.asset(&household.dhw) {
        Some(Asset::Dhw(t)) => t.clone(),
        _ => unreachable!("the household always has a tank"),
    };
    let mut tank = TankSim {
        cop: tank_asset.cop,
        standing_loss: tank_asset.standing_loss,
        ..TankSim::new(tank_asset.usable_heat(), tank_asset.heater)
    };
    let mut battery = BatterySim::new(scenario.config.battery_kwh, scenario.config.battery_power);
    // Thirty per cent, or the backup reserve if the household asked for more: a
    // house that boots below its own promise is a real case, but it is not the
    // one these days are about, and starting there makes every reserve figure
    // look like it was broken by a standing loss.
    battery.stored = scenario.config.battery_kwh * scenario.config.reserve_soc.fraction().max(0.30);
    let mut evse = EvseSim::new();
    if scenario.config.evse_switchable {
        evse = evse.switchable();
    }
    let mut evse = evse.with_vehicle(VehicleSim {
        capacity: Energy::from_kwh(60.0),
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
    let mut lpc = LpcMachine::new(
        LpcConfig {
            failsafe_limit: Power::from_kw(4.2),
            ..LpcConfig::default()
        },
        start,
    );

    // ── Prices and forecasts ────────────────────────────────────────────────
    let tariff = crate::site::tariff_for(&scenario.prices_ct, day);
    let prices = PriceStack::build(&tariff, day);

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
        &weather,
        &array,
        site.location,
        start,
        household_load,
        hot_water_draw,
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
    let load_forecast = crate::forecasting::load_forecast(&learned, full);

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
    // Production and load accumulated over the quarter hour in progress, so the
    // forecast can be scored against what a meter would have registered rather
    // than against an instant.
    let mut pv_slot_wh = 0.0_f64;
    let mut load_slot_wh = 0.0_f64;
    let mut slot_minutes = 0_u32;
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

    let mut result = DayResult {
        grid_event_respected: true,
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
        for event in steuerbox.poll(now) {
            let _ = lpc.handle(event, now);
        }
        while let Some(deadline) = lpc.next_deadline() {
            if deadline > now || lpc.tick(now).is_none() {
                break;
            }
        }
        let ceiling = lpc.effective_limit();
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
        // with a control device is in operation.
        let feed_in = hems_grid::para9::site_feed_in_ceiling(site, None, None);
        result.feed_in_ceiling_kw = feed_in.map(|(p, _)| p.kw());
        let limits = GridLimits {
            steuve_ceiling: ceiling,
            steuve_since: ceiling_since,
            in_failsafe: !lpc.state().is_controlled(),
            feed_in_ceiling: feed_in.map(|(p, _)| p),
            feed_in_rule: feed_in.map(|(_, r)| r),
        };

        // Re-plan every half hour **or when something the plan assumed stops
        // being true**. A plan made while the car still needed 3 kWh keeps
        // asking for them after the car is full, and half an hour of that is a
        // kilowatt-hour of the evening tariff bought for nothing. The design has
        // always said "every five minutes or on event"; the car reaching its
        // target is an event.
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
            let horizon_draw: Vec<f64> = horizon.slots().map(|s| hot_water_draw(s).get()).collect();
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
                    reserve_soc: scenario.config.reserve_soc,
                    degradation_eur_per_kwh: scenario.config.battery_wear_eur_per_kwh,
                    grid_charging_allowed: true,
                })
                .with_thermal(
                    ThermalModel {
                        state: building.state,
                        building: building.building,
                        comfort_min_c: scenario.config.comfort_min_c,
                        comfort_max_c: scenario.config.comfort_max_c,
                        discomfort_eur_per_kelvin_hour: DISCOMFORT_EUR_PER_KELVIN_HOUR,
                        heat_pump: HeatPumpModel {
                            modulating: scenario.config.heat_pump_modulating,
                            cop: building.cop,
                            ..HeatPumpModel::modulating(scenario.config.heat_pump_power)
                        },
                    },
                    &horizon_outdoor,
                )
                .with_dhw(
                    DhwModel {
                        capacity: tank_asset.usable_heat(),
                        stored_now: tank.stored,
                        heater: tank_asset.heater,
                        cop: tank_asset.cop,
                        standing_loss: tank_asset.standing_loss,
                        shortfall_eur_per_kwh: HOT_WATER_SHORTFALL_EUR_PER_KWH,
                    },
                    &horizon_draw,
                )
                .with_limits(planning_limits(&limits, lpc.limit_ends_at(), site));

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
                && let Some(Asset::Evse(evse_asset)) = site.asset(&household.evse)
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
                if departure >= Slot::containing(now) && worth_planning {
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
                        // limit, or a surplus with no plan behind it (D22).
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
            ev_in_plan = problem.ev.is_some();
            match solve(&problem, &household.names, now) {
                Ok(solved) => {
                    // A shortfall is not an error and not a silence: the plan
                    // that came back is the best achievable, and it is short.
                    result.unmet_charge_kwh =
                        result.unmet_charge_kwh.max(solved.unmet_charge.kwh());
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
            .get(&household.pv)
            .copied()
            .map_or(available, Power::outflow);
        let (pv_now, _) = inverter.step(available, ceiling);
        let load_now = weather.load_at(now, household_load(slot));

        let mut state = SiteState::default();
        for (id, power) in [
            (&household.pv, pv_now),
            (&household.load, load_now),
            (
                &household.battery,
                previous
                    .get(&household.battery)
                    .copied()
                    .unwrap_or(Power::ZERO),
            ),
            (
                &household.evse,
                previous
                    .get(&household.evse)
                    .copied()
                    .unwrap_or(Power::ZERO),
            ),
        ] {
            state
                .assets
                .insert(id.clone(), Measurement::power(now, power));
        }
        // What the roof *could* do, which is the one thing a curtailed inverter
        // cannot be asked for indirectly: read its output and a controller
        // learns only what it already commanded.
        if let Some(m) = state.assets.get_mut(&household.pv) {
            m.available_power = Some(available);
        }
        // The guard needs the state of charge as well as the power: a full
        // battery must not be told to charge and one at its backup reserve must
        // not be told to discharge, and both happen between re-plans.
        if let Some(m) = state.assets.get_mut(&household.battery) {
            m.soc = Some(battery.soc());
        }
        state.assets.insert(
            household.dhw.clone(),
            Measurement::power(
                now,
                previous.get(&household.dhw).copied().unwrap_or(Power::ZERO),
            ),
        );
        state.assets.insert(
            household.heat_pump.clone(),
            Measurement::power(
                now,
                previous
                    .get(&household.heat_pump)
                    .copied()
                    .unwrap_or(Power::ZERO),
            ),
        );
        // What the charge point's contactor is actually in. The guard bounds a
        // device by what it *is* doing, not by what it was told to do — a
        // single-phase session is a Schieflast and a three-phase one is not.
        state.phases.insert(household.evse.clone(), evse.mode);
        let grid_now: Power = state.assets.values().filter_map(|m| m.power).sum();
        state.grid = Some(Measurement::power(now, grid_now));

        if slot != delivered_slot {
            // Score the quarter hour that just ended: what the forecasts said
            // against what the house did. This is the number that says whether
            // the saving below means anything.
            if slot_minutes > 0 {
                let minutes = f64::from(slot_minutes);
                if let Some(band) = pv_forecast.at(delivered_slot) {
                    pv_scored.push((band, pv_slot_wh / minutes * 60.0));
                }
                if let Some(band) = load_forecast.at(delivered_slot) {
                    load_scored.push((band, load_slot_wh / minutes * 60.0));
                }
            }
            pv_slot_wh = 0.0;
            load_slot_wh = 0.0;
            slot_minutes = 0;
            delivered.clear();
            delivered_slot = slot;
        }
        pv_slot_wh += available.get() * step.as_seconds_f64() / 3600.0;
        load_slot_wh += load_now.inflow().get() * step.as_seconds_f64() / 3600.0;
        slot_minutes += 1;

        let decision = arbiter.tick(Tick {
            now,
            site,
            state: &state,
            limits: &limits,
            plan: plan.as_ref(),
            overrides: &overrides,
            previous: &previous,
            delivered: &delivered,
            phases: &phase_state,
        });

        // The hardware answers. The contactor first: changing the conductor
        // count while current is flowing is what welds one shut.
        if let Some(next) = decision.phases.get(&household.evse)
            && evse.set_mode(next.mode)
        {
            result.phase_switches += 1;
        }
        phase_state = decision.phases.clone();

        let battery_actual = battery.step(
            decision
                .commanded
                .get(&household.battery)
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
                    .get(&household.evse)
                    .copied()
                    .unwrap_or(Power::ZERO)
            } else {
                Power::ZERO
            },
            step,
        );
        let hp_actual = building.step(
            decision
                .commanded
                .get(&household.heat_pump)
                .copied()
                .unwrap_or(Power::ZERO),
            outdoor_now,
            step,
        );
        let (dhw_actual, dhw_short) = tank.step(
            decision
                .commanded
                .get(&household.dhw)
                .copied()
                .unwrap_or(Power::ZERO),
            weather.draw_in(slot, hot_water_draw(slot)) * (step / SLOT),
            step,
        );
        previous.insert(household.battery.clone(), battery_actual);
        previous.insert(household.evse.clone(), evse_actual);
        previous.insert(household.heat_pump.clone(), hp_actual);
        previous.insert(household.dhw.clone(), dhw_actual);
        result.dhw_kwh += dhw_actual.kw() * step.as_seconds_f64() / 3600.0;
        result.cold_water_kwh += dhw_short.kwh();
        result.tank_min_fill = result.tank_min_fill.min(tank.fill());
        // The inverter's ceiling for the next tick. Kept with the other
        // commanded values so a stale one behaves the way a stale command to any
        // other device does.
        let pv_allowed = decision
            .commanded
            .get(&household.pv)
            .copied()
            .unwrap_or(-available)
            .outflow();
        previous.insert(household.pv.clone(), -pv_allowed);
        // Curtailment is production the *controller refused*, measured against
        // what the weather offered this tick. Taking the difference between the
        // weather and the meter instead would count the inverter's own settling
        // time as a decision, and print a curtailment figure every sunrise on a
        // day when nothing was curtailed at all.
        result.curtailed_kwh +=
            (available - pv_allowed).max(Power::ZERO).kw() * step.as_seconds_f64() / 3600.0;
        for (id, power) in [
            (&household.battery, battery_actual),
            (&household.evse, evse_actual),
            (&household.heat_pump, hp_actual),
            (&household.dhw, dhw_actual),
        ] {
            *delivered.entry(id.clone()).or_insert(Energy::ZERO) += power.over(step);
        }

        // ── Accounting ──────────────────────────────────────────────────────
        let hours = step.as_seconds_f64() / 3600.0;
        let grid = load_now + battery_actual + evse_actual + hp_actual + dhw_actual + pv_now;

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
            register.grid_feed_in += kwh(grid.outflow());
            register.device_consumption += kwh(battery_actual.inflow() + evse_actual.inflow());
            register.device_generation += kwh(battery_actual.outflow() + evse_actual.outflow());
        }
        result.produced_kwh += pv_now.outflow().kw() * hours;
        result.consumed_kwh += load_now.inflow().kw() * hours;
        result.imported_kwh += grid.inflow().kw() * hours;
        result.exported_kwh += grid.outflow().kw() * hours;
        result.battery_throughput_kwh += battery_actual.abs().kw() * hours;
        if evse.mode == PhaseMode::Single && evse_actual > Power::ZERO {
            result.single_phase_minutes += 1;
        }
        if scenario.planner
            && plan
                .as_ref()
                .is_none_or(|p| p.is_stale(now, arbiter.config().max_plan_age))
        {
            result.minutes_without_a_plan += 1;
        }
        result.battery_soc_min = result.battery_soc_min.min(battery.soc().fraction());
        result.ev_charged_kwh += evse_actual.inflow().kw() * hours;
        result.heat_pump_kwh += hp_actual.inflow().kw() * hours;
        result.indoor_min_c = result.indoor_min_c.min(building.indoor_c());
        result.indoor_max_c = result.indoor_max_c.max(building.indoor_c());
        let outside_band = (scenario.config.comfort_min_c - building.indoor_c())
            .max(building.indoor_c() - scenario.config.comfort_max_c)
            .max(0.0);
        result.discomfort_kelvin_hours += outside_band * hours;

        if let Some(price) = prices.at(slot) {
            result.cost.energy_eur += grid.inflow().kw() * hours * price.import_f64()
                - grid.outflow().kw() * hours * price.export_f64();
        }
        // Half the wear on each leg, so a full cycle pays it once — the same
        // convention the planner's objective uses.
        result.cost.wear_eur +=
            battery_actual.abs().kw() * hours * (scenario.config.battery_wear_eur_per_kwh / 2.0);
        result.cost.discomfort_eur += outside_band * hours * DISCOMFORT_EUR_PER_KELVIN_HOUR;

        // Two different stories, and they were being told as one. A limit from a
        // Steuerbox is a network operator reducing the house; the same number
        // arrived at from `init` or `failsafe` is the manager restraining itself
        // because nothing is talking to it. Counting the second as a § 14a event
        // reports a reduction on a day when the operator said nothing at all.
        if limits.steuve_ceiling.is_some() {
            if limits.in_failsafe {
                result.failsafe_minutes += 1;
            } else {
                result.limited_minutes += 1;
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
                netzwirksam: hems_grid::netzwirksamer_leistungsbezug(
                    battery_actual.inflow() + evse_actual.inflow() + hp_actual.inflow(),
                    load_now.inflow(),
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
    result.worst_overshoot_w = evidence
        .closed()
        .iter()
        .filter_map(hems_grid::ControlEvent::worst_overshoot)
        .map(Power::get)
        .fold(0.0, f64::max);

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
    result.baseline = baseline_cost(scenario, &weather, &array, &prices, site.location);

    result.quarter_hours = registers.into_values().collect();
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
    result.pv_forecast = Calibration::score(pv_scored.iter().copied());
    result.load_forecast = Calibration::score(load_scored.iter().copied());
    result.roof_correction = learned
        .roof
        .ratio_at(Slot::containing(start + Duration::hours(12)));
    result.history_days = learned.days;

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

    // ── The site, in S2 terms ───────────────────────────────────────────────
    //
    // Every asset described the way EN 50491-12-2 describes it, which is what a
    // Resource Manager or a Customer Energy Manager on the other end of a
    // WebSocket would be sent. Describing the site every run is what keeps the
    // flexibility model a dependency rather than documentation.
    result.s2_resources = site
        .assets
        .iter()
        .filter(|a| !matches!(a, Asset::Meter(_) | Asset::Relay(_)))
        .filter(|a| {
            hems_flex::control_type_for(a, scenario.ev.is_some())
                != hems_flex::ControlType::NotControllable
        })
        .count();

    let self_used = (result.produced_kwh - result.exported_kwh).max(0.0);
    let total_consumed =
        result.consumed_kwh + result.ev_charged_kwh + result.heat_pump_kwh + result.dhw_kwh;
    if total_consumed > 0.0 {
        result.self_sufficiency = (self_used / total_consumed).clamp(0.0, 1.0);
    }
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
fn baseline_cost(
    scenario: &Scenario,
    weather: &Weather,
    array: &ArrayModel,
    prices: &PriceStack,
    location: GeoPoint,
) -> CostBreakdown {
    let start = scenario.start();
    let step = CONTROL_PERIOD;
    let hours = step.as_seconds_f64() / 3600.0;
    let mut car_remaining = scenario.ev.map_or(Energy::ZERO, |e| {
        (e.energy_target - e.energy_now).max(Energy::ZERO)
    });
    let mut building = BuildingSim {
        nominal_electrical: scenario.config.heat_pump_power,
        thermostat_set_c: scenario.config.comfort_min_c + 0.5,
        thermostat_max_c: scenario.config.comfort_max_c,
        ..BuildingSim::new(21.0)
    };
    let mut thermostat_on = false;
    let mut cost = CostBreakdown::default();
    let mut tank = TankSim::new(
        Energy::from_kwh(
            scenario.config.dhw_litres * hems_core::asset::WATER_KWH_PER_LITRE_KELVIN * 15.0,
        ),
        scenario.config.dhw_heater,
    );
    let tank_open = tank.stored;
    let mut now = start;

    while now < start + Duration::days(1) {
        let slot = Slot::containing(now);
        // The **same day**, down to the cloud at 12:19. A baseline run against
        // a different realisation than the optimised day is not a comparison at
        // all — it prices two different Tuesdays and calls the difference a
        // saving.
        let pv = -weather.production_at(array, location, now);
        let load = weather.load_at(now, household_load(slot));

        // An unmanaged wallbox starts as soon as the car is plugged in and runs
        // until the car is full — which means it cannot start before the cable
        // goes in either. A baseline that charged a car that was not there
        // would buy the cheap hours of the night the optimiser is being judged
        // on finding.
        let plugged_in = scenario
            .ev
            .is_none_or(|e| now >= start + e.arrival && now < start + e.departure);
        let ev = if car_remaining > Energy::ZERO && plugged_in {
            let p = Power::from_kw(11.0);
            let delivered = Energy::new(p.get() * hours * 0.92);
            car_remaining = (car_remaining - delivered).max(Energy::ZERO);
            p
        } else {
            Power::ZERO
        };

        // An ordinary thermostat with a half-kelvin hysteresis: on at the
        // bottom of the comfort band, off half a degree above it. It knows
        // nothing about the price or the weather, which is the whole of the
        // difference being measured.
        let low = scenario.config.comfort_min_c;
        if building.indoor_c() < low {
            thermostat_on = true;
        } else if building.indoor_c() > low + 0.5 {
            thermostat_on = false;
        }
        let hp = building.step(
            if thermostat_on {
                scenario.config.heat_pump_power
            } else {
                Power::ZERO
            },
            weather.outdoor_at(now),
            step,
        );

        // An unmanaged tank reheats whenever it is not full and stops when it
        // is. It never uses the store as a store, which is the whole of the
        // difference being measured.
        let (dhw, _) = tank.step(
            scenario.config.dhw_heater,
            weather.draw_in(slot, hot_water_draw(slot)) * (step / SLOT),
            step,
        );

        let grid = load + ev + hp + dhw + pv;
        if let Some(price) = prices.at(slot) {
            cost.energy_eur += grid.inflow().kw() * hours * price.import_f64()
                - grid.outflow().kw() * hours * price.export_f64();
        }
        // A thermostat is not free of discomfort: it starts reheating only once
        // the house has already fallen through the band. Leaving that out would
        // credit the planner with comfort the baseline also delivered.
        let outside = (scenario.config.comfort_min_c - building.indoor_c())
            .max(building.indoor_c() - scenario.config.comfort_max_c)
            .max(0.0);
        cost.discomfort_eur += outside * hours * DISCOMFORT_EUR_PER_KELVIN_HOUR;
        now += step;
    }
    // The same ledger the optimised day closes, so both sides are measured in
    // the same way. A thermostat ends the day about where it started, so this is
    // usually near zero — which is exactly why it has to be computed rather than
    // assumed.
    let mean_import = prices
        .slots
        .iter()
        .map(hems_tariff::SlotPrice::import_f64)
        .sum::<f64>()
        / prices.slots.len().max(1) as f64;
    cost.stored_eur = (tank_open - tank.stored).kwh() / tank.cop.max(f64::EPSILON) * mean_import;
    cost
}

/// A household base load: low at night, peaks morning and evening.
/// Heat drawn from the hot-water tank in one quarter hour, watt-hours.
///
/// A four-person household uses six to seven kilowatt-hours of hot water a day,
/// and almost none of it at random: a shower before work, washing-up after
/// lunch, baths and dishes in the evening. That predictability is the whole
/// reason a tank is worth planning with — a store whose demand nobody can
/// forecast is a store nobody can pre-charge.
fn hot_water_draw(slot: Slot) -> Energy {
    let hour = f64::from(slot.local_minute_of_day()) / 60.0;
    let peak = |centre: f64, width: f64, kwh: f64| kwh * (-((hour - centre) / width).powi(2)).exp();
    // Kilowatt-hours of heat per hour, sampled at the middle of the slot and
    // taken over a quarter of an hour.
    let kw = peak(7.0, 0.8, 1.7) + peak(13.0, 0.7, 0.6) + peak(20.0, 1.1, 1.4) + 0.03;
    Energy::from_kwh(kw * 0.25)
}

fn household_load(slot: Slot) -> Power {
    let minute = f64::from(slot.local_minute_of_day());
    let hour = minute / 60.0;
    let base = 250.0;
    let morning = 700.0 * (-((hour - 7.5) / 1.2).powi(2)).exp();
    let evening = 1100.0 * (-((hour - 19.0) / 2.0).powi(2)).exp();
    Power::new(base + morning + evening)
}
