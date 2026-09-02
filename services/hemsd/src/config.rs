//! What a box is told about the house it manages.
//!
//! Until this existed `hemsd` could only manage `HouseholdConfig::default()` —
//! a reference household compiled into the binary. That is the right thing for a
//! simulated day and useless on a wall, so this is the file an installer edits:
//! the connection, the assets, the drivers that speak to them, and how fast the
//! control loop runs.
//!
//! # Units are in the field names, and that is deliberate
//!
//! The domain types are `Power`, `Energy`, `Current`, `Soc`. None of them
//! appears here. A configuration file is read by a person with a clipboard
//! standing in a cellar, and `battery_kwh = 10.0` is a sentence they can check
//! against a label where `battery = { watt_hours = 10000 }` is a sentence they
//! can get wrong by three orders of magnitude. So every field carries its unit
//! in its name, and [`SiteSettings::household`] is the one place the conversion
//! happens.
//!
//! It also keeps the wire format independent of the domain: adding a serde
//! derive to a newtype decides how it travels for every consumer of the crate,
//! and a configuration file is not a good reason to make that decision (P3).
//!
//! # Everything has a default except the one thing that cannot have one
//!
//! `hems-service` reads a file, then the environment, then the defaults, and a
//! file that is absent is a deployment nobody has customised rather than an
//! error. Every field of [`SiteSettings`] therefore defaults to the reference
//! household — the same house every figure in this project was measured on.
//!
//! The **driver list** does not, and it is the one thing `hemsd run` refuses to
//! start without. A box with no drivers measures nothing, so the guard assumes
//! every controllable device is drawing its nameplate power, for ever; and if
//! the household is under § 14a, nothing could hear a reduction. Coming up
//! quietly in either state is the failure this workspace keeps finding in
//! itself, so it is refused rather than warned about
//! (`crate::drivers::RegistryError::Uncommissioned`).

use std::collections::BTreeMap;

use hems_core::asset::Programme;
use hems_core::prelude::{Current, Energy, GeoPoint, Para9Status, Power, Site, Slot, Soc};
use hems_tariff::levies::Levies;
use hems_tariff::tariff::{EnergyPrice, FeedIn, NetworkCharge, Tariff};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::site::HouseholdConfig;

/// The whole of a box's configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shell: where the health surface listens, how it logs, how long a
    /// shutdown may take.
    pub service: hems_service::Settings,
    /// The house.
    pub site: SiteSettings,
    /// What the household pays and earns for a kilowatt-hour.
    pub tariff: TariffSettings,
    /// Where the box asks for prices and weather.
    pub fleet: FleetSettings,
    /// Where the box forwards its own two years, once it has kept them.
    pub histd: crate::runtime::outbox::HistdSettings,
    /// How the box presents itself on the EEBUS network, and whom it trusts.
    pub ship: crate::runtime::ship::ShipSettings,
    /// How fast the control planes run.
    pub control: ControlSettings,
    /// What speaks to the hardware. One entry per device.
    pub drivers: Vec<DriverSettings>,
    /// Where the box keeps its own two years, and what it has learned.
    ///
    /// `None` runs entirely in memory, which is right for a demonstration and
    /// wrong for a household: `[A1 7.3]` documents a control event for two years
    /// and G3 says the house is never worse off when the cloud is gone, so a
    /// record that exists only once it has been uploaded is an intention with a
    /// network dependency. It is also where the box keeps what it has *learned* —
    /// the correction its own roof has earned and its own household's quarter
    /// hours — and without it a reboot costs a fortnight of both.
    pub store_path: Option<std::path::PathBuf>,
}

impl AsMut<hems_service::Settings> for Settings {
    /// The shell's own fields, so `hems_service::load` can let the environment
    /// override them.
    ///
    /// A daemon on a gateway box is configured from a file an installer edited
    /// and a daemon in a fleet from an orchestrator that only knows how to set
    /// environment variables. Both are true at once, so `HEMS_HEMSD_LISTEN`
    /// wins over the file — the file is what somebody wrote down last month.
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}

/// How fast the three planes run, and how patient they are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ControlSettings {
    /// How often the guard and the arbiter decide, in seconds.
    ///
    /// The guard needs it as more than a schedule: a bound on a *state* — a
    /// backup reserve, a tank's ceiling — is only a bound on a *rate* once you
    /// know how long the rate will be held for, so this number is an input to
    /// the arithmetic and not only to the timer.
    pub tick_period_s: u64,
    /// How often the planner re-solves, in seconds.
    ///
    /// A receding horizon: the plan is remade long before its last slot
    /// arrives. Five minutes is what a gateway box can afford and comfortably
    /// inside the arbiter's own tolerance for a stale plan.
    pub replan_every_s: u64,
    /// How many quarter hours the planner looks ahead.
    ///
    /// Two days rather than one, and the second is not decoration: a re-plan at
    /// six in the evening on a one-day horizon is told there is no sun *and* no
    /// household tomorrow, which is a lie in both directions and one the
    /// terminal values only partly hide.
    pub horizon_slots: usize,
    /// How long the solver may spend, in seconds. Zero means "no limit".
    ///
    /// A wall-clock budget makes the answer depend on how busy the box was, so
    /// a household that wants the same inputs to give the same plan asks for
    /// none and waits.
    pub solve_budget_s: f64,
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            tick_period_s: 1,
            replan_every_s: 300,
            horizon_slots: 96 * 2,
            solve_budget_s: 10.0,
        }
    }
}

impl ControlSettings {
    /// The control period, as the guard wants it.
    #[must_use]
    pub const fn tick_period(&self) -> time::Duration {
        time::Duration::seconds(self.tick_period_s.cast_signed())
    }

    /// How long between re-plans.
    #[must_use]
    pub const fn replan_every(&self) -> time::Duration {
        time::Duration::seconds(self.replan_every_s.cast_signed())
    }
}

/// What the household pays and earns for a kilowatt-hour.
///
/// The one thing that was hard-coded for the whole life of this project and
/// could not be, once a box managed a house that was not the reference one: a
/// planner optimising against somebody else's tariff produces a schedule that
/// is optimal for nobody, and nothing about the result looks wrong.
///
/// Everything here is **net** ct/kWh, because that is how a German price sheet
/// is written; `hems_tariff::Levies` adds the levies and the value-added tax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TariffSettings {
    /// The supplier's markup on the day-ahead price, ct/kWh.
    ///
    /// § 41a EnWG has obliged every supplier to offer a dynamic tariff since
    /// 01.01.2025, and since 01.10.2025 the market time unit has been a quarter
    /// hour — which is why this whole workspace plans in quarter hours.
    pub markup_ct_per_kwh: f64,
    /// What to assume where no day-ahead price is known: past tomorrow's
    /// auction, and after an outage.
    ///
    /// A *flat* number, and that is the right shape: it makes the plan
    /// indifferent about when to act out there, which is exactly the state of
    /// knowledge. Refusing to plan at all would be worse.
    pub fallback_ct_per_kwh: f64,
    /// A fixed price instead of a dynamic one, ct/kWh.
    ///
    /// Set it and the box stops asking `tariffd` for anything: a household on a
    /// fixed tariff has no day-ahead curve to optimise against, and a planner
    /// given one anyway would shift load for a spread the household is not
    /// charged. It still plans — against the roof, the battery and the § 14a
    /// ceiling, which is most of the value on a flat tariff.
    pub fixed_ct_per_kwh: Option<f64>,
    /// The network working price, ct/kWh.
    pub network_ct_per_kwh: f64,
    /// The § 14a module the household is on.
    pub modul: Modul,
    /// Modul 1's annual reduction, euros.
    ///
    /// A lump sum, so it never changes a marginal price and never changes what
    /// the optimiser does — it is here because it is what the household is
    /// billed, and a saving figure that left it out would be wrong by exactly
    /// this much.
    pub modul_1_reduction_eur_per_year: f64,
    /// What exporting earns under the EEG, ct/kWh. Zero means nothing is paid.
    pub feed_in_ct_per_kwh: f64,
    /// The supplier's annual standing charge, euros.
    pub standing_charge_eur_per_year: f64,
}

impl Default for TariffSettings {
    fn default() -> Self {
        Self {
            markup_ct_per_kwh: 3.0,
            fallback_ct_per_kwh: 20.0,
            fixed_ct_per_kwh: None,
            network_ct_per_kwh: 10.0,
            modul: Modul::Modul1,
            modul_1_reduction_eur_per_year: 120.0,
            feed_in_ct_per_kwh: 7.86,
            standing_charge_eur_per_year: 120.0,
        }
    }
}

impl TariffSettings {
    /// The tariff this describes, priced against `spot` where it has a price.
    ///
    /// `site` is read for one thing only and it is the one a tariff cannot know:
    /// whether § 51 EEG has reached this plant, which turns on when its
    /// intelligent metering system was **fitted** and not on anything the
    /// supplier agreed (`hems_grid::para9::para51_applies_from`).
    #[must_use]
    pub fn tariff(&self, site: &Site, spot: BTreeMap<Slot, Decimal>) -> Tariff {
        let ct = |v: f64| Decimal::from_f64_retain(v).unwrap_or_default().round_dp(4);
        Tariff {
            energy: match self.fixed_ct_per_kwh {
                Some(fixed) => EnergyPrice::Fixed {
                    ct_per_kwh: ct(fixed),
                },
                None => EnergyPrice::Dynamic {
                    spot,
                    markup_ct_per_kwh: ct(self.markup_ct_per_kwh),
                    fallback_ct_per_kwh: ct(self.fallback_ct_per_kwh),
                },
            },
            network: match self.modul {
                Modul::None => NetworkCharge::None {
                    arbeitspreis: ct(self.network_ct_per_kwh),
                },
                Modul::Modul1 => NetworkCharge::Modul1 {
                    arbeitspreis: ct(self.network_ct_per_kwh),
                    reduction_eur_per_year: ct(self.modul_1_reduction_eur_per_year),
                },
                Modul::Modul2 => NetworkCharge::Modul2 {
                    arbeitspreis: ct(self.network_ct_per_kwh),
                    // 60 % off the working price, at a Marktlokation of its own
                    // — so 40 % remains, and the second metering point carries
                    // its own annual charge. `hems_tariff::advisor` is what says
                    // whether the trade pays for a given household.
                    remaining_share: Decimal::new(4, 1),
                    metering_eur_per_year: Decimal::new(25, 0),
                },
            },
            levies: Levies::household_2026(),
            feed_in: if self.feed_in_ct_per_kwh > 0.0 {
                FeedIn::eeg(ct(self.feed_in_ct_per_kwh))
            } else {
                FeedIn {
                    scheme: hems_tariff::tariff::Remuneration::None,
                    para51_from: None,
                }
            }
            .under_para51_from(
                hems_grid::para9::GenerationProfile::of_site(site)
                    .as_ref()
                    .and_then(hems_grid::para9::para51_applies_from),
            ),
            sharing: None,
            standing_charge_eur_per_year: ct(self.standing_charge_eur_per_year),
        }
    }
}

/// Which § 14a network-charge module the household chose.
///
/// Modul 3 is deliberately absent, and the absence is a refusal rather than an
/// omission: it needs the network operator's own Zählzeitdefinition, and this
/// workspace will not invent one. A curated calendar per operator is `tariffd`'s
/// job, and until a box can be handed one, a household on Modul 3 is better
/// planned as Modul 1 than as an invented set of windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Modul {
    /// No § 14a module at all.
    None,
    /// A flat annual reduction, the working price unchanged.
    #[default]
    Modul1,
    /// 60 % off the working price, at a Marktlokation of its own.
    Modul2,
}

/// Where the box asks for prices and weather.
///
/// Both are **optional**, and a box with neither still runs: the guard and the
/// arbiter need nothing but measurements, which is the whole of G3. What it
/// loses is the plan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FleetSettings {
    /// `tariffd`'s base URL, e.g. `http://tariffd.internal:7380`.
    pub tariffd_url: Option<String>,
    /// `forecastd`'s base URL.
    pub forecastd_url: Option<String>,
    /// Which of `forecastd`'s configured locations is this household's.
    ///
    /// A name rather than a latitude: `forecastd` fetches the sky for locations
    /// its *operator* configured, and the sun position a production figure is
    /// computed from has to be the one the irradiance was fetched at. A box that
    /// could name its own coordinates would be given somebody else's sky.
    pub location: Option<String>,
    /// How long a request may take, seconds.
    #[serde(default = "default_request_timeout_s")]
    pub request_timeout_s: u64,
}

fn default_request_timeout_s() -> u64 {
    10
}

/// The house, in the units its paperwork is written in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SiteSettings {
    /// Installed photovoltaic power, kWp.
    ///
    /// Zero describes a roof of no size rather than no roof: the asset is still
    /// there, and it is still something the arbiter decides for and the S2
    /// description names. That is a limitation of the site model rather than a
    /// choice — `Household::build` builds the same assets whatever this says —
    /// and it is why `/v1/status` lists the controllable devices no driver
    /// speaks for.
    pub pv_kwp: f64,
    /// The inverter's alternating-current limit, kW.
    pub pv_ac_kw: f64,
    /// Battery capacity, kWh. Zero is a battery of no size, not the absence of
    /// one — see [`SiteSettings::pv_kwp`].
    pub battery_kwh: f64,
    /// Battery power in both directions, kW.
    pub battery_kw: f64,
    /// The fraction of the battery held back for a power cut, 0…1.
    pub reserve_soc: f64,
    /// What a kilowatt-hour of battery throughput costs in wear, €/kWh.
    ///
    /// Leave it at zero and the plan will cycle the pack for a spread that does
    /// not cover the damage — measured at up to ten times the saving in the
    /// literature. The cell price over the warranted throughput is the figure;
    /// a €4 000 pack warranted for 2,4 MWh per kWh of capacity is about 8 ct.
    pub battery_wear_eur_per_kwh: f64,
    /// The main fuse, amperes per outer conductor.
    pub fuse_a: f64,
    /// The federal state whose public holidays decide a day type, ISO 3166-2:DE
    /// (`"BE"`, `"BY"`, `"NW"` …).
    ///
    /// It matters because a load profile is indexed by day type and a public
    /// holiday counts as a Sunday: Fronleichnam is a working day in Berlin and
    /// is not in Bayern, so a box in the wrong Land learns the wrong Thursdays.
    pub bundesland: metering::Bundesland,
    /// Where the house is, for the solar geometry.
    pub latitude: f64,
    /// Likewise.
    pub longitude: f64,
    /// Metres above sea level.
    pub altitude_m: f64,
    /// Electrical power of the heat pump at full output, kW.
    pub heat_pump_kw: f64,
    /// Whether the heat pump modulates rather than switching on and off.
    pub heat_pump_modulating: bool,
    /// The bottom of the comfort band, °C.
    pub comfort_min_c: f64,
    /// The top of the comfort band, °C.
    pub comfort_max_c: f64,
    /// Volume of the hot-water tank, litres. Zero is a tank of no size, not the
    /// absence of one — see [`SiteSettings::pv_kwp`].
    pub dhw_litres: f64,
    /// Electrical power of the hot-water heater, kW.
    pub dhw_heater_kw: f64,
    /// Whether the charge point can drop to a single conductor.
    ///
    /// It decides whether two kilowatts of surplus charge a car or are
    /// exported: three-phase charging cannot start below 4,14 kW, single-phase
    /// below 1,38 kW. Almost every wallbox sold in Germany since 2022 can.
    pub evse_switchable: bool,
    /// The state of charge the household asks its car to stop at, 0…1.
    pub ev_charge_limit: Option<f64>,
    /// Whether an intelligent metering system with a control device is in
    /// operation, which is what lifts the § 9 Abs. 2 EEG 60 % feed-in cap.
    ///
    /// **Off by default, and that is the realistic answer rather than the tidy
    /// one.** § 9 Abs. 2 lifts the cap only after the network operator's first
    /// successful Ansteuerbarkeit test, which is a different event on a
    /// different clock from the meter being fitted. A box that assumed the cap
    /// was gone would plan a roof it is not allowed to have.
    pub imsys_control_device: bool,
    /// The date an intelligent metering system was put into operation, if one
    /// was — which is what starts § 51 EEG taking the negative quarter hours.
    ///
    /// RFC 3339 date, `2024-06-01`.
    pub imsys_since: Option<String>,
    /// The programme a shiftable appliance is loaded with, as the average power
    /// in kW of each consecutive quarter hour.
    ///
    /// Steps rather than a duration and an average, because a dishwasher draws
    /// two kilowatts while it heats and two hundred watts while it washes. A
    /// planner given the average schedules seven hundred watts into every sunny
    /// slot, which no dishwasher will do.
    pub dishwasher_kw_steps: Vec<f64>,
}

impl Default for SiteSettings {
    /// The reference German household of 2026 — the same one the simulated days
    /// run against, so a box with no configuration file behaves like the house
    /// every figure in this project was measured on.
    fn default() -> Self {
        let reference = HouseholdConfig::default();
        Self {
            pv_kwp: reference.pv_kwp.kw(),
            pv_ac_kw: reference.pv_ac_nominal.kw(),
            battery_kwh: reference.battery_kwh.kwh(),
            battery_kw: reference.battery_power.kw(),
            reserve_soc: reference.reserve_soc.fraction(),
            bundesland: metering::Bundesland::Be,
            battery_wear_eur_per_kwh: reference.battery_wear_eur_per_kwh,
            fuse_a: reference.fuse.get(),
            latitude: reference.location.latitude,
            longitude: reference.location.longitude,
            altitude_m: reference.location.altitude_m,
            heat_pump_kw: reference.heat_pump_power.kw(),
            heat_pump_modulating: reference.heat_pump_modulating,
            comfort_min_c: reference.comfort_min_c,
            comfort_max_c: reference.comfort_max_c,
            dhw_litres: reference.dhw_litres,
            dhw_heater_kw: reference.dhw_heater.kw(),
            evse_switchable: reference.evse_switchable,
            ev_charge_limit: reference.ev_charge_limit.map(Soc::fraction),
            imsys_control_device: false,
            imsys_since: Some("2024-06-01".into()),
            dishwasher_kw_steps: reference
                .dishwasher
                .as_ref()
                .map(|p| p.steps.iter().map(|s| s.kw()).collect())
                .unwrap_or_default(),
        }
    }
}

/// Why a configuration does not describe a house.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SettingsError {
    /// A fraction was given outside 0…1.
    #[error("{field} is {value}, and a fraction is between 0 and 1")]
    NotAFraction {
        /// Which field.
        field: &'static str,
        /// What was given.
        value: String,
    },
    /// A date was given that is not one.
    #[error("{field} is {value:?}, which is not an RFC 3339 date like 2024-06-01")]
    NotADate {
        /// Which field.
        field: &'static str,
        /// What was given.
        value: String,
    },
    /// The comfort band is upside down.
    #[error("the comfort band is {min} °C to {max} °C, which is not a band")]
    NotABand {
        /// The bottom.
        min: f64,
        /// The top.
        max: f64,
    },
}

impl SiteSettings {
    /// The household this describes.
    ///
    /// # Errors
    /// [`SettingsError`] where a fraction is not one, a date is not one, or the
    /// comfort band is upside down. Each of those is a typo that would otherwise
    /// present as a house that is planned strangely for a year.
    pub fn household(&self) -> Result<HouseholdConfig, SettingsError> {
        let fraction = |field: &'static str, value: f64| {
            Soc::new(value).map_err(|_| SettingsError::NotAFraction {
                field,
                value: value.to_string(),
            })
        };
        if self.comfort_max_c < self.comfort_min_c {
            return Err(SettingsError::NotABand {
                min: self.comfort_min_c,
                max: self.comfort_max_c,
            });
        }
        let para9 = match &self.imsys_since {
            Some(date) => {
                let parsed =
                    time::Date::parse(date, &time::format_description::well_known::Iso8601::DATE)
                        .map_err(|_| SettingsError::NotADate {
                        field: "imsys_since",
                        value: date.clone(),
                    })?;
                Para9Status::default().with_imsys_since(parsed)
            }
            None => Para9Status::default(),
        };
        let para9 = if self.imsys_control_device {
            para9.with_relief(hems_core::asset::CapRelief::ImsysWithControl)
        } else {
            para9
        };

        Ok(HouseholdConfig {
            pv_kwp: Power::from_kw(self.pv_kwp),
            pv_ac_nominal: Power::from_kw(self.pv_ac_kw),
            battery_kwh: Energy::from_kwh(self.battery_kwh),
            battery_power: Power::from_kw(self.battery_kw),
            reserve_soc: fraction("reserve_soc", self.reserve_soc)?,
            battery_wear_eur_per_kwh: self.battery_wear_eur_per_kwh,
            fuse: Current::new(self.fuse_a),
            location: GeoPoint {
                latitude: self.latitude,
                longitude: self.longitude,
                altitude_m: self.altitude_m,
            },
            heat_pump_power: Power::from_kw(self.heat_pump_kw),
            comfort_min_c: self.comfort_min_c,
            comfort_max_c: self.comfort_max_c,
            heat_pump_modulating: self.heat_pump_modulating,
            dhw_litres: self.dhw_litres,
            dhw_heater: Power::from_kw(self.dhw_heater_kw),
            para9,
            ev_charge_limit: self
                .ev_charge_limit
                .map(|v| fraction("ev_charge_limit", v))
                .transpose()?,
            dishwasher: (!self.dishwasher_kw_steps.is_empty()).then(|| {
                Programme::from_steps(self.dishwasher_kw_steps.iter().copied().map(Power::from_kw))
            }),
            evse_switchable: self.evse_switchable,
        })
    }
}

/// One driver, and where to find the device it speaks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "kebab-case")]
pub enum DriverSettings {
    /// SunSpec over Modbus TCP — an inverter, a meter or a battery.
    ///
    /// The one protocol that needs no membership, no registration and no
    /// certificate, and the one most inverters sold in Germany speak.
    Sunspec(SunspecSettings),
    /// The EEBUS Controllable System of § 14a — the household side of a
    /// network operator's Steuerbox.
    EebusLpc(EebusSettings),
}

impl DriverSettings {
    /// Which asset this driver speaks for.
    #[must_use]
    pub fn asset(&self) -> &str {
        match self {
            DriverSettings::Sunspec(s) => &s.asset,
            DriverSettings::EebusLpc(s) => &s.asset,
        }
    }
}

/// A SunSpec device on a Modbus TCP address.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SunspecSettings {
    /// Which asset of the site this device is.
    pub asset: String,
    /// `host:port`. Modbus TCP is 502 by convention.
    pub address: String,
    /// The Modbus unit identifier. One gateway can front several devices.
    #[serde(default = "default_unit")]
    pub unit: u8,
    /// How often a full read is issued, milliseconds.
    ///
    /// A floor rather than a schedule: the driver does not start a new poll
    /// while one is outstanding, so a device that answers in two seconds is
    /// polled every two.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// How long an unanswered request may stand before the link is stale.
    ///
    /// Has to be **shorter** than the guard's `max_measurement_age`, or a
    /// device could be silent for a whole control period while the guard still
    /// counted its last reading as fresh.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Read and never write.
    ///
    /// What a meter is, and what an inverter should be where the household has
    /// not agreed that hems may curtail it.
    #[serde(default)]
    pub listens_only: bool,
    /// The inverter's alternating-current rating in kW, for curtailment.
    ///
    /// SunSpec model 123 expresses a production ceiling as a **percentage of
    /// the rating**, so a driver with no rating cannot express one at all —
    /// which is refused rather than guessed, because guessing it wrong is a
    /// § 9 EEG breach in one direction and a curtailed roof in the other.
    #[serde(default)]
    pub rating_kw: Option<f64>,
}

fn default_unit() -> u8 {
    1
}

fn default_poll_ms() -> u64 {
    1_000
}

fn default_timeout_ms() -> u64 {
    5_000
}

/// The EEBUS Controllable System, and the Energy Guard it answers to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EebusSettings {
    /// Which asset the limit applies to — the connection point, ordinarily.
    pub asset: String,
    /// What the household restrains itself to when the operator goes quiet, kW.
    ///
    /// `None` computes it from the site: `[A1 4.5.2]`'s minimum grows with the
    /// number of controllable devices, and that is the right answer. A box that
    /// falls back to a vendor's flat 4,2 kW on a household owed 10,5 kW has
    /// given away six kilowatts nobody asked it to.
    #[serde(default)]
    pub failsafe_kw: Option<f64>,
    /// How long the failsafe is held at minimum, hours. Two to twenty-four.
    #[serde(default = "default_failsafe_hours")]
    pub failsafe_hours: u64,
    /// How this box names itself on the EEBUS network — the vendor part of the
    /// SPINE device address, `i:46925` or `n:hems`.
    #[serde(default)]
    pub spine_vendor: Option<String>,
    /// What distinguishes this box from the next one off the same line.
    #[serde(default)]
    pub spine_unique: Option<String>,
}

fn default_failsafe_hours() -> u64 {
    2
}

/// Every driver's asset, and how many drivers name it.
///
/// A duplicate is refused by [`crate::drivers::Registry::register`] anyway; this
/// is what lets the daemon say so before it has opened a socket, which is where
/// a configuration mistake belongs.
#[must_use]
pub fn assets_named(drivers: &[DriverSettings]) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for driver in drivers {
        *counts.entry(driver.asset()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_configuration_is_the_reference_household() {
        // A box with no file behaves like the house every figure in this
        // project was measured on, which is what makes those figures
        // reproducible by somebody who has just installed it.
        let settings = Settings::default();
        let built = settings.site.household().expect("the defaults are valid");
        assert_eq!(built, HouseholdConfig::default());
    }

    /// The example file that ships with the daemon.
    ///
    /// Parsed by a test rather than trusted, because a commented example that
    /// no longer matches the struct it documents is worse than none: it is
    /// wrong with authority, and the person it misleads is an installer in a
    /// cellar. `include_str!` makes the example a build input.
    const EXAMPLE: &str = include_str!("../hemsd.example.toml");

    #[test]
    fn the_example_configuration_parses_and_describes_a_house() {
        let settings: Settings = toml::from_str(EXAMPLE).expect("the shipped example parses");
        settings
            .site
            .household()
            .expect("and describes a household");
        assert!(
            !settings.drivers.is_empty(),
            "an example with no drivers would document a box that manages nothing"
        );
    }

    #[test]
    fn a_driver_list_is_read_as_the_devices_it_names() {
        let settings: Settings = toml::from_str(EXAMPLE).expect("the shipped example parses");
        let named = assets_named(&settings.drivers);
        assert!(
            named.values().all(|n| *n == 1),
            "two drivers for one asset are two sources of truth about one meter: {named:?}"
        );
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        // The difference between a deployment that has not been customised and
        // one that has been customised wrongly. A typo in a field name that was
        // silently ignored is a box running on a default nobody chose.
        let text = "[site]\nbattery_kwh = 10.0\nbattery_kw_h = 10.0\n";
        assert!(toml::from_str::<Settings>(text).is_err());
    }

    #[test]
    fn a_fraction_outside_zero_to_one_is_a_typo_and_is_named() {
        let settings = SiteSettings {
            reserve_soc: 30.0,
            ..SiteSettings::default()
        };
        assert!(matches!(
            settings.household(),
            Err(SettingsError::NotAFraction {
                field: "reserve_soc",
                ..
            })
        ));
    }

    #[test]
    fn an_upside_down_comfort_band_is_refused() {
        let settings = SiteSettings {
            comfort_min_c: 23.0,
            comfort_max_c: 20.0,
            ..SiteSettings::default()
        };
        assert!(matches!(
            settings.household(),
            Err(SettingsError::NotABand { .. })
        ));
    }

    #[test]
    fn the_feed_in_cap_stays_on_until_the_operator_has_tested_the_box() {
        // § 9 Abs. 2 EEG lifts the 60 % cap only after the network operator's
        // first successful Ansteuerbarkeit test — a different event on a
        // different clock from the meter being fitted. Defaulting the other way
        // would plan a roof the household is not allowed to have.
        let built = SiteSettings::default()
            .household()
            .expect("the defaults are valid");
        assert_eq!(built.para9, HouseholdConfig::default().para9);
    }
}
