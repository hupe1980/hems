//! The box, running.
//!
//! Everything else in `hemsd` runs a *simulated* day: a scenario supplies the
//! weather, the prices and the Steuerbox, and the whole thing finishes in
//! seconds. This is the same three planes against a real clock and real
//! sockets — the last seam between "the logic is right" and "the house is
//! managed".
//!
//! # What runs, and at what speed
//!
//! | Task | Cadence | What it owns |
//! |---|---|---|
//! | one [`transport`] task per driver | its own socket's | the socket, and nothing else |
//! | the control loop ([`control`]) | `tick_period_s` | guard, arbiter, and the commands that come out |
//! | the HTTP surface (`hems-service`) | on request | health, readiness, and what the box is doing |
//!
//! The drivers are shared behind one lock rather than owned by the loop,
//! because the two questions are asked at different speeds: a socket wants
//! waking when *its* deadline passes, and a control period is a property of the
//! house. Giving the loop the sockets would make a slow inverter's cadence the
//! cadence of the guard.
//!
//! # There is deliberately no planner here yet, and the absence is reported
//!
//! The guard and the arbiter need nothing but measurements, which is the whole
//! of the offline-first promise (G3): with the WAN cut the house stays inside
//! every limit and tracks its own surplus. The **planner** needs prices and a
//! forecast, and on a real box those come from `tariffd` and `forecastd` over a
//! network this daemon does not yet call. So the arbiter runs with no plan and
//! says so — [`Status::minutes_without_a_plan`] is the number, and it is on the
//! health surface rather than in a comment, because a box that quietly never
//! plans looks exactly like one that plans badly.

pub mod api;
pub mod control;
pub mod day;
pub mod fleet;
pub mod outbox;
pub mod overrides;
pub mod planner;
pub mod ship;
pub mod transport;

use std::collections::BTreeMap;
use std::sync::Arc;

use hems_core::prelude::{AssetId, Power, Site};
use hems_drv::modbus::{Cadence, SunSpec};
use hems_service::{Health, Shutdown};
use tokio::sync::Mutex;

use crate::config::{DriverSettings, Settings};
use crate::drivers::Registry;
use crate::site::Household;

pub use control::{Live, Managed, Status};
pub use transport::Shared;

/// Why a box could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration does not describe a house.
    #[error("the site configuration is not usable: {0}")]
    Site(#[from] crate::config::SettingsError),
    /// The site could not be built from it.
    #[error("the site could not be built: {0}")]
    Build(String),
    /// A driver names an asset the site does not have, or two name the same one.
    #[error("the drivers do not describe this site: {0}")]
    Drivers(#[from] crate::drivers::RegistryError),
    /// A driver's own configuration is not usable.
    #[error("the `{asset}` driver cannot be built: {detail}")]
    Driver {
        /// Which one.
        asset: String,
        /// Why not.
        detail: String,
    },
    /// The household is on Modul 3 and its calendar is not one anybody may bill.
    ///
    /// A refusal rather than a warning, and the argument is the same one every
    /// other refusal here makes: a box that started anyway would price a year of
    /// somebody's electricity in windows the Anwendungshilfe does not allow, and
    /// would look exactly like a box that was working.
    #[error("the Modul 3 calendar is not one this household may be billed on: {0}")]
    Modul3(String),
}

/// Everything a running box holds.
pub struct Running {
    /// The house.
    pub household: Household,
    /// The box's EEBUS identity, where it has a session to use one on.
    ///
    /// The number an installer has to give the metering point operator, and the
    /// reason it is a field rather than a log line: field reports make that
    /// exchange the single most common § 14a commissioning failure there is, and
    /// grepping a log for it is not a commissioning step anybody will follow.
    pub ski: Option<String>,
    /// The drivers, shared with their transport tasks.
    pub registry: Shared,
    /// What the control loop has decided, for the API and the health surface.
    pub status: Arc<Mutex<Status>>,
    /// What the household itself has asked for, shared with the HTTP surface.
    pub overrides: overrides::Overrides,
}

/// Build the site, the drivers and the registry, and check that they agree.
///
/// Everything that can be refused is refused **here**, before a socket is
/// opened: a driver for an asset the site does not have, two drivers for one
/// asset, a controllable asset whose driver cannot take commands, and a § 14a
/// household with nothing that could hear a reduction. Each of those is silent
/// at runtime and loud at start-up, which is the right way round.
///
/// # Errors
/// [`StartError`] for any of them.
pub fn assemble(settings: &Settings, now: time::OffsetDateTime) -> Result<Running, StartError> {
    let config = settings.site.household()?;
    let household = Household::build(&config).map_err(|e| StartError::Build(e.to_string()))?;

    let mut registry = Registry::new();
    for driver in &settings.drivers {
        let built = build_driver(driver, &household.site, now)?;
        registry.register(built, &household.site)?;
    }
    registry.validate(&household.site, now)?;
    check_modul3(settings, now)?;

    Ok(Running {
        ski: None,
        overrides: overrides::Overrides::new(),
        household,
        registry: Arc::new(Mutex::new(registry)),
        status: Arc::new(Mutex::new(Status::default())),
    })
}

/// Check a Modul 3 household's calendar against the Anwendungshilfe
/// (`specs/bnetza/bdew-awh-modul-3-v1.1-20250207.pdf`) and against its own
/// delivery point.
///
/// There is no machine-readable national format for one — a PDF or an Excel
/// sheet per network operator — so it is transcribed by whoever commissions the
/// box, and this is what makes transcribing safe rather than a second way to be
/// wrong (D126). The failures are quiet: a Niedertarif band written as one
/// wrapping window leaves that register declared and **unreachable**, the day
/// still covered by the fallback and every other rule passing, so the household
/// is billed for a module whose cheap hours it can never be in.
///
/// [`Modul3Conformance::Unknown`] is **not** a refusal: it means a rule could not
/// be checked rather than that one was broken, and refusing on it would tie the
/// box's willingness to start to how much `metering` knows this release.
fn check_modul3(settings: &Settings, now: time::OffsetDateTime) -> Result<(), StartError> {
    use hems_grid::modul3::{Modul3Conformance, Modul3Eligibility};

    if settings.tariff.modul != crate::config::Modul::Modul3 {
        // A calendar nothing reads is a calendar nobody notices is wrong.
        if settings.tariff.modul3.is_some() {
            return Err(StartError::Modul3(
                "a `[tariff.modul3]` calendar is configured and `modul` is not `modul3`,                  so nothing would ever read it"
                    .into(),
            ));
        }
        return Ok(());
    }

    let Some(configured) = settings.tariff.modul3.as_ref() else {
        return Err(StartError::Modul3(
            "`modul = \"modul3\"` needs a `[tariff.modul3]` calendar; without one the box              would price every hour the same and shift nothing"
                .into(),
        ));
    };
    let Some(calendar) = configured.calendar() else {
        return Err(StartError::Modul3(format!(
            "`billed_quarters` must be drawn from Q1, Q2, Q3 and Q4, and this says {:?}",
            configured.billed_quarters
        )));
    };
    if configured.source.is_none() {
        return Err(StartError::Modul3(
            "`source` is required: when a household queries a bill, the first question is \
             which document said so, and a calendar nobody can trace is one nobody can \
             defend"
                .into(),
        ));
    }

    // § 1 of the Anwendungshilfe: Modul 3 only together with Modul 1, only with
    // an intelligent metering system, and only without registrierende
    // Leistungsmessung. The metering system is a fact about the site, so it is
    // read from the site rather than declared twice.
    let household = settings.site.household()?;
    let eligibility = Modul3Eligibility {
        has_modul_1: true,
        has_imsys: household.para9.imsys_since.is_some(),
        has_rlm: false,
    };
    if !eligibility.is_eligible_on(metering::calendar::local_day(now)) {
        return Err(StartError::Modul3(
            "this delivery point may not be billed on Modul 3: it needs Modul 1, an              intelligent metering system, and no registrierende Leistungsmessung — and the              module has only been orderable since 01.04.2025"
                .into(),
        ));
    }

    let (verdict, findings) = calendar.assess(&eligibility.as_context());
    match verdict {
        Modul3Conformance::Violates => Err(StartError::Modul3(format!(
            "{findings:?} — see specs/bnetza/bdew-awh-modul-3-v1.1-20250207.pdf"
        ))),
        Modul3Conformance::Unknown => {
            tracing::warn!(
                ?findings,
                "the Modul 3 calendar breaks no rule this build can check, and something                  could not be checked"
            );
            Ok(())
        }
        Modul3Conformance::Conforms => Ok(()),
    }
}

/// One configured driver, built.
fn build_driver(
    settings: &DriverSettings,
    site: &Site,
    now: time::OffsetDateTime,
) -> Result<Box<dyn hems_drv::Driver + Send>, StartError> {
    let asset = |name: &str| {
        AssetId::new(name).map_err(|e| StartError::Driver {
            asset: name.to_string(),
            detail: e.to_string(),
        })
    };
    match settings {
        DriverSettings::Sunspec(s) => {
            let mut driver = SunSpec::new(
                asset(&s.asset)?,
                s.unit,
                Cadence {
                    poll: time::Duration::milliseconds(s.poll_ms.cast_signed()),
                    timeout: time::Duration::milliseconds(s.timeout_ms.cast_signed()),
                },
            );
            if s.listens_only {
                driver = driver.listening_only();
            }
            if let Some(kw) = s.rating_kw {
                driver = driver.with_rating(Power::from_kw(kw));
            }
            Ok(Box::new(driver))
        }
        DriverSettings::EebusLpc(s) => {
            // The household's own § 14a minimum where none is configured.
            // `[A1 4.5.2]`'s minimum grows with the number of controllable
            // devices, so a vendor's flat 4,2 kW on a household owed 10,5 kW
            // gives away six kilowatts nobody asked it to.
            let failsafe = s.failsafe_kw.map_or_else(
                || {
                    hems_grid::para14a::minimum_power(
                        &hems_grid::classify_at(&site.assets, now),
                        hems_grid::para14a::ControlMode::Ems,
                    )
                    .max(hems_grid::para14a::MINDESTLEISTUNG)
                },
                Power::from_kw,
            );
            let identity = hems_drv::eebus::SpineIdentity {
                vendor: s
                    .spine_vendor
                    .clone()
                    .unwrap_or_else(|| hems_drv::eebus::SpineIdentity::default().vendor),
                unique: s
                    .spine_unique
                    .clone()
                    .unwrap_or_else(|| hems_drv::eebus::SpineIdentity::default().unique),
            };
            let driver = hems_drv::eebus::Lpc::with_identity(
                asset(&s.asset)?,
                hems_drv::eebus::Use::Lpc,
                failsafe,
                std::time::Duration::from_secs(s.failsafe_hours.saturating_mul(3600)),
                now,
                &identity,
            )
            .map_err(|e| StartError::Driver {
                asset: s.asset.clone(),
                detail: e.to_string(),
            })?;
            Ok(Box::new(driver))
        }
    }
}

/// Where each configured driver's plain TCP socket is.
///
/// EEBUS has none, and that is not an omission: its session is TLS with mutual
/// authentication under a WebSocket under a SHIP handshake, and it is
/// `runtime::ship`'s rather than the byte-pump's.
fn transport_address(settings: &DriverSettings) -> Option<String> {
    match settings {
        DriverSettings::Sunspec(s) => Some(s.address.clone()),
        DriverSettings::EebusLpc(_) => None,
    }
}

/// Start every transport, the control loop and the HTTP surface, and run until
/// the process is asked to stop.
///
/// # Errors
/// [`StartError`] where the configuration and the site do not agree; after that
/// nothing here fails, because a household gateway box is not a request that can
/// fail — a device that is unreachable is reconnected to for ever.
pub async fn run(
    settings: &Settings,
    health: &Health,
    shutdown: &Shutdown,
) -> anyhow::Result<Running> {
    let now = time::OffsetDateTime::now_utc();
    let mut running = assemble(settings, now)?;

    if settings.drivers.is_empty() {
        // Not an error and not a silence. A box with no drivers keeps the house
        // safe by assuming the worst about every device, for ever, and saying so
        // once at start-up is cheaper than working it out from a screen of
        // nameplate assumptions at three in the morning.
        tracing::warn!(
            "no drivers are configured: nothing will be measured and every \
             controllable device will be assumed to be drawing its nameplate power"
        );
    }

    let eebus_asset = start_drivers(settings, &running, health, shutdown)?;

    // Named once, here, rather than warned about on every tick. A controllable
    // device with no driver is one the arbiter will decide a setpoint for all
    // day and have nowhere to send — a configuration fact, equally true every
    // second, and therefore a number on the status surface rather than a log
    // line eighty-six thousand times a day.
    let undriven: Vec<AssetId> = {
        let held = running.registry.lock().await;
        held.undriven(&running.household.site).cloned().collect()
    };
    if !undriven.is_empty() {
        tracing::warn!(
            assets = ?undriven,
            "controllable devices with no driver: the arbiter will decide for them \
             and have nowhere to send it"
        );
    }

    // The box's own store, where its two years and its learning live. Opening it
    // is allowed to fail loudly: a household configured for a record it cannot
    // keep is one whose Nachweis will be missing on the day it is asked for.
    let store = match &settings.store_path {
        Some(path) => Some(Arc::new(Mutex::new(crate::store::Store::open(path)?))),
        None => None,
    };
    if let Some(asset) = eebus_asset {
        running.ski =
            Some(start_ship(settings, &running, store.as_ref(), asset, health, shutdown).await?);
    }

    let overrides = overrides::Overrides::new();
    running.overrides = overrides.clone();

    let (plan, prices, published, learned) =
        start_planner(settings, &running, store.clone(), health, shutdown).await?;

    // The record's last leg. The box has already kept its own copy, so this is
    // the fleet's convenience rather than the household's safety — which is why
    // a failure here is a retry and never a reason to stop controlling a house.
    let to_histd = outbox::Outbox::new(&settings.histd)?;
    let to_obsd = outbox::Reporter::new(&settings.obsd)?;
    match store.clone() {
        Some(store) if to_histd.is_some() || to_obsd.is_some() => {
            tokio::spawn(outbox::run(
                to_histd,
                to_obsd,
                store,
                std::time::Duration::from_secs(settings.histd.every_s.max(30)),
                settings.histd.batch.max(1),
                shutdown.clone(),
            ));
        }
        _ => {
            if settings.histd.is_configured() || settings.obsd.is_configured() {
                tracing::warn!(
                    "a fleet endpoint is configured and this box has no store, so \
                     there is nothing to forward"
                );
            }
        }
    }

    tokio::spawn(control::run(
        control::Managed {
            site: running.household.site.clone(),
            grid_meter: Some(running.household.grid_meter.clone()),
            undriven,
            // `[A1 4.4.b]`: hems is an energy management system, so the operator
            // addresses everything behind it with one number rather than each
            // device on its own. The evidence record has to state which, because
            // the minimum a reduction may not go below depends on it.
            control_mode: hems_grid::para14a::ControlMode::Ems,
            pv: running.household.pv.clone(),
            battery: running.household.battery.clone(),
            evse: running.household.evse.clone(),
            modelled_pv: published.modelled_pv.clone(),
            bands: published.bands.clone(),
        },
        control::Live {
            registry: Arc::clone(&running.registry),
            status: Arc::clone(&running.status),
            plan,
            prices,
            learned,
            store,
            overrides: overrides.clone(),
        },
        settings.control.clone(),
        health.clone(),
        shutdown.clone(),
    ));

    Ok(running)
}

/// Give every configured driver whatever moves its bytes.
///
/// Returns the asset of the EEBUS Controllable System where one is configured
/// *and* has a session to accept on: its transport is TLS under a WebSocket
/// under a handshake, so it is started separately once the box's identity
/// exists.
fn start_drivers(
    settings: &Settings,
    running: &Running,
    health: &Health,
    shutdown: &Shutdown,
) -> Result<Option<AssetId>, StartError> {
    let mut unreachable = Vec::new();
    let mut eebus_asset = None;
    for driver in &settings.drivers {
        let asset = AssetId::new(driver.asset()).map_err(|e| StartError::Driver {
            asset: driver.asset().to_string(),
            detail: e.to_string(),
        })?;
        if let Some(address) = transport_address(driver) {
            tokio::spawn(transport::tcp(
                Arc::clone(&running.registry),
                asset,
                address,
                shutdown.clone(),
            ));
        } else if matches!(driver, DriverSettings::EebusLpc(_)) && settings.ship.listen.is_some() {
            // The § 14a session. Started once, below, because the identity has
            // to be created before anything can accept on it.
            eebus_asset = Some(asset);
        } else {
            // Still given its clock. Every transition out of the LPC machine's
            // first state is a timer, so a Controllable System nobody ticks sits
            // in `Init` for ever — holding the household at its § 14a minimum on
            // the strength of an implementation accident, and reporting a state
            // it is not really in. See `transport::clock_only`.
            tokio::spawn(transport::clock_only(
                Arc::clone(&running.registry),
                asset.clone(),
                shutdown.clone(),
            ));
            unreachable.push(asset);
        }
    }
    if !unreachable.is_empty() {
        // The one state this daemon must never be quiet about: a § 14a
        // household whose Steuerbox has no session is one that believes it is
        // participating and cannot hear a reduction. Its driver still runs —
        // `transport::clock_only` — so what it reports is the state the LPC
        // machine is really in rather than a frozen one. Honest, and not the
        // same as working, so the readiness probe says so.
        tracing::error!(
            drivers = ?unreachable,
            "configured with no session, so no reduction can arrive and the \
             Controllable System will report itself out of contact — set \
             `[ship] listen` to accept one"
        );
        health.bad(
            "grid",
            "an EEBUS driver is configured with no `[ship] listen`, so a § 14a \
             reduction could not arrive",
        );
    }
    Ok(eebus_asset)
}

/// Start the § 14a session: the box's own identity, a listener, and the task
/// that moves datagrams between a Steuerbox and the Controllable System.
///
/// The SKI is logged whether or not a Steuerbox ever connects, because it is
/// what an installer has to hand the metering point operator — and field reports
/// make that exchange the most common § 14a commissioning failure there is.
async fn start_ship(
    settings: &Settings,
    running: &Running,
    store: Option<&Arc<Mutex<crate::store::Store>>>,
    asset: AssetId,
    health: &Health,
    shutdown: &Shutdown,
) -> anyhow::Result<String> {
    let now = time::OffsetDateTime::now_utc();
    let (node, ski) = ship::identity(&settings.ship, store, now).await?;
    let address = settings.ship.listen.clone().unwrap_or_default();
    let listener = node
        .listen(&address)
        .await
        .map_err(|e| ship::ShipError::Listen {
            address: address.clone(),
            source: std::io::Error::other(e.to_string()),
        })?;
    tracing::info!(
        %address,
        ski = %ski.to_display_string(),
        trusted = settings.ship.trust.len(),
        "listening for a Steuerbox — this SKI is what the metering point operator \
         has to be given"
    );
    health.bad("grid", "no Steuerbox has connected yet");
    tokio::spawn(ship::run(
        node,
        listener,
        Arc::clone(&running.registry),
        asset,
        shutdown.clone(),
    ));
    Ok(ski.to_display_string())
}

/// Start the planning loop, where the box has anything to plan against.
///
/// Returns the three handles the control loop shares with it: the plan it
/// follows, the modelled production the corrector is taught against, and the
/// learning itself.
///
/// A box with no `forecastd` gets all three and no task, which is a working box
/// rather than a broken one: the guard and the arbiter need nothing but
/// measurements (G3). What it loses is the plan, and it says so.
type PlannerHandles = (
    Arc<tokio::sync::RwLock<Option<hems_core::prelude::Plan>>>,
    Arc<tokio::sync::RwLock<Option<hems_tariff::PriceStack>>>,
    planner::Published,
    Arc<Mutex<planner::Learned>>,
);

async fn start_planner(
    settings: &Settings,
    running: &Running,
    store: Option<Arc<Mutex<crate::store::Store>>>,
    health: &Health,
    shutdown: &Shutdown,
) -> anyhow::Result<PlannerHandles> {
    let plan = Arc::new(tokio::sync::RwLock::new(None));
    let prices = Arc::new(tokio::sync::RwLock::new(None));
    let published = planner::Published {
        modelled_pv: Arc::new(tokio::sync::RwLock::new(BTreeMap::new())),
        // What the box forecast, kept so it can score itself once each slot has
        // happened (D117).
        bands: Arc::new(tokio::sync::RwLock::new(BTreeMap::new())),
    };
    // What the box remembered from before it was restarted. A fortnight of
    // observations is what makes a forecast worth having, and relearning it
    // every reboot is the difference between a box that plans on its first
    // evening and one that does not.
    let learned = Arc::new(Mutex::new(match &store {
        Some(store) => planner::Learned::restored(&*store.lock().await, settings.site.bundesland),
        None => planner::Learned::new(settings.site.bundesland),
    }));

    let fleet = fleet::Fleet::new(&settings.fleet)?;
    if !fleet.has_weather() {
        // Loud, because this is the seam the box is most likely to be quietly
        // broken at: a household that is being kept safe and is not being kept
        // cheap looks, from every screen, exactly like one that is.
        tracing::warn!(
            "no `forecastd` and location are configured, so this box cannot \
             forecast its own roof and will not plan: the arbiter will track the \
             measured surplus instead"
        );
        health.bad(
            "planner",
            "no forecastd is configured, so the box cannot plan",
        );
        return Ok((plan, prices, published, learned));
    }

    health.bad("planner", "no plan has been produced yet");
    tokio::spawn(planner::run(
        planner::Planner {
            household: running.household.clone(),
            array: array_of(&running.household.site, &settings.site),
            tariff: settings.tariff.clone(),
            control: settings.control.clone(),
            wear_eur_per_kwh: settings.site.battery_wear_eur_per_kwh,
        },
        Arc::clone(&running.registry),
        fleet,
        Arc::clone(&plan),
        Arc::clone(&prices),
        published.clone(),
        Arc::clone(&learned),
        store,
        health.clone(),
        shutdown.clone(),
    ));
    Ok((plan, prices, published, learned))
}

/// The roof as the solar model sees it.
///
/// The tilt and the azimuth are the array's own and live on the asset; the
/// nameplate figures are the household's configuration. Reading the asset rather
/// than the settings for the geometry keeps one source of truth for what the
/// planner and the S2 description both describe.
fn array_of(site: &Site, settings: &crate::config::SiteSettings) -> hems_forecast::ArrayModel {
    let (tilt, azimuth) = site
        .assets
        .iter()
        .find_map(|a| match a {
            hems_core::prelude::Asset::Pv(pv) => Some((pv.tilt_deg, pv.azimuth_deg)),
            _ => None,
        })
        .unwrap_or((35.0, 180.0));
    hems_forecast::ArrayModel::new(
        Power::from_kw(settings.pv_kwp),
        Power::from_kw(settings.pv_ac_kw),
        tilt,
        azimuth,
    )
}

/// What every asset was last commanded, for the API.
pub type Commanded = BTreeMap<AssetId, Power>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Modul, Modul3Settings, Settings};

    const NOW: time::OffsetDateTime = time::macros::datetime!(2026-06-15 10:00:00 UTC);

    /// A calendar an installer would transcribe correctly: a three-hour
    /// Hochtarif, a Niedertarif that wraps past midnight, and two billed
    /// quarters.
    fn calendar() -> Modul3Settings {
        Modul3Settings {
            id: "NB-14A-3-2026".into(),
            year: 2026,
            hochtarif_minutes: [17 * 60, 20 * 60],
            niedertarif_minutes: [22 * 60, 6 * 60],
            billed_quarters: vec!["Q1".into(), "Q4".into()],
            ht_ct_per_kwh: 18.0,
            st_ct_per_kwh: 10.0,
            nt_ct_per_kwh: 4.0,
            source: Some("https://example-netz.de/preisblatt-2026.pdf".into()),
        }
    }

    fn on_modul_3(modul3: Option<Modul3Settings>) -> Settings {
        let mut settings = Settings::default();
        settings.tariff.modul = Modul::Modul3;
        settings.tariff.modul3 = modul3;
        settings
    }

    #[test]
    fn a_transcribed_calendar_that_conforms_lets_the_box_start() {
        check_modul3(&on_modul_3(Some(calendar())), NOW).expect("it conforms");
    }

    #[test]
    fn a_module_with_no_calendar_and_a_calendar_with_no_module_are_both_refused() {
        // Both halves or neither. A box told to bill in windows and given none
        // would price every hour the same and shift nothing, which is a
        // household paying for a module it is not getting; a calendar with no
        // module is one nothing reads, and therefore one nobody notices is
        // wrong.
        assert!(check_modul3(&on_modul_3(None), NOW).is_err());

        let mut orphan = Settings::default();
        orphan.tariff.modul = Modul::Modul1;
        orphan.tariff.modul3 = Some(calendar());
        assert!(check_modul3(&orphan, NOW).is_err());
    }

    #[test]
    fn a_hochtarif_under_two_hours_is_not_a_calendar_anybody_may_be_billed_on() {
        // The Anwendungshilfe's own floor. A box that started on this would
        // shift a household's load out of ninety minutes it is barely charged
        // extra for.
        let settings = on_modul_3(Some(Modul3Settings {
            hochtarif_minutes: [17 * 60, 18 * 60 + 30],
            ..calendar()
        }));
        let error = check_modul3(&settings, NOW).expect_err("ninety minutes is not two hours");
        assert!(
            format!("{error}").contains("HochtarifBelowTwoHours"),
            "{error}"
        );
    }

    #[test]
    fn one_billed_quarter_is_refused_because_the_rule_asks_for_two() {
        let settings = on_modul_3(Some(Modul3Settings {
            billed_quarters: vec!["Q1".into()],
            ..calendar()
        }));
        assert!(check_modul3(&settings, NOW).is_err());
    }

    #[test]
    fn a_quarter_that_is_not_one_of_the_four_is_a_typo_and_not_an_empty_list() {
        // Reporting a mistyped quarter as "the operator did not say" names the
        // wrong problem: `BilledQuartersUnknown` is a real finding about a real
        // gap, and a typo is neither.
        let settings = on_modul_3(Some(Modul3Settings {
            billed_quarters: vec!["Q1".into(), "Q5".into()],
            ..calendar()
        }));
        let error = check_modul3(&settings, NOW).expect_err("there is no Q5");
        assert!(format!("{error}").contains("Q5"), "{error}");
    }

    #[test]
    fn a_calendar_nobody_can_trace_to_a_document_is_refused() {
        let settings = on_modul_3(Some(Modul3Settings {
            source: None,
            ..calendar()
        }));
        assert!(check_modul3(&settings, NOW).is_err());
    }

    #[test]
    fn a_household_with_no_intelligent_metering_system_may_not_take_modul_3() {
        // § 1 of the Anwendungshilfe. The fact is a property of the *plant* and
        // is read from the site rather than declared a second time on the
        // tariff, so the two can never disagree.
        let mut settings = on_modul_3(Some(calendar()));
        settings.site.imsys_since = None;
        assert!(check_modul3(&settings, NOW).is_err());
    }
}
