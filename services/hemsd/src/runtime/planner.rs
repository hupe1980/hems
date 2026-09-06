//! The receding horizon, on a real box.
//!
//! Every five minutes: ask the fleet what electricity costs and what the sky
//! will do, correct the roof model with what *this* roof has actually been
//! delivering, read the battery's charge off its own meter, solve, and publish a
//! plan the arbiter will follow for the next quarter hour.
//!
//! # What it plans, and what it deliberately does not
//!
//! A [`Problem`] has an `Option` for every store, and
//! the planner fills in **exactly what the drivers can actually tell it**:
//!
//! | Modelled | Needs | State |
//! |---|---|---|
//! | grid, roof, load, § 14a and § 9 ceilings | prices, sky, measurements | ✅ |
//! | the battery | a fresh state of charge off its own meter | ✅ |
//! | the car | an arrival, a departure and a target — nothing reports them yet | ⏳ |
//! | the building | an indoor temperature — no driver publishes one yet | ⏳ |
//! | the hot-water tank | a tank temperature — likewise | ⏳ |
//!
//! Leaving a store out is not the same as modelling it as absent, and the
//! difference is [`AssetNames`]. A plan that *named* the charge point while
//! having no car in the problem would emit a target of zero watts with an
//! envelope pinned at zero — which the arbiter would read as an instruction not
//! to charge, all day, from a planner that had simply not been told about the
//! car. So an asset is named if and only if the problem models it, and the two
//! are built from the same three values, three lines apart, so they cannot
//! drift.
//!
//! # It is allowed to fail, and a failure is a number
//!
//! No prices, no sky, an infeasible solve, a solve that overran its budget:
//! every one of them leaves the last plan in place until it goes stale, and then
//! leaves the arbiter with none — which is surplus tracking, which keeps the
//! house safe and lawful (G3). What must never happen is that any of them is
//! *silent*, so [`crate::runtime::Status::minutes_without_a_plan`] counts from
//! the last plan the box actually published rather than from when it started.

use std::sync::Arc;

use hems_core::prelude::{Asset, Horizon, Plan, Power, Site, Slot};
use hems_forecast::Forecast;
use hems_forecast::load::LoadProfile;
use hems_forecast::quantile::Band;
use hems_forecast::residual::ResidualModel;
use hems_forecast::{ArrayModel, WeatherSeries};
use hems_optimizer::model::{BatteryModel, DhwModel, Problem};
use hems_optimizer::solve::{AssetNames, solve};
use hems_service::{Health, Shutdown};
use hems_tariff::PriceStack;
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock};

use crate::config::{ControlSettings, TariffSettings};
use crate::runtime::fleet::{Fleet, Prices, Sky};
use crate::runtime::transport::Shared;
use crate::site::Household;

/// What the box has learned about its own house.
///
/// Two models, both online and both cheap: the multiplicative corrector that
/// turns a *geometric roof model* into a forecast of **this** roof, and the
/// household's own quarter hours by day type. Neither can be shipped from a
/// factory — the tree that shades the east string and the hour somebody puts the
/// oven on are properties of one address.
#[derive(Debug)]
pub struct Learned {
    /// What this roof actually delivers against what the model says it should.
    pub pv: ResidualModel,
    /// This household's own load, by day type and quarter hour.
    pub load: LoadProfile,
    /// Which house this is: the thermal record, and the building fitted from it.
    ///
    /// The one of the three that changes the *shape* of a plan rather than its
    /// inputs. The fabric capacity decides whether pre-heating into a cheap hour
    /// pays at all, and it differs by a factor of three between a 1970s
    /// solid-wall house and a new timber frame — so a box planning every
    /// household against `Rc2::house()` over-heats two thirds of them and pays
    /// the comfort slack for the overshoot.
    pub building: hems_forecast::building::Record,
}

/// The names the two models are stored under.
///
/// Constants rather than literals at the call sites, because a typo in one of
/// two spellings is a box that saves its learning under one name and looks for
/// it under another — and looks, from every screen, exactly like a box that has
/// just been installed.
pub const PV_MODEL: &str = "pv-residual";
/// The household's own load profile.
pub const LOAD_MODEL: &str = "load-profile";
/// The house's own thermal record and the building fitted from it.
pub const BUILDING_MODEL: &str = "building";

impl Learned {
    /// A box that has just been switched on and knows nothing.
    #[must_use]
    pub fn new(land: metering::Bundesland) -> Self {
        Self {
            pv: ResidualModel::new(hems_forecast::residual::DEFAULT_ALPHA),
            load: LoadProfile::new(land),
            building: hems_forecast::building::Record::default(),
        }
    }

    /// What the box remembered from before it was restarted.
    ///
    /// A fortnight of observations is what makes a forecast worth having, and a
    /// box that forgot them on every reboot would start from a factory roof and
    /// refuse to plan at all until it had seen a quarter hour of its own load.
    ///
    /// Either half may be missing — a fresh install, or a model whose shape has
    /// moved on since it was written — and a missing half is simply relearned.
    /// The alternative is a box that will not start after an update because it
    /// cannot read something it can perfectly well rebuild.
    #[must_use]
    pub fn restored(store: &crate::store::Store, land: metering::Bundesland) -> Self {
        let mut learned = Self::new(land);
        match store.learned::<ResidualModel>(PV_MODEL) {
            Ok(Some(pv)) => learned.pv = pv,
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "the roof's own correction could not be read"),
        }
        match store.learned::<LoadProfile>(LOAD_MODEL) {
            // The Bundesland is configuration and the profile is history, so the
            // configured one wins: a household that corrected its Land in the
            // file must not be given back the old one by its own store.
            Ok(Some(mut load)) => {
                load.land = land;
                learned.load = load;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "the household's own profile could not be read"),
        }
        match store.learned::<hems_forecast::building::Record>(BUILDING_MODEL) {
            Ok(Some(building)) => learned.building = building,
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "the house's own thermal record could not be read");
            }
        }
        learned
    }

    /// Keep both, so a reboot does not cost a fortnight.
    pub fn remember(&self, store: &crate::store::Store, now: OffsetDateTime) {
        for (name, written) in [
            (PV_MODEL, store.put_learned(PV_MODEL, &self.pv, now)),
            (LOAD_MODEL, store.put_learned(LOAD_MODEL, &self.load, now)),
            (
                BUILDING_MODEL,
                store.put_learned(BUILDING_MODEL, &self.building, now),
            ),
        ] {
            if let Err(error) = written {
                // A warning rather than a failure: what is at stake is a week of
                // slightly worse forecasts, and a box that stopped controlling a
                // house because it could not write a cache would be trading the
                // wrong thing away.
                tracing::warn!(name, %error, "what the box has learned could not be kept");
            }
        }
    }

    /// One completed quarter hour of the box's own history.
    ///
    /// Called from the control loop on the slot boundary, because that is the
    /// only place that knows a quarter hour is *over*: a sample taken part-way
    /// through one teaches the model a mean of a fraction of a slot, which is
    /// the same mistake as reading a meter register mid-interval.
    pub fn observe(&mut self, slot: Slot, modelled_pv: Option<f64>, pv: f64, load: Power) {
        if let Some(modelled) = modelled_pv {
            self.pv.observe(slot, modelled, pv);
        }
        self.load.observe(slot, load);
    }

    /// One completed quarter hour of the *house*, where the box can see one.
    ///
    /// Separate from [`Learned::observe`] because it needs three things that
    /// arrive together or not at all — how warm it was inside, how warm outside,
    /// and how much heat went in — and a household with no indoor sensor has
    /// none of them while still having a perfectly good load profile.
    pub fn observe_house(&mut self, slot: Slot, indoor_c: f64, outdoor_c: f64, heat_kw: f64) {
        self.building.observe(slot, indoor_c, outdoor_c, heat_kw);
    }
}

/// Everything the planning loop needs that does not change while it runs.
pub struct Planner {
    /// The house.
    pub household: Household,
    /// The roof, as the solar model sees it.
    pub array: ArrayModel,
    /// What a kilowatt-hour costs and earns.
    pub tariff: TariffSettings,
    /// Cadences and budgets.
    pub control: ControlSettings,
    /// What a kilowatt-hour of battery throughput costs in wear, €/kWh.
    ///
    /// Not a property of the pack in the site model, because it is a property of
    /// what the household *paid* for it: the cell price over the warranted
    /// throughput. Leaving it at zero reproduces a cost-only optimiser, which
    /// the literature measures cycling a battery for a spread that does not
    /// cover the damage.
    pub wear_eur_per_kwh: f64,
    /// When the car has to be charged by, and how far — where a charge point
    /// driver can say whether a car is there at all.
    ///
    /// `None` on a household with no such driver, and that is the whole of the
    /// gate: a departure and a target are what the *household* wants, and a
    /// deadline without a car on the cable is a schedule the arbiter spends the
    /// afternoon failing to follow.
    pub charging: Option<crate::config::EebusEvSettings>,
}

/// Why a re-plan produced nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// No day-ahead prices, and none cached.
    ///
    /// A household on a **fixed** tariff never reaches this: it has no curve to
    /// ask for and its own price is in its configuration.
    NoPrices,
    /// No sky, and none cached — so no production forecast.
    NoSky,
    /// The box has no load profile **and** cannot read its own connection
    /// point, so it has neither a history to plan against nor a reading to
    /// persist from.
    ///
    /// A box with no profile but a working meter does not reach this: it plans
    /// against persistence, with a band that widens into the horizon.
    NoHistory,
    /// The battery's state of charge is not fresh, so its level is unknown.
    ///
    /// A store whose fill nobody can read is one no plan may move: a schedule
    /// built on a guessed state of charge empties a pack it thought was full.
    NoStateOfCharge,
    /// The solver refused, or failed.
    Unsolvable(String),
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::NoPrices => write!(f, "no day-ahead prices"),
            Reason::NoSky => write!(f, "no weather forecast"),
            Reason::NoHistory => write!(
                f,
                "no load history and no grid meter, so there is nothing to plan against"
            ),
            Reason::NoStateOfCharge => write!(f, "the battery's charge is not being reported"),
            Reason::Unsolvable(why) => write!(f, "the solver could not answer: {why}"),
        }
    }
}

/// What the last fetch returned, kept so a network blip is not a lost plan.
///
/// A day-ahead curve fetched an hour ago is still tomorrow's auction result, and
/// an ICON-D2 run is three hours old by construction. Refusing to plan because
/// the WAN is down would throw away a perfectly good answer — so the cache has
/// no expiry, and what ages out is the *coverage*: prices run out at the end of
/// the published day, and a horizon past them is priced by the tariff's own flat
/// fallback, which is the honest shape for "nobody knows yet".
#[derive(Debug, Default)]
struct Cached {
    prices: Option<Prices>,
    sky: Option<Sky>,
    /// The grid's carbon intensity, where the household prices it.
    ///
    /// Empty by default and empty for ever on a household whose
    /// `co2_eur_per_kg` is zero, which is the ordinary case: the planner then
    /// falls back to a flat annual figure, and a flat figure multiplied by a
    /// zero price is the same plan either way.
    carbon: std::collections::BTreeMap<Slot, f64>,
    /// When each was last successfully fetched, for the health surface.
    prices_at: Option<OffsetDateTime>,
    sky_at: Option<OffsetDateTime>,
}

/// What the planner leaves for the control loop to read.
///
/// Two handles that travel together because they are read together: on each
/// quarter-hour boundary the loop teaches the residual model against
/// `modelled_pv` and scores itself against `bands`. Bundled so that adding the
/// second one did not widen a signature that is already at the limit.
#[derive(Clone)]
pub struct Published {
    /// The plane-of-array figure each slot's forecast was built from.
    ///
    /// The corrector has to be taught against the same modelled number the
    /// forecast used, or it learns the cloud rather than the roof.
    pub modelled_pv: Arc<RwLock<std::collections::BTreeMap<Slot, f64>>>,
    /// The bands the standing plan was made against (D117).
    pub bands: Arc<RwLock<crate::runtime::day::PublishedBands>>,
    /// The outdoor temperature the plan was made against, by slot.
    ///
    /// Published for the same reason `modelled_pv` is: the control loop is the
    /// only place that knows a quarter hour is *over*, and it is the planner
    /// that has the weather. A box that fetched its own forecast on the slot
    /// boundary would be teaching its building from one series and planning it
    /// against another.
    pub outdoor: Arc<RwLock<std::collections::BTreeMap<Slot, f64>>>,
}

/// Plan, publish, sleep, repeat — until the process is asked to stop.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    planner: Planner,
    registry: Shared,
    fleet: Fleet,
    plan: Arc<RwLock<Option<Plan>>>,
    prices: Arc<RwLock<Option<PriceStack>>>,
    published: Published,
    learned: Arc<Mutex<Learned>>,
    store: Option<Arc<Mutex<crate::store::Store>>>,
    health: Health,
    shutdown: Shutdown,
) {
    let mut cached = Cached::default();
    // Which local day the building was last identified on. Once a day: the
    // search is a few hundred passes over a fortnight of quarter hours, which is
    // nothing on a schedule of days and is pure waste every five minutes — and
    // a house does not change between two plans.
    let mut fitted_on: Option<time::Date> = None;
    // Whether this household prices carbon at all. Decided once: it is
    // configuration, and a box that re-derived it every five minutes would be
    // asking the same question of the same file eighty-eight times a day.
    let carbon_matters = planner.control.preferences.co2_eur_per_kg > 0.0;
    let period = std::time::Duration::from_secs(planner.control.replan_every_s.max(60));
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            () = shutdown.clone().wait() => {
                tracing::info!("the planner is stopping");
                return;
            }
            _ = ticker.tick() => {}
        }
        let now = OffsetDateTime::now_utc();
        refresh(&fleet, &mut cached, carbon_matters, now).await;

        let today = metering::calendar::local_day(now);
        if fitted_on != Some(today) {
            fitted_on = Some(today);
            let found = learned
                .lock()
                .await
                .building
                .refit(hems_core::prelude::SLOT);
            if let Some(found) = found {
                tracing::info!(
                    fabric_kwh_per_k = found.building.mass_capacity_kwh_per_k,
                    loss_k_per_kw = found.building.r_air_out_k_per_kw,
                    rmse_k = found.rmse_k,
                    improvement = found.improvement(),
                    samples = found.samples,
                    "the box identified its own building"
                );
            }
        }

        match attempt(
            &planner, &registry, &cached, &prices, &published, &learned, now,
        )
        .await
        {
            Ok(solved) => {
                let cost = solved
                    .plan
                    .expected_cost
                    .as_ref()
                    .map(hems_core::prelude::CostBreakdown::total);
                tracing::info!(
                    slots = solved.plan.slots.len(),
                    expected_eur = cost,
                    "a plan was published"
                );
                *plan.write().await = Some(solved.plan);
                health.good("planner", now);
                // Kept here rather than on every slot boundary: the models are
                // taught ninety-six times a day and written twenty-eight, which
                // is the difference between a cache and a write-ahead log on a
                // box whose storage is an SD card.
                if let Some(store) = &store {
                    learned.lock().await.remember(&*store.lock().await, now);
                }
            }
            Err(reason) => {
                tracing::warn!(%reason, "no plan this round");
                health.bad("planner", reason.to_string());
            }
        }
    }
}

/// Ask the fleet, keeping whatever came back last.
async fn refresh(fleet: &Fleet, cached: &mut Cached, carbon_matters: bool, now: OffsetDateTime) {
    // Two days of prices, because the planner's horizon is two days: a re-plan
    // in the evening on one day of prices is told the whole of tomorrow costs
    // the flat fallback, and defers every flexible kilowatt-hour into it.
    let horizon = Horizon::new(now, 96 * 2);
    match fleet.prices(horizon).await {
        Ok(Some(prices)) => {
            cached.prices_at = Some(now);
            cached.prices = Some(prices);
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "keeping the prices this box already had"),
    }
    match fleet.sky().await {
        Ok(Some(sky)) => {
            cached.sky_at = Some(now);
            cached.sky = Some(sky);
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "keeping the weather this box already had"),
    }
    // Only where the household actually prices carbon. Fetching a series to
    // multiply it by zero is a request nobody needed, and the planner's flat
    // fallback is exactly the right answer for a plan that does not care.
    if carbon_matters {
        match fleet.carbon(horizon).await {
            Ok(Some(carbon)) => cached.carbon = carbon.series(),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "keeping the carbon intensity this box already had");
            }
        }
    }
}

/// One solve, from what the box knows right now.
async fn attempt(
    planner: &Planner,
    registry: &Shared,
    cached: &Cached,
    prices_out: &Arc<RwLock<Option<PriceStack>>>,
    published: &Published,
    learned: &Arc<Mutex<Learned>>,
    now: OffsetDateTime,
) -> Result<hems_optimizer::solve::Solved, Reason> {
    let site = &planner.household.site;
    let horizon = Horizon::new(now, planner.control.horizon_slots.max(4));

    // ── What it costs ───────────────────────────────────────────────────────
    //
    // A fixed tariff needs nobody: its price is in the configuration, and a
    // household on one has no spread to shift load for. A dynamic one without a
    // curve is a plan optimised against a flat number, which is worse than
    // useless — it would move the battery for a spread that does not exist — so
    // it is refused.
    let spot = match (&cached.prices, planner.tariff.fixed_ct_per_kwh) {
        (Some(prices), _) => prices.spot(),
        (None, Some(_)) => std::collections::BTreeMap::new(),
        (None, None) => return Err(Reason::NoPrices),
    };
    let mut tariff = planner.tariff.tariff(site, spot);
    // The last link of a chain that used to be dead end to end: the
    // Energy-Charts parser had no consumer, the poller dropped what it
    // fetched, `SlotPrice::co2_g_per_kwh` was hard-coded `None`, and the
    // objective term that reads it could therefore only ever see a flat
    // annual constant — which makes a carbon price algebraically the same
    // thing as an autarky premium, because every hour is equally dirty.
    tariff.carbon_g_per_kwh = cached.carbon.clone();
    let prices = PriceStack::build(&tariff, horizon);
    // Published for the control loop. The quarter-hour registers MiSpeL and
    // § 42c settle from carry two *prices* — the anzulegender Wert and the spot
    // price — and the loop that measures the quantities has no tariff of its
    // own. Sharing this one is what lets it write a whole register rather than
    // a register with two zeros in it.
    *prices_out.write().await = Some(prices.clone());

    // ── What the roof will make ─────────────────────────────────────────────
    let sky = cached.sky.as_ref().ok_or(Reason::NoSky)?;
    let series = to_series(sky);
    let modelled = series.modelled_production(&planner.array, site.location);
    // Published for the control loop, which teaches the corrector against
    // exactly these figures on every slot boundary. Handing it a fresh
    // clear-sky number instead would teach it the weather rather than the roof.
    *published.modelled_pv.write().await = modelled.iter().copied().collect();

    // ── What the house will use, and what it has ────────────────────────────
    let observed = {
        let mut guard = registry.lock().await;
        guard.observe(Some(&planner.household.grid_meter), now)
    };
    let held = learned.lock().await;
    let pv = corrected(&held.pv, &modelled, horizon);
    // The household's own load, from its own history where it has any.
    //
    // Where it has none — a box installed this morning — the fallback is
    // **persistence**: the next hours look like the last one, with a band that
    // widens the further out it reaches, because persistence is excellent for
    // the next quarter hour and worthless by tomorrow. That is worth having
    // rather than refusing to plan: a household gets a plan on its first
    // evening instead of on its second, and the band says how much to trust it.
    //
    // What is *not* worth having is a guess with nothing behind it. A box that
    // cannot even read its own connection point has no load to persist and no
    // history to fall back on, and inventing one would plan a house nobody is
    // measuring.
    //
    // The gate is **has this profile learned anything**, not "does the horizon's
    // first slot have a cell". Asking about the first slot let a Friday-evening
    // re-plan through on a week of workdays and then forecast an empty house for
    // the whole of Saturday and Sunday, because `band_at` answered an unbacked
    // cell with a *certain zero*. It no longer does — an empty cell borrows the
    // same quarter hour from the day types the household has been seen on — and
    // the gate now asks the question the profile can actually answer.
    let load = if held.load.is_empty() {
        let recent = crate::drivers::household_load(&observed).ok_or(Reason::NoHistory)?;
        // Doubling by this time tomorrow, which is about what a single reading
        // is worth twenty-four hours out.
        hems_forecast::naive::persistence(recent, horizon, 0.9)
    } else {
        held.load.forecast(horizon)
    };
    // Read while the lock is already held: the alternative is taking it a second
    // time forty lines further down for one `Copy` value.
    let building = held.building.building();
    drop(held);

    publish(published, &series, &pv, &load).await;

    // ── The battery, if its meter is telling us where it is ─────────────────
    let battery = battery_model(
        site,
        &observed,
        &planner.household,
        planner.wear_eur_per_kwh,
    );

    let limits = crate::site::planning_limits(&observed.limits, None, site, now);
    // The names are decided from the same three facts the problem is built from,
    // right here, so the two cannot disagree about whether there is a battery.
    // See the module note on why naming an asset the problem does not model is
    // an instruction rather than an omission.
    let dhw = dhw_model(site, &observed, &planner.household);
    // The house itself. Built here rather than inside the blocking task because
    // it reads the learned building, and the lock is not something to carry
    // across a ten-second solve.
    let thermal = heat_pump_model(site, &observed, &planner.household, building);
    let outdoor: Vec<f64> = series.outdoor_c_over(horizon);
    let ev = ev_session(
        site,
        &observed,
        &planner.household,
        planner.charging.as_ref(),
        horizon,
    );
    // The same prior the reference days plan against, and deliberately the same
    // one: a box that planned its tank against a different shape from the day
    // that measured the saving would be a box whose figures describe a household
    // nobody can buy.
    let draw: Vec<f64> = horizon
        .slots()
        .map(|s| hems_forecast::hotwater::draw(s).get())
        .collect();
    let names = AssetNames {
        battery: battery.and(planner.household.battery.clone()),
        pv: planner.household.pv.clone(),
        evse: ev.as_ref().and(planner.household.evse.clone()),
        // Named exactly where the problem models it, for the same reason the
        // tank is: an asset named but not modelled emits a target of zero with
        // an envelope pinned at zero, and the arbiter obeys that as an
        // instruction (D96).
        heat_pump: thermal.as_ref().and(planner.household.heat_pump.clone()),
        // Named only where the problem models it, which here means only where a
        // driver measured the tank: naming an asset the problem does not model
        // emits a target of zero with an envelope pinned at zero, and the
        // arbiter obeys that as an instruction (D96).
        dhw: dhw.as_ref().and(planner.household.dhw.clone()),
        shiftable: Vec::new(),
    };
    let budget = planner.control.solve_budget_s;
    let objective = planner.control.preferences.objective();
    let risk = planner.control.risk.model();

    // Off the runtime, with everything it needs **moved** in. HiGHS is a
    // synchronous C++ solver and a ten-second solve on a runtime thread is ten
    // seconds in which the guard's own tick cannot be scheduled — which on a
    // gateway box with two cores is the difference between a control period and
    // a missed one. `Problem` borrows its inputs, so it is assembled inside the
    // task rather than sent into it.
    tokio::task::spawn_blocking(move || {
        let mut problem = Problem::new(horizon, &prices, &pv, &load)
            .with_limits(limits)
            .with_objective(objective)
            // Until this was wired the box could only ever plan against one
            // median: the whole scenario set, its CVaR tail and the quantile
            // knob were reachable from `hemsd simulate` and from nothing on a
            // wall, so a household could not act on the trade-off `hemsd risk`
            // had measured for it.
            .with_risk(risk);
        if let Some(model) = battery {
            problem = problem.with_battery(model);
        }
        if let Some(model) = dhw {
            problem = problem.with_dhw(model, &draw);
        }
        if let Some(session) = ev {
            problem = problem.with_ev(session);
        }
        if let Some(model) = thermal {
            problem = problem.with_thermal(model, &outdoor);
        }
        problem.solve_budget_s = budget;
        solve(&problem, &names, now)
    })
    .await
    .map_err(|e| Reason::Unsolvable(format!("the solve panicked: {e}")))?
    .map_err(|e| Reason::Unsolvable(e.to_string()))
}

/// Hand the control loop the three series the plan was made against.
///
/// All three for the same reason, which is D117's: the loop is the only place
/// that knows a quarter hour is *over*, and it must score and learn against
/// what was **acted on** rather than against a fresh forecast. A band nobody
/// planned against says nothing about the plan, and a building identified from
/// one weather series and planned against another is identified from noise.
async fn publish(published: &Published, series: &WeatherSeries, pv: &Forecast, load: &Forecast) {
    *published.outdoor.write().await = series
        .slots
        .iter()
        .map(|(slot, point)| (*slot, point.temperature_c))
        .collect();
    *published.bands.write().await = pv
        .slots
        .iter()
        .zip(load.slots.iter())
        .map(|((slot, p), (_, l))| (*slot, (*p, *l)))
        .collect();
}

/// `forecastd`'s answer, as the forecasting crate's own type.
fn to_series(sky: &Sky) -> WeatherSeries {
    WeatherSeries {
        slots: sky
            .points
            .iter()
            .map(|p| {
                (
                    Slot::containing(p.slot),
                    hems_forecast::weather::WeatherPoint {
                        ghi_w_per_m2: p.ghi_w_per_m2,
                        temperature_c: p.temperature_c,
                        cloud_cover: p.cloud_cover,
                    },
                )
            })
            .collect(),
        published_minutes: sky.published_minutes,
    }
}

/// The modelled roof, corrected into a forecast of **this** roof.
///
/// A slot the weather run does not reach gets a band of zero, and that is not
/// the same lie as an absent one: the horizon runs two days and ICON-D2 runs
/// less far, and past the end of the sky the honest statement about a roof is
/// that nobody knows — which for *production* is nearest to nothing, because the
/// alternative is a plan that defers load into sunshine it has invented. The
/// load forecast, which is where the symmetric mistake would matter, comes from
/// the household's own history and covers every slot by construction.
fn corrected(residual: &ResidualModel, modelled: &[(Slot, f64)], horizon: Horizon) -> Forecast {
    let by_slot: std::collections::BTreeMap<Slot, f64> = modelled.iter().copied().collect();
    Forecast {
        slots: horizon
            .slots()
            .map(|slot| {
                let band = by_slot
                    .get(&slot)
                    .map_or(Band::certain(0.0), |m| residual.correct(slot, *m));
                (slot, band)
            })
            .collect(),
    }
}

/// The battery as the planner may model it, or nothing.
///
/// `None` where the pack has no fresh state of charge, and that is a refusal
/// rather than a default: a plan built on a guessed fill empties a store it
/// thought was full, and the guard would then be the only thing between the
/// household and a flat battery on the evening it wanted one.
/// The hot-water tank the planner may move, where a driver has said how warm it
/// is.
///
/// `None` where nothing measured it, and that is the whole gate. A store's state
/// of charge is not a thing to assume: a plan built on a guessed tank decides
/// when to heat from a number nobody read, and it is wrong in the expensive
/// direction on exactly the mornings it matters — a tank guessed full is one
/// nobody heats overnight, and the household finds out in the shower.
///
/// The arithmetic is the site asset's own (`DhwTank::stored_heat`), because how
/// many litres there are and how cold the household will let them get are
/// properties of the *installation*: a driver that knew either would be a driver
/// that had been told about the house.
fn dhw_model(
    site: &Site,
    observed: &crate::drivers::Observed,
    household: &Household,
) -> Option<DhwModel> {
    let dhw_id = household.dhw.as_ref()?;
    let Some(Asset::Dhw(tank)) = site.asset(dhw_id) else {
        return None;
    };
    let degrees = observed.state.asset(dhw_id)?.temperature_c?;
    Some(DhwModel {
        capacity: tank.usable_heat(),
        stored_now: tank.stored_heat(degrees),
        heater: tank.heater,
        cop: tank.cop,
        standing_loss: tank.standing_loss,
        ..DhwModel::tank(tank.usable_heat(), tank.heater)
    })
}

/// What a kelvin-hour outside the comfort band is worth avoiding, €.
///
/// The price the whole heating plan turns on: too low and the house is cold
/// whenever electricity is dear, too high and the heat pump ignores the price
/// entirely. €1,50 puts a degree of discomfort on a par with a few
/// kilowatt-hours, which is `ThermalModel::house`'s own figure and the one every
/// reference day is calibrated against.
const DISCOMFORT_EUR_PER_KELVIN_HOUR: f64 = 1.5;

/// The house the planner may pre-heat, where something is measuring how warm it
/// is.
///
/// `None` without an indoor temperature, and that is the same refusal the
/// battery and the tank make: a thermal plan built on a guessed indoor
/// temperature decides when to heat from a number nobody read, and it is wrong
/// in the expensive direction on exactly the cold mornings it matters.
///
/// # Where each number comes from
///
/// * the **band** and the **COP curve** are the installation's, from the site;
/// * the **building** is learned — see [`Learned::building`] — and falls back to
///   a documented prior until the house has taught the box otherwise;
/// * the **minimum runtimes** are the appliance's own where it announces them
///   over EEBUS OHPCF, and the configured default otherwise. A figure the
///   machine states beats the same figure typed into a file, and this is the
///   only place the two can be told apart;
/// * the **compressor state** is what the appliance says it is doing, which is
///   the whole of what makes a minimum runtime mean anything on a receding
///   horizon (see [`CompressorState`](hems_core::prelude::CompressorState)).
fn heat_pump_model(
    site: &Site,
    observed: &crate::drivers::Observed,
    household: &Household,
    building: hems_core::prelude::Rc2,
) -> Option<hems_optimizer::model::ThermalModel> {
    use hems_core::prelude::{CompressorState, ThermalState};
    use hems_optimizer::model::{HeatPumpModel, ThermalModel};

    let id = household.heat_pump.as_ref()?;
    let Some(Asset::HeatPump(hp)) = site.asset(id) else {
        return None;
    };
    let indoor_c = observed.state.asset(id)?.temperature_c?;

    let mut unit = if hp.modulating {
        HeatPumpModel::modulating(hp.electrical_nominal)
    } else {
        HeatPumpModel::on_off(hp.electrical_nominal)
    };
    unit.cop = hp.cop;
    if let Some(offer) = observed.flexibility.get(id) {
        if let Some(min_run) = offer.min_run {
            unit.min_on_slots = slots_of(min_run);
        }
        if let Some(min_rest) = offer.min_rest {
            unit.min_off_slots = slots_of(min_rest);
        }
        // How long it has been running is not on the wire — OHPCF publishes the
        // elapsed slot time of the *process*, not of the compressor — so the
        // conservative reading is taken: a unit whose state is fresh is one the
        // minimum runtime still binds. `settled` would be the optimistic one,
        // and optimism here is a plan that stops a compressor it may not stop.
        unit.compressor = CompressorState {
            running: offer.running,
            slots_in_state: 0,
        };
    }
    Some(ThermalModel {
        // Both masses at the one temperature there is. The fabric is a hidden
        // state nobody measures, and seeding it from the air is what
        // `building::Record` does for the same reason — a plan that guessed it
        // warmer than the air would think the house had heat banked in it.
        state: ThermalState::uniform(indoor_c),
        building,
        comfort_min_c: hp.comfort_min_c,
        comfort_max_c: hp.comfort_max_c,
        discomfort_eur_per_kelvin_hour: DISCOMFORT_EUR_PER_KELVIN_HOUR,
        heat_pump: unit,
    })
}

/// A duration as whole slots, at least one.
///
/// Rounded **up**: a minimum runtime of twenty minutes that the plan honoured
/// for one quarter hour is a minimum runtime the appliance then extends by
/// itself, and the plan spends the difference somewhere it did not budget for.
fn slots_of(duration: time::Duration) -> usize {
    let slots = (duration / hems_core::prelude::SLOT).ceil();
    if slots.is_finite() && slots >= 1.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = slots as usize;
        n
    } else {
        1
    }
}

/// The charging session, where a driver has said a car is on the charge point.
///
/// `None` on three quite different facts, and the planner needs all three to
/// mean the same thing here — leave the car out — while meaning different things
/// to a household:
///
/// * nothing can *tell* whether a car is plugged in (a Modbus wallbox reports
///   power and has no idea what is on the end of the cable);
/// * a charge point that can tell says there is none;
/// * a car that is there and is already at its target.
///
/// What is deliberately **not** here is a guess. A session invented for a car
/// that is not on the cable is not merely optimistic: it is a schedule the
/// arbiter spends the afternoon failing to follow, so the energy has to be found
/// again in whatever hours are left, which are the expensive ones.
fn ev_session(
    site: &Site,
    observed: &crate::drivers::Observed,
    household: &Household,
    charging: Option<&crate::config::EebusEvSettings>,
    horizon: Horizon,
) -> Option<hems_optimizer::model::EvSession> {
    use hems_core::prelude::{Energy, PhaseMode};

    let evse_id = household.evse.as_ref()?;
    let Some(Asset::Evse(evse)) = site.asset(evse_id) else {
        return None;
    };
    let charging = charging?;
    let present = observed.vehicles.get(evse_id)?;
    if !present.connected {
        return None;
    }
    // Both halves or nothing. A percentage says nothing about how long charging
    // takes and a capacity says nothing about how much is needed, so a planner
    // given one of them would be planning a charge for a battery it invented.
    let (soc, capacity_wh) = (present.soc?, present.capacity_wh?);
    let capacity = Energy::from_kwh(capacity_wh / 1000.0);
    let energy_now = capacity * soc;
    let energy_target = capacity * charging.target_soc.clamp(0.0, 1.0);
    if energy_now >= energy_target {
        // Already there. Left out rather than added with nothing to do, because
        // a named asset the problem does not move is a target of zero the
        // arbiter obeys (D96).
        return None;
    }
    let departure = charging.departure_slot(horizon)?;
    Some(hems_optimizer::model::EvSession {
        energy_now,
        energy_target,
        capacity,
        max_charge: evse.max_power(PhaseMode::Three),
        // The minimum in the mode the plan *assumes*, which is the wiring's
        // default rather than the mode the contactor happens to be in — see the
        // same decision in `scenario.rs`. Phase switching is the arbiter's
        // lever, not the planner's.
        min_charge: evse.min_power(PhaseMode::Three),
        efficiency: 0.92,
        // Plugged in already, which is the only case this function returns for:
        // the session was created when the driver saw the cable go in.
        arrival: None,
        departure,
    })
}

fn battery_model(
    site: &Site,
    observed: &crate::drivers::Observed,
    household: &Household,
    wear_eur_per_kwh: f64,
) -> Option<BatteryModel> {
    let battery_id = household.battery.as_ref()?;
    let Some(Asset::Battery(b)) = site.asset(battery_id) else {
        return None;
    };
    let soc = observed.state.asset(battery_id)?.soc?;
    Some(BatteryModel {
        capacity: b.capacity,
        soc_now: soc,
        max_charge: b.max_charge,
        max_discharge: b.max_discharge,
        efficiency_charge: b.efficiency_charge,
        efficiency_discharge: b.efficiency_discharge,
        soc_min: b.soc_min,
        soc_max: b.soc_max,
        reserve_soc: b.reserve_soc,
        degradation_eur_per_kwh: wear_eur_per_kwh,
        grid_charging_allowed: b.grid_charging_allowed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::Power;

    /// A tank nobody measured is left out of the plan rather than guessed at.
    ///
    /// The gate that makes the whole driver worth building. A store's state of
    /// charge is not a thing to assume, and the guess is wrong in the expensive
    /// direction: a tank guessed full is one nobody heats overnight, and the
    /// household finds out in the shower. D96 is the other half — naming an
    /// asset the problem does not model emits a target of zero with an envelope
    /// pinned at zero, which the arbiter obeys as an instruction, so an unread
    /// tank must be absent from *both*.
    /// The three different `None`s, and why the planner treats them alike.
    #[test]
    fn a_car_nobody_can_see_is_not_a_car_to_plan_for() {
        use hems_core::prelude::Horizon;

        let household = crate::site::Household::build(&crate::HouseholdConfig::default())
            .expect("the reference household");
        let now = time::OffsetDateTime::now_utc();
        let horizon = Horizon::new(now, 96);
        let charging = crate::config::EebusEvSettings {
            asset: "wallbox".into(),
            address: "192.168.1.50:4712".into(),
            ski: "00".repeat(20),
            departure: "07:00".into(),
            target_soc: 0.8,
            spine_vendor: None,
            spine_unique: None,
        };

        // Nothing reported anything at all — a Modbus wallbox reports power and
        // has no idea what is on the end of the cable.
        let observed = crate::drivers::Observed::default();
        assert!(
            ev_session(
                &household.site,
                &observed,
                &household,
                Some(&charging),
                horizon
            )
            .is_none()
        );

        // A charge point that *can* tell, saying there is no car.
        let evse = household.evse.clone().expect("the reference wallbox");
        let mut observed = crate::drivers::Observed::default();
        observed.vehicles.insert(
            evse.clone(),
            hems_drv::VehiclePresence {
                connected: false,
                soc: None,
                capacity_wh: None,
                at: now,
            },
        );
        assert!(
            ev_session(
                &household.site,
                &observed,
                &household,
                Some(&charging),
                horizon
            )
            .is_none()
        );

        // A car that is there but cannot say how full it is — IEC 61851, a pilot
        // wire and nothing else. Left out rather than charged against an
        // invented battery.
        observed.vehicles.insert(
            evse.clone(),
            hems_drv::VehiclePresence {
                connected: true,
                soc: None,
                capacity_wh: Some(58_000.0),
                at: now,
            },
        );
        assert!(
            ev_session(
                &household.site,
                &observed,
                &household,
                Some(&charging),
                horizon
            )
            .is_none(),
            "half a battery is not a battery"
        );
    }

    /// A car that is there, and short of its target, is a session to plan.
    #[test]
    fn a_car_on_the_cable_becomes_a_charging_deadline() {
        use hems_core::prelude::Horizon;

        let household = crate::site::Household::build(&crate::HouseholdConfig::default())
            .expect("the reference household");
        // A fixed instant so the departure is a fact rather than whatever time
        // this test happens to run at.
        let now = time::macros::datetime!(2026-01-15 18:00:00 UTC);
        let horizon = Horizon::new(now, 96);
        let charging = crate::config::EebusEvSettings {
            asset: "wallbox".into(),
            address: "192.168.1.50:4712".into(),
            ski: "00".repeat(20),
            departure: "07:00".into(),
            target_soc: 0.8,
            spine_vendor: None,
            spine_unique: None,
        };
        let evse = household.evse.clone().expect("the reference wallbox");
        let mut observed = crate::drivers::Observed::default();
        observed.vehicles.insert(
            evse,
            hems_drv::VehiclePresence {
                connected: true,
                soc: Some(0.35),
                capacity_wh: Some(58_000.0),
                at: now,
            },
        );

        let session = ev_session(
            &household.site,
            &observed,
            &household,
            Some(&charging),
            horizon,
        )
        .expect("a session");

        assert!(
            (session.energy_now.kwh() - 20.3).abs() < 1e-6,
            "35 % of 58 kWh"
        );
        assert!(
            (session.energy_target.kwh() - 46.4).abs() < 1e-6,
            "and 80 % is what the household asked for, not 100 %"
        );
        // Half-open: the target is met by the end of the slot *before* the car
        // leaves, so a car leaving at seven is not planned charging at 07:14.
        assert_eq!(session.departure.local_minute_of_day(), 7 * 60);
        assert!(session.deadline() < session.departure);

        // …and a car already at its target is not a session at all: a named
        // asset the problem does not move is a target of zero the arbiter obeys.
        observed.vehicles.insert(
            household.evse.clone().expect("the wallbox"),
            hems_drv::VehiclePresence {
                connected: true,
                soc: Some(0.9),
                capacity_wh: Some(58_000.0),
                at: now,
            },
        );
        assert!(
            ev_session(
                &household.site,
                &observed,
                &household,
                Some(&charging),
                horizon
            )
            .is_none()
        );
    }

    #[test]
    fn an_unmeasured_tank_is_absent_from_the_plan_rather_than_assumed() {
        let household = crate::site::Household::build(&crate::HouseholdConfig::default())
            .expect("the reference household");
        let now = time::OffsetDateTime::now_utc();
        let mut registry = crate::drivers::Registry::new();
        let observed = registry.observe(None, now);

        assert!(
            dhw_model(&household.site, &observed, &household).is_none(),
            "nothing reported a temperature, so there is no store to plan"
        );

        // …and the site does have a tank, so the `None` is the *measurement*
        // missing rather than the asset.
        assert!(household.dhw.is_some());
    }

    /// A tank that has been measured is the store the optimiser was written for.
    #[test]
    fn a_measured_tank_becomes_a_store_the_planner_can_move() {
        use hems_core::prelude::{Asset, Measurement};

        let household = crate::site::Household::build(&crate::HouseholdConfig::default())
            .expect("the reference household");
        let now = time::OffsetDateTime::now_utc();
        let dhw_id = household.dhw.clone().expect("the reference tank");
        let mut state = hems_realtime::guard::SiteState::default();
        let mut measured = Measurement::at(now);
        measured.temperature_c = Some(52.5);
        state.assets.insert(dhw_id.clone(), measured);
        let observed = crate::drivers::Observed {
            state,
            ..crate::drivers::Observed::default()
        };

        let model = dhw_model(&household.site, &observed, &household).expect("a store");

        let Some(Asset::Dhw(tank)) = household.site.asset(&dhw_id) else {
            panic!("the reference tank is a tank");
        };
        // The arithmetic is the site asset's own, because how many litres there
        // are and how cold the household will let them get are properties of the
        // installation rather than of the thermometer.
        assert_eq!(model.stored_now, tank.stored_heat(52.5));
        assert_eq!(model.capacity, tank.usable_heat());
        assert!(
            model.stored_now < model.capacity,
            "a tank at 52,5 °C is not a full one"
        );
    }

    #[test]
    fn a_restart_does_not_cost_a_fortnight_of_learning() {
        // The difference between a box that plans on its first evening back and
        // one that refuses to plan at all until it has seen a quarter hour of
        // its own load. Both models are cheap to keep and expensive to relearn.
        let store = crate::store::Store::in_memory().expect("a store");
        let now = time::OffsetDateTime::now_utc();
        let land = metering::Bundesland::Be;

        let mut before = Learned::new(land);
        // A fortnight, so every day type has been seen: a profile is indexed by
        // day type and quarter hour, and one day of history teaches Mondays
        // nothing about Sundays.
        let start = Slot::containing(now).offset(-96 * 14);
        for i in 0..(96 * 14) {
            let slot = start.offset(i);
            before.load.observe(slot, Power::from_kw(0.6));
            // A roof delivering nine tenths of what the model says it should —
            // a tree, a datasheet that was optimistic, dust on the glass.
            before.pv.observe(slot, 1_000.0, 900.0);
        }
        // Asserted rather than assumed: `remember` only warns, because a box
        // that stopped controlling a house because it could not write a cache
        // would be trading the wrong thing away — so a silent failure here is
        // exactly what a test has to catch.
        store
            .put_learned(PV_MODEL, &before.pv, now)
            .expect("the roof's correction has to be storable");
        store
            .put_learned(LOAD_MODEL, &before.load, now)
            .expect("and so does the household's own profile");

        let after = Learned::restored(&store, land);
        let slot = Slot::containing(now);
        assert!(
            after.load.support(slot) > 0,
            "a restored box knows its own household and can plan at once"
        );
        assert!(
            (after.pv.ratio_at(start) - before.pv.ratio_at(start)).abs() < 1e-9,
            "…and it knows what its own roof actually delivers"
        );
    }

    #[test]
    fn a_model_this_build_cannot_read_is_relearned_rather_than_fatal() {
        // A box that will not start after an update because it cannot read a
        // fortnight of learning it can perfectly well rebuild has traded the
        // wrong thing away: the cost is a week of slightly worse forecasts, and
        // the alternative is a household with no energy manager at all.
        let store = crate::store::Store::in_memory().expect("a store");
        let now = time::OffsetDateTime::now_utc();
        store
            .put_learned(PV_MODEL, &"a shape from some other version", now)
            .expect("it stores");

        let restored = Learned::restored(&store, metering::Bundesland::Be);
        assert!(
            !restored.pv.is_trained(),
            "the unreadable half is simply relearned"
        );
    }

    #[test]
    fn the_configured_bundesland_wins_over_the_stored_one() {
        // The Land is *configuration* and the profile is *history*. A household
        // that corrected its Land in the file — because somebody typed the wrong
        // one at commissioning, and a public holiday counts as a Sunday — must
        // not be handed the old answer back by its own store.
        let store = crate::store::Store::in_memory().expect("a store");
        let now = time::OffsetDateTime::now_utc();
        let learned = Learned::new(metering::Bundesland::By);
        learned.remember(&store, now);

        let restored = Learned::restored(&store, metering::Bundesland::Nw);
        assert_eq!(restored.load.land, metering::Bundesland::Nw);
    }
}
