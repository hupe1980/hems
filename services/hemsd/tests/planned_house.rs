//! A box that plans, against a fleet on loopback.
//!
//! `managed_house.rs` proves a reading gets from a socket to the guard. This
//! proves the other half: that prices and a sky get from two HTTP services into
//! a plan the arbiter can follow.
//!
//! The fleet here is two `axum` routes serving what the real daemons serve,
//! byte for byte in shape. That is deliberate rather than lazy: what is being
//! tested is the **client and the planner**, and standing up the real daemons
//! would test their upstreams instead — which are already tested against
//! captured bodies in their own crates.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use hems_core::prelude::{Measurement, Power, Rc2, Slot, Soc};
use hems_service::Shutdown;
use hemsd::drivers::Registry;
use hemsd::runtime::planner::{Learned, Planner};
use tokio::sync::{Mutex, RwLock};

/// A clear January day, priced cheap at night and dear at teatime.
///
/// The shape is what makes the test mean something: a battery with a wear cost
/// only moves energy when the spread covers the damage, so a flat curve would
/// produce a plan that does nothing and prove nothing.
fn price_body(from: time::OffsetDateTime, slots: usize) -> String {
    let first = Slot::containing(from);
    let points: Vec<String> = (0..slots)
        .map(|i| {
            let slot = first.offset(i64::try_from(i).unwrap_or(0));
            let hour = slot.start().hour();
            let ct = match hour {
                0..=5 => "2.5",
                17..=19 => "28.0",
                _ => "12.0",
            };
            format!(
                r#"{{"slot":"{}","price_ct":"{ct}","source":"entsoe"}}"#,
                rfc3339(slot.start())
            )
        })
        .collect();
    format!(
        r#"{{"points":[{}],"coverage":1.0,"contiguous_until":"{}"}}"#,
        points.join(","),
        rfc3339(first.offset(i64::try_from(slots).unwrap_or(0)).start())
    )
}

/// A day of sky: dark until eight, a bell through the middle, dark after four.
fn sky_body(from: time::OffsetDateTime, slots: usize) -> String {
    let first = Slot::containing(from);
    let points: Vec<String> = (0..slots)
        .map(|i| {
            let slot = first.offset(i64::try_from(i).unwrap_or(0));
            let hour = f64::from(slot.start().hour());
            // A crude winter bell, peaking at noon. The planner does not need it
            // to be right — it needs it to be *there*, and to be zero at night.
            let ghi = if (8.0..16.0).contains(&hour) {
                300.0 * (1.0 - ((hour - 12.0) / 4.0).abs())
            } else {
                0.0
            };
            format!(
                r#"{{"slot":"{}","ghi_w_per_m2":{ghi},"temperature_c":3.0,"cloud_cover":0.3}}"#,
                rfc3339(slot.start())
            )
        })
        .collect();
    format!(
        r#"{{"fetched_at":"{}","published_minutes":15,"points":[{}]}}"#,
        rfc3339(from),
        points.join(",")
    )
}

fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .expect("a formattable instant")
}

/// Two routes on loopback, answering what the real daemons answer.
async fn fleet_on_loopback() -> (String, Shutdown) {
    let app = Router::new()
        .route(
            "/v1/prices",
            get(|| async move {
                let now = time::OffsetDateTime::now_utc();
                ([("content-type", "application/json")], price_body(now, 192))
            }),
        )
        .route(
            "/v1/weather/{location}",
            get(|| async move {
                let now = time::OffsetDateTime::now_utc();
                ([("content-type", "application/json")], sky_body(now, 192))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = format!("http://{}", listener.local_addr().expect("its address"));
    let (signal, trigger) = Shutdown::channel();
    let stop = signal.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(stop.wait())
            .await;
    });
    // The trigger is kept alive by the returned `Shutdown`'s sender; handing the
    // caller the signal is enough to keep the server up for the test.
    std::mem::forget(trigger);
    (address, signal)
}

/// A registry whose battery reports a real state of charge.
///
/// The one measurement the planner refuses to guess: a plan built on an assumed
/// fill empties a pack it thought was full.
fn registry_with_a_battery_at(soc: f64) -> (Arc<Mutex<Registry>>, hemsd::Household) {
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let mut registry = Registry::new();
    let mut battery = Reporting::new(
        household
            .battery
            .clone()
            .expect("the reference household has a battery"),
    );
    let mut m = Measurement::at(time::OffsetDateTime::now_utc());
    m.power = Some(Power::ZERO);
    m.soc = Soc::new(soc).ok();
    battery.say(m);
    registry
        .register(Box::new(battery), &household.site)
        .expect("the site has a battery");

    // …and a grid meter, because the household's own load is the meter less the
    // assets and a box with no meter has nothing to learn from.
    let mut meter = Reporting::new(household.grid_meter.clone());
    let mut m = Measurement::at(time::OffsetDateTime::now_utc());
    m.power = Some(Power::from_kw(0.6));
    meter.say(m);
    registry
        .register(Box::new(meter), &household.site)
        .expect("the site has a meter");

    (Arc::new(Mutex::new(registry)), household)
}

/// A driver that reports one measurement and never changes its mind.
#[derive(Debug)]
struct Reporting {
    asset: hems_core::prelude::AssetId,
    events: Vec<hems_drv::DriverEvent>,
}

impl Reporting {
    fn new(asset: hems_core::prelude::AssetId) -> Self {
        Self {
            asset,
            events: vec![hems_drv::DriverEvent::Link(hems_drv::LinkState::Up)],
        }
    }
    fn say(&mut self, m: Measurement) {
        self.events.push(hems_drv::DriverEvent::Measured(m));
    }
}

impl hems_drv::Driver for Reporting {
    fn asset(&self) -> &hems_core::prelude::AssetId {
        &self.asset
    }
    fn capabilities(&self) -> hems_drv::DriverCapabilities {
        hems_drv::DriverCapabilities::device()
    }
    fn on_bytes(&mut self, _: &[u8], _: time::OffsetDateTime) -> Result<(), hems_drv::DriverError> {
        Ok(())
    }
    fn on_timeout(&mut self, _: time::OffsetDateTime) {}
    fn command(
        &mut self,
        _: &hems_core::setpoint::Command,
        _: time::OffsetDateTime,
    ) -> Result<(), hems_drv::DriverError> {
        Ok(())
    }
    fn poll_event(&mut self) -> Option<hems_drv::DriverEvent> {
        if self.events.is_empty() {
            None
        } else {
            Some(self.events.remove(0))
        }
    }
    fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        None
    }
    fn poll_deadline(&self) -> Option<time::OffsetDateTime> {
        None
    }
}

/// A box that has been running long enough to know its own household.
fn learned_household(land: metering::Bundesland) -> Learned {
    let mut learned = Learned::new(land, Rc2::house());
    // Three weeks of a flat six hundred watts. Enough support for the profile to
    // answer, and deliberately dull: what is being tested is that a *forecast*
    // reaches the planner, not that this one is good.
    let start = Slot::containing(time::OffsetDateTime::now_utc()).offset(-96 * 21);
    for i in 0..(96 * 21) {
        learned.load.observe(start.offset(i), Power::from_kw(0.6));
    }
    learned
}

#[tokio::test]
async fn a_box_with_a_fleet_plans_against_real_prices_and_a_real_sky() {
    let (fleet_url, _keep) = fleet_on_loopback().await;
    let (registry, household) = registry_with_a_battery_at(0.5);
    let land = metering::Bundesland::Be;

    let settings = hemsd::Settings {
        fleet: hemsd::config::FleetSettings {
            tariffd_url: Some(fleet_url.clone()),
            forecastd_url: Some(fleet_url),
            location: Some("berlin".into()),
            request_timeout_s: 5,
        },
        control: hemsd::ControlSettings {
            // One day rather than two: the whole point is a plan, and 96 slots
            // solve in a fraction of the time 192 do.
            horizon_slots: 96,
            solve_budget_s: 20.0,
            ..hemsd::ControlSettings::default()
        },
        ..hemsd::Settings::default()
    };

    let plan = Arc::new(RwLock::new(None));
    let modelled = Arc::new(RwLock::new(BTreeMap::new()));
    let learned = Arc::new(Mutex::new(learned_household(land)));
    let health = hems_service::Health::new();
    let (signal, trigger) = Shutdown::channel();

    tokio::spawn(hemsd::runtime::planner::run(
        Planner {
            household: household.clone(),
            array: hems_forecast::ArrayModel::new(
                Power::from_kw(9.8),
                Power::from_kw(8.0),
                35.0,
                180.0,
            ),
            facade_azimuth_deg: 180.0,
            tariff: settings.tariff.clone(),
            control: settings.control.clone(),
            charging: None,
            wear_eur_per_kwh: 0.08,
        },
        Arc::clone(&registry),
        hemsd::runtime::fleet::Fleet::new(&settings.fleet, &hems_service::HttpSettings::default())
            .expect("a client"),
        Arc::clone(&plan),
        // The price stack the plan was made against, which a running box hands
        // to the loop that writes its quarter-hour registers. Not read here.
        Arc::new(RwLock::new(None)),
        hemsd::runtime::planner::Published {
            modelled_pv: Arc::clone(&modelled),
            bands: Arc::new(RwLock::new(BTreeMap::new())),
            outdoor: Arc::new(RwLock::new(BTreeMap::new())),
            window: Arc::new(RwLock::new(BTreeMap::new())),
        },
        Arc::clone(&learned),
        // No store: what is being tested is the planning loop, and a box that
        // keeps its learning is `planner`'s own round-trip test.
        None,
        health.clone(),
        signal,
    ));

    // The first tick of a tokio interval fires immediately, so a plan is due at
    // once; a solve of 96 slots with one store is seconds at most.
    let mut published = None;
    for _ in 0..300 {
        if let Some(p) = plan.read().await.clone() {
            published = Some(p);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    trigger.trigger();

    let plan = published.expect(
        "a box with prices, a sky, a load profile and a battery has everything it \
         needs to plan",
    );
    assert_eq!(plan.slots.len(), 96);

    // The battery is named and commanded, because the problem modelled it.
    let commands_battery = plan.slots.iter().any(|s| {
        s.targets
            .iter()
            .any(|t| Some(&t.asset) == household.battery.as_ref())
    });
    assert!(
        commands_battery,
        "the battery reported a state of charge, so the plan may move it"
    );

    // …and the charge point is **not**, because no car was in the problem. A
    // plan that named it would emit a target of zero with an envelope pinned at
    // zero, which the arbiter obeys — an instruction not to charge, all day,
    // from a planner that had simply not been told about the car.
    let commands_evse = plan.slots.iter().any(|s| {
        s.targets
            .iter()
            .any(|t| Some(&t.asset) == household.evse.as_ref())
    });
    assert!(
        !commands_evse,
        "an asset the problem does not model must not be named: an envelope \
         pinned at zero is an instruction, not an omission"
    );

    // The plan carries its own arithmetic, and every term of the objective is a
    // term of it.
    let cost = plan.expected_cost.expect("a plan prices itself");
    assert!(
        cost.total().is_finite(),
        "a plan that cannot say what it costs cannot be compared with anything"
    );
    assert!(
        plan.baseline_cost.is_some(),
        "…and it has to say what the same day would have cost without it"
    );

    // The corrector's own input was published for the control loop to teach
    // against. Without it the residual model would be compared with a fresh
    // clear-sky figure and would learn the weather rather than the roof.
    assert!(
        !modelled.read().await.is_empty(),
        "the modelled production has to reach the loop that teaches the corrector"
    );
}

#[tokio::test]
async fn a_battery_whose_charge_nobody_reports_is_left_out_of_the_plan() {
    // The refusal that matters most here. A store whose fill is unknown is one
    // no plan may move: a schedule built on a guessed state of charge empties a
    // pack it thought was full, and the guard would then be the only thing
    // between the household and a flat battery on the evening it wanted one.
    //
    // The box still plans — the roof, the load and the § 14a ceiling are all
    // still there — it simply does not command the battery.
    let (fleet_url, _keep) = fleet_on_loopback().await;
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let mut registry = Registry::new();
    let mut meter = Reporting::new(household.grid_meter.clone());
    let mut m = Measurement::at(time::OffsetDateTime::now_utc());
    m.power = Some(Power::from_kw(0.6));
    meter.say(m);
    registry
        .register(Box::new(meter), &household.site)
        .expect("the site has a meter");
    let registry = Arc::new(Mutex::new(registry));

    let settings = hemsd::Settings {
        fleet: hemsd::config::FleetSettings {
            tariffd_url: Some(fleet_url.clone()),
            forecastd_url: Some(fleet_url),
            location: Some("berlin".into()),
            request_timeout_s: 5,
        },
        control: hemsd::ControlSettings {
            horizon_slots: 96,
            solve_budget_s: 20.0,
            ..hemsd::ControlSettings::default()
        },
        ..hemsd::Settings::default()
    };

    let plan = Arc::new(RwLock::new(None));
    let modelled = Arc::new(RwLock::new(BTreeMap::new()));
    let learned = Arc::new(Mutex::new(learned_household(metering::Bundesland::Be)));
    let (signal, trigger) = Shutdown::channel();

    tokio::spawn(hemsd::runtime::planner::run(
        Planner {
            household: household.clone(),
            array: hems_forecast::ArrayModel::new(
                Power::from_kw(9.8),
                Power::from_kw(8.0),
                35.0,
                180.0,
            ),
            facade_azimuth_deg: 180.0,
            tariff: settings.tariff.clone(),
            control: settings.control.clone(),
            charging: None,
            wear_eur_per_kwh: 0.08,
        },
        registry,
        hemsd::runtime::fleet::Fleet::new(&settings.fleet, &hems_service::HttpSettings::default())
            .expect("a client"),
        Arc::clone(&plan),
        Arc::new(RwLock::new(None)),
        hemsd::runtime::planner::Published {
            modelled_pv: modelled,
            bands: Arc::new(RwLock::new(BTreeMap::new())),
            outdoor: Arc::new(RwLock::new(BTreeMap::new())),
            window: Arc::new(RwLock::new(BTreeMap::new())),
        },
        learned,
        None,
        hems_service::Health::new(),
        signal,
    ));

    let mut published = None;
    for _ in 0..300 {
        if let Some(p) = plan.read().await.clone() {
            published = Some(p);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    trigger.trigger();

    let plan = published.expect("the box still plans without a battery it can read");
    assert!(
        !plan.slots.iter().any(|s| s
            .targets
            .iter()
            .any(|t| Some(&t.asset) == household.battery.as_ref())),
        "a store nobody can read the fill of is one no plan may move"
    );
}

#[tokio::test]
async fn a_box_installed_this_morning_plans_from_persistence() {
    // A household should get a plan on its first evening, not its second.
    //
    // `hems-forecast` has had a persistence fallback since the beginning —
    // "the next hours look like the last one", with a band that widens the
    // further out it reaches — and until now nothing called it: the planner
    // refused outright where the profile had no support. A module that is built,
    // documented and reached by nothing is the failure mode this workspace keeps
    // finding in itself.
    //
    // What it must *not* do is invent one. A box that cannot read its own
    // connection point has no load to persist from, and that case is refused —
    // which is the other test in this file's `NoHistory` reason.
    let (fleet_url, _keep) = fleet_on_loopback().await;
    let (registry, household) = registry_with_a_battery_at(0.5);

    let settings = hemsd::Settings {
        fleet: hemsd::config::FleetSettings {
            tariffd_url: Some(fleet_url.clone()),
            forecastd_url: Some(fleet_url),
            location: Some("berlin".into()),
            request_timeout_s: 5,
        },
        control: hemsd::ControlSettings {
            horizon_slots: 96,
            solve_budget_s: 20.0,
            ..hemsd::ControlSettings::default()
        },
        ..hemsd::Settings::default()
    };

    let plan = Arc::new(RwLock::new(None));
    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::planner::run(
        Planner {
            household: household.clone(),
            array: hems_forecast::ArrayModel::new(
                Power::from_kw(9.8),
                Power::from_kw(8.0),
                35.0,
                180.0,
            ),
            facade_azimuth_deg: 180.0,
            tariff: settings.tariff.clone(),
            control: settings.control.clone(),
            charging: None,
            wear_eur_per_kwh: 0.08,
        },
        registry,
        hemsd::runtime::fleet::Fleet::new(&settings.fleet, &hems_service::HttpSettings::default())
            .expect("a client"),
        Arc::clone(&plan),
        Arc::new(RwLock::new(None)),
        hemsd::runtime::planner::Published {
            modelled_pv: Arc::new(RwLock::new(BTreeMap::new())),
            bands: Arc::new(RwLock::new(BTreeMap::new())),
            outdoor: Arc::new(RwLock::new(BTreeMap::new())),
            window: Arc::new(RwLock::new(BTreeMap::new())),
        },
        // A box that has learned nothing at all — switched on this morning.
        Arc::new(Mutex::new(Learned::new(
            metering::Bundesland::Be,
            Rc2::house(),
        ))),
        None,
        hems_service::Health::new(),
        signal,
    ));

    let mut published = None;
    for _ in 0..300 {
        if let Some(p) = plan.read().await.clone() {
            published = Some(p);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    trigger.trigger();

    let plan = published.expect(
        "a box with a meter but no history plans from persistence rather than \
         refusing — a household gets a plan on its first evening",
    );
    assert_eq!(plan.slots.len(), 96);
    assert!(
        plan.expected_cost.is_some(),
        "and it is a real plan, priced like any other"
    );
}

#[test]
fn every_risk_policy_a_household_can_configure_reaches_the_planner() {
    // Until the box could be told, it always planned against one median: the
    // scenario set, its Rockafellar–Uryasev tail and the quantile knob were
    // selectable from `hemsd simulate` and from **nothing on a wall**, so a
    // household could not act on the trade-off `hemsd risk` had measured for it.
    //
    // Each setting has to map to a *distinct* policy, because four names for the
    // same plan would be a configuration surface that decides nothing — which is
    // the failure this workspace keeps finding in itself.
    use hemsd::config::RiskSettings;

    let median = RiskSettings::Median.model();
    let expected = RiskSettings::Expected.model();
    let hedged = RiskSettings::Hedged.model();
    let pessimistic = RiskSettings::Pessimistic.model();

    // One future against three: the deterministic policy declares no scenario
    // variables or rows at all, which is what makes it a comparison against the
    // old planner rather than a differently-conditioned one.
    assert_eq!(median.realisations().len(), 1);
    assert_eq!(expected.realisations().len(), 3);
    assert_eq!(hedged.realisations().len(), 3);
    assert_eq!(
        pessimistic.realisations().len(),
        1,
        "a quantile plan is one future, which is the whole of what is wrong with it"
    );

    // `expected` and `hedged` differ only in the tail weight, and that is the
    // distinction: pricing three futures at all, against pricing the worst of
    // them more.
    assert_eq!(expected.tail_weight(), 0.0);
    assert!(hedged.tail_weight() > 0.0);

    // And the default is the median, because every figure in this workspace is
    // calibrated against it.
    assert_eq!(RiskSettings::default(), RiskSettings::Median);
    assert_eq!(
        hemsd::config::ControlSettings::default().risk,
        RiskSettings::Median
    );
}

/// One planning round with whatever registry is handed in, and the plan it made.
async fn plan_with(
    registry: Arc<Mutex<Registry>>,
    household: &hemsd::Household,
) -> Option<hems_core::prelude::Plan> {
    let (fleet_url, _keep) = fleet_on_loopback().await;
    let settings = hemsd::Settings {
        fleet: hemsd::config::FleetSettings {
            tariffd_url: Some(fleet_url.clone()),
            forecastd_url: Some(fleet_url),
            location: Some("berlin".into()),
            request_timeout_s: 5,
        },
        control: hemsd::ControlSettings {
            horizon_slots: 96,
            solve_budget_s: 20.0,
            ..hemsd::ControlSettings::default()
        },
        ..hemsd::Settings::default()
    };
    let plan = Arc::new(RwLock::new(None));
    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::planner::run(
        Planner {
            household: household.clone(),
            array: hems_forecast::ArrayModel::new(
                Power::from_kw(9.8),
                Power::from_kw(8.0),
                35.0,
                180.0,
            ),
            facade_azimuth_deg: 180.0,
            tariff: settings.tariff.clone(),
            control: settings.control.clone(),
            charging: None,
            wear_eur_per_kwh: 0.08,
        },
        registry,
        hemsd::runtime::fleet::Fleet::new(&settings.fleet, &hems_service::HttpSettings::default())
            .expect("a client"),
        Arc::clone(&plan),
        Arc::new(RwLock::new(None)),
        hemsd::runtime::planner::Published {
            modelled_pv: Arc::new(RwLock::new(BTreeMap::new())),
            bands: Arc::new(RwLock::new(BTreeMap::new())),
            outdoor: Arc::new(RwLock::new(BTreeMap::new())),
            window: Arc::new(RwLock::new(BTreeMap::new())),
        },
        Arc::new(Mutex::new(learned_household(metering::Bundesland::Be))),
        None,
        hems_service::Health::new(),
        signal,
    ));
    let mut published = None;
    for _ in 0..300 {
        if let Some(p) = plan.read().await.clone() {
            published = Some(p);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    trigger.trigger();
    published
}

/// A meter, a battery and — optionally — a heat pump reporting how warm the
/// house is.
fn registry_with_a_house_at(indoor_c: Option<f64>) -> (Arc<Mutex<Registry>>, hemsd::Household) {
    let (registry, household) = registry_with_a_battery_at(0.5);
    if let Some(indoor_c) = indoor_c {
        let id = household
            .heat_pump
            .clone()
            .expect("the reference household has a heat pump");
        let mut pump = Reporting::new(id);
        let mut m = Measurement::at(time::OffsetDateTime::now_utc());
        m.power = Some(Power::ZERO);
        m.temperature_c = Some(indoor_c);
        pump.say(m);
        registry
            .try_lock()
            .expect("nothing else holds the registry yet")
            .register(Box::new(pump), &household.site)
            .expect("the site has a heat pump");
    }
    (registry, household)
}

#[tokio::test]
async fn a_house_nobody_measures_the_temperature_of_is_not_pre_heated() {
    // The same refusal the battery makes, for the same reason. A thermal plan
    // built on a guessed indoor temperature decides when to heat from a number
    // nobody read, and it is wrong in the expensive direction on exactly the
    // cold mornings it matters. The box still plans everything else.
    let (registry, household) = registry_with_a_house_at(None);
    let plan = plan_with(registry, &household)
        .await
        .expect("a box with no room sensor still plans its roof and its battery");
    assert!(
        !plan.slots.iter().any(|s| s
            .targets
            .iter()
            .any(|t| Some(&t.asset) == household.heat_pump.as_ref())),
        "a house nobody measured is one no plan may pre-heat"
    );
}

#[tokio::test]
async fn a_measured_house_is_planned_rather_than_left_to_its_own_thermostat() {
    // The lever a thermal model exists for. Until this was wired the planner
    // passed `heat_pump: None` unconditionally — every household's heating was
    // its own thermostat's business, and the RC model, the COP curve, the
    // comfort band and the minimum-runtime rows were exercised only by the
    // simulator.
    let (registry, household) = registry_with_a_house_at(Some(21.0));
    let plan = plan_with(registry, &household)
        .await
        .expect("a box that can read its house plans it");
    let commanded: Vec<_> = plan
        .slots
        .iter()
        .flat_map(|s| s.targets.iter())
        .filter(|t| Some(&t.asset) == household.heat_pump.as_ref())
        .collect();
    assert!(
        !commanded.is_empty(),
        "the heat pump is named in the plan because the problem models it"
    );
    // A January day at 3 °C: the house loses heat, so the plan has to put some
    // back. A model that named the asset and asked it for nothing all day would
    // be a target of zero the arbiter obeys as an instruction (D96).
    assert!(
        commanded.iter().any(|t| t.power.inflow().get() > 0.0),
        "and it is asked to run, because a house at 3 °C outside cools"
    );
}
