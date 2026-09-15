//! Building the household the daemon manages.

use hems_core::asset::{
    AssetMeta, Battery, Capabilities, DhwTank, Evse, FlexibleLoad, HeatPump, HeatPumpControl,
    LegacyStatus, LoadKind, Programme, PvArray, SteuVeExemption,
};
use hems_core::prelude::*;
use hems_optimizer::model::{PlanningLimits, SteuVeDevices, TimedLimit};
use hems_optimizer::solve::AssetNames;
use hems_realtime::guard::GridLimits;
use hems_tariff::levies::Levies;
use hems_tariff::tariff::{EnergyPrice, FeedIn, NetworkCharge, Tariff};
use rust_decimal::Decimal;
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// The roof, where the household has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PvConfig {
    /// Installed photovoltaic power.
    pub kwp: Power,
    /// The inverter's alternating-current limit.
    pub ac_nominal: Power,
    /// How far the modules are tilted from horizontal, degrees.
    ///
    /// Zero is flat, 90 a wall. It and [`PvConfig::azimuth_deg`] are the whole
    /// of the geometry the clear-sky model has, and they are what decides *when*
    /// the roof produces rather than how much.
    pub tilt_deg: f64,
    /// Which way the modules face, degrees clockwise from north — 180 is due
    /// south, 90 east, 270 west.
    ///
    /// # Why this is not a constant
    ///
    /// It was one, for every household, at due south: the box modelled an ideal
    /// roof and let the residual corrector absorb the difference. That works
    /// for a *level* error and not for a **shape** one. An east–west array
    /// produces two shoulders where the model predicts one midday peak, and the
    /// corrector — a multiplicative ratio per hour of the day, bounded at 3 —
    /// has to learn a factor near its own bound in the morning and near its
    /// floor at noon, separately for every season, before it can say so. Until
    /// it has, the plan charges the battery from a sun that is not there and
    /// leaves the evening short.
    ///
    /// A number an installer reads off the roof in ten seconds is not a thing to
    /// learn from a fortnight of meter readings.
    pub azimuth_deg: f64,
    /// The § 9 EEG facts declared about the roof.
    ///
    /// The default is the realistic pair rather than the tidy one: an
    /// intelligent metering system fitted years ago, so § 51 EEG has been taking
    /// the negative quarter hours since the end of that year — and the 60 %
    /// feed-in cap still on, because § 9 Abs. 2 lifts it only after the network
    /// operator's first successful Ansteuerbarkeit test, which is a different
    /// event on a different clock and has not happened.
    pub para9: Para9Status,
}

/// The stationary battery, where the household has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryConfig {
    /// Usable capacity.
    pub kwh: Energy,
    /// Power, both directions.
    pub power: Power,
    /// Energy held back for a power cut.
    pub reserve_soc: Soc,
    /// What a kilowatt-hour of throughput costs in wear, €/kWh.
    pub wear_eur_per_kwh: f64,
}

/// The charge point, where the household has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvseConfig {
    /// The largest current per conductor, amperes.
    pub max_current: Current,
    /// Whether the charge point can drop to a single conductor.
    ///
    /// Almost every wallbox sold in Germany since about 2022 can, and it is what
    /// decides whether 2 kW of surplus charges a car or is exported: three-phase
    /// charging cannot start below 4,14 kW, single-phase below 1,38 kW.
    pub switchable: bool,
    /// Whether the charge point can discharge the car into the house.
    ///
    /// It is a fact about the **hardware on the wall**, not about the cable:
    /// what the wallbox and the car negotiate between themselves is ISO 15118,
    /// which this box never sees. What it changes here is what a manager may be
    /// told the wallbox can do — a `EnergyProducer` role, a discharge operation
    /// mode in the S2 description, and an envelope whose floor is negative — and
    /// therefore what the planner is allowed to ask for.
    pub bidirectional: bool,
    /// The state of charge the household asked its car to reach.
    ///
    /// `None` means "fill it". It is only ever read by the real-time fallback —
    /// the planner is given an energy target and a departure, which say the same
    /// thing more precisely — but that is the mode the fallback is about, and a
    /// surplus tracker with no notion of *enough* charges past the limit in
    /// preference to exporting.
    pub charge_limit: Option<Soc>,
}

/// The heat pump and the comfort band it owes, where the household has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatPumpConfig {
    /// Electrical power at full output.
    pub power: Power,
    /// Whether the unit modulates rather than switching on and off.
    pub modulating: bool,
    /// The bottom of the comfort band, °C.
    pub comfort_min_c: f64,
    /// The top of the comfort band, °C.
    pub comfort_max_c: f64,
    /// Electrical power in **cooling** mode, where the unit is reversible.
    ///
    /// `None` is a heating-only unit. A reversible one is what most new German
    /// installations are since the GEG made a heat pump the default heating
    /// system in January 2026 — the hardware is a four-way valve and the KfW
    /// subsidy covers the function automatically — so this is a fact an installer
    /// reads off the unit rather than a preference (D202).
    pub cooling_electrical: Option<Power>,
    /// How the unit takes instructions.
    ///
    /// A ceiling by default, because every § 14a heat pump can be told to use
    /// less and only some can be told to start. See [`HeatPumpControl`].
    pub control: HeatPumpControl,
}

/// The hot-water tank, where the household has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DhwConfig {
    /// Volume, litres.
    pub litres: f64,
    /// Electrical power of the heater.
    pub heater: Power,
    /// Thermal kilowatt-hours delivered per electrical kilowatt-hour.
    ///
    /// One for an immersion heater, around three for a hot-water heat pump. It
    /// is the difference between a tank that costs a kilowatt-hour to fill and
    /// one that costs three, so a household fitted with the first and modelled
    /// as the second has its hot water priced at a third of what it pays.
    pub cop: f64,
    /// Standing loss — the reason a tank left alone is cold in the morning.
    pub standing_loss: Power,
    /// Lowest acceptable temperature, °C. Below it the household has a cold
    /// shower, which the plan pays for rather than forbidding.
    pub t_min_c: f64,
    /// The temperature the household's **own thermostat** holds, °C.
    ///
    /// Not a bound on the plan — the plan works between `t_min_c` and `t_max_c`
    /// and uses the tank as a store. This is what the household would have
    /// without a manager, so it is what the **baseline** is held at, and
    /// therefore what the saving is measured against (D183).
    pub t_set_c: f64,
    /// Highest safe temperature, °C — a scald bound, not a target.
    pub t_max_c: f64,
}

/// The identifiers this daemon gives the household's assets.
///
/// Every asset the site model can hold, in one list, because three things index
/// by it: [`HouseholdConfig::declared`], the `[[drivers]]` list's `asset` key,
/// and `run --check`. A name that existed in two of the three and not the third
/// is a declaration an installer wrote and nothing ever read.
pub const REFERENCE_ASSETS: &[&str] = &[
    "pv",
    "battery",
    "wallbox",
    "waermepumpe",
    "warmwasser",
    "spuelmaschine",
    "haushalt",
    "netzanschluss-zaehler",
];

/// When the reference household's intelligent metering system was fitted.
///
/// Every § 14a household has one — the Steuerungseinrichtung a network operator
/// writes limits through comes with it — so § 51 EEG has been taking the
/// negative quarter hours since the start of 2025.
pub const REFERENCE_IMSYS_SINCE: time::Date = time::macros::date!(2024 - 06 - 01);

/// When the reference household was commissioned.
///
/// After 25.02.2025, so § 9 Abs. 2 caps its roof at 60 %, and after 31.12.2023,
/// so `[A1 3.1.b]` makes every controllable device mandatory. Both are what the
/// reference days are measured under.
pub const REFERENCE_COMMISSIONING: time::Date = time::macros::date!(2025 - 03 - 01);

/// What an installer declares about one asset, and no datasheet carries.
///
/// # Why every one of these has to be asked rather than assumed
///
/// Two statutes read a commissioning date and reach opposite conclusions from
/// silence, and both of them decide money.
///
/// **§ 9 EEG** ties all three of its feed-in limitations to one: a system
/// commissioned from 25.02.2025 is capped at 60 %, one from the window
/// 01.01.2023–24.02.2025 is capped at nothing at all by § 100 Abs. 3b, and one
/// from before 2023 is at 70 % only if that is how it met its obligation. A box
/// that assumed the newest of those would curtail a 2024 roof that the statute
/// never reached — every sunny midday, for the life of the installation.
///
/// **§ 14a EnWG** goes the other way: `[A1 3.1.b]` binds devices commissioned
/// after 31.12.2023, and `[A1 10]` leaves an older one either out of scope, on
/// the old reduced network fee until 31.12.2028, or — a Nachtspeicherheizung —
/// on it indefinitely. Treating such a device as a controllable one hands the
/// network operator a share of its power it has no right to reduce *and* counts
/// its consumption as netzwirksamer Leistungsbezug when it is ordinary load.
///
/// So the facts are configured, defaulted to the reference household's, and
/// named at start-up — see `crate::runtime::check_the_declarations` — because an
/// installer standing in a cellar is the only person who can correct them and
/// the last person who will ever be asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Declared {
    /// The date of technical commissioning.
    ///
    /// `None` leaves the device **in** the § 14a group (`[A1 3.1.b]`, the safe
    /// direction there) and **outside** every § 9 EEG limitation (the safe
    /// direction here) — see `hems_grid::para14a::participation` and
    /// `hems_grid::para9::GenerationProfile::statutory_limit`.
    pub commissioned_at: Option<time::Date>,
    /// What the device brings with it from before 2024, `[A1 10]`.
    pub legacy_status: LegacyStatus,
    /// Whether the operator moved it into the netzorientierte Steuerung
    /// voluntarily, `[A1 10.4]` — irreversible, and the operator may not refuse.
    pub switched_voluntarily: bool,
    /// Why it is not a steuerbare Verbrauchseinrichtung at all, `[A1 3.1.b]`.
    pub exemption: Option<SteuVeExemption>,
}

/// What the connection agreement says, beyond the fuse in the cupboard.
///
/// All optional, and all of it paperwork rather than measurement: the numbers
/// are on the § 14a agreement and the Netzanschlussvertrag.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConnectionConfig {
    /// The contractually agreed connection power, where one is agreed.
    ///
    /// It narrows three things at once and none of them is cosmetic: the
    /// guard's physical headroom, the planner's import ceiling, and the
    /// `ContractualConsumptionNominalMax` an EEBUS Energy Guard is told
    /// `[LPC-042]`. A box that left it unset told a network operator its only
    /// limit was a 35 A fuse.
    pub contract_power: Option<Power>,
    /// The market location, as it appears on the supplier's invoice.
    pub malo: Option<MaloId>,
    /// The metering location.
    pub melo: Option<MeloId>,
    /// The network operator's BDEW code number, from the § 14a agreement.
    pub dso_code: Option<String>,
    /// The Netzbereich the operator has assigned the connection to, `[A1 8.2.b]`
    /// — what lets a household find its own row in the monthly publication.
    pub netzbereich: Option<String>,
}

/// How the house is put together.
///
/// Every asset is optional, because a site model is a list of what is *there*:
/// a household with no battery describes no battery, rather than a battery of no
/// size the arbiter decides for and `/v1/status` lists as undriven. An asset
/// that exists carries its own facts, so "a battery with no capacity" is not
/// representable — the same argument as `LoadKind::Shiftable` carrying its
/// `Programme` (D54).
#[derive(Debug, Clone, PartialEq)]
pub struct HouseholdConfig {
    /// The roof, or `None` for a household without one.
    pub pv: Option<PvConfig>,
    /// The battery, or `None`.
    pub battery: Option<BatteryConfig>,
    /// The charge point, or `None`.
    pub evse: Option<EvseConfig>,
    /// The heat pump, or `None`.
    pub heat_pump: Option<HeatPumpConfig>,
    /// The hot-water tank, or `None`.
    pub dhw: Option<DhwConfig>,
    /// The main fuse.
    pub fuse: Current,
    /// What the connection agreement says beyond it.
    pub connection: ConnectionConfig,
    /// Which house this is, thermally, until the box has identified its own.
    ///
    /// The prior `hems_forecast::building::Record` starts from. It changes the
    /// **shape** of a heating plan rather than its inputs: the fabric capacity
    /// decides whether pre-heating into a cheap hour pays at all, and it differs
    /// by a factor of five across the archetypes.
    pub building: Rc2,
    /// The § 14a and § 9 EEG facts declared about each asset, by identifier.
    ///
    /// Absent means [`Declared::default`], which is the conservative reading of
    /// both statutes — see [`Declared`].
    pub declared: BTreeMap<String, Declared>,
    /// Where the house is.
    pub location: GeoPoint,
    /// The programme a shiftable appliance is loaded with, if the household has
    /// one waiting.
    ///
    /// The dishwasher is the cheapest flexibility a house owns: nothing is
    /// stored, nothing degrades, and the only cost of moving it is that somebody
    /// unloads it later. `None` leaves the household without one, which is what
    /// every reference day was until it existed.
    pub dishwasher: Option<Programme>,
}

impl Default for HouseholdConfig {
    /// A common German single-family house in 2026: 9,8 kWp on the roof behind
    /// an 8 kW inverter, a 10 kWh battery, an 11 kW wallbox, a modulating heat
    /// pump drawing 5 kW electrical, and 300 litres of hot water.
    fn default() -> Self {
        Self {
            pv: Some(PvConfig {
                kwp: Power::from_kw(9.8),
                ac_nominal: Power::from_kw(8.0),
                // The German default roof: 35° pitch, due south. It is what
                // every figure in this workspace is measured on, and it is the
                // one thing an installer must not leave alone if the array
                // faces anywhere else.
                tilt_deg: 35.0,
                azimuth_deg: 180.0,
                // The ordinary German § 14a household of 2026, and the two
                // halves are deliberately different answers. An intelligent
                // metering system has been in since 2024 — every § 14a
                // household has one, because the Steuerungseinrichtung the
                // network operator writes limits through comes with it — so
                // § 51 EEG has been taking the negative quarter hours since the
                // start of 2025. And the § 9 Abs. 2 cap is **still on**,
                // because that one runs until the operator's first successful
                // Ansteuerbarkeit test and nobody has run it. `--imsys` is that
                // test happening.
                para9: Para9Status::default().with_imsys_since(REFERENCE_IMSYS_SINCE),
            }),
            battery: Some(BatteryConfig {
                kwh: Energy::from_kwh(10.0),
                power: Power::from_kw(5.0),
                reserve_soc: Soc::new(0.1).unwrap_or(Soc::EMPTY),
                // A €4 000 pack warranted for 2,4 MWh per kWh of capacity works
                // out near 8 ct per kilowatt-hour of throughput. Leaving it at
                // zero is what makes a plan cycle a battery for a two-cent
                // spread.
                wear_eur_per_kwh: 0.08,
            }),
            evse: Some(EvseConfig {
                max_current: Current::new(16.0),
                switchable: true,
                // Not bidirectional, which is the honest reference: a wallbox
                // that can discharge a car was still a special order in Germany
                // in 2026. `a_bidirectional_wallbox_is_offered_as_a_producer`
                // is where the other branch is measured, on its own household,
                // so the reference days keep measuring what they were tuned for.
                bidirectional: false,
                // Three quarters, the figure most owners of a car they drive
                // daily set: it is where lithium ageing turns and where a
                // charging session stops being worth waiting for.
                charge_limit: Soc::new(0.75).ok(),
            }),
            heat_pump: Some(HeatPumpConfig {
                power: Power::from_kw(5.0),
                modulating: true,
                comfort_min_c: 20.0,
                comfort_max_c: 23.0,
                // Reversible, at four fifths of its heating rating — the ordinary
                // shape of an air-to-water unit, and the ordinary shape of a new
                // German installation. It is what makes the June day a question
                // about *control* rather than a fortnight of unavoidable
                // discomfort the planner watches and pays for (D202).
                cooling_electrical: Some(Power::from_kw(4.0)),
                control: HeatPumpControl::PowerCeiling,
            }),
            // Three hundred litres on a hot-water heat pump — the standard
            // German fitting, and about five kilowatt-hours of heat between 45
            // and 60 °C for under two kilowatt-hours of electricity.
            dhw: Some(DhwConfig {
                litres: 300.0,
                heater: Power::from_kw(0.5),
                cop: 3.0,
                standing_loss: Power::new(45.0),
                t_min_c: 45.0,
                t_set_c: 55.0,
                t_max_c: 60.0,
            }),
            fuse: Current::new(35.0),
            connection: ConnectionConfig::default(),
            building: Rc2::house(),
            // Every asset of the reference household went in together, in the
            // March after the Solarspitzengesetz — so the roof is capped at
            // 60 % by § 9 Abs. 2 and every controllable device is mandatory
            // under `[A1 3.1.b]`. One date, stated once, rather than the same
            // literal repeated at five asset constructors.
            declared: REFERENCE_ASSETS
                .iter()
                .map(|id| {
                    (
                        (*id).to_owned(),
                        Declared {
                            commissioned_at: Some(REFERENCE_COMMISSIONING),
                            ..Declared::default()
                        },
                    )
                })
                .collect(),
            // Ninety minutes: heat the water, wash, heat again to dry. The shape
            // is what makes it worth carrying a programme rather than a duration
            // and an average — a plan allowed to smear 700 W over six hours
            // would schedule a machine that does not exist.
            dishwasher: Some(Programme::from_steps([
                Power::from_kw(2.0),
                Power::from_kw(0.2),
                Power::from_kw(0.2),
                Power::from_kw(0.2),
                Power::from_kw(1.8),
                Power::from_kw(0.1),
            ])),
            location: GeoPoint {
                latitude: 52.52,
                longitude: 13.40,
                altitude_m: 34.0,
            },
        }
    }
}

impl HouseholdConfig {
    /// What was declared about `asset`, or the conservative reading of silence.
    #[must_use]
    pub fn declared_for(&self, asset: &str) -> Declared {
        self.declared.get(asset).copied().unwrap_or_default()
    }
}

/// The site, plus the names the planner uses for its parts.
///
/// Each asset's identifier is `Some` exactly where the configuration describes
/// the asset, so "does this household have a battery" is one `Option` rather
/// than a size compared with zero.
#[derive(Debug, Clone)]
pub struct Household {
    /// The site itself.
    pub site: Site,
    /// The photovoltaic array, where the household has one.
    pub pv: Option<AssetId>,
    /// The battery, where the household has one.
    pub battery: Option<AssetId>,
    /// The charge point, where the household has one.
    pub evse: Option<AssetId>,
    /// The heat pump, where the household has one.
    pub heat_pump: Option<AssetId>,
    /// The uncontrollable household load.
    pub load: AssetId,
    /// The hot-water tank, where the household has one.
    pub dhw: Option<AssetId>,
    /// The meter at the connection point.
    ///
    /// The one measurement every § 14a decision starts from `[A1 2.3]`, and the
    /// only witness for the part of the house nobody instrumented. Without it
    /// the guard has to close its balance from the sum of the sub-meters, which
    /// is exact only in a house where every load has one.
    pub grid_meter: AssetId,
    /// The shiftable appliance, where the household has one.
    pub dishwasher: Option<AssetId>,
    /// The names, as the optimiser wants them.
    pub names: AssetNames,
}

impl Household {
    /// Build a site from a configuration.
    ///
    /// Only the assets the configuration describes are built: a household that
    /// says it has no battery gets a site with no battery in it, so the arbiter
    /// never decides for one, the S2 description never names one, and
    /// `run --check` refuses a driver configured for one.
    ///
    /// # Errors
    /// When the identifiers or the circuit tree do not validate — which can only
    /// happen if this function is edited badly, and is worth failing on rather
    /// than unwrapping.
    pub fn build(config: &HouseholdConfig) -> anyhow::Result<Self> {
        let main = CircuitId::new("main")?;
        let garage = CircuitId::new("garage")?;

        let present = |there: bool, id: &str| -> anyhow::Result<Option<AssetId>> {
            Ok(there.then(|| AssetId::new(id)).transpose()?)
        };
        let pv = present(config.pv.is_some(), "pv")?;
        let battery = present(config.battery.is_some(), "battery")?;
        let evse = present(config.evse.is_some(), "wallbox")?;
        let heat_pump = present(config.heat_pump.is_some(), "waermepumpe")?;
        let dhw = present(config.dhw.is_some(), "warmwasser")?;
        let load = AssetId::new("haushalt")?;
        let grid_meter = AssetId::new("netzanschluss-zaehler")?;
        let dishwasher = present(config.dishwasher.is_some(), "spuelmaschine")?;

        let assets = assets_of(config, &main, &garage)?;
        let site = Site::new(
            SiteId::new(),
            config.location,
            connection(config),
            Circuits::new(vec![
                Circuit::new(main.clone(), None, config.fuse),
                Circuit::new(garage.clone(), Some(main.clone()), Current::new(20.0)),
            ])?,
            assets,
        )?;

        Ok(Self {
            names: AssetNames {
                battery: battery.clone(),
                evse: evse.clone(),
                pv: pv.clone(),
                heat_pump: heat_pump.clone(),
                dhw: dhw.clone(),
                shiftable: dishwasher.iter().cloned().collect(),
            },
            site,
            pv,
            battery,
            evse,
            heat_pump,
            load,
            dhw,
            grid_meter,
            dishwasher,
        })
    }
}

/// The three kilowatts of Zusatzheizung an ordinary German fitting has.
///
/// `[A1 2.4.1.b]` folds it into the heat pump's own Fallgruppe, so it is part of
/// this asset's nameplate rather than a load of its own.
const HEATING_ROD: Power = Power::new_const(3_000.0);

/// The heat pump the configuration describes, less its identity.
///
/// Split out because it is where the *household's* answers live — the comfort
/// band it will accept and how the unit takes instructions — and those were once
/// configurable and silently dropped here, so a household that widened its band
/// got the plan of one that had not touched it.
fn heat_pump(hp: &HeatPumpConfig, declared: Declared) -> HeatPump {
    HeatPump {
        meta: declare(
            AssetMeta::new(
                AssetId::new("waermepumpe").expect("a literal identifier"),
                CircuitId::new("main").expect("a literal identifier"),
                PhaseConnection::Three,
                hp.power,
            ),
            declared,
        ),
        electrical_nominal: hp.power,
        heating_rod: Some(HEATING_ROD),
        cooling_electrical: None,
        control: hp.control,
        modulating: hp.modulating,
        comfort_min_c: hp.comfort_min_c,
        comfort_max_c: hp.comfort_max_c,
        cop: CopCurve::air_source(),
    }
}

/// Apply what the installer declared to an asset's own facts.
///
/// One place, so a device whose § 14a regime is decided here cannot be built by
/// a second path that forgets to ask.
fn declare(meta: AssetMeta, declared: Declared) -> AssetMeta {
    let mut meta = meta;
    meta.commissioned_at = declared.commissioned_at;
    meta.legacy_status = declared.legacy_status;
    meta.switched_voluntarily = declared.switched_voluntarily;
    meta.steuve_exemption = declared.exemption;
    meta
}

/// The connection point, with whatever the agreement adds to the fuse.
fn connection(config: &HouseholdConfig) -> GridConnection {
    let c = &config.connection;
    GridConnection {
        malo: c.malo,
        melo: c.melo.clone(),
        dso_code: c.dso_code.clone(),
        netzbereich: c.netzbereich.clone(),
        contract_power: c.contract_power,
        ..GridConnection::new(config.fuse)
    }
}

/// The assets the reference household is made of.
///
/// Split out from [`Household::build`] because the list is the interesting part
/// and the plumbing around it is not.
fn assets_of(
    config: &HouseholdConfig,
    main: &CircuitId,
    garage: &CircuitId,
) -> anyhow::Result<Vec<Asset>> {
    // What each device can actually be told, rather than one bitset for
    // everything. The distinction earns its keep in the arbiter: an asset the
    // manager can only *limit* has controls of its own and an absent instruction
    // means "no limit", while an asset the manager *drives* does nothing until
    // it is asked to.
    let meta = |id: &str,
                kw: f64,
                circuit: &CircuitId,
                capabilities: Capabilities|
     -> anyhow::Result<AssetMeta> {
        Ok(declare(
            AssetMeta::new(
                AssetId::new(id)?,
                circuit.clone(),
                PhaseConnection::Three,
                Power::from_kw(kw),
            )
            .with_capabilities(Capabilities::MEASURE | capabilities),
            config.declared_for(id),
        ))
    };
    let driven = Capabilities::LIMIT_CONSUMPTION | Capabilities::SET_POWER;
    let mut assets = Vec::new();
    if let Some(pv) = &config.pv {
        assets.push(Asset::Pv(PvArray {
            meta: meta("pv", pv.kwp.kw(), main, Capabilities::LIMIT_PRODUCTION)?,
            kwp_dc: pv.kwp,
            ac_nominal: pv.ac_nominal,
            tilt_deg: pv.tilt_deg,
            azimuth_deg: pv.azimuth_deg,
            para9: pv.para9,
        }));
    }
    if let Some(battery) = &config.battery {
        assets.push(Asset::Battery(Battery {
            meta: meta("battery", battery.power.kw(), main, driven)?,
            capacity: battery.kwh,
            max_charge: battery.power,
            max_discharge: battery.power,
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.05)?,
            soc_max: Soc::FULL,
            reserve_soc: battery.reserve_soc,
            grid_charging_allowed: true,
        }));
    }
    if let Some(evse) = &config.evse {
        // The nameplate, as the class it is sold as: 16 A three-phase is an
        // "11 kW" wallbox and 32 A a "22 kW" one, not 11,04 and 22,08. The
        // current is what bounds the command path (`Evse::max_power` takes the
        // minimum of the two), so trimming the odd forty watts off the
        // nameplate costs nothing and keeps the § 14a facts in the units the
        // paperwork is written in.
        let kw = (evse.max_current.get() * 3.0 * 230.0 / 100.0).floor() / 10.0;
        assets.push(Asset::Evse(Evse {
            meta: {
                let mut m = meta("wallbox", kw, garage, driven)?;
                if evse.switchable {
                    m.phases = PhaseConnection::Switchable { phase: Phase::L1 };
                }
                m
            },
            min_current: Current::new(6.0),
            max_current: evse.max_current,
            bidirectional: evse.bidirectional,
            public: false,
            // The household's Ladelimit, as a fraction of the vehicle's own
            // capacity. The planner works from an energy target and a departure
            // and never reads this; the real-time fallback has neither, and
            // without it a box with no plan pushes surplus into a car that
            // already has what it was asked for rather than exporting it.
            charge_limit: evse.charge_limit,
        }));
    }
    if let Some(hp) = &config.heat_pump {
        assets.push(Asset::HeatPump(HeatPump {
            meta: meta(
                "waermepumpe",
                hp.power.kw() + HEATING_ROD.kw(),
                main,
                Capabilities::LIMIT_CONSUMPTION,
            )?,
            ..heat_pump(hp, config.declared_for("waermepumpe"))
        }));
    }
    if let Some(dhw) = &config.dhw {
        assets.push(Asset::Dhw(DhwTank {
            meta: meta(
                "warmwasser",
                dhw.heater.kw(),
                main,
                Capabilities::LIMIT_CONSUMPTION,
            )?,
            volume_l: dhw.litres,
            heater: dhw.heater,
            cop: dhw.cop,
            standing_loss: dhw.standing_loss,
            t_min_c: dhw.t_min_c,
            t_set_c: dhw.t_set_c,
            t_max_c: dhw.t_max_c,
        }));
    }
    assets.push(base_load(main)?);
    assets.push(grid_meter(main, config.fuse)?);
    assets.extend(
        config
            .dishwasher
            .clone()
            .map(|p| shiftable_appliance(p, main)),
    );
    Ok(assets)
}

/// The meter at the connection point.
///
/// `[A1 2.3]` measures the netzwirksamer Leistungsbezug *there* and nowhere
/// else, so this is the one asset whose absence changes what every other one is
/// allowed to do: without it the guard closes its balance from the sub-meters
/// alone, which understates the rest of the house by exactly the loads nobody
/// instrumented — and understating the house overstates the surplus, which is
/// the one direction a § 14a budget may never be wrong in.
///
/// It measures and nothing else. A meter is not a device the arbiter may
/// command, and the capability set is what says so.
fn grid_meter(circuit: &CircuitId, fuse: Current) -> anyhow::Result<Asset> {
    Ok(Asset::Meter(hems_core::asset::Meter {
        meta: AssetMeta::new(
            AssetId::new("netzanschluss-zaehler")?,
            circuit.clone(),
            PhaseConnection::Three,
            // What can cross it, which is the fuse rather than any one device.
            Power::new(fuse.get() * 230.0 * 3.0),
        )
        .with_capabilities(Capabilities::MEASURE),
        role: hems_core::asset::MeterRole::GridConnection,
        subject: None,
    }))
}

/// The part of the house nobody manages.
///
/// No control capability at all, so the arbiter leaves it alone and the guard
/// counts it as what it is: load that happens whatever anybody decides.
fn base_load(circuit: &CircuitId) -> anyhow::Result<Asset> {
    Ok(Asset::Load(FlexibleLoad {
        meta: AssetMeta::new(
            AssetId::new("haushalt")?,
            circuit.clone(),
            PhaseConnection::Three,
            Power::from_kw(3.0),
        ),
        nominal: Power::from_kw(0.5),
        kind: LoadKind::Fixed,
    }))
}

/// The shiftable appliance, where the household has loaded one.
///
/// `SCHEDULE`, and deliberately **not** `LIMIT_CONSUMPTION`: the only thing
/// anybody may tell a running dishwasher is when to start. Giving it a
/// consumption ceiling would let the arbiter shed a kilowatt from the one device
/// in the house that cannot give one — and the guard would then count on power
/// that kept flowing anyway, which is the failure mode the whole
/// nameplate-assumption rule exists to prevent.
fn shiftable_appliance(programme: Programme, circuit: &CircuitId) -> Asset {
    Asset::Load(FlexibleLoad {
        meta: AssetMeta::new(
            AssetId::new("spuelmaschine").expect("a valid identifier"),
            circuit.clone(),
            PhaseConnection::Three,
            programme.peak(),
        )
        .with_capabilities(Capabilities::MEASURE | Capabilities::SCHEDULE)
        .commissioned(time::macros::date!(2025 - 03 - 01)),
        nominal: programme.peak(),
        kind: LoadKind::Shiftable(programme),
    })
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
///
/// Shared by the simulated day and the running box, because they are the same
/// translation and a second copy of it would be a second place for a § 14a
/// ceiling to be read one slot short.
pub fn planning_limits(
    limits: &GridLimits,
    ends_at: Option<OffsetDateTime>,
    site: &Site,
    now: OffsetDateTime,
) -> PlanningLimits {
    let mut planning = PlanningLimits::default()
        .with_import_ceiling(site.grid.import_ceiling())
        // Which of the planner's three controllable devices a § 14a ceiling
        // actually binds, answered by the same function the guard asks every
        // tick. A heat pump below 4,2 kW is not a steuerbare
        // Verbrauchseinrichtung, and a planner that charged one against the
        // ceiling anyway left the house colder than the Festlegung asks for on
        // exactly the evenings a reduction happens.
        .with_steuve_devices(steuve_devices(site, now))
        // What the *baseline* household lives under while the same reduction is
        // in force. It has no energy manager, so it cannot be addressed as one
        // `[A1 4.4.b]`: its Steuerbox turns each device down on its own
        // `[A1 4.4.a]`, and may not take any of them below the minimum of
        // `[A1 4.5.1]`. The plan is unaffected — this bounds the comparison, not
        // the optimisation.
        .with_direct_control_ceiling(hems_grid::para14a::MINDESTLEISTUNG);
    if let Some(ceiling) = limits.steuve_ceiling {
        planning = planning.with_steuve(match ends_at {
            Some(end) => TimedLimit::until(ceiling, Slot::containing(end)),
            None => TimedLimit::always(ceiling),
        });
    }
    // Derived from the site, exactly as the guard derives it — § 9 EEG applies
    // to the plant by force of law and not because an operator sent something,
    // so a planner that only knew about a reported ceiling would plan a roof at
    // its full rating and then watch the guard curtail it every sunny midday.
    // `site_feed_in_ceiling` folds the reported limit in and returns the
    // strictest, so an operator asking for less is still what binds.
    let feed_in =
        hems_grid::para9::site_feed_in_ceiling(site, limits.mgcp_factor, limits.feed_in_ceiling)
            .map(|(p, _)| p)
            .or(limits.feed_in_ceiling);
    if let Some(ceiling) = feed_in {
        planning = planning.with_feed_in(TimedLimit::always(ceiling));
    }
    planning
}

fn steuve_devices(site: &Site, now: OffsetDateTime) -> SteuVeDevices {
    let classified = hems_grid::classify_at(&site.assets, now);
    let has = |fallgruppe| classified.iter().any(|s| s.fallgruppe == fallgruppe);
    SteuVeDevices {
        battery: has(Fallgruppe::Stromspeicher),
        ev: has(Fallgruppe::Ladepunkt),
        heat_pump: has(Fallgruppe::Waermepumpe),
    }
}

/// The § 14a network-charge modules this household could choose between.
///
/// The first is the reference — what it is on today — and the rest are what
/// [`hems_tariff::compare_moduls`] prices against it.
///
/// **Modul 3 appears only where the household has a calendar.** It is not a
/// module anybody can be advised into in the abstract: its value is the shape of
/// one network operator's own windows, and a comparison against invented ones
/// would be a recommendation computed from a guess. Where the box *has* been
/// given the operator's calendar — transcribed by whoever commissioned it, and
/// refused at start-up unless it conforms — the comparison is real and is worth
/// making, because shifting out of the Hochtarif is the one flexibility a
/// household knows about a year in advance.
#[must_use]
pub fn modul_choices(current: &Tariff) -> Vec<hems_tariff::ModulChoice> {
    let arbeitspreis = Decimal::new(1000, 2);
    let mut choices = vec![
        hems_tariff::ModulChoice {
            label: "Modul 1".into(),
            tariff: current.clone(),
        },
        hems_tariff::ModulChoice {
            label: "Modul 2".into(),
            tariff: Tariff {
                network: NetworkCharge::Modul2 {
                    arbeitspreis,
                    // 60 % off the working price, at a Marktlokation of its own.
                    remaining_share: Decimal::new(4, 1),
                    metering_eur_per_year: Decimal::new(25, 0),
                },
                ..current.clone()
            },
        },
        hems_tariff::ModulChoice {
            label: "no module".into(),
            tariff: Tariff {
                network: NetworkCharge::None { arbeitspreis },
                ..current.clone()
            },
        },
    ];
    // The household's own calendar, priced against the flat charge it would
    // otherwise pay. Only where it has one: the comparison is the shape of one
    // operator's windows and cannot be made against invented ones.
    if let NetworkCharge::Modul3 { .. } = &current.network {
        choices.insert(
            0,
            hems_tariff::ModulChoice {
                label: "Modul 3".into(),
                tariff: current.clone(),
            },
        );
        choices[1] = hems_tariff::ModulChoice {
            label: "Modul 1".into(),
            tariff: Tariff {
                network: NetworkCharge::Modul1 {
                    arbeitspreis,
                    reduction_eur_per_year: -current.network.annual_fixed_eur(),
                },
                ..current.clone()
            },
        };
    }
    choices
}

/// A dynamic tariff with the given day-ahead prices, in ct/kWh per slot.
///
/// `site` is read for one thing only, and it is the thing a tariff cannot know
/// about itself: whether § 51 EEG reaches this household's roof, and from when.
/// The answer is a property of the *plant* — its size and the year an intelligent
/// metering system went in (§ 51 Abs. 2) — so it is derived from the site rather
/// than set as a preference on the tariff.
#[must_use]
pub fn tariff_for(site: &Site, prices_ct: &[i64], horizon: Horizon) -> Tariff {
    let spot: BTreeMap<Slot, Decimal> = horizon
        .slots()
        .enumerate()
        .map(|(i, s)| (s, Decimal::new(prices_ct[i % prices_ct.len()], 0)))
        .collect();
    Tariff {
        energy: EnergyPrice::Dynamic {
            spot,
            markup_ct_per_kwh: Decimal::new(3, 0),
            fallback_ct_per_kwh: Decimal::new(20, 0),
        },
        network: NetworkCharge::Modul1 {
            arbeitspreis: Decimal::new(1000, 2),
            reduction_eur_per_year: Decimal::new(120, 0),
        },
        levies: Levies::household_2026(),
        // 7,86 ct/kWh, and nothing at all in a quarter hour with a negative
        // day-ahead price **once § 51 EEG reaches this plant** — which for a
        // household roof is the first of January after its intelligent metering
        // system goes in, and not before (§ 51 Abs. 2 Nr. 1).
        feed_in: FeedIn::eeg(Decimal::new(786, 2)).under_para51_from(
            hems_grid::para9::GenerationProfile::of_site(site)
                .as_ref()
                .and_then(hems_grid::para9::para51_applies_from),
        ),
        sharing: None,
        carbon_g_per_kwh: german_grid_intensity(horizon),
        standing_charge_eur_per_year: Decimal::new(120, 0),
    }
}

/// The German grid's own carbon intensity through a day, g CO₂/kWh.
///
/// # Why the reference day needs one at all
///
/// Without it every quarter hour is equally dirty — the planner falls back to a
/// flat annual figure — and a **carbon price then does exactly what an autarky
/// premium does**, because a constant intensity times a price is a constant
/// adder on every imported kilowatt-hour. Two dials, one behaviour, and no way
/// to tell whether either works. That was the state of this workspace for four
/// versions: `hems-tariff::source::energy_charts_co2` parsed this series,
/// nothing consumed it, and `SlotPrice::co2_g_per_kwh` was hard-coded `None`.
///
/// # The shape, and why it is not the price curve
///
/// The whole value of a carbon signal is where it **disagrees** with the price.
/// The German grid's intensity follows residual load rather than the merit
/// order alone:
///
/// * **night** — cheap, and only moderately clean: demand is low but so is
///   solar, and lignite runs through it. ~330 g/kWh.
/// * **midday** — cheap *and* clean, because that is when solar is on the
///   system. ~180 g/kWh.
/// * **the evening peak** — dear *and* dirty: solar is gone and gas and coal
///   cover the ramp. ~520 g/kWh.
///
/// So a household that prices carbon moves flexible load out of the **cheap
/// night** and into the **cheap middle of the day** — which the price signal
/// alone does not ask for, because both are cheap. Where the two agree, in the
/// evening peak, the dial correctly changes nothing.
///
/// A shape rather than a recorded series, and deliberately: it is the reference
/// *day*, and it has to be a pure function of the slot so the day still replays
/// to the last cent (D23). A real box takes the real series from `tariffd`,
/// which fetches it from Energy-Charts.
#[must_use]
pub fn german_grid_intensity(horizon: Horizon) -> BTreeMap<Slot, f64> {
    horizon
        .slots()
        .map(|slot| {
            // The local hour, because the sun and the evening peak keep local
            // time and a fixed offset would move both by an hour every summer.
            let hour = f64::from(slot.index_in_local_day()) / 4.0;
            // Two Gaussians on a base: solar carving out the middle of the day,
            // and the evening ramp piling on top of it.
            let solar = 150.0 * (-((hour - 12.5) / 3.2).powi(2)).exp();
            let peak = 200.0 * (-((hour - 19.0) / 2.0).powi(2)).exp();
            (slot, 330.0 - solar + peak)
        })
        .collect()
}
