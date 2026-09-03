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
use hems_optimizer::model::{BatteryModel, Problem};
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

impl Learned {
    /// A box that has just been switched on and knows nothing.
    #[must_use]
    pub fn new(land: metering::Bundesland) -> Self {
        Self {
            pv: ResidualModel::new(hems_forecast::residual::DEFAULT_ALPHA),
            load: LoadProfile::new(land),
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
        learned
    }

    /// Keep both, so a reboot does not cost a fortnight.
    pub fn remember(&self, store: &crate::store::Store, now: OffsetDateTime) {
        for (name, written) in [
            (PV_MODEL, store.put_learned(PV_MODEL, &self.pv, now)),
            (LOAD_MODEL, store.put_learned(LOAD_MODEL, &self.load, now)),
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
        refresh(&fleet, &mut cached, now).await;

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
async fn refresh(fleet: &Fleet, cached: &mut Cached, now: OffsetDateTime) {
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
    let tariff = planner.tariff.tariff(site, spot);
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
    let load = if held.load.support(horizon.first) > 0 {
        held.load.forecast(horizon)
    } else {
        let recent = crate::drivers::household_load(&observed).ok_or(Reason::NoHistory)?;
        // Doubling by this time tomorrow, which is about what a single reading
        // is worth twenty-four hours out.
        hems_forecast::naive::persistence(recent, horizon, 0.9)
    };
    drop(held);

    // Published for the control loop, which scores each slot against the band
    // the plan was actually made against once the slot has happened. Written
    // here rather than derived later: a score of a band nobody planned against
    // says nothing about the plan (D117).
    *published.bands.write().await = pv
        .slots
        .iter()
        .zip(load.slots.iter())
        .map(|((slot, p), (_, l))| (*slot, (*p, *l)))
        .collect();

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
    let names = AssetNames {
        battery: battery.map(|_| planner.household.battery.clone()),
        pv: Some(planner.household.pv.clone()),
        evse: None,
        heat_pump: None,
        dhw: None,
        shiftable: Vec::new(),
    };
    let budget = planner.control.solve_budget_s;

    // Off the runtime, with everything it needs **moved** in. HiGHS is a
    // synchronous C++ solver and a ten-second solve on a runtime thread is ten
    // seconds in which the guard's own tick cannot be scheduled — which on a
    // gateway box with two cores is the difference between a control period and
    // a missed one. `Problem` borrows its inputs, so it is assembled inside the
    // task rather than sent into it.
    tokio::task::spawn_blocking(move || {
        let mut problem = Problem::new(horizon, &prices, &pv, &load).with_limits(limits);
        if let Some(model) = battery {
            problem = problem.with_battery(model);
        }
        problem.solve_budget_s = budget;
        solve(&problem, &names, now)
    })
    .await
    .map_err(|e| Reason::Unsolvable(format!("the solve panicked: {e}")))?
    .map_err(|e| Reason::Unsolvable(e.to_string()))
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
fn battery_model(
    site: &Site,
    observed: &crate::drivers::Observed,
    household: &Household,
    wear_eur_per_kwh: f64,
) -> Option<BatteryModel> {
    let Some(Asset::Battery(b)) = site.asset(&household.battery) else {
        return None;
    };
    let soc = observed.state.asset(&household.battery)?.soc?;
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
