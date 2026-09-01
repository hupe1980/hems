//! Building the household the daemon manages.

use hems_core::asset::{
    AssetMeta, Battery, Capabilities, Chemistry, DhwTank, Evse, FlexibleLoad, HeatPump,
    HeatPumpControl, LoadKind, Programme, PvArray,
};
use hems_core::prelude::*;
use hems_optimizer::solve::AssetNames;
use hems_tariff::levies::Levies;
use hems_tariff::tariff::{EnergyPrice, FeedIn, NetworkCharge, Tariff};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// How the house is put together.
#[derive(Debug, Clone, PartialEq)]
pub struct HouseholdConfig {
    /// Installed photovoltaic power.
    pub pv_kwp: Power,
    /// The inverter's alternating-current limit.
    pub pv_ac_nominal: Power,
    /// Battery capacity.
    pub battery_kwh: Energy,
    /// Battery power, both directions.
    pub battery_power: Power,
    /// Energy held back for a power cut.
    pub reserve_soc: Soc,
    /// What a kilowatt-hour of battery throughput costs in wear, €/kWh.
    pub battery_wear_eur_per_kwh: f64,
    /// The main fuse.
    pub fuse: Current,
    /// Where the house is.
    pub location: GeoPoint,
    /// Electrical power of the heat pump at full output.
    pub heat_pump_power: Power,
    /// The bottom of the comfort band, °C.
    pub comfort_min_c: f64,
    /// The top of the comfort band, °C.
    pub comfort_max_c: f64,
    /// Whether the heat pump modulates rather than switching on and off.
    pub heat_pump_modulating: bool,
    /// Volume of the hot-water tank, litres. Zero leaves the house without one.
    pub dhw_litres: f64,
    /// Electrical power of the hot-water heat pump.
    pub dhw_heater: Power,
    /// The § 9 EEG facts declared about the roof.
    ///
    /// The default is the realistic pair rather than the tidy one: an
    /// intelligent metering system fitted years ago, so § 51 EEG has been taking
    /// the negative quarter hours since the end of that year — and the 60 %
    /// feed-in cap still on, because § 9 Abs. 2 lifts it only after the network
    /// operator's first successful Ansteuerbarkeit test, which is a different
    /// event on a different clock and has not happened.
    pub para9: Para9Status,
    /// The state of charge the household asked its car to reach.
    ///
    /// `None` means "fill it". It is only ever read by the real-time fallback —
    /// the planner is given an energy target and a departure, which say the same
    /// thing more precisely — but that is the mode the fallback is about, and a
    /// surplus
    /// tracker with no notion of *enough* charges past the limit in preference to
    /// exporting.
    pub ev_charge_limit: Option<Soc>,
    /// The programme a shiftable appliance is loaded with, if the household has
    /// one waiting.
    ///
    /// The dishwasher is the cheapest flexibility a house owns: nothing is
    /// stored, nothing degrades, and the only cost of moving it is that somebody
    /// unloads it later. `None` leaves the household without one, which is what
    /// every reference day was until it existed.
    pub dishwasher: Option<Programme>,
    /// Whether the charge point can drop to a single conductor.
    ///
    /// Almost every wallbox sold in Germany since about 2022 can, and it is what
    /// decides whether 2 kW of surplus charges a car or is exported: three-phase
    /// charging cannot start below 4,14 kW, single-phase below 1,38 kW.
    pub evse_switchable: bool,
}

impl Default for HouseholdConfig {
    /// A common German single-family house in 2026: 9,8 kWp on the roof behind
    /// an 8 kW inverter, a 10 kWh battery, an 11 kW wallbox, a modulating heat
    /// pump drawing 5 kW electrical, and 300 litres of hot water.
    fn default() -> Self {
        Self {
            pv_kwp: Power::from_kw(9.8),
            pv_ac_nominal: Power::from_kw(8.0),
            battery_kwh: Energy::from_kwh(10.0),
            battery_power: Power::from_kw(5.0),
            reserve_soc: Soc::new(0.1).unwrap_or(Soc::EMPTY),
            // A €4 000 pack warranted for 2,4 MWh per kWh of capacity works out
            // near 8 ct per kilowatt-hour of throughput. Leaving it at zero is
            // what makes a plan cycle a battery for a two-cent spread.
            battery_wear_eur_per_kwh: 0.08,
            fuse: Current::new(35.0),
            heat_pump_power: Power::from_kw(5.0),
            comfort_min_c: 20.0,
            comfort_max_c: 23.0,
            heat_pump_modulating: true,
            // Three hundred litres on a hot-water heat pump — the standard
            // German fitting, and about five kilowatt-hours of heat between 45
            // and 60 °C for under two kilowatt-hours of electricity.
            dhw_litres: 300.0,
            dhw_heater: Power::from_kw(0.5),
            // The ordinary German § 14a household of 2026, and the two halves
            // are deliberately different answers. An intelligent metering
            // system has been in since 2024 — every § 14a household has one,
            // because the Steuerungseinrichtung the network operator writes
            // limits through comes with it — so § 51 EEG has been taking the
            // negative quarter hours since the start of 2025. And the § 9
            // Abs. 2 cap is **still on**, because that one runs until the
            // operator's first successful Ansteuerbarkeit test and nobody has
            // run it. `--imsys` is that test happening.
            para9: Para9Status::default().with_imsys_since(time::macros::date!(2024 - 06 - 01)),
            // Three quarters, the figure most owners of a car they drive daily
            // set: it is where lithium ageing turns and where a charging session
            // stops being worth waiting for.
            ev_charge_limit: Soc::new(0.75).ok(),
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
            evse_switchable: true,
            location: GeoPoint {
                latitude: 52.52,
                longitude: 13.40,
                altitude_m: 34.0,
            },
        }
    }
}

/// The site, plus the names the planner uses for its parts.
#[derive(Debug, Clone)]
pub struct Household {
    /// The site itself.
    pub site: Site,
    /// The photovoltaic array.
    pub pv: AssetId,
    /// The battery.
    pub battery: AssetId,
    /// The charge point.
    pub evse: AssetId,
    /// The heat pump.
    pub heat_pump: AssetId,
    /// The uncontrollable household load.
    pub load: AssetId,
    /// The hot-water tank.
    pub dhw: AssetId,
    /// The shiftable appliance, where the household has one.
    pub dishwasher: Option<AssetId>,
    /// The names, as the optimiser wants them.
    pub names: AssetNames,
}

impl Household {
    /// Build a site from a configuration.
    ///
    /// # Errors
    /// When the identifiers or the circuit tree do not validate — which can only
    /// happen if this function is edited badly, and is worth failing on rather
    /// than unwrapping.
    pub fn build(config: &HouseholdConfig) -> anyhow::Result<Self> {
        let main = CircuitId::new("main")?;
        let garage = CircuitId::new("garage")?;

        let pv = AssetId::new("pv")?;
        let battery = AssetId::new("battery")?;
        let evse = AssetId::new("wallbox")?;
        let heat_pump = AssetId::new("waermepumpe")?;
        let load = AssetId::new("haushalt")?;
        let dhw = AssetId::new("warmwasser")?;
        let dishwasher = config
            .dishwasher
            .as_ref()
            .map(|_| AssetId::new("spuelmaschine"))
            .transpose()?;

        let assets = assets_of(config, &main, &garage)?;
        let site = Site::new(
            SiteId::new(),
            config.location,
            GridConnection::new(config.fuse),
            Circuits::new(vec![
                Circuit::new(main.clone(), None, config.fuse),
                Circuit::new(garage.clone(), Some(main.clone()), Current::new(20.0)),
            ])?,
            assets,
        )?;

        Ok(Self {
            names: AssetNames {
                battery: Some(battery.clone()),
                evse: Some(evse.clone()),
                pv: Some(pv.clone()),
                heat_pump: Some(heat_pump.clone()),
                dhw: Some(dhw.clone()),
                shiftable: dishwasher.iter().cloned().collect(),
            },
            site,
            pv,
            battery,
            evse,
            heat_pump,
            load,
            dhw,
            dishwasher,
        })
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
        Ok(AssetMeta::new(
            AssetId::new(id)?,
            circuit.clone(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
        .with_capabilities(Capabilities::MEASURE | capabilities)
        .commissioned(time::macros::date!(2025 - 03 - 01)))
    };
    let driven = Capabilities::LIMIT_CONSUMPTION | Capabilities::SET_POWER;
    Ok(vec![
        Asset::Pv(PvArray {
            meta: meta(
                "pv",
                config.pv_kwp.kw(),
                main,
                Capabilities::LIMIT_PRODUCTION,
            )?,
            kwp_dc: config.pv_kwp,
            ac_nominal: config.pv_ac_nominal,
            tilt_deg: 35.0,
            azimuth_deg: 180.0,
            para9: config.para9,
        }),
        Asset::Battery(Battery {
            meta: meta("battery", config.battery_power.kw(), main, driven)?,
            capacity: config.battery_kwh,
            max_charge: config.battery_power,
            max_discharge: config.battery_power,
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.05)?,
            soc_max: Soc::FULL,
            reserve_soc: config.reserve_soc,
            chemistry: Chemistry::Lfp,
            grid_charging_allowed: true,
        }),
        Asset::Evse(Evse {
            meta: {
                let mut m = meta("wallbox", 11.0, garage, driven)?;
                if config.evse_switchable {
                    m.phases = PhaseConnection::Switchable { phase: Phase::L1 };
                }
                m
            },
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
            // The household's Ladelimit, as a fraction of the vehicle's own
            // capacity. The planner works from an energy target and a departure
            // and never reads this; the real-time fallback has neither, and
            // without it a box with no plan pushes surplus into a car that
            // already has what it was asked for rather than exporting it.
            charge_limit: config.ev_charge_limit,
        }),
        Asset::HeatPump(HeatPump {
            meta: meta(
                "waermepumpe",
                config.heat_pump_power.kw() + 3.0,
                main,
                Capabilities::LIMIT_CONSUMPTION,
            )?,
            electrical_nominal: config.heat_pump_power,
            heating_rod: Some(Power::from_kw(3.0)),
            control: HeatPumpControl::PowerCeiling,
            modulating: true,
        }),
        Asset::Dhw(DhwTank {
            meta: meta(
                "warmwasser",
                config.dhw_heater.kw(),
                main,
                Capabilities::LIMIT_CONSUMPTION,
            )?,
            volume_l: config.dhw_litres,
            heater: config.dhw_heater,
            cop: 3.0,
            standing_loss: Power::new(45.0),
            t_min_c: 45.0,
            t_set_c: 55.0,
            t_max_c: 60.0,
        }),
        base_load(main)?,
    ]
    .into_iter()
    .chain(
        config
            .dishwasher
            .clone()
            .map(|p| shiftable_appliance(p, main)),
    )
    .collect())
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

/// The § 14a network-charge modules this household could choose between.
///
/// The first is the reference — what it is on today — and the rest are what
/// [`hems_tariff::compare_moduls`] prices against it. Modul 3 is deliberately
/// absent: it needs the network operator's own Zählzeitdefinition, and this
/// workspace refuses to invent one. A curated calendar per operator is
/// `tariffd`'s job.
#[must_use]
pub fn modul_choices(current: &Tariff) -> Vec<hems_tariff::ModulChoice> {
    let arbeitspreis = Decimal::new(1000, 2);
    vec![
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
    ]
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
        standing_charge_eur_per_year: Decimal::new(120, 0),
    }
}
