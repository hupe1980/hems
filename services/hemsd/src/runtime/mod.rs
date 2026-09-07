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
use crate::drivers::{Attached, Registry};
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
    /// The building the planner would be given is not one it can integrate.
    ///
    /// A refusal, and the only one here that is about physics rather than
    /// paperwork: D17's whole argument is that the exact discretisation is a
    /// contraction at any step size, and a set of parameters for which it is not
    /// is a set for which the planner's own temperature predictions diverge.
    #[error("the building model is not one the planner can integrate: {0}")]
    Physics(String),
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
    /// The box's own measurement series, shared with the HTTP surface so the
    /// household can read the history its box took.
    pub series: Option<Arc<crate::series::Series>>,
    /// Each configured driver's registry identity, in the order `[[drivers]]`
    /// lists them.
    ///
    /// Kept rather than recomputed from the index: an asset may have two
    /// drivers now — one that commands it and one that measures it — so a
    /// transport that looked its driver up by asset would have run one of them
    /// and starved the other, and one that assumed registration order matched
    /// configuration order would depend on an ordering nothing declares.
    pub drivers: Vec<crate::drivers::DriverId>,
    /// Approving a Steuerbox on a running box, shared with the HTTP surface.
    ///
    /// `None` where this household has no EEBUS identity — a box with no § 14a
    /// driver has nothing to pair with.
    pub trust: Option<ship::Trust>,
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
pub fn assemble(
    settings: &Settings,
    kept: Option<&crate::store::Store>,
    now: time::OffsetDateTime,
) -> Result<Running, StartError> {
    let config = settings.site.household()?;
    let household = Household::build(&config).map_err(|e| StartError::Build(e.to_string()))?;

    let mut registry = Registry::new();
    let mut drivers = Vec::with_capacity(settings.drivers.len());
    for driver in &settings.drivers {
        let built = build_driver(driver, &household.site, kept, now)?;
        drivers.push(registry.register(built, &household.site)?);
    }
    registry.validate(&household.site, now)?;
    check_modul3(settings, now)?;
    check_the_physics(&settings.site, &config, &household)?;
    // The Berlin calendar date, through the same helper `classify_at` uses.
    // `[A1 3.1.b]`'s cutoff is 31.12.2023 and `[A1 10.1]`'s is 31.12.2028, and
    // both are German dates — a start-up line that disagreed with the decision
    // it is describing would be worse than no line at all.
    name_the_statutory_limits(&household, metering::calendar::local_day(now));

    Ok(Running {
        drivers,
        ski: None,
        overrides: overrides::Overrides::new(),
        series: None,
        trust: None,
        household,
        registry: Arc::new(Mutex::new(registry)),
        status: Arc::new(Mutex::new(Status::default())),
    })
}

/// The box's own two years, where it has a store to keep them in.
fn open_store(settings: &Settings) -> anyhow::Result<Option<Arc<Mutex<crate::store::Store>>>> {
    let Some(path) = &settings.store_path else {
        return Ok(None);
    };
    Ok(Some(Arc::new(Mutex::new(crate::store::Store::open(path)?))))
}

/// The box's measurement series, where the household keeps one.
///
/// Opened at start-up so a locked directory — which is what a second `hemsd` on
/// the same box looks like — is a start-up failure rather than a warning on
/// every tick for ever.
fn open_series(settings: &Settings) -> anyhow::Result<Option<Arc<crate::series::Series>>> {
    let Some(configured) = &settings.series else {
        return Ok(None);
    };
    Ok(Some(Arc::new(crate::series::Series::open(
        &configured.path,
        configured.keep_days,
    )?)))
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
        has_imsys: household
            .pv
            .is_some_and(|pv| pv.para9.imsys_since.is_some()),
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

/// Whether the house the planner is about to be given is a house it can plan.
///
/// Two checks, both on mechanisms that existed and were exercised by nothing but
/// their own unit tests, and both about the *building* rather than the wiring —
/// which is why they belong here beside the driver mismatches rather than in the
/// planner, where the answer would arrive once every five minutes for ever.
///
/// # The discretisation has to be a contraction
///
/// D17 rests on it: the exact zero-order hold is a contraction at **any** step
/// size, where explicit Euler at a quarter-hour step gives the air node's
/// eigenvalue the wrong sign and rings — and is only conditionally stable, with
/// nothing checking the condition. [`Rc2Discrete::is_contraction`] is that
/// property, and it is asserted here against the building the box was actually
/// **given** — the archetype the installer picked, or the four numbers they
/// typed. It cannot fail for physically valid parameters, which is exactly why
/// checking it is cheap and why a failure means the parameters are not
/// physical: a capacity or a resistance of zero, or a negative one.
///
/// # And the heat pump has to be able to heat the house
///
/// [`Rc2::steady_state_heat_kw`] is, in its own words, "the number that says
/// whether a heat pump is big enough for the house at all", and nothing asked
/// it. An undersized unit is **not** a refusal: it is a comfort problem the
/// planner already prices through the discomfort band, a bivalent system with a
/// Heizstab is an ordinary German fitting, and a box that would not start
/// because a cold snap might be uncomfortable would be refusing to manage the
/// household that needs managing most. So it is said once, loudly, at start-up —
/// where an installer is standing in front of it — with the heating rod counted
/// in, because that is what the unit can actually deliver.
fn check_the_physics(
    site: &crate::config::SiteSettings,
    config: &crate::site::HouseholdConfig,
    household: &Household,
) -> Result<(), StartError> {
    use hems_core::prelude::Asset;

    let building = config.building;
    // The planner's own step, which is the quarter hour every price, every
    // register and every plan in this workspace is written in.
    let step = hems_core::prelude::SLOT;
    if !building.discretise(step).is_contraction() {
        return Err(StartError::Physics(format!(
            "the building model does not contract over a {} minute step, so the \
             planner's own temperature predictions would diverge — check the \
             thermal parameters",
            step.whole_minutes()
        )));
    }

    // What it takes to hold the bottom of the comfort band at the design
    // outdoor temperature. `NORM_AUSSENTEMPERATUR_C` is the conservative end of
    // DIN EN 12831-1's German range; the exact figure is per location and an
    // installer who has it can read this warning against their own.
    let Some(heat_pump) = household
        .heat_pump
        .as_ref()
        .and_then(|id| household.site.asset(id))
    else {
        return Ok(());
    };
    let Asset::HeatPump(unit) = heat_pump else {
        return Ok(());
    };
    let needed = building.steady_state_heat_kw(site.comfort_min_c, NORM_AUSSENTEMPERATUR_C);
    // Electrical, through the coefficient of performance the unit would have at
    // that temperature — a 5 kW compressor at a COP of 2,48 delivers 12,4 kW of
    // heat.
    //
    // The **Heizstab is not multiplied by it**, and that is not a rounding
    // matter: a resistive element puts one kilowatt of heat in for one kilowatt
    // of electricity, by construction. `group_power()` is the Fallgruppe's
    // summed *electrical* power — the right number for `[A1 2.4.1.b]` and the
    // wrong one to hand a coefficient of performance — and putting the whole of
    // it through the COP credited an ordinary 3 kW rod with 7,4 kW of heat at
    // −12 °C, which is more than the entire design load of the average house.
    // The warning could then not fire for any household anybody would install.
    let cop = hems_core::prelude::CopCurve::air_source().at(NORM_AUSSENTEMPERATUR_C);
    let rod_kw = unit.heating_rod.unwrap_or(Power::ZERO).kw();
    let deliverable = unit.electrical_nominal.kw() * cop + rod_kw;
    if deliverable < needed {
        tracing::warn!(
            needed_kw = format!("{needed:.1}"),
            deliverable_kw = format!("{deliverable:.1}"),
            outdoor_c = NORM_AUSSENTEMPERATUR_C,
            "this heat pump cannot hold the comfort band at the design outdoor \
             temperature, heating rod included — the plan will be honest about it \
             and price the shortfall as discomfort, but the house will be cold on \
             the coldest days"
        );
    }
    Ok(())
}

/// Say out loud which statutory regime each declaration produced.
///
/// # Why this is a log line and not a check
///
/// Nothing here can be *wrong* in a way software can detect: a commissioning
/// date is a fact off a Netzanschlussportal record, and a box has no way to
/// verify one. What it can do is state the consequence in the words the
/// paperwork uses, once, at start-up, where an installer is standing in front of
/// it — because the consequence is not obvious from the date and it is expensive
/// in both directions.
///
/// A roof commissioned in the window 01.01.2023–24.02.2025 is capped at
/// **nothing** by § 100 Abs. 3b EEG, and a box that had been left on the default
/// would curtail it at 60 % every sunny midday for the life of the installation.
/// A heat pump commissioned in 2019 on the old reduced network fee is on
/// `[A1 10.1]` until 31.12.2028, and treating it as a new SteuVE hands the
/// network operator a share of its power it may not reduce *and* counts its
/// consumption as netzwirksamer Leistungsbezug when it is ordinary load.
///
/// So: one line per controllable device, one for the roof, and the installer can
/// read them against the folder in their hand.
fn name_the_statutory_limits(household: &Household, today: time::Date) {
    use hems_core::prelude::Asset;

    for asset in &household.site.assets {
        let meta = asset.meta();
        if matches!(asset, Asset::Meter(_) | Asset::Load(_)) {
            continue;
        }
        let participation = hems_grid::para14a::participation(
            meta.commissioned_at,
            meta.steuve_exemption,
            meta.legacy_status,
            meta.switched_voluntarily,
        );
        tracing::info!(
            asset = %meta.id,
            commissioned = ?meta.commissioned_at,
            participation = ?participation,
            controlled = participation.is_controlled_on(today),
            "§ 14a"
        );
    }

    let Some(profile) = hems_grid::para9::GenerationProfile::of_site(&household.site) else {
        return;
    };
    if let Some(limit) = profile.statutory_limit() {
        tracing::info!(
            limit = ?limit,
            ceiling_kw = profile.statutory_cap().map(|p| format!("{:.2}", p.kw())),
            commissioned = ?profile.commissioned_at,
            "§ 9 EEG: this roof's feed-in is capped by statute"
        );
    } else {
        tracing::info!(
            commissioned = ?profile.commissioned_at,
            "§ 9 EEG: no statutory feed-in cap applies to this roof"
        );
    }
}

/// The design outdoor temperature the heating check is made at, °C.
///
/// −12 °C is the conservative end of the German range in DIN EN 12831-1: the
/// per-location Norm-Außentemperatur runs from about −10 on the coast to −16 in
/// the Alps. A single figure is right for a *warning* — an installer with the
/// table for their postcode can read the two numbers against their own — and
/// wrong for anything that refused to start.
const NORM_AUSSENTEMPERATUR_C: f64 = -12.0;

/// The `direction` column a § 14a failsafe is stored under.
///
/// A literal in one place rather than at the two call sites: a box that wrote
/// its failsafe under one spelling and looked for it under another would look,
/// from every screen, exactly like a box no operator had ever written to.
pub const FAILSAFE_CONSUMPTION: &str = "consumption";

/// [`assemble`], with the box's own record behind a lock.
///
/// # Errors
/// [`StartError`], as [`assemble`].
async fn assembled(
    settings: &Settings,
    store: Option<&Arc<Mutex<crate::store::Store>>>,
    now: time::OffsetDateTime,
) -> Result<Running, StartError> {
    let kept = match store {
        Some(store) => Some(store.lock().await),
        None => None,
    };
    assemble(settings, kept.as_deref(), now)
}

/// A device read through the vendor's own register map.
///
/// Split out because it is the one driver whose whole configuration is a *list*
/// — every other kind takes an address and a handful of scalars — and the list
/// is what an installer transcribes from a PDF.
fn register_map(
    settings: &crate::config::RegisterSettings,
    asset: AssetId,
) -> Result<Box<dyn hems_drv::Driver + Send>, StartError> {
    let millis = |ms: u64, fallback: i64| {
        time::Duration::milliseconds(i64::try_from(ms).unwrap_or(fallback))
    };
    let driver = hems_drv::modbus::registers::Registers::new(
        asset,
        settings.unit,
        hems_drv::modbus::Cadence {
            poll: millis(settings.poll_ms, 1_000),
            timeout: millis(settings.timeout_ms, 5_000),
        },
        settings.points.clone(),
    )
    .map_err(|e| StartError::Driver {
        asset: settings.asset.clone(),
        detail: e.to_string(),
    })?;
    Ok(Box::new(driver))
}

/// The failsafe a Controllable System comes up holding.
///
/// What the operator wrote wins over what the file says, because `[LPC-021]`
/// makes this theirs to change and §2.15 of the implementation guide makes
/// accepting the change mandatory. A box that came back from a power cut on its
/// own configuration would have quietly undone it —
/// `ATC_LPC_COM_PT_CSInit_003`, and a household restrained to the wrong number
/// with nobody talking to it.
///
/// **Public because it is the thing under test.** Two device-level conformance
/// cases are questions about exactly this function: `CSInit_003` asks that an
/// operator's write survives a power cut, and `CSInit_002` asks that a factory
/// reset puts the declared defaults back — which here is the *fallback* arm,
/// reached because [`crate::store::Store::factory_reset`] leaves no written
/// value. `tests/conformance.rs` calls it rather than reimplementing the
/// precedence, because a harness that decided the answer for itself would be
/// testing the harness.
pub fn failsafe_in_force(
    kept: Option<&crate::store::Store>,
    configured: Power,
    configured_for: std::time::Duration,
) -> (Power, std::time::Duration) {
    let written = kept.and_then(|store| match store.eebus_failsafe(FAILSAFE_CONSUMPTION) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "the operator's failsafe could not be read");
            None
        }
    });
    let Some(written) = written else {
        return (configured, configured_for);
    };
    tracing::info!(
        watts = written.watts,
        seconds = written.minimum_s,
        "the failsafe the network operator wrote is back in force"
    );
    (
        Power::new(written.watts),
        std::time::Duration::from_secs(written.minimum_s.unsigned_abs()),
    )
}

/// How this box names itself to an EEBUS peer.
///
/// One SPINE device address for every dialled device, because the SKI follows
/// the key: a box that named itself differently per driver would be several
/// devices on its own network, all but one of which an installer has never been
/// shown (D136).
fn spine_identity(vendor: Option<&str>, unique: Option<&str>) -> hems_drv::eebus::SpineIdentity {
    let default = hems_drv::eebus::SpineIdentity::default();
    hems_drv::eebus::SpineIdentity {
        vendor: vendor.map_or(default.vendor, str::to_owned),
        unique: unique.map_or(default.unique, str::to_owned),
    }
}

/// One configured driver, built.
///
/// `kept` is what a network operator has already written to this box and the
/// box wrote down — today only the § 14a failsafe, which is the one value in
/// the exchange that has to survive a power cut.
fn build_driver(
    settings: &DriverSettings,
    site: &Site,
    kept: Option<&crate::store::Store>,
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
            let configured = s.failsafe_kw.map_or_else(
                || {
                    hems_grid::para14a::minimum_power(
                        &hems_grid::classify_at(&site.assets, now),
                        hems_grid::para14a::ControlMode::Ems,
                    )
                    .max(hems_grid::para14a::MINDESTLEISTUNG)
                },
                Power::from_kw,
            );
            let (failsafe, failsafe_for) = failsafe_in_force(
                kept,
                configured,
                std::time::Duration::from_secs(s.failsafe_hours.saturating_mul(3600)),
            );
            let identity = spine_identity(s.spine_vendor.as_deref(), s.spine_unique.as_deref());
            let driver = hems_drv::eebus::Lpc::with_identity(
                asset(&s.asset)?,
                hems_drv::eebus::Use::Lpc,
                failsafe,
                failsafe_for,
                now,
                &identity,
            )
            .map_err(|e| StartError::Driver {
                asset: s.asset.clone(),
                detail: e.to_string(),
            })?;
            Ok(Box::new(driver))
        }
        DriverSettings::EebusEv(s) => {
            let identity = spine_identity(s.spine_vendor.as_deref(), s.spine_unique.as_deref());
            let driver = hems_drv::eebus_ev::EvCharger::new(asset(&s.asset)?, now, &identity)
                .map_err(|e| StartError::Driver {
                    asset: s.asset.clone(),
                    detail: e.to_string(),
                })?;
            Ok(Box::new(driver))
        }
        DriverSettings::EebusDhw(s) => {
            let identity = spine_identity(s.spine_vendor.as_deref(), s.spine_unique.as_deref());
            let driver = hems_drv::eebus_dhw::DhwTank::new(asset(&s.asset)?, now, &identity)
                .map_err(|e| StartError::Driver {
                    asset: s.asset.clone(),
                    detail: e.to_string(),
                })?;
            Ok(Box::new(driver))
        }
        DriverSettings::Registers(s) => register_map(s, asset(&s.asset)?),
        DriverSettings::EebusHeatPump(s) => {
            let identity = spine_identity(s.spine_vendor.as_deref(), s.spine_unique.as_deref());
            let driver = hems_drv::eebus_heat_pump::HeatPump::new(asset(&s.asset)?, now, &identity)
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
        DriverSettings::Registers(s) => Some(s.address.clone()),
        // Both EEBUS kinds, and for the same reason in opposite directions: the
        // session is TLS with mutual authentication under a WebSocket under a
        // SHIP handshake, and it is `runtime::ship`'s rather than the
        // byte-pump's — whether this box accepts it or opens it.
        DriverSettings::EebusLpc(_)
        | DriverSettings::EebusDhw(_)
        | DriverSettings::EebusEv(_)
        | DriverSettings::EebusHeatPump(_) => None,
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

    // The box's own store, where its two years and its learning live. Opening it
    // is allowed to fail loudly: a household configured for a record it cannot
    // keep is one whose Nachweis will be missing on the day it is asked for.
    //
    // Before the drivers, because one of them needs it: the § 14a failsafe the
    // network operator wrote is kept here, and a Controllable System built from
    // the configuration file alone would come back from a power cut having
    // undone it.
    let store = open_store(settings)?;
    let mut running = assembled(settings, store.as_ref(), now).await?;

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

    let sessions = start_drivers(settings, &running, health, shutdown)?;

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

    // One identity for both directions. The SKI follows the key, and a box that
    // dialled a heat pump under a second key would be two devices on its own
    // network — one of which an installer has never been shown.
    if sessions.listening.is_some() || !sessions.dialled.is_empty() {
        let now = time::OffsetDateTime::now_utc();
        let (node, ski, key_pem) = ship::identity(&settings.ship, store.as_ref(), now).await?;
        let node = Arc::new(node);
        if let Some(on) = sessions.listening.clone() {
            start_ship(settings, &running, &node, ski, on, health, shutdown).await?;
        }
        start_dialled(settings, &running, &node, &sessions.dialled, shutdown)?;
        running.ski = Some(ski.to_display_string());
        running.trust = Some(ship::Trust::new(
            Arc::clone(&node),
            store.clone(),
            settings.ship.ship_id.clone(),
            key_pem,
        ));
    }

    let overrides = overrides::Overrides::new();
    running.overrides = overrides.clone();
    running.series = open_series(settings)?;

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

    // The one task this box cannot do its job without. A control loop that has
    // panicked leaves a process answering every request and managing nothing —
    // no guard, no arbiter, no evidence record — and `/livez` is what an
    // orchestrator restarts on.
    health.vital(
        "control",
        shutdown.clone(),
        control::run(
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
                heat_pump: running.household.heat_pump.clone(),
                modelled_pv: published.modelled_pv.clone(),
                outdoor: published.outdoor.clone(),
                bands: published.bands.clone(),
            },
            control::Live {
                registry: Arc::clone(&running.registry),
                status: Arc::clone(&running.status),
                plan,
                prices,
                learned,
                store,
                series: running.series.clone(),
                overrides: overrides.clone(),
            },
            settings.control.clone(),
            health.clone(),
            shutdown.clone(),
        ),
    );

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
) -> Result<Sessions, StartError> {
    let mut unreachable = Vec::new();
    let mut dialled = Vec::new();
    let mut eebus_asset = None;
    for (driver, id) in settings.drivers.iter().zip(running.drivers.iter().copied()) {
        let asset = AssetId::new(driver.asset()).map_err(|e| StartError::Driver {
            asset: driver.asset().to_string(),
            detail: e.to_string(),
        })?;
        if let Some(address) = transport_address(driver) {
            tokio::spawn(transport::tcp(
                Arc::clone(&running.registry),
                Attached { driver: id, asset },
                address,
                shutdown.clone(),
            ));
        } else if matches!(driver, DriverSettings::EebusLpc(_)) && settings.ship.listen.is_some() {
            // The § 14a session. Started once, below, because the identity has
            // to be created before anything can accept on it.
            eebus_asset = Some(Attached { driver: id, asset });
        } else if matches!(
            driver,
            DriverSettings::EebusDhw(_)
                | DriverSettings::EebusEv(_)
                | DriverSettings::EebusHeatPump(_)
        ) {
            // Dialled rather than accepted, and started with the § 14a session
            // for the same reason: it needs the box's own identity, which is
            // created once.
            dialled.push(Attached { driver: id, asset });
        } else {
            // Still given its clock. Every transition out of the LPC machine's
            // first state is a timer, so a Controllable System nobody ticks sits
            // in `Init` for ever — holding the household at its § 14a minimum on
            // the strength of an implementation accident, and reporting a state
            // it is not really in. See `transport::clock_only`.
            tokio::spawn(transport::clock_only(
                Arc::clone(&running.registry),
                id,
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
    Ok(Sessions {
        listening: eebus_asset,
        dialled,
    })
}

/// The EEBUS sessions that could not be started with the other transports.
///
/// Both need the box's own identity, which is created once and after the
/// drivers are registered: a key that a driver had already used would be a
/// second identity on the same box.
struct Sessions {
    /// The § 14a Controllable System, where one is configured with a listener.
    listening: Option<Attached>,
    /// Devices on the household's own network this box dials.
    dialled: Vec<Attached>,
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
    node: &Arc<eebus::runtime::Node>,
    ski: eebus::ship::Ski,
    on: Attached,
    health: &Health,
    shutdown: &Shutdown,
) -> anyhow::Result<()> {
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
        Arc::clone(node),
        listener,
        Arc::clone(&running.registry),
        on,
        settings.ship.clone(),
        ski,
        shutdown.clone(),
    ));
    Ok(())
}

/// Start one dialling task per EEBUS device on the household's own network.
///
/// The § 14a session is the one this box *accepts*; these are the ones it opens.
/// They share the box's identity, because the SKI follows the key and a box that
/// dialled under a second one would be two devices on its own network — one of
/// which an installer has never been shown.
fn start_dialled(
    settings: &Settings,
    running: &Running,
    node: &Arc<eebus::runtime::Node>,
    assets: &[Attached],
    shutdown: &Shutdown,
) -> anyhow::Result<()> {
    for driver in &settings.drivers {
        let (name, address, ski) = match driver {
            DriverSettings::EebusDhw(s) => (&s.asset, &s.address, &s.ski),
            DriverSettings::EebusEv(s) => (&s.asset, &s.address, &s.ski),
            DriverSettings::EebusHeatPump(s) => (&s.asset, &s.address, &s.ski),
            _ => continue,
        };
        let asset = AssetId::new(name).map_err(|e| StartError::Driver {
            asset: name.clone(),
            detail: e.to_string(),
        })?;
        let Some(on) = assets.iter().find(|a| a.asset == asset) else {
            continue;
        };
        let peer: eebus::ship::Ski = ski
            .parse()
            .map_err(|_| ship::ShipError::NotASki(ski.clone()))?;
        tracing::info!(
            %address,
            peer = %peer.to_display_string(),
            %asset,
            "dialling an EEBUS device"
        );
        tokio::spawn(ship::dial(
            Arc::clone(node),
            address.clone(),
            peer,
            Arc::clone(&running.registry),
            on.clone(),
            shutdown.clone(),
        ));
    }
    Ok(())
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
        // The outdoor temperature the plan was made against, so the control
        // loop teaches the building from the same series (D117 again, for the
        // house rather than the roof).
        outdoor: Arc::new(tokio::sync::RwLock::new(BTreeMap::new())),
    };
    // What the box remembered from before it was restarted. A fortnight of
    // observations is what makes a forecast worth having, and relearning it
    // every reboot is the difference between a box that plans on its first
    // evening and one that does not.
    // The building the installer described is the *prior*; a record the box has
    // already fitted from its own thermometer wins over it, which is what
    // `restored` decides. Configuring one therefore helps a new box and never
    // overrides a box that has learned better.
    let building = settings.site.building.rc2()?;
    let learned = Arc::new(Mutex::new(match &store {
        Some(store) => {
            planner::Learned::restored(&*store.lock().await, settings.site.bundesland, building)
        }
        None => planner::Learned::new(settings.site.bundesland, building),
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

    // The other half of the same seam, and it was written and never asked. A
    // household on a **fixed** tariff legitimately has no `tariffd`: there is no
    // day-ahead curve to optimise against, and shifting load for a spread the
    // household is not charged is not a saving. A household with *neither* is a
    // different thing — it plans against `fallback_ct_per_kwh`, which is a flat
    // number, and a flat price makes the plan indifferent about *when* to act.
    // That is the whole economic case for a planner, gone, with a box that looks
    // from every screen exactly like one that is optimising.
    //
    // A warning rather than a refusal, for the reason the fallback exists: the
    // plan is still right about the roof, the battery and the § 14a ceiling, and
    // a box that would not plan at all would be worse.
    if !fleet.has_prices() && settings.tariff.fixed_ct_per_kwh.is_none() {
        tracing::warn!(
            fallback_ct_per_kwh = settings.tariff.fallback_ct_per_kwh,
            "no `tariffd` is configured and no fixed tariff is set, so every hour \
             costs the same fallback price: the plan will be right about the roof, \
             the battery and the § 14a ceiling and will not shift a kilowatt-hour \
             for a spread it cannot see. Set `[fleet] tariffd_url` for a dynamic \
             tariff, or `[tariff] fixed_ct_per_kwh` if the household is on a fixed \
             one"
        );
    }

    health.bad("planner", "no plan has been produced yet");
    tokio::spawn(planner::run(
        planner::Planner {
            household: running.household.clone(),
            array: array_of(&running.household.site, &settings.site),
            tariff: settings.tariff.clone(),
            control: settings.control.clone(),
            wear_eur_per_kwh: settings.site.battery_wear_eur_per_kwh,
            // The one charge point that can say whether a car is on it. A
            // second would be two departure times for one household, and a
            // household has one car park rather than one per driver.
            charging: settings.drivers.iter().find_map(|d| match d {
                DriverSettings::EebusEv(s) => Some(s.clone()),
                _ => None,
            }),
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
///
/// A site with no roof cannot reach here — the planner is only built where there
/// is one — so the fallback is a flat, south-facing plane rather than a second
/// copy of the reference household's pitch. A constant that is *also* the
/// default is the one that hides a broken chain: it was 35°/180° here, and for
/// as long as the asset carried the same pair it was impossible to tell whether
/// this line was reading the configuration or ignoring it.
fn array_of(site: &Site, settings: &crate::config::SiteSettings) -> hems_forecast::ArrayModel {
    let (tilt, azimuth) = site
        .assets
        .iter()
        .find_map(|a| match a {
            hems_core::prelude::Asset::Pv(pv) => Some((pv.tilt_deg, pv.azimuth_deg)),
            _ => None,
        })
        .unwrap_or((0.0, 180.0));
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
        settings.site.para9.imsys_since = None;
        assert!(check_modul3(&settings, NOW).is_err());
    }
}

#[cfg(test)]
mod physics_tests {
    use super::*;
    use crate::config::{BuildingSettings, SiteSettings};
    use hems_core::prelude::{BuildingClass, Rc2};

    fn built(settings: &SiteSettings) -> (crate::site::HouseholdConfig, Household) {
        let config = settings.household().expect("a household");
        let household = Household::build(&config).expect("a site");
        (config, household)
    }

    #[test]
    fn the_reference_household_passes_its_own_physics_check() {
        // A gate that refuses everything is not a gate, and a gate that accepts
        // everything is not one either — so the pair is the test, and
        // `a_building_that_is_not_physical_is_refused` is the other half.
        let settings = SiteSettings::default();
        let (config, household) = built(&settings);
        assert!(check_the_physics(&settings, &config, &household).is_ok());
    }

    #[test]
    fn a_building_that_is_not_physical_is_refused() {
        // The half that could not previously happen: the check ran against
        // `Rc2::house()`, a constant, so it was a gate in front of an open door.
        // A `HouseholdConfig` assembled by hand — which is what the reference-day
        // harness and any embedder do — can carry parameters `BuildingSettings`
        // would have rejected, and this is where they are caught.
        let settings = SiteSettings::default();
        let (mut config, household) = built(&settings);
        config.building = Rc2 {
            r_air_out_k_per_kw: 0.0,
            ..Rc2::house()
        };
        assert!(matches!(
            check_the_physics(&settings, &config, &household),
            Err(StartError::Physics(_))
        ));
    }

    #[test]
    fn every_archetype_is_a_contraction_at_every_step_size() {
        // D17's property, over the whole span an installer can pick rather than
        // over one constant. It is what makes the exact zero-order hold safe at
        // a quarter-hour step where explicit Euler gives the air node's
        // eigenvalue the wrong sign, rings after every change of heat input, and
        // diverges outright at a flat's air capacity — which is exactly the
        // archetype that would otherwise have gone unchecked.
        for class in [
            BuildingClass::Average,
            BuildingClass::NewBuild,
            BuildingClass::SolidWall,
            BuildingClass::Apartment,
        ] {
            for minutes in [1_i64, 5, 15, 60, 240] {
                let d = class.rc2().discretise(time::Duration::minutes(minutes));
                assert!(
                    d.is_contraction(),
                    "{class:?} is not a contraction at {minutes} minutes"
                );
            }
        }
    }

    #[test]
    fn a_household_with_no_heat_pump_has_no_heating_to_check() {
        // The check reads the *asset*, so a household that declared none has
        // nothing to warn about rather than a zero-kilowatt unit that fails.
        let settings = SiteSettings {
            heat_pump_kw: 0.0,
            ..SiteSettings::default()
        };
        let (config, household) = built(&settings);
        assert!(household.heat_pump.is_none());
        assert!(check_the_physics(&settings, &config, &household).is_ok());
    }

    #[test]
    fn the_heating_check_now_depends_on_which_house_it_is() {
        // The reference 5 kW unit plus its 3 kW rod holds the average house at
        // −12 °C and cannot hold an unretrofitted solid-wall one: 32 K over
        // 2,5 K/kW is 12,8 kW of loss against 8 kW at a coefficient of 2,48.
        // Before the building was configurable both answers were the same
        // number, which is what made this warning unable to fire.
        let average = SiteSettings::default().building.rc2().unwrap();
        let solid = BuildingSettings {
            class: BuildingClass::SolidWall,
            ..BuildingSettings::default()
        }
        .rc2()
        .unwrap();
        // 5 kW of compressor at a COP of 2,48 plus 3 kW of resistive rod at 1,0
        // is 15,4 kW of heat. The average house asks 5,3 kW of that at −12 °C
        // and the solid-wall one 12,8 — so the reference unit covers both, and
        // the *bigger* house that does not fit is a 1970s one with the 3 kW
        // unit an installer sizes from a heat-load calculation somebody did for
        // the retrofit rather than for the building as it stands.
        let cop = hems_core::prelude::CopCurve::air_source().at(NORM_AUSSENTEMPERATUR_C);
        let small = 3.0 * cop + 3.0;
        assert!(
            average.steady_state_heat_kw(20.0, NORM_AUSSENTEMPERATUR_C) < small,
            "a 3 kW unit holds the average house"
        );
        assert!(
            solid.steady_state_heat_kw(20.0, NORM_AUSSENTEMPERATUR_C) > small,
            "and cannot hold an unretrofitted solid-wall one"
        );

        // …and the rod is worth its own kilowatts and no more. Crediting it with
        // the compressor's coefficient — which is what `group_power() * cop`
        // did — inflates a 3 kW element to 7,4 kW of heat.
        assert!(
            ((3.0 + 3.0) * cop - small - 3.0 * (cop - 1.0)).abs() < 1e-9,
            "the difference is exactly (COP − 1) × the rod"
        );
    }
}
