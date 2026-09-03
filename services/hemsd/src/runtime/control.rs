//! Guard and arbiter, once a control period, against a real clock.
//!
//! The simulated day runs this same pair a minute at a time in virtual time.
//! What changes here is only where the numbers come from — the drivers rather
//! than a scenario — and the fact that the loop can fall behind, which is the
//! one failure a simulated day cannot have.
//!
//! # The order is the guard's, and nothing here may reorder it
//!
//! 1. Everything the drivers have said is drained into `SiteState` and
//!    `GridLimits`: what the house is doing, and what the network operator is
//!    asking for.
//! 2. The arbiter decides, which internally runs the guard first and narrows
//!    every desire into what the grid, the fuses and the hardware leave open.
//! 3. The setpoints go back to the drivers.
//!
//! Step 2 is `hems-realtime`'s and takes time as a parameter, so the property
//! that no input can make it exceed a grid limit is a unit test rather than a
//! hope about this file. What this file adds is the clock and the sockets.
//!
//! # Falling behind is a measurement, not a panic
//!
//! A control period is a promise the guard's arithmetic depends on: "stop
//! discharging once the reserve is reached" is turned into a bound on *power*
//! by assuming the value will be held for one period. A loop that took two
//! periods over a tick would break that assumption silently. So the tick is
//! scheduled on an interval that **skips** rather than bursts, and the overruns
//! are counted — [`Status::overruns`] is the number, and it belongs on a screen.

use std::collections::BTreeMap;
use std::sync::Arc;

use hems_core::prelude::{AssetId, Energy, Power, Site, Slot, UserOverride};
use hems_realtime::guard::GuardConfig;
use hems_realtime::{Arbiter, ArbiterConfig, PhaseState, Tick};
use hems_service::{Health, Shutdown};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::config::ControlSettings;
use crate::drivers::Observed;
use crate::runtime::transport::Shared;

/// What the control loop has decided, as a screen would show it.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// When the last tick ran.
    pub at: Option<OffsetDateTime>,
    /// What every asset was last told, in watts.
    pub commanded: BTreeMap<AssetId, Power>,
    /// The assets no driver has been heard from.
    ///
    /// Each of these is a device the guard is being conservative about, and
    /// being conservative costs the household money — so it is a number the
    /// household is entitled to see rather than one buried in a log line.
    pub silent: Vec<AssetId>,
    /// Devices whose available power is a nameplate rather than a reading.
    pub assumed_available: Vec<AssetId>,
    /// Controllable assets no driver speaks for.
    ///
    /// The arbiter decides a setpoint for each of these on every tick and has
    /// nowhere to send it. It is a configuration fact rather than a fault, so it
    /// is a number here rather than a log line repeated every second — but it is
    /// the first thing to look at when a device is not doing what the screen
    /// says it was told to.
    pub undriven: Vec<AssetId>,
    /// The § 14a ceiling in force, if any.
    pub steuve_ceiling: Option<Power>,
    /// How much the controllable devices may draw in total, surplus included.
    pub steuve_budget: Option<Power>,
    /// The netzwirksamer Leistungsbezug measured now, `[A1 2.3]`.
    pub netzwirksam: Option<Power>,
    /// How far the measured grid power is from the sum of the measured assets.
    ///
    /// A residual that is not near zero means a meter is missing or mis-signed,
    /// which is the commissioning fault that is hardest to see from a working
    /// screen: every individual number looks plausible.
    pub balance_residual: Option<Power>,
    /// How long the box has been running without a plan, in minutes.
    ///
    /// The seam this daemon is most likely to be quietly broken at. A box that
    /// never plans looks exactly like one that plans badly, and the difference
    /// is a whole tariff's worth of money.
    pub minutes_without_a_plan: i64,
    /// How many ticks took longer than a control period.
    pub overruns: u64,
    /// What the plan in force expects the horizon to cost, euros.
    ///
    /// The plan's own arithmetic, and every term of the objective is a term of
    /// it — the battery wear, the discomfort and the service the plan decided
    /// not to deliver as well as the electricity. A saving that left them out
    /// would be a discount the plan helped itself to.
    pub plan_expected_eur: Option<f64>,
    /// What the same horizon would cost with no energy manager, euros.
    pub plan_baseline_eur: Option<f64>,
}

/// The household this loop is deciding for, and the two facts about it that are
/// settled before the first tick.
///
/// One struct rather than three parameters because none of them changes while
/// the box runs, and threading three constants through every call is how a
/// fourth gets added in the wrong order.
#[derive(Debug, Clone)]
pub struct Managed {
    /// The installation.
    pub site: Site,
    /// Which asset is the meter at the connection point.
    ///
    /// Named rather than inferred: `[A1 2.3]` is measured there and nowhere
    /// else, and deducing the most important fact in the system from an asset
    /// kind is how a second `MeterRole::GridConnection` on a sub-panel silently
    /// becomes the connection point.
    pub grid_meter: Option<AssetId>,
    /// The controllable assets no driver speaks for, as found at start-up.
    pub undriven: Vec<AssetId>,
    /// Which asset the roof is.
    pub pv: AssetId,
    /// The storage system, whose own flows MiSpeL counts apart.
    pub battery: AssetId,
    /// The charge point, likewise.
    pub evse: AssetId,
    /// How the network operator addresses this site, `[A1 4.4]` — which the
    /// evidence record has to state, because the minimum a reduction may not go
    /// below depends on it.
    pub control_mode: hems_grid::para14a::ControlMode,
    /// What the geometric model said the roof would make in each slot, shared
    /// with the planner that computed it.
    ///
    /// The corrector is taught against **this**, not against a fresh clear-sky
    /// number: the residual it exists to learn is the roof's, and comparing a
    /// cloudy afternoon with a clear-sky model would teach it the weather.
    pub modelled_pv: Arc<tokio::sync::RwLock<BTreeMap<Slot, f64>>>,
    /// The bands the standing plan was made against, for the box to score
    /// itself once each slot has happened.
    ///
    /// The forecast that was **acted on**, not a fresh one: a score of a band
    /// nobody planned against says nothing about the plan (D117).
    pub bands: Arc<tokio::sync::RwLock<crate::runtime::day::PublishedBands>>,
}

/// The state this loop shares with the rest of the box.
///
/// Four handles, and each is shared with exactly one other task: the drivers
/// with their transports, the status with the HTTP surface, the plan and the
/// learning with the planner. Bundled because they are always passed together
/// and a loop that took them one by one is a loop somebody eventually passes in
/// the wrong order.
pub struct Live {
    /// The drivers, shared with their transport tasks.
    pub registry: Shared,
    /// What the last tick decided, for the API and the health surface.
    pub status: Arc<Mutex<Status>>,
    /// The plan in force, published by the planner.
    pub plan: Arc<tokio::sync::RwLock<Option<hems_core::prelude::Plan>>>,
    /// The prices the plan was made against, for the quarter-hour registers.
    ///
    /// The registers MiSpeL and § 42c settle from carry two *prices* as well as
    /// the metered quantities, and this loop has no tariff of its own — so
    /// without them it could only write a register with two zeros in it, which
    /// is worse than no register at all.
    pub prices: Arc<tokio::sync::RwLock<Option<hems_tariff::PriceStack>>>,
    /// What the box has learned about its own roof and household.
    pub learned: Arc<Mutex<crate::runtime::planner::Learned>>,
    /// The box's own two years, where it has one.
    pub store: Option<Arc<Mutex<crate::store::Store>>>,
    /// What the household itself has asked for, shared with the HTTP surface.
    pub overrides: crate::runtime::overrides::Overrides,
}

/// Run the guard and the arbiter until the process is asked to stop.
pub async fn run(
    managed: Managed,
    shared: Live,
    settings: ControlSettings,
    health: Health,
    shutdown: Shutdown,
) {
    let Live {
        registry,
        status,
        plan,
        prices,
        learned,
        store,
        overrides,
    } = shared;
    let arbiter = Arbiter::new(ArbiterConfig {
        guard: GuardConfig {
            // The guard needs the period as arithmetic and not only as a
            // schedule: a bound on a state is a bound on a rate only once you
            // know how long the rate is held for.
            tick_period: settings.tick_period(),
            ..GuardConfig::default()
        },
        ..ArbiterConfig::default()
    });

    let period = std::time::Duration::from_secs(settings.tick_period_s.max(1));
    let mut ticker = tokio::time::interval(period);
    // Skip rather than burst. The default behaviour of a tokio interval is to
    // fire immediately for every missed tick, which after a pause would run the
    // guard several times against the same measurements and then command a
    // device from a decision that was already stale — the opposite of catching
    // up.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let started = OffsetDateTime::now_utc();

    let mut carried = Carried {
        previous: BTreeMap::new(),
        phases: BTreeMap::new(),
        delivered: BTreeMap::new(),
        delivered_slot: Slot::containing(started),
        local_day: metering::calendar::local_day(started),
        // A box that has just started did not watch the earlier part of today,
        // and saying so is the difference between "nothing went wrong" and
        // "nobody was looking" (D116).
        unplanned: crate::runtime::day::Unplanned::resumed(),
        clipping: crate::runtime::day::Clipping::default(),
        scored: crate::runtime::day::Scored::default(),
        overruns: 0,
        pv_wh: 0.0,
        pv_samples: 0,
        load_wh: 0.0,
        samples: 0,
        grid_draw_wh: 0.0,
        grid_feed_wh: 0.0,
        device_draw_wh: 0.0,
        device_feed_wh: 0.0,
        evidence: hems_grid::evidence::EvidenceRecorder::new(),
    };

    loop {
        tokio::select! {
            biased;
            () = shutdown.clone().wait() => {
                tracing::info!("the control loop is stopping");
                return;
            }
            _ = ticker.tick() => {}
        }
        let now = OffsetDateTime::now_utc();
        let began = std::time::Instant::now();

        // Read afresh each tick, and expired entries drop out on the way past:
        // an override is a *desire* the arbiter narrows, so it costs nothing to
        // ask and a household that boosted its car this morning is not still
        // boosting it tonight.
        let wanted = overrides.active(now).await;
        let held = plan.read().await;
        let (screen, silent) = tick(
            &arbiter,
            &managed,
            &registry,
            &wanted,
            held.as_ref(),
            &learned,
            &prices,
            store.as_ref(),
            &mut carried,
            period,
            now,
        )
        .await;
        drop(held);

        let elapsed = began.elapsed();
        if elapsed > period {
            carried.overruns += 1;
            tracing::warn!(
                took_ms = elapsed.as_millis(),
                period_ms = period.as_millis(),
                "a control tick took longer than its period"
            );
        }
        *status.lock().await = screen;

        // Liveness is the loop; readiness is whether the house is actually being
        // measured. A box whose every driver is silent is running perfectly and
        // managing nothing, and a probe that could not tell those apart would be
        // a probe nobody should route on.
        if silent == 0 {
            health.good("drivers", now);
        } else {
            health.bad(
                "drivers",
                format!("{silent} of the configured devices are not being heard from"),
            );
        }
    }
}

/// Observe one tick for the § 14a evidence record, and keep whatever it closed.
///
/// The netzwirksamer Leistungsbezug it records is the **guard's own** figure.
/// That is not a convenience: `[A1 2.3]` measures what the controllable devices
/// draw *from the grid*, so a battery discharging into the wallbox is kilowatts
/// that never crossed the connection point — and a second derivation of that
/// quantity for the Nachweis would be a second chance to report a reduction as
/// breached on a day it was respected.
///
/// A closed record is written straight through. It is a handful of rows a few
/// times a day, and holding it in memory until some later flush is how the one
/// document a network operator asks for is lost to a power cut.
async fn record(
    managed: &Managed,
    carried: &mut Carried,
    observed: &Observed,
    decision: &hems_realtime::arbiter::Decision,
    store: Option<&Arc<Mutex<crate::store::Store>>>,
    now: OffsetDateTime,
) {
    let assets: Vec<AssetId> = decision
        .verdict
        .steuve
        .iter()
        .flat_map(|s| s.assets.iter().cloned())
        .collect();
    carried.evidence.observe(
        hems_grid::evidence::Observation {
            ceiling: observed.limits.steuve_ceiling,
            rule: if observed.limits.in_failsafe {
                hems_core::setpoint::GuardRule::Failsafe
            } else {
                hems_core::setpoint::GuardRule::Lpc
            },
            mode: managed.control_mode,
            minimum_power: decision.verdict.minimum_power,
            netzwirksam: decision.verdict.netzwirksam,
            applied: !decision.setpoints.is_empty(),
        },
        &assets,
        now,
    );

    // Taken rather than read, because the store is the record and the recorder
    // is a buffer: one that kept a copy of everything it had ever built would
    // hold every minute-resolution trace of two years in memory for nothing.
    let closed = carried.evidence.take_closed();
    if closed.is_empty() {
        return;
    }
    for event in &closed {
        tracing::info!(
            rule = ?event.rule,
            ceiling_kw = event.strictest_ceiling().kw(),
            compliant = event.fully_compliant(),
            "a control event closed"
        );
    }

    let Some(store) = store else {
        // Loud, because a § 14a household with nowhere to write its evidence is
        // one that cannot answer the question `[A1 7.3]` gives an operator two
        // years to ask.
        tracing::error!(
            events = closed.len(),
            "a control event closed and this box has no store configured, so the \
             record `[A1 7.2]` asks for has nowhere to go"
        );
        return;
    };
    let mut held = store.lock().await;
    for event in &closed {
        if let Err(error) = held.put_control_event(event) {
            tracing::error!(%error, "the § 14a record could not be written");
        }
    }

    // The statutory retention: `[A1 7.3]` documents an event for two years, and
    // keeping it longer is holding a household's control history for no reason
    // anybody asked for (G6). The column says when each row expires; something
    // has to act on it, and the moment a record closes is the cheapest moment
    // there is — a few times a day rather than on a timer of its own.
    if let Err(error) = held.prune(now) {
        tracing::warn!(%error, "the retention sweep did not run");
    }
}

/// Add this tick's share of the quarter hour to what is being accumulated.
///
/// **Measured** rather than commanded, throughout: a forecast trained on what
/// the box asked for would learn its own behaviour, and a settlement computed
/// from what a controller wanted is not a settlement.
///
/// A tick where the grid meter is silent contributes nothing rather than a zero.
/// A house nobody is measuring did not use nothing, and a register that averaged
/// the gaps in would understate a month.
fn meter(managed: &Managed, carried: &mut Carried, observed: &Observed, seconds: f64) {
    let Some(load) = crate::drivers::household_load(observed) else {
        return;
    };
    let measured_pv = observed
        .state
        .asset(&managed.pv)
        .and_then(|m| m.power)
        .map(Power::outflow);
    let pv = measured_pv.unwrap_or(Power::ZERO);
    let hours = seconds / 3600.0;
    carried.pv_wh += pv.get() * hours;
    // Whether anything was *read* — a roof nobody metered did not produce
    // nothing, and the quarter-hour record says `None` rather than nought
    // (D124). Kept apart from `samples`, which counts what the *load* teacher
    // saw.
    carried.pv_samples += usize::from(measured_pv.is_some());
    carried.load_wh += load.get() * hours;
    carried.samples += 1;

    // …and the four registers a settlement is computed from. The connection
    // point in both directions, and what the storage system and the charge
    // point drew and gave — `Z1NB¼`, `Z1NE¼`, `Z2V¼`, `Z2E¼`.
    if let Some(grid) = observed.state.grid.and_then(|m| m.power) {
        carried.grid_draw_wh += grid.inflow().get() * hours;
        carried.grid_feed_wh += grid.outflow().get() * hours;
    }
    for asset in [&managed.battery, &managed.evse] {
        let Some(p) = observed.state.asset(asset).and_then(|m| m.power) else {
            continue;
        };
        carried.device_draw_wh += p.inflow().get() * hours;
        carried.device_feed_wh += p.outflow().get() * hours;
    }
}

/// Write the completed quarter hour's meter registers.
///
/// `Z1NB¼`, `Z1NE¼`, `Z2V¼` and `Z2E¼` — what crossed the connection point in
/// each direction, and what the storage system and the charge point drew and
/// gave. They are the quantities MiSpeL's Abgrenzung and § 42c's allocation are
/// computed from, so every one is an **exact decimal**: a settlement that went
/// through an `f64` is one nobody can reproduce (P3).
///
/// The two *prices* the register carries come from the plan's own stack. A
/// register written with a zero anzulegender Wert would settle a month at the
/// wrong figure and look complete while doing it, so a slot with no price is a
/// slot with no register: skipped, and said out loud once.
/// Queue the day's report where a Berlin calendar day has just ended.
///
/// Called on the quarter-hour boundary and does nothing on the other ninety-five
/// of them. The day is built from the rows this loop already wrote rather than
/// from a counter, so a box that restarted at half past eleven still reports the
/// whole day — see [`crate::runtime::day`].
///
/// A failure is logged and not fatal. What is at stake is a fleet dashboard; the
/// household's own `[A1 7.3]` record is already written, and a box that stopped
/// controlling a house because a report could not be queued would have the
/// priorities exactly backwards.
async fn close_the_day(
    managed: &Managed,
    carried: &mut Carried,
    store: Option<&Arc<Mutex<crate::store::Store>>>,
    now: OffsetDateTime,
) {
    let today = metering::calendar::local_day(now);
    if today == carried.local_day {
        return;
    }
    let finished = carried.local_day;
    carried.local_day = today;
    let unplanned = carried.unplanned;
    let clipping = carried.clipping;
    let scored = carried.scored.clone();
    carried.unplanned.roll();
    carried.clipping.roll();
    carried.scored.roll();

    let Some(store) = store else {
        // No store is already an error the loop reports once at start-up: a
        // § 14a household that cannot keep its two years is the bigger problem,
        // and this one is downstream of it.
        return;
    };

    let site = managed.site.id.to_string();
    let mut guard = store.lock().await;
    let built = crate::runtime::day::kpis(&guard, &site, finished, unplanned, clipping, &scored);
    match built {
        Ok(Some(day)) => {
            let event = hems_events::Event::new(
                hems_events::SITE_DAY_REPORTED,
                format!("hems://sites/{site}"),
                // Derived from what the report is *about*, so a box that
                // recomputes a day is correcting one message rather than
                // sending a second (D110).
                format!("{site}:{finished}"),
                now,
                &day,
            )
            .about(finished.to_string());
            match event.to_bytes() {
                Ok(body) => {
                    if let Err(error) =
                        guard.queue_event(&event.id, hems_events::SITE_DAY_REPORTED, &body, now)
                    {
                        tracing::error!(%error, %finished, "the day report could not be queued");
                    } else {
                        tracing::info!(
                            %finished,
                            imported_kwh = day.imported_kwh,
                            control_events = day.control_events,
                            "the day is closed and queued for the fleet"
                        );
                    }
                }
                Err(error) => tracing::error!(%error, %finished, "the day would not serialise"),
            }
        }
        Ok(None) => tracing::info!(
            %finished,
            "no register for that day, so nothing to report — a box that was off \
             is not a household that used nothing"
        ),
        Err(error) => tracing::error!(%error, %finished, "the day could not be built"),
    }
}

async fn register(
    managed: &Managed,
    carried: &Carried,
    prices: &Arc<tokio::sync::RwLock<Option<hems_tariff::PriceStack>>>,
    store: Option<&Arc<Mutex<crate::store::Store>>>,
) {
    let (Some(store), true) = (store, carried.samples > 0) else {
        return;
    };
    let slot = carried.delivered_slot;
    let held = prices.read().await;
    let Some(price) = held.as_ref().and_then(|stack| stack.at(slot)) else {
        tracing::debug!(%slot, "no price for this quarter hour, so no register");
        return;
    };
    let _ = managed;

    // Watt-hours to kilowatt-hours, at the millionth — four orders of magnitude
    // finer than anything a household meter resolves, and the same tolerance the
    // rest of the settlement arithmetic works to.
    let kwh = |wh: f64| {
        rust_decimal::Decimal::from_f64_retain(wh / 1000.0)
            .unwrap_or_default()
            .round_dp(6)
    };
    let quarter = hems_grid::mispel::QuarterHour {
        slot,
        grid_draw: kwh(carried.grid_draw_wh),
        grid_feed_in: kwh(carried.grid_feed_wh),
        device_consumption: kwh(carried.device_draw_wh),
        device_generation: kwh(carried.device_feed_wh),
        storage_consumption: None,
        storage_generation: None,
        anzulegender_wert: price.export_ct,
        spot_price: price.energy_ct,
    };
    drop(held);

    // The roof is recorded **beside** the registers rather than inside them: the
    // MiSpeL set is the Festlegung's own and `Z2E¼` is the storage system's
    // generation, not the sun's (D124). `None` would be a box with no production
    // measurement; this one has been accumulating the meter all quarter hour.
    let recorded = crate::store::Recorded {
        registers: quarter,
        production: (carried.pv_samples > 0).then(|| kwh(carried.pv_wh)),
    };

    if let Err(error) = store
        .lock()
        .await
        .put_quarter_hour(&recorded, OffsetDateTime::now_utc())
    {
        tracing::warn!(%error, "a quarter-hour register could not be written");
    }
}

/// Hand a completed quarter hour to the models that learn from it.
///
/// Two lessons, and each is only teachable here — on the boundary, because that
/// is the only moment at which a quarter hour is *over*. A sample taken part-way
/// through one teaches a mean of a fraction of a slot, which is the same mistake
/// as reading a meter register mid-interval.
///
/// The **roof**: what the geometric model said this slot should have made,
/// against what the inverter actually delivered. That ratio is the whole of
/// `hems_forecast::residual` — a tree shading the east string, a datasheet that
/// was optimistic, dust on the glass — and it is what turns a *model* into a
/// forecast of **this** roof.
///
/// The **household**: the mean power of its own uninstrumented load, by day type
/// and quarter hour. A Tuesday teaches Tuesdays.
///
/// A slot the box saw none of teaches nothing rather than teaching a zero.
async fn teach(
    managed: &Managed,
    learned: &Arc<Mutex<crate::runtime::planner::Learned>>,
    carried: &mut Carried,
    period: std::time::Duration,
) {
    if carried.samples == 0 {
        return;
    }
    // The mean over what was actually observed rather than over the whole slot.
    // A box restarted at 12:07 saw half a quarter hour, and dividing its half by
    // a whole would teach the household as half the size it is.
    let covered_hours = f64::from(carried.samples) * period.as_secs_f64() / 3600.0;
    if covered_hours <= 0.0 {
        return;
    }
    let pv = carried.pv_wh / covered_hours;
    let load = Power::new(carried.load_wh / covered_hours);

    // The corrector has to be fed the same modelled figure the *forecast* used —
    // the plane-of-array from the weather the box was given — and not a fresh
    // clear-sky number, or it would learn the cloud rather than the roof. Where
    // no weather reached this slot there is nothing to compare against, and only
    // the load is taught.
    let modelled = managed
        .modelled_pv
        .read()
        .await
        .get(&carried.delivered_slot)
        .copied();
    learned
        .lock()
        .await
        .observe(carried.delivered_slot, modelled, pv, load);

    // The same truth, scored against the band the plan was actually made
    // against. This is the one half of the fleet's forecast picture a box can
    // produce honestly, and without it twenty independent *real* days would
    // never reach `obsd` — so `forecast_is_calibrated` could only ever be true
    // of simulations (D117).
    let published = managed.bands.read().await;
    carried
        .scored
        .observe(published.get(&carried.delivered_slot), pv, load.get());
}

/// What one tick hands to the next.
struct Carried {
    /// What every asset was told last tick, for ramping and the deadband.
    previous: BTreeMap<AssetId, Power>,
    /// The conductor policy for each switchable charge point.
    phases: BTreeMap<AssetId, PhaseState>,
    /// Energy each asset has moved since the quarter hour began.
    delivered: BTreeMap<AssetId, Energy>,
    /// Which quarter hour that is.
    delivered_slot: Slot,
    /// Which Berlin calendar day the loop is in, so the end of one is noticed.
    local_day: time::Date,
    /// Minutes the arbiter has spent on the fallback today.
    unplanned: crate::runtime::day::Unplanned,
    /// What the hardware would not take today.
    clipping: crate::runtime::day::Clipping,
    /// How today's forecasts have scored against what actually happened.
    scored: crate::runtime::day::Scored,
    /// How many ticks have overrun their period.
    overruns: u64,
    /// Production accumulated over the quarter hour in progress, watt-hours.
    pv_wh: f64,
    /// How many ticks of it were actually read off a meter. Zero is a roof
    /// nobody measured, which is a different fact from a roof that produced
    /// nothing.
    pv_samples: usize,
    /// The household's own load over the same, watt-hours.
    load_wh: f64,
    /// How many ticks contributed to those two.
    samples: u32,
    /// The quarter hour's metered flows, watt-hours, in the four registers
    /// MiSpeL and § 42c settle from.
    ///
    /// `Z1NB¼` and `Z1NE¼` are what crossed the connection point; `Z2V¼` and
    /// `Z2E¼` are what the storage system and the charge point did. Accumulated
    /// from **measurements** rather than from commands, because a settlement
    /// computed from what a controller asked for is not a settlement.
    grid_draw_wh: f64,
    grid_feed_wh: f64,
    device_draw_wh: f64,
    device_feed_wh: f64,
    /// The § 14a record, built as the loop runs.
    ///
    /// `[A1 7.2]` is a document about what a household *did* while a reduction
    /// was in force, and the only place that can be built is here: a record
    /// reconstructed afterwards from logs that were never kept is not a record.
    evidence: hems_grid::evidence::EvidenceRecorder,
}

/// One turn of the loop: drain the drivers, decide, command, account.
///
/// Returns what a screen should show and how many devices could not be heard
/// from, which is what readiness is computed from.
#[allow(clippy::too_many_arguments)]
async fn tick(
    arbiter: &Arbiter,
    managed: &Managed,
    registry: &Shared,
    overrides: &BTreeMap<AssetId, UserOverride>,
    plan: Option<&hems_core::prelude::Plan>,
    learned: &Arc<Mutex<crate::runtime::planner::Learned>>,
    prices: &Arc<tokio::sync::RwLock<Option<hems_tariff::PriceStack>>>,
    store: Option<&Arc<Mutex<crate::store::Store>>>,
    carried: &mut Carried,
    period: std::time::Duration,
    now: OffsetDateTime,
) -> (Status, usize) {
    // A quarter hour is what a plan commits energy over, so the tally that
    // tracks it resets on the boundary rather than on a timer of its own — and
    // the boundary is also the only moment at which a quarter hour is *over*,
    // which is what makes it the right place to teach the forecasts.
    let slot = Slot::containing(now);
    if slot != carried.delivered_slot {
        teach(managed, learned, carried, period).await;
        register(managed, carried, prices, store).await;
        // After the register, because the day being closed is built from the
        // rows this loop has written — including the one that was just written.
        close_the_day(managed, carried, store, now).await;
        carried.delivered.clear();
        carried.samples = 0;
        carried.pv_wh = 0.0;
        carried.pv_samples = 0;
        carried.load_wh = 0.0;
        carried.grid_draw_wh = 0.0;
        carried.grid_feed_wh = 0.0;
        carried.device_draw_wh = 0.0;
        carried.device_feed_wh = 0.0;
        carried.delivered_slot = slot;
    }

    // Counted here rather than derived from the plan's age on the screen: the
    // screen answers "how stale is the plan now", and the fleet asks "how much
    // of the day did this box spend without one" — which is a sum over ticks and
    // cannot be recovered from a single instant (D116).
    carried.unplanned.tick(
        plan.is_some(),
        u32::try_from(period.as_secs()).unwrap_or(u32::MAX),
    );

    // ── 1. What the drivers said ────────────────────────────────────────────
    let observed: Observed = {
        let mut guard = registry.lock().await;
        // Taken rather than merely folded: the § 14a evidence record is built
        // from exactly these, and an event nobody journals is a control action
        // nobody can prove was carried out.
        for (asset, event) in guard.drain() {
            tracing::debug!(%asset, ?event, "a driver reported");
        }
        // The connection point is named rather than guessed: `[A1 2.3]` is
        // measured *there*, and a registry that had to work out which of its
        // meters was the grid one would be inferring the most important fact in
        // the system from an asset kind.
        guard.observe(managed.grid_meter.as_ref(), now)
    };

    // ── 2. The decision ─────────────────────────────────────────────────────
    let decision = arbiter.tick(Tick {
        now,
        site: &managed.site,
        state: &observed.state,
        limits: &observed.limits,
        // No plan: `tariffd` and `forecastd` are not called yet, so there are no
        // prices and no forecast to plan against. The arbiter falls back to
        // tracking the measured surplus, which is what keeps the house safe and
        // sensible with the WAN cut — and the *reason* it is doing so is
        // reported rather than assumed.
        plan,
        overrides,
        previous: &carried.previous,
        delivered: &carried.delivered,
        phases: &carried.phases,
    });

    // ── 3. Back to the hardware ─────────────────────────────────────────────
    {
        let mut guard = registry.lock().await;
        for setpoint in &decision.setpoints {
            // A command that fails is loud, because a command nobody sends and
            // nobody reports is how a device quietly stops being managed — and
            // under a § 14a reduction it is how a household quietly stops
            // complying.
            //
            // With one exception, and it is the difference between a fault and a
            // fact. `NoDriver` says this asset has no driver, which is equally
            // true on every tick; it is named once at start-up and counted on
            // the status surface, so a partially commissioned box does not write
            // one line per asset per second and bury the real fault inside it.
            if let Err(error) = guard.command(setpoint, now)
                && !matches!(error, hems_drv::DriverError::NoDriver(_))
            {
                tracing::warn!(
                    asset = %setpoint.asset,
                    reason = ?setpoint.reason,
                    %error,
                    "a setpoint could not be delivered"
                );
            }
        }
    }

    // The energy each asset has moved since the slot began, which is what turns
    // a plan into a commitment. Accumulated from what was *commanded* rather
    // than from what was measured, because not every asset has a sub-meter and a
    // commitment tracked only for the instrumented half of a house is worse than
    // one tracked for none of it.
    let seconds = period.as_secs_f64();
    for (asset, watts) in &decision.commanded {
        let moved = Energy::new(watts.get() * seconds / 3600.0);
        *carried
            .delivered
            .entry(asset.clone())
            .or_insert(Energy::ZERO) += moved;
    }

    // What the hardware would not take. The arbiter resolves a command a device
    // cannot hold — a charge point below 6 A, a hot-water relay part way on —
    // and every layer above then believes the decided value; counting it is the
    // only thing that says a household's wallbox is not the one the planner is
    // modelling.
    carried.clipping.tick(
        decision.clipped.values().copied().sum(),
        u32::try_from(period.as_secs()).unwrap_or(u32::MAX),
    );

    // ── 4. The record `[A1 7.2]` asks for ──────────────────────────────────
    //
    // Built as the loop runs, because a record reconstructed afterwards from
    // logs that were never kept is not a record — and what it documents is the
    // one thing a network operator may come back and ask about two years later.
    record(managed, carried, &observed, &decision, store, now).await;

    // …and what the *house* did, which is a different question and is what the
    // forecasts are taught from. **Measured** rather than commanded: a forecast
    // trained on what the box asked for would learn its own behaviour.
    //
    // Under the load convention the household's own uninstrumented load is the
    // grid meter less the sum of the assets — the same residual the arbiter
    // reports. Where the grid meter is silent there is nothing to learn from
    // this tick, and nothing is invented: a house nobody measured did not use
    // nothing.
    meter(managed, carried, &observed, seconds);

    carried.previous.clone_from(&decision.commanded);
    carried.phases.clone_from(&decision.phases);

    let silent = observed.silent.len();
    (
        Status {
            at: Some(now),
            commanded: decision.commanded,
            silent: observed.silent.iter().cloned().collect(),
            assumed_available: observed.assumed_available.iter().cloned().collect(),
            undriven: managed.undriven.clone(),
            steuve_ceiling: observed.limits.steuve_ceiling,
            steuve_budget: decision.verdict.steuve_budget,
            netzwirksam: Some(decision.verdict.netzwirksam),
            balance_residual: decision.balance_residual,
            // From the plan the box actually published, not from when the
            // process started: a box that has been planning for a week and
            // stopped an hour ago is a different thing from one that has never
            // planned, and only one of them is a fault.
            minutes_without_a_plan: plan.map_or(i64::MAX, |p| (now - p.created_at).whole_minutes()),
            overruns: carried.overruns,
            plan_expected_eur: plan
                .and_then(|p| p.expected_cost.as_ref())
                .map(hems_core::prelude::CostBreakdown::total),
            plan_baseline_eur: plan
                .and_then(|p| p.baseline_cost.as_ref())
                .map(hems_core::prelude::CostBreakdown::total),
        },
        silent,
    )
}
