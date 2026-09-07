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
use hems_core::asset::{LegacyStatus, SteuVeExemption};
use hems_core::prelude::{
    BuildingClass, Current, Energy, GeoPoint, MaloId, MeloId, Para9Status, Power, Rc2, Site, Slot,
    Soc,
};
use hems_tariff::levies::Levies;
use hems_tariff::tariff::{EnergyPrice, FeedIn, NetworkCharge, Tariff};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::site::{ConnectionConfig, Declared, HouseholdConfig, REFERENCE_ASSETS};

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
    /// Where the box reports how the day went.
    pub obsd: crate::runtime::outbox::ObsdSettings,
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
    /// Where the box keeps its own one-second measurement series, and for how
    /// long.
    ///
    /// `None` keeps none, which is what a demonstration wants. On a household
    /// box it is the difference between being able to answer *what was the
    /// battery doing at half past two* and not: the guard reads every meter once
    /// a control period, and without this every one of those readings is
    /// discarded the moment it has been acted on.
    ///
    /// It is a **separate** store from `store_path` and holds no settlement
    /// quantity — a series field is an `f64` and a MiSpeL register is an exact
    /// decimal (D168).
    #[serde(default)]
    pub series: Option<SeriesSettings>,
}

/// The box's measurement series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesSettings {
    /// The directory it lives in. `chronix` takes an exclusive lock on it, so
    /// two `hemsd` processes on one box is a start-up failure rather than two
    /// half-written histories.
    pub path: std::path::PathBuf,
    /// How many days of it to keep.
    ///
    /// Seven by default. It is a *diagnostic* history rather than the statutory
    /// one — `[A1 7.3]`'s two years are events and registers, and they are in
    /// the other store — so the window is what somebody would actually look
    /// back over, and the flash it costs is a household's.
    #[serde(default = "default_series_days")]
    pub keep_days: u16,
}

const fn default_series_days() -> u16 {
    7
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
    ///
    /// It decides one more thing, and it is the § 9 EEG one. The guard lends the
    /// inverter the household's own consumption as feed-in headroom, which the
    /// statute allows because it bounds the Einspeisung at the connection point
    /// — but a loan is outstanding until the guard next runs, so a thermostat
    /// that cuts out puts the roof over a statutory ceiling for exactly one of
    /// these. `GuardConfig::lend_window` is what turns that into a fraction: at
    /// a second the whole draw is lendable, and a box asked to tick once a
    /// minute lends a sixth of it and curtails the rest.
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
    /// What this household will pay to avoid something other than money.
    ///
    /// Both default to zero, which is the plain economic plan. They are here
    /// because for a long time they were reachable from nothing: the solver read
    /// them and no configuration could set them, so a household that had bought
    /// a battery for independence had no way to say so.
    pub preferences: Preferences,
    /// How the planner treats the fact that its forecasts are wrong.
    ///
    /// The median by default, which is what every figure in this workspace is
    /// measured against — and, until this field existed, the only policy a real
    /// box could run at all. The whole scenario construction (three futures on
    /// the band the forecast already publishes, non-anticipative on the first
    /// slot, a Rockafellar–Uryasev tail) was reachable from `hemsd simulate`
    /// and from nothing on a wall, so a household could not act on the very
    /// trade-off `hemsd risk` measured for it.
    pub risk: RiskSettings,
}

/// How the planner treats the fact that its forecasts are wrong.
///
/// The measured answer is in `concepts/PLANNER.md`: over twenty seeded weathers
/// on each of two days, three futures beat the median on the **mean** where a
/// service is at risk (€2,96 against €2,81) and take the undelivered charge from
/// €0,07 to €0,01 — and cost **€1,04 a day** where nothing is at risk, while
/// improving no worst day at all. That is why the default is one median and why
/// this is a household's decision rather than an inherited one: three futures
/// also cost about five to seven times the solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskSettings {
    /// One future, the median of both forecasts, priced as though it were
    /// certain. What every deterministic energy manager does, and the plan every
    /// reference figure here is calibrated against.
    #[default]
    Median,
    /// Three futures from the band the forecast already carries, minimising the
    /// **expected** cost across them.
    ///
    /// Better than a single pessimistic quantile even with no tail weight,
    /// because the model has *priced* all three rather than assuming one.
    Expected,
    /// The same three futures, with a third of the objective on the worst.
    ///
    /// A household that would rather not be caught out — and the honest
    /// statement about it is that it buys a better **mean where a service is at
    /// risk** and a smaller shortfall, not a better bad day.
    Hedged,
    /// One chosen quantile of each forecast — the cheap robustness knob.
    ///
    /// Pessimistic production and pessimistic load, one solve. It is the
    /// inferior construction and the documentation says so: a quantile plan is
    /// optimal against a world nobody expects, and the household pays the hedge
    /// on every ordinary day with no credit on the bad one, because the model
    /// never priced the bad one. It is here because a household with a small
    /// battery against a large array sometimes wants exactly that and nothing
    /// more expensive — a case the code claimed to serve while no configuration
    /// could select it.
    Pessimistic,
}

impl RiskSettings {
    /// This setting as the planner's own type.
    ///
    /// `adaptive` is deliberately absent: it triggers on
    /// `EvSession::tightness`, and a real box has no charging session in its
    /// plan yet — the car is a *driver* away (see `concepts/ROADMAP.md`). A
    /// policy that could never fire would be a fifth name for the median.
    #[must_use]
    pub fn model(self) -> hems_optimizer::Risk {
        use hems_optimizer::Quantile;
        match self {
            Self::Median => hems_optimizer::Risk::deterministic(),
            Self::Expected => hems_optimizer::Risk::expected(),
            Self::Hedged => hems_optimizer::Risk::hedged(),
            // Dull *and* hungry: a household's bad day is the correlated one.
            Self::Pessimistic => hems_optimizer::Risk::at_quantile(Quantile::P10, Quantile::P90),
        }
    }
}

/// What a household is willing to pay to avoid a kilogram of carbon dioxide and
/// a kilowatt-hour off the grid.
///
/// **Prices, not an objective switch**, and that is D15: an enum of goals —
/// cost, carbon, self-sufficiency — replaces the energy price with grams while
/// leaving battery wear, comfort and curtailment in euros, so the plan minimises
/// a sum of two currencies and behaves only because the two happen to be numbers
/// of a similar size. Expressed as prices they are comparable with every other
/// term, they add up, and setting one to zero switches it off.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Preferences {
    /// What a kilogram of carbon dioxide is worth avoiding, €/kg.
    ///
    /// Zero ignores the grid's carbon intensity. The German price for heating
    /// and transport fuels is the obvious anchor — 55 €/t is `0.055` — and the
    /// intensity itself comes from the price stack (Energy-Charts), so a plan
    /// with a carbon price moves load towards the hours the grid is clean
    /// rather than only the hours it is cheap.
    pub co2_eur_per_kg: f64,
    /// What a kilowatt-hour taken from the grid is worth avoiding **beyond**
    /// what it costs, €/kWh.
    ///
    /// The self-sufficiency dial, and it is honest about its price: a value near
    /// the spread makes the plan prefer its own roof even where importing would
    /// be marginally cheaper, and the difference is what independence cost. That
    /// is what somebody who bought a battery for autarky actually wants, and it
    /// belongs in the objective rather than in a marketing figure.
    pub autarky_eur_per_kwh: f64,
}

impl Preferences {
    /// These preferences as the optimiser's own type.
    #[must_use]
    pub fn objective(&self) -> hems_optimizer::model::Objective {
        hems_optimizer::model::Objective::cost()
            .with_carbon_price(self.co2_eur_per_kg)
            .with_autarky_premium(self.autarky_eur_per_kwh)
    }
}

impl Default for ControlSettings {
    fn default() -> Self {
        Self {
            tick_period_s: 1,
            replan_every_s: 300,
            horizon_slots: 96 * 2,
            solve_budget_s: 10.0,
            preferences: Preferences::default(),
            risk: RiskSettings::default(),
        }
    }
}

impl ControlSettings {
    /// The control period, as the guard wants it.
    #[must_use]
    pub const fn tick_period(&self) -> time::Duration {
        time::Duration::seconds(self.tick_period_s.cast_signed())
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
    /// The network operator's Modul 3 calendar, where the household is on one.
    ///
    /// Required by [`Modul::Modul3`] and refused with any other module, because
    /// a calendar nothing reads is a calendar nobody notices is wrong.
    #[serde(default)]
    pub modul3: Option<Modul3Settings>,
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
            modul3: None,
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
                // A household on Modul 3 with no calendar is refused at
                // start-up (`Config::check`), so reaching this arm without one
                // is a box that was started past its own gate. The flat charge
                // is the conservative answer rather than an invented set of
                // windows: it prices every hour the same, so the plan shifts
                // nothing for a spread it cannot see.
                Modul::Modul3 => match self
                    .modul3
                    .as_ref()
                    .and_then(|m| m.calendar().map(|c| (m, c)))
                {
                    Some((m, calendar)) => NetworkCharge::Modul3 {
                        calendar,
                        ht: ct(m.ht_ct_per_kwh),
                        st: ct(m.st_ct_per_kwh),
                        nt: ct(m.nt_ct_per_kwh),
                        // Modul 3 is only available together with Modul 1
                        // (§ 1 of the Anwendungshilfe), so the annual reduction
                        // comes with it and is the same lump sum.
                        reduction_eur_per_year: ct(self.modul_1_reduction_eur_per_year),
                    },
                    None => NetworkCharge::None {
                        arbeitspreis: ct(self.network_ct_per_kwh),
                    },
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
            carbon_g_per_kwh: BTreeMap::new(),
            standing_charge_eur_per_year: ct(self.standing_charge_eur_per_year),
        }
    }
}

/// Which § 14a network-charge module the household chose.
///
/// # Modul 3 is transcribed, never invented
///
/// There is no machine-readable national format for a Modul 3 calendar: it is a
/// PDF or an Excel sheet per network operator. This workspace will not invent
/// one — but an installer standing in a cellar with the operator's price sheet
/// can *transcribe* it, and [`Modul3Settings`] is where. What makes that safe
/// rather than a second way to be wrong is that the box **refuses to start** on
/// a calendar that breaks the Anwendungshilfe: `run --check` runs
/// `hems_grid::modul3::Modul3Calendar::assess` before a byte moves, and a
/// household billed on a non-conformant calendar is a year of somebody's
/// electricity priced against a tariff nobody may sell.
///
/// See `concepts/DECISIONS.md` D126.
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
    /// Time-variable network charges, on the operator's own calendar.
    ///
    /// Needs `[tariff.modul3]`; a box configured for it without one refuses to
    /// start rather than falling back to a flat charge, because a household that
    /// asked to be billed in windows and is planned on an average is one whose
    /// energy manager is moving load for a spread it is not charged.
    Modul3,
}

/// The network operator's Modul 3 calendar, as an installer transcribes it.
///
/// The shape is [`hems_grid::modul3::Transcription`] — one transcription
/// format shared with `tariffd`'s curated per-Netzbetreiber catalogue, so a
/// calendar copied out of a price sheet once is portable between a single box
/// and a fleet. The Europe/Berlin calendar, the Bundesland's statutory
/// holidays and the two awkward days of the year are `metering`'s
/// (`hems_grid::modul3::Modul3Calendar`), so a Sunday is not a Hochtarif, the
/// repeated hour of the long October day is inside the same window twice, and
/// the skipped hour of the short March day is inside none.
pub type Modul3Settings = hems_grid::modul3::Transcription;

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
    /// **Zero means the household has no roof, and the site says so**: no PV
    /// asset is built, the arbiter never decides for one, the S2 description
    /// never names one, and `run --check` refuses a driver configured for one.
    pub pv_kwp: f64,
    /// The inverter's alternating-current limit, kW.
    pub pv_ac_kw: f64,
    /// How far the modules are tilted from horizontal, degrees — 0 flat, 90 a
    /// wall. Roughly the pitch of the roof they are on.
    pub pv_tilt_deg: f64,
    /// Which way they face, degrees clockwise from north: 180 south, 90 east,
    /// 270 west.
    ///
    /// The single most consequential number in this section after the peak
    /// power, and the one nobody thinks to change. It decides *when* the roof
    /// produces, and an east–west array left on the default produces two
    /// shoulders where the plan expects one midday peak. The residual corrector
    /// learns a *level*; it takes it seasons to learn a shape, and it is bounded
    /// while it does.
    pub pv_azimuth_deg: f64,
    /// The § 9 EEG facts declared about the roof.
    #[serde(default)]
    pub para9: Para9Settings,
    /// The contractually agreed connection power, kW, where one is agreed.
    ///
    /// It narrows the guard's headroom, the planner's import ceiling and the
    /// `ContractualConsumptionNominalMax` an EEBUS Energy Guard is told
    /// `[LPC-042]`. Omit it and the only limit any of the three sees is the
    /// fuse.
    #[serde(default)]
    pub contract_power_kw: Option<f64>,
    /// The market location, eleven digits with a valid check digit.
    #[serde(default)]
    pub malo: Option<String>,
    /// The metering location, thirty-three characters.
    #[serde(default)]
    pub melo: Option<String>,
    /// The network operator's BDEW code number, from the § 14a agreement.
    #[serde(default)]
    pub dso_code: Option<String>,
    /// The Netzbereich the operator assigned this connection to `[A1 8.2.b]` —
    /// what lets a household find its own row in the monthly publication.
    #[serde(default)]
    pub netzbereich: Option<String>,
    /// Which house this is, thermally, until the box has identified its own.
    #[serde(default)]
    pub building: BuildingSettings,
    /// The § 14a and § 9 EEG facts declared per asset, keyed by identifier.
    ///
    /// `[site.declared.waermepumpe]`, `[site.declared.pv]`, and so on. A key
    /// that names no asset this household has is refused at start-up, the same
    /// way a `[[drivers]]` entry for one is: a declaration nothing reads looks
    /// exactly like a declaration that worked.
    #[serde(default)]
    pub declared: BTreeMap<String, DeclaredSettings>,
    /// Battery capacity, kWh. Zero means the household has no battery — see
    /// [`SiteSettings::pv_kwp`].
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
    /// Electrical power of the heat pump at full output, kW. Zero means the
    /// household has no heat pump — see [`SiteSettings::pv_kwp`].
    pub heat_pump_kw: f64,
    /// Whether the heat pump modulates rather than switching on and off.
    pub heat_pump_modulating: bool,
    /// How the heat pump takes instructions.
    #[serde(default)]
    pub heat_pump_control: HeatPumpInterface,
    /// The bottom of the comfort band, °C.
    pub comfort_min_c: f64,
    /// The top of the comfort band, °C.
    pub comfort_max_c: f64,
    /// Volume of the hot-water tank, litres. Zero means the household has no
    /// tank — see [`SiteSettings::pv_kwp`].
    pub dhw_litres: f64,
    /// Electrical power of the hot-water heater, kW.
    pub dhw_heater_kw: f64,
    /// The charge point's largest current per conductor, amperes. Zero means
    /// the household has no charge point — see [`SiteSettings::pv_kwp`].
    ///
    /// 16 A three-phase is the ordinary "11 kW" wallbox, 32 A the "22 kW" one.
    pub evse_max_a: f64,
    /// Whether the charge point can drop to a single conductor.
    ///
    /// It decides whether two kilowatts of surplus charge a car or are
    /// exported: three-phase charging cannot start below 4,14 kW, single-phase
    /// below 1,38 kW. Almost every wallbox sold in Germany since 2022 can.
    pub evse_switchable: bool,
    /// The state of charge the household asks its car to stop at, 0…1.
    pub ev_charge_limit: Option<f64>,
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
        let pv = reference.pv.expect("the reference household has a roof");
        let battery = reference
            .battery
            .expect("the reference household has a battery");
        let evse = reference
            .evse
            .expect("the reference household has a charge point");
        let heat_pump = reference
            .heat_pump
            .expect("the reference household has a heat pump");
        let dhw = reference
            .dhw
            .expect("the reference household has a hot-water tank");
        Self {
            pv_kwp: pv.kwp.kw(),
            pv_ac_kw: pv.ac_nominal.kw(),
            pv_tilt_deg: pv.tilt_deg,
            pv_azimuth_deg: pv.azimuth_deg,
            para9: Para9Settings::from(pv.para9),
            contract_power_kw: reference.connection.contract_power.map(Power::kw),
            malo: reference.connection.malo.map(|m| m.to_string()),
            melo: reference.connection.melo.map(|m| m.to_string()),
            dso_code: reference.connection.dso_code.clone(),
            netzbereich: reference.connection.netzbereich.clone(),
            building: BuildingSettings::default(),
            declared: reference
                .declared
                .iter()
                .map(|(id, d)| (id.clone(), DeclaredSettings::from(*d)))
                .collect(),
            battery_kwh: battery.kwh.kwh(),
            battery_kw: battery.power.kw(),
            reserve_soc: battery.reserve_soc.fraction(),
            bundesland: metering::Bundesland::Be,
            battery_wear_eur_per_kwh: battery.wear_eur_per_kwh,
            fuse_a: reference.fuse.get(),
            latitude: reference.location.latitude,
            longitude: reference.location.longitude,
            altitude_m: reference.location.altitude_m,
            heat_pump_kw: heat_pump.power.kw(),
            heat_pump_modulating: heat_pump.modulating,
            heat_pump_control: heat_pump.control.into(),
            comfort_min_c: heat_pump.comfort_min_c,
            comfort_max_c: heat_pump.comfort_max_c,
            dhw_litres: dhw.litres,
            dhw_heater_kw: dhw.heater.kw(),
            evse_max_a: evse.max_current.get(),
            evse_switchable: evse.switchable,
            ev_charge_limit: evse.charge_limit.map(Soc::fraction),
            dishwasher_kw_steps: reference
                .dishwasher
                .as_ref()
                .map(|p| p.steps.iter().map(|s| s.kw()).collect())
                .unwrap_or_default(),
        }
    }
}

/// Which house this is, thermally.
///
/// Either an archetype — what an installer can answer from the front door — or
/// the four parameters of the two-mass model, for somebody who has them from a
/// building simulation. An explicit parameter wins over the archetype, so a
/// household can pick the closest class and correct the one number it knows.
///
/// It is a **prior**, not a fact: the box identifies its own building from the
/// household's own thermometer once it has watched a few excited days
/// (`hems_forecast::building`). What the prior buys is the weeks before that,
/// and the difference is not cosmetic — the fabric capacity decides whether
/// pre-heating into a cheap hour pays at all, and it spans a factor of five
/// across the classes.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BuildingSettings {
    /// The archetype, from [`BuildingClass`]: `average` (the default),
    /// `new-build`, `solid-wall` or `apartment`.
    pub class: BuildingClass,
    /// Heat capacity of the indoor air and the furniture that follows it,
    /// kWh/K. Overrides the archetype's.
    pub air_capacity_kwh_per_k: Option<f64>,
    /// Heat capacity of the building fabric, kWh/K — the large, slow one, and
    /// the parameter a heating plan turns on.
    pub mass_capacity_kwh_per_k: Option<f64>,
    /// Thermal resistance from the indoor air to outdoors, K/kW.
    ///
    /// `ΔT / Q` at the design pair: a house losing 6,8 kW at 21 °C indoors and
    /// −20 °C outdoors is `41 / 6,8` = 6 K/kW.
    pub r_air_out_k_per_kw: Option<f64>,
    /// Thermal resistance from the indoor air to the fabric, K/kW.
    pub r_air_mass_k_per_kw: Option<f64>,
}

impl BuildingSettings {
    /// The model this describes.
    ///
    /// # Errors
    /// [`SettingsError::NotABuilding`] where the result is not physical — a
    /// non-positive capacity or resistance, which makes the planner's own
    /// temperature predictions diverge rather than merely be wrong.
    pub fn rc2(&self) -> Result<Rc2, SettingsError> {
        let base = self.class.rc2();
        let built = Rc2 {
            air_capacity_kwh_per_k: self
                .air_capacity_kwh_per_k
                .unwrap_or(base.air_capacity_kwh_per_k),
            mass_capacity_kwh_per_k: self
                .mass_capacity_kwh_per_k
                .unwrap_or(base.mass_capacity_kwh_per_k),
            r_air_out_k_per_kw: self.r_air_out_k_per_kw.unwrap_or(base.r_air_out_k_per_kw),
            r_air_mass_k_per_kw: self.r_air_mass_k_per_kw.unwrap_or(base.r_air_mass_k_per_kw),
        };
        if built.is_valid() {
            Ok(built)
        } else {
            Err(SettingsError::NotABuilding {
                found: format!("{built:?}"),
            })
        }
    }
}

/// The § 14a and § 9 EEG facts declared about one asset.
///
/// Every one of them is a fact off a contract or a Netzanschlussportal record
/// rather than off a datasheet, which is why the box has to be told and cannot
/// work it out. See [`crate::site::Declared`] for what each decides and which
/// way silence errs.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeclaredSettings {
    /// The date of technical commissioning, `2019-04-01`.
    ///
    /// It decides the § 9 EEG feed-in limitation for a roof and the § 14a
    /// regime for a controllable device, and the two read silence in opposite
    /// directions — see [`crate::site::Declared::commissioned_at`].
    pub commissioned: Option<String>,
    /// What the device brings with it from before 2024 `[A1 10]`: `none` (the
    /// default), `reduced_network_fee` or `nachtspeicher`.
    pub legacy_status: LegacyStatus,
    /// Whether the operator moved it into the netzorientierte Steuerung
    /// voluntarily `[A1 10.4]`. Irreversible, and the operator may not refuse.
    pub switched_voluntarily: bool,
    /// Why it is not a steuerbare Verbrauchseinrichtung at all `[A1 3.1.b]`:
    /// `public_charge_point`, `emergency_services` or
    /// `non_residential_heating_or_cooling`.
    pub exemption: Option<SteuVeExemption>,
}

impl From<Declared> for DeclaredSettings {
    /// The round trip, so `SiteSettings::default()` can be written from the
    /// household it describes rather than from a second copy of the same dates.
    fn from(d: Declared) -> Self {
        Self {
            commissioned: d.commissioned_at.map(iso_date),
            legacy_status: d.legacy_status,
            switched_voluntarily: d.switched_voluntarily,
            exemption: d.exemption,
        }
    }
}

/// The § 9 EEG facts declared about the roof.
///
/// All four in one table because they are one question — *what limits this
/// roof's feed-in* — and the answer is a decision tree over them rather than
/// four independent switches. Flat on `[site]` they read like four unrelated
/// booleans; here the order they are consulted in is visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Para9Settings {
    /// Whether an intelligent metering system **with a control device** is in
    /// operation, which is what lifts the § 9 Abs. 2 60 % cap.
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
    /// ISO date, `2024-06-01`.
    pub imsys_since: Option<String>,
    /// A Steckersolargerät within **both** size tests of § 9 Abs. 2 S. 4 EEG —
    /// 2 kW DC and 800 W AC — which is outside Nr. 3 altogether.
    pub steckersolargeraet: bool,
    /// The § 100 Abs. 3 S. 2 Nr. 2 EEG 70 % limitation: a system commissioned
    /// before 2023 that met its obligation that way rather than by accepting a
    /// Rundsteuerempfänger. Leave it false for anything newer.
    pub legacy_70_percent: bool,
}

impl Default for Para9Settings {
    /// The reference household's roof: an intelligent metering system fitted in
    /// 2024, so § 51 EEG has been taking the negative quarter hours since the
    /// start of 2025 — and the § 9 Abs. 2 cap **still on**, because that one
    /// runs until the operator's first successful Ansteuerbarkeit test and
    /// nobody has run it.
    fn default() -> Self {
        Self::from(Para9Status::default().with_imsys_since(crate::site::REFERENCE_IMSYS_SINCE))
    }
}

impl From<Para9Status> for Para9Settings {
    fn from(p: Para9Status) -> Self {
        Self {
            imsys_control_device: matches!(p.relief, hems_core::asset::CapRelief::ImsysWithControl),
            imsys_since: p.imsys_since.map(iso_date),
            steckersolargeraet: p.steckersolargeraet,
            legacy_70_percent: p.legacy_70_percent,
        }
    }
}

impl Para9Settings {
    /// The § 9 EEG facts this declares.
    ///
    /// # Errors
    /// [`SettingsError::NotADate`] on a meter date that is not one.
    pub fn status(&self) -> Result<Para9Status, SettingsError> {
        let imsys_since = self
            .imsys_since
            .as_deref()
            .map(|date| iso("imsys_since", date))
            .transpose()?;
        Ok(Para9Status {
            relief: if self.imsys_control_device {
                hems_core::asset::CapRelief::ImsysWithControl
            } else {
                hems_core::asset::CapRelief::None
            },
            imsys_since,
            steckersolargeraet: self.steckersolargeraet,
            legacy_70_percent: self.legacy_70_percent,
        })
    }
}

/// An ISO date, or which field was not one.
fn iso(field: &'static str, date: &str) -> Result<time::Date, SettingsError> {
    time::Date::parse(date, &time::format_description::well_known::Iso8601::DATE).map_err(|_| {
        SettingsError::NotADate {
            field,
            value: date.to_owned(),
        }
    })
}

/// A date as a configuration file spells it.
fn iso_date(date: time::Date) -> String {
    date.format(&time::format_description::well_known::Iso8601::DATE)
        .unwrap_or_default()
}

impl DeclaredSettings {
    /// What this declares, with the date parsed.
    ///
    /// # Errors
    /// [`SettingsError::NotADate`] on a commissioning date that is not one. A
    /// typo here is not a small thing: it is the input to both statutory
    /// regimes, and a silent fallback would present as a household planned
    /// strangely for a year.
    pub fn declared(&self) -> Result<Declared, SettingsError> {
        let commissioned_at = self
            .commissioned
            .as_deref()
            .map(|date| iso("commissioned", date))
            .transpose()?;
        Ok(Declared {
            commissioned_at,
            legacy_status: self.legacy_status,
            switched_voluntarily: self.switched_voluntarily,
            exemption: self.exemption,
        })
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
    /// A thermal model was given that no house could have.
    #[error(
        "the building parameters are not physical ({found}); every capacity and \
         resistance has to be finite and greater than zero, or the planner's own \
         temperature predictions diverge"
    )]
    NotABuilding {
        /// What was built from the archetype and the overrides.
        found: String,
    },
    /// An identifier was given that the market would not accept.
    #[error("{field} is {value:?}, which is not a valid {field}: {why}")]
    NotAnIdentifier {
        /// Which field.
        field: &'static str,
        /// What was given.
        value: String,
        /// What the parser said.
        why: String,
    },
    /// Something was declared about an asset this household does not have.
    #[error(
        "[site.declared.{asset}] names an asset this household has none of; \
         the ones it can name are {known}"
    )]
    UndeclarableAsset {
        /// The key that named nothing.
        asset: String,
        /// What it could have named.
        known: String,
    },
}

/// Parse a market identifier, or say which one was wrong and why.
///
/// A MaLo-ID carries a check digit and a MeLo-ID a Vergabestelle prefix, so a
/// transposed pair of digits is *detectable* — and swallowing that with an
/// `.ok()` would put an identifier on a Marktkommunikation message that names
/// somebody else's connection point. Refuse rather than guess (P5).
fn market_id<T: std::str::FromStr<Err = metering::ParseError>>(
    field: &'static str,
    value: Option<&str>,
) -> Result<Option<T>, SettingsError> {
    value
        .map(|v| {
            v.parse::<T>()
                .map_err(|why| SettingsError::NotAnIdentifier {
                    field,
                    value: v.to_owned(),
                    why: why.to_string(),
                })
        })
        .transpose()
}

impl SiteSettings {
    /// The § 14a facts declared per asset, with every key checked.
    ///
    /// # Errors
    /// [`SettingsError::UndeclarableAsset`] for a key that names no asset this
    /// household has — the same rule a `[[drivers]]` entry lives under, and for
    /// the same reason: a § 14a regime declared for a device that does not exist
    /// looks, from every screen, exactly like one that took effect.
    /// [`SettingsError::NotADate`] for a commissioning date that is not one.
    fn declarations(&self) -> Result<BTreeMap<String, Declared>, SettingsError> {
        self.declared
            .iter()
            .map(|(asset, d)| {
                if REFERENCE_ASSETS.contains(&asset.as_str()) {
                    Ok((asset.clone(), d.declared()?))
                } else {
                    Err(SettingsError::UndeclarableAsset {
                        asset: asset.clone(),
                        known: REFERENCE_ASSETS.join(", "),
                    })
                }
            })
            .collect()
    }

    /// What the connection agreement adds to the fuse.
    ///
    /// # Errors
    /// [`SettingsError::NotAnIdentifier`] where a market identifier fails its
    /// own check digit or prefix.
    fn connection(&self) -> Result<ConnectionConfig, SettingsError> {
        Ok(ConnectionConfig {
            contract_power: self.contract_power_kw.map(Power::from_kw),
            malo: market_id::<MaloId>("malo", self.malo.as_deref())?,
            melo: market_id::<MeloId>("melo", self.melo.as_deref())?,
            dso_code: self.dso_code.clone(),
            netzbereich: self.netzbereich.clone(),
        })
    }
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
        let para9 = self.para9.status()?;
        let declared = self.declarations()?;
        let connection = self.connection()?;

        // A size of zero is the configuration saying the household has no such
        // device, and the site then says so too: the asset is simply not built
        // (see the field docs). This is the conversion where "no battery" stops
        // being a number and becomes an absence the type system carries.
        Ok(HouseholdConfig {
            pv: (self.pv_kwp > 0.0).then(|| crate::site::PvConfig {
                kwp: Power::from_kw(self.pv_kwp),
                ac_nominal: Power::from_kw(self.pv_ac_kw),
                tilt_deg: self.pv_tilt_deg,
                azimuth_deg: self.pv_azimuth_deg,
                para9,
            }),
            battery: (self.battery_kwh > 0.0 && self.battery_kw > 0.0)
                .then(|| {
                    Ok::<_, SettingsError>(crate::site::BatteryConfig {
                        kwh: Energy::from_kwh(self.battery_kwh),
                        power: Power::from_kw(self.battery_kw),
                        reserve_soc: fraction("reserve_soc", self.reserve_soc)?,
                        wear_eur_per_kwh: self.battery_wear_eur_per_kwh,
                    })
                })
                .transpose()?,
            evse: (self.evse_max_a > 0.0)
                .then(|| {
                    Ok::<_, SettingsError>(crate::site::EvseConfig {
                        max_current: Current::new(self.evse_max_a),
                        switchable: self.evse_switchable,
                        charge_limit: self
                            .ev_charge_limit
                            .map(|v| fraction("ev_charge_limit", v))
                            .transpose()?,
                    })
                })
                .transpose()?,
            heat_pump: (self.heat_pump_kw > 0.0).then(|| crate::site::HeatPumpConfig {
                power: Power::from_kw(self.heat_pump_kw),
                modulating: self.heat_pump_modulating,
                comfort_min_c: self.comfort_min_c,
                comfort_max_c: self.comfort_max_c,
                control: self.heat_pump_control.into(),
            }),
            dhw: (self.dhw_litres > 0.0 && self.dhw_heater_kw > 0.0).then(|| {
                crate::site::DhwConfig {
                    litres: self.dhw_litres,
                    heater: Power::from_kw(self.dhw_heater_kw),
                }
            }),
            fuse: Current::new(self.fuse_a),
            connection,
            building: self.building.rc2()?,
            declared,
            location: GeoPoint {
                latitude: self.latitude,
                longitude: self.longitude,
                altitude_m: self.altitude_m,
            },
            dishwasher: (!self.dishwasher_kw_steps.is_empty()).then(|| {
                Programme::from_steps(self.dishwasher_kw_steps.iter().copied().map(Power::from_kw))
            }),
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
    /// An EEBUS hot-water circuit, read over MDT.
    ///
    /// The other direction from `eebus-lpc`, and the distinction is what makes
    /// it a separate kind rather than a flag: § 14a is a network operator
    /// dialling *this* box, and a hot-water tank is a device on the household's
    /// own network that this box dials.
    EebusDhw(EebusDhwSettings),
    /// An EEBUS charge point, read over EVCC and EVSOC — whether a car is
    /// plugged in and how full it is.
    ///
    /// It reads and never commands. The charge point is already commanded, over
    /// Modbus or through the § 14a envelope the arbiter shares out, and two
    /// drivers claiming to command one wallbox is one wallbox nobody can
    /// predict.
    EebusEv(EebusEvSettings),
    /// An EEBUS heat-pump compressor, over OHPCF.
    ///
    /// The only driver in the box that can ask an appliance to consume **more**.
    /// Everything else on the grid side is a ceiling, and a ceiling an appliance
    /// is already under changes nothing — so a plan that has worked out the
    /// house will be cheaper if the compressor runs while the roof is exporting
    /// has no other way to say so.
    ///
    /// It commands and measures nothing: what the unit draws is the site meter's
    /// business, and two drivers claiming to measure one asset are refused at
    /// registration.
    EebusHeatPump(EebusHeatPumpSettings),
    /// A device read through a **vendor's own register map** over Modbus TCP.
    ///
    /// For everything that answers Modbus and publishes no SunSpec model list,
    /// which is most of the German heat-pump market: the register numbers are in
    /// a PDF and every unit's are different.
    ///
    /// It reads and never writes, so it pairs with whatever commands the device —
    /// an `eebus-heat-pump` compressor, say. The one measurement it exists for is
    /// the **indoor temperature**: the planner models the building, learns it
    /// from the household's own record and can pre-heat, and all of it is gated
    /// on a number no EEBUS use case carries and a heat pump has been publishing
    /// in a register the whole time.
    Registers(RegisterSettings),
}

impl DriverSettings {
    /// Which asset this driver speaks for.
    #[must_use]
    pub fn asset(&self) -> &str {
        match self {
            DriverSettings::Sunspec(s) => &s.asset,
            DriverSettings::EebusLpc(s) => &s.asset,
            DriverSettings::EebusDhw(s) => &s.asset,
            DriverSettings::EebusEv(s) => &s.asset,
            DriverSettings::EebusHeatPump(s) => &s.asset,
            DriverSettings::Registers(s) => &s.asset,
        }
    }
}

/// A device read through a declared register map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterSettings {
    /// Which asset of the site this is.
    pub asset: String,
    /// `host:port`. Modbus TCP is 502 by convention.
    pub address: String,
    /// The Modbus unit identifier. One gateway can front several devices.
    #[serde(default = "default_unit")]
    pub unit: u8,
    /// How often to read the whole map, milliseconds.
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// How long an unanswered read may stand before the link is stale.
    ///
    /// Shorter than the guard's own `max_measurement_age`, or a device could be
    /// silent for a whole control period while the guard still called its last
    /// reading fresh.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// The map itself. At least one point, or the driver would poll nothing and
    /// report a device that is perfectly reachable and says nothing.
    pub points: Vec<hems_drv::modbus::registers::Point>,
}

/// A heat-pump compressor on the household's own network, over EEBUS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EebusHeatPumpSettings {
    /// Which asset of the site this is — the heat pump.
    pub asset: String,
    /// `host:port` to dial. EEBUS SHIP is 4712 by convention.
    pub address: String,
    /// The heat pump's own SKI, as printed on it.
    pub ski: String,
    /// How this box names itself to the heat pump.
    #[serde(default)]
    pub spine_vendor: Option<String>,
    /// What distinguishes this box from the next one off the same line.
    #[serde(default)]
    pub spine_unique: Option<String>,
}

/// A charge point on the household's own network, over EEBUS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EebusEvSettings {
    /// Which asset of the site this is — the charge point.
    pub asset: String,
    /// `host:port` to dial. EEBUS SHIP is 4712 by convention.
    pub address: String,
    /// The charge point's own SKI, as printed on it.
    pub ski: String,
    /// What time of day the car has to be charged by, local clock, `HH:MM`.
    ///
    /// A **household preference**, not something the car knows, and no EEBUS use
    /// case carries one: a car that published a departure time would be
    /// publishing a guess about its driver. It is what turns a state of charge
    /// into a deadline the planner can price.
    #[serde(default = "default_departure")]
    pub departure: String,
    /// How full the car should be when it leaves, `0..=1`.
    ///
    /// The other half of the same preference. Below 1,0 on purpose: charging the
    /// last fifteen per cent is the slowest and least useful part of a session,
    /// and a household that asks for a full battery every night pays for the
    /// privilege on every one of them.
    #[serde(default = "default_charge_target")]
    pub target_soc: f64,
    /// How this box names itself to the charge point.
    #[serde(default)]
    pub spine_vendor: Option<String>,
    /// What distinguishes this box from the next one off the same line.
    #[serde(default)]
    pub spine_unique: Option<String>,
}

impl EebusEvSettings {
    /// The first slot the car is gone, inside `horizon`.
    ///
    /// Half-open, like every other deadline in the planner, and that is not a
    /// presentation choice: read as "the last slot it can charge in", a car
    /// leaving at seven is planned as though it could still be charging at
    /// 07:14, and at 11 kW that is 2,75 kWh the car never receives.
    ///
    /// The **next** occurrence of the time of day, so a plan made at ten in the
    /// evening aims at tomorrow morning rather than at one that has passed.
    /// `None` where the departure is not a time, or is beyond the horizon —
    /// which is a car that has all the time in the world and needs no deadline.
    #[must_use]
    pub fn departure_slot(&self, horizon: hems_core::prelude::Horizon) -> Option<Slot> {
        let (hours, minutes) = self.departure.split_once(':')?;
        let hours: u32 = hours.trim().parse().ok()?;
        let minutes: u32 = minutes.trim().parse().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        let wanted = hours * 60 + minutes;
        // The first slot at which the horizon *crosses* the time of day, not the
        // first that is numerically past it. A plan made at seven in the evening
        // starts at minute 1140, which is already past a 07:00 departure in the
        // arithmetic and nowhere near it on the clock — taking that slot would
        // give the car a deadline four minutes ago and a plan that charges
        // nothing. So the answer is the slot whose predecessor was still before
        // the time and which is not, which is the same rule either side of
        // midnight.
        //
        // A horizon that begins exactly on the departure minute aims at
        // tomorrow, which is the safe reading: a deadline at this instant has
        // passed, and inventing one in the past is worse than aiming at the next
        // one.
        let mut previous: Option<u32> = None;
        horizon.slots().find(|slot| {
            let minute = u32::from(slot.local_minute_of_day());
            let crossed = previous.is_some_and(|before| before < wanted) && minute >= wanted;
            previous = Some(minute);
            crossed
        })
    }
}

/// How a heat pump takes instructions, as a configuration file spells it.
///
/// The domain has this enumeration too, in `snake_case` like everything else in
/// `hems-core`; this file is `kebab-case` like everything else in a `hemsd`
/// configuration. One value written both ways in the same file is how an
/// installer learns that a setting is fussy about something nobody documented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HeatPumpInterface {
    /// Two relay contacts carrying the four SG Ready states.
    SgReady,
    /// A continuous electrical power ceiling — EEBUS LPC, or a vendor register.
    ///
    /// The default: the one thing every § 14a heat pump understands.
    #[default]
    PowerCeiling,
    /// Discrete modes over a digital interface.
    OperationModes,
    /// A process the box starts and stops — EEBUS OHPCF.
    ///
    /// The only one of the four that can ask the unit to consume *more*, and so
    /// the only one a plan can pre-heat through. It needs an `eebus-heat-pump`
    /// driver.
    Compressor,
}

impl From<HeatPumpInterface> for hems_core::asset::HeatPumpControl {
    fn from(interface: HeatPumpInterface) -> Self {
        match interface {
            HeatPumpInterface::SgReady => Self::SgReady,
            HeatPumpInterface::PowerCeiling => Self::PowerCeiling,
            HeatPumpInterface::OperationModes => Self::OperationModes,
            HeatPumpInterface::Compressor => Self::Compressor,
        }
    }
}

impl From<hems_core::asset::HeatPumpControl> for HeatPumpInterface {
    fn from(control: hems_core::asset::HeatPumpControl) -> Self {
        match control {
            hems_core::asset::HeatPumpControl::SgReady => Self::SgReady,
            hems_core::asset::HeatPumpControl::PowerCeiling => Self::PowerCeiling,
            hems_core::asset::HeatPumpControl::OperationModes => Self::OperationModes,
            hems_core::asset::HeatPumpControl::Compressor => Self::Compressor,
        }
    }
}

fn default_departure() -> String {
    "07:00".into()
}

fn default_charge_target() -> f64 {
    0.8
}

/// A hot-water circuit on the household's own network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EebusDhwSettings {
    /// Which asset of the site this circuit is — the hot-water tank.
    pub asset: String,
    /// `host:port` to dial. EEBUS SHIP is 4712 by convention.
    pub address: String,
    /// The circuit's own SKI, as printed on it.
    ///
    /// Required, and it is the whole of the trust decision: TLS proves a peer's
    /// SKI, and a box that dialled whatever answered would take a tank
    /// temperature from anything on the network that offered one — which the
    /// plan would then heat against.
    pub ski: String,
    /// How this box names itself to the circuit — the vendor part of the SPINE
    /// device address.
    #[serde(default)]
    pub spine_vendor: Option<String>,
    /// What distinguishes this box from the next one off the same line.
    #[serde(default)]
    pub spine_unique: Option<String>,
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
    fn the_shipped_example_is_a_box_that_would_start() {
        // Stronger than counting the assets its drivers name, which is what this
        // used to do through a `assets_named` helper the daemon itself never
        // called. `assemble` runs the *real* rules — an asset the site does not
        // have, two drivers for one role, a controllable asset nothing can move,
        // a § 14a household with nothing to hear a reduction — and opens no
        // socket doing it. An example that has drifted from them now fails the
        // build rather than misleading an installer in a cellar.
        let settings: Settings = toml::from_str(EXAMPLE).expect("the shipped example parses");
        crate::runtime::assemble(&settings, None, time::OffsetDateTime::now_utc())
            .expect("the shipped example describes a box that starts");
    }

    #[test]
    fn the_roof_the_installer_described_is_the_roof_the_box_models() {
        // The chain that was broken: `PvArray` has carried a tilt and an azimuth
        // since the site model existed, the forecast reads both, and the daemon
        // wrote 35°/180° into every household in the country. An east-west array
        // was modelled as due south and left the correction to absorb a *shape*
        // error it can only take a level out of.
        let settings = SiteSettings {
            pv_tilt_deg: 12.0,
            pv_azimuth_deg: 95.0,
            ..SiteSettings::default()
        };
        let household = crate::site::Household::build(&settings.household().expect("a household"))
            .expect("a site");
        let roof = household
            .site
            .assets
            .iter()
            .find_map(|a| match a {
                hems_core::prelude::Asset::Pv(pv) => Some(pv),
                _ => None,
            })
            .expect("a roof");
        assert!((roof.tilt_deg - 12.0).abs() < 1e-9);
        assert!((roof.azimuth_deg - 95.0).abs() < 1e-9);
    }

    #[test]
    fn a_roof_from_the_exempt_window_is_not_capped_at_sixty_per_cent() {
        // § 100 Abs. 3b EEG disapplies § 9 Abs. 2 entirely to a system
        // commissioned between 01.01.2023 and 24.02.2025. The daemon used to
        // stamp 2025-03-01 on every asset, so such a roof was curtailed at 60 %
        // every sunny midday for the life of the installation — for a limitation
        // the statute never reached it with.
        let capped = SiteSettings::default();
        let exempt = SiteSettings {
            declared: [(
                "pv".to_owned(),
                DeclaredSettings {
                    commissioned: Some("2024-06-01".to_owned()),
                    ..DeclaredSettings::default()
                },
            )]
            .into_iter()
            .collect(),
            ..SiteSettings::default()
        };
        let limit = |s: &SiteSettings| {
            let household = crate::site::Household::build(&s.household().expect("a household"))
                .expect("a site");
            hems_grid::para9::GenerationProfile::of_site(&household.site)
                .expect("a roof")
                .statutory_limit()
        };
        assert!(limit(&capped).is_some(), "March 2025 is capped at 60 %");
        assert_eq!(limit(&exempt), None, "and June 2024 is capped at nothing");
    }

    #[test]
    fn a_legacy_heat_pump_is_not_a_new_steuve() {
        // `[A1 10.1]`: a device commissioned before 2024 on the old reduced
        // network fee stays on it until 31.12.2028. Counting it as a new SteuVE
        // hands the operator a share of its power it may not reduce and counts
        // its consumption as netzwirksamer Leistungsbezug when it is ordinary
        // load — and there was no way to say so.
        let settings = SiteSettings {
            declared: [(
                "waermepumpe".to_owned(),
                DeclaredSettings {
                    commissioned: Some("2019-04-01".to_owned()),
                    legacy_status: LegacyStatus::ReducedNetworkFee,
                    ..DeclaredSettings::default()
                },
            )]
            .into_iter()
            .collect(),
            ..SiteSettings::default()
        };
        let household = crate::site::Household::build(&settings.household().expect("a household"))
            .expect("a site");
        let unit = household
            .heat_pump
            .as_ref()
            .and_then(|id| household.site.asset(id))
            .expect("a heat pump");
        assert!(matches!(
            hems_grid::para14a::participation(
                unit.meta().commissioned_at,
                unit.meta().steuve_exemption,
                unit.meta().legacy_status,
                unit.meta().switched_voluntarily,
            ),
            hems_grid::para14a::Participation::Legacy { .. }
        ));
    }

    #[test]
    fn a_declaration_for_an_asset_nobody_has_is_refused() {
        // The same rule a `[[drivers]]` entry lives under, and for the same
        // reason: a § 14a regime declared for a device that does not exist looks,
        // from every screen, exactly like one that took effect.
        let settings = SiteSettings {
            declared: [("waermepump".to_owned(), DeclaredSettings::default())]
                .into_iter()
                .collect(),
            ..SiteSettings::default()
        };
        assert!(matches!(
            settings.household(),
            Err(SettingsError::UndeclarableAsset { .. })
        ));
    }

    #[test]
    fn the_connection_agreement_narrows_what_the_fuse_allows() {
        // `contract_power` binds the guard's headroom, the planner's import
        // ceiling and the ContractualConsumptionNominalMax an Energy Guard is
        // told `[LPC-042]` — and until it could be configured, a box told a
        // network operator its only limit was a 35 A fuse.
        let settings = SiteSettings {
            contract_power_kw: Some(14.0),
            ..SiteSettings::default()
        };
        let household = crate::site::Household::build(&settings.household().expect("a household"))
            .expect("a site");
        assert_eq!(
            household.site.grid.import_ceiling(),
            hems_core::prelude::Power::from_kw(14.0)
        );
        // …and it is deliberately *not* applied to export: an agreement about
        // how much a household may draw is not a feed-in limitation.
        assert!(household.site.grid.export_ceiling() > hems_core::prelude::Power::from_kw(20.0));
    }

    #[test]
    fn a_market_identifier_that_fails_its_own_check_digit_is_refused() {
        // A MaLo-ID carries a check digit precisely so a transposition is
        // detectable, and swallowing that would put an identifier on a market
        // message that names somebody else's connection point.
        let settings = SiteSettings {
            malo: Some("41373559214".to_owned()),
            ..SiteSettings::default()
        };
        assert!(matches!(
            settings.household(),
            Err(SettingsError::NotAnIdentifier { field: "malo", .. })
        ));
    }

    #[test]
    fn a_building_the_installer_named_reaches_the_planner() {
        let settings = SiteSettings {
            building: BuildingSettings {
                class: BuildingClass::SolidWall,
                ..BuildingSettings::default()
            },
            ..SiteSettings::default()
        };
        let config = settings.household().expect("a household");
        assert_eq!(config.building, BuildingClass::SolidWall.rc2());

        // …and one parameter may be corrected without giving up the class.
        let corrected = SiteSettings {
            building: BuildingSettings {
                class: BuildingClass::SolidWall,
                mass_capacity_kwh_per_k: Some(9.0),
                ..BuildingSettings::default()
            },
            ..SiteSettings::default()
        };
        let built = corrected.household().expect("a household").building;
        assert!((built.mass_capacity_kwh_per_k - 9.0).abs() < 1e-9);
        assert!(
            (built.r_air_out_k_per_kw - BuildingClass::SolidWall.rc2().r_air_out_k_per_kw).abs()
                < 1e-9
        );
    }

    #[test]
    fn a_building_that_could_not_exist_is_refused_rather_than_planned_against() {
        let settings = SiteSettings {
            building: BuildingSettings {
                mass_capacity_kwh_per_k: Some(0.0),
                ..BuildingSettings::default()
            },
            ..SiteSettings::default()
        };
        assert!(matches!(
            settings.household(),
            Err(SettingsError::NotABuilding { .. })
        ));
    }

    #[test]
    fn a_wallbox_commanded_over_modbus_and_read_over_eebus_starts() {
        // The deployment `eebus-ev`'s own documentation describes — "it reads and
        // never commands; the charge point is already commanded, over Modbus" —
        // and which no household could configure: alone the EEBUS driver failed
        // `CannotCommand`, and beside the Modbus one it failed `Duplicate`. A
        // whole driver, with its own use cases, config and dial path, that could
        // not be registered anywhere.
        let text = r#"
[[drivers]]
kind = "sunspec"
asset = "wallbox"
address = "192.0.2.20:502"

[[drivers]]
kind = "eebus-ev"
asset = "wallbox"
address = "192.168.1.50:4712"
ski = "0000000000000000000000000000000000000000"

[[drivers]]
kind = "eebus-lpc"
asset = "netzanschluss"
"#;
        let settings: Settings = toml::from_str(text).expect("a valid driver list");
        crate::runtime::assemble(&settings, None, time::OffsetDateTime::now_utc())
            .expect("one driver drives the wallbox and the other watches it");
    }

    #[test]
    fn a_heat_pump_that_speaks_eebus_needs_nothing_else() {
        // One driver, one session, both roles: OHPCF starts the compressor and
        // MRT reports the air temperature of the rooms — which is the state the
        // whole thermal plan is integrated from, and which no EEBUS use case
        // carried before 0.7.
        let text = r#"
[[drivers]]
kind = "eebus-heat-pump"
asset = "waermepumpe"
address = "192.168.1.60:4712"
ski = "0000000000000000000000000000000000000000"

[[drivers]]
kind = "eebus-lpc"
asset = "netzanschluss"
"#;
        let settings: Settings = toml::from_str(text).expect("a valid driver list");
        crate::runtime::assemble(&settings, None, time::OffsetDateTime::now_utc())
            .expect("the unit drives its compressor and reports its rooms");
    }

    #[test]
    fn a_heat_pump_that_does_not_speak_eebus_is_read_from_its_register_map() {
        // The other half of the market: a unit that answers Modbus and
        // publishes no model list, where the register numbers are in a PDF.
        //
        // The register map is the EEBUS driver's **alternative**, not its
        // companion — both measure, and two measuring drivers for one asset is
        // two sources of truth with nothing downstream that could tell which to
        // believe. What it cannot do is command, so a household on this path
        // limits its heat pump the § 14a way rather than starting its
        // compressor.
        let text = r#"
[[drivers]]
kind = "registers"
asset = "waermepumpe"
address = "192.0.2.30:502"
points = [
  { space = "input", register = 507, word = "s16", scale = 0.1, field = "temperature_c" },
  { register = 2240, word = "u32", field = "power" },
]

[[drivers]]
kind = "eebus-lpc"
asset = "netzanschluss"
"#;
        let settings: Settings = toml::from_str(text).expect("a valid driver list");
        let DriverSettings::Registers(map) = &settings.drivers[0] else {
            panic!("the first driver is the register map");
        };
        assert_eq!(map.unit, 1, "the ordinary default, not something invented");
        assert!(
            (map.points[0].scale - 0.1).abs() < f64::EPSILON,
            "a temperature in tenths of a kelvin"
        );
        assert!(
            (map.points[1].scale - 1.0).abs() < f64::EPSILON,
            "and an unscaled one defaults to 1, not to 0"
        );
        // …and it is refused on its own, because a heat pump is controllable and
        // a register map never writes: a typo in this file would otherwise start
        // a compressor.
        let refused = crate::runtime::assemble(&settings, None, time::OffsetDateTime::now_utc())
            .err()
            .expect("a heat pump nothing can move is refused");
        assert!(
            matches!(
                refused,
                crate::runtime::StartError::Drivers(crate::drivers::RegistryError::CannotCommand(
                    _
                ))
            ),
            "a house the box can read and cannot heat is a plan nobody executes: {refused}"
        );
    }

    #[test]
    fn a_hot_water_tank_needs_only_its_own_eebus_driver() {
        // A tank is a **controllable** asset, and until CDSF this driver was a
        // meter — so a household whose only tank driver was this one was refused
        // at start-up with `CannotCommand`, and the plan moved a store nothing
        // could carry the decision to. The third driver in this workspace with
        // that shape.
        let text = r#"
[[drivers]]
kind = "eebus-dhw"
asset = "warmwasser"
address = "192.168.1.40:4712"
ski = "0000000000000000000000000000000000000000"

[[drivers]]
kind = "eebus-lpc"
asset = "netzanschluss"
"#;
        let settings: Settings = toml::from_str(text).expect("a valid driver list");
        crate::runtime::assemble(&settings, None, time::OffsetDateTime::now_utc())
            .expect("one driver both reads the tank and asks it to heat");
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
        assert_eq!(
            built.pv.map(|pv| pv.para9),
            HouseholdConfig::default().pv.map(|pv| pv.para9)
        );
    }

    #[test]
    fn a_size_of_zero_is_the_absence_of_the_device() {
        // The gap this closes: a household with no battery used to describe a
        // battery of no size — an asset the arbiter decided for, the S2
        // description named, and `/v1/status` listed as undriven. A site model
        // is a list of what is *there*.
        let settings = SiteSettings {
            pv_kwp: 0.0,
            battery_kwh: 0.0,
            evse_max_a: 0.0,
            heat_pump_kw: 0.0,
            dhw_litres: 0.0,
            ..SiteSettings::default()
        };
        let config = settings.household().expect("a bare house is a valid one");
        assert!(config.pv.is_none(), "no roof");
        assert!(config.battery.is_none(), "no battery");
        assert!(config.evse.is_none(), "no charge point");
        assert!(config.heat_pump.is_none(), "no heat pump");
        assert!(config.dhw.is_none(), "no tank");

        let household = crate::site::Household::build(&config).expect("still a valid site");
        for (id, what) in [
            (&household.pv, "roof"),
            (&household.battery, "battery"),
            (&household.evse, "charge point"),
            (&household.heat_pump, "heat pump"),
            (&household.dhw, "tank"),
        ] {
            assert!(id.is_none(), "the site should carry no {what}");
        }
        // The planner's names come from the same facts, so an absent asset is
        // never *named* either — naming one emits a zero-pinned envelope the
        // arbiter obeys as an instruction (D96).
        assert!(household.names.battery.is_none());
        assert!(household.names.evse.is_none());
        // What is left is the load, the connection-point meter, and the
        // dishwasher the default settings still carry.
        assert_eq!(household.site.assets.len(), 3);
    }

    #[test]
    fn the_defaults_still_describe_the_whole_reference_household() {
        // The other half of the same rule: a box with no configuration file
        // behaves like the house every figure in this project was measured on.
        let household = crate::site::Household::build(
            &SiteSettings::default()
                .household()
                .expect("the defaults are valid"),
        )
        .expect("the reference household is a valid site");
        assert!(household.pv.is_some());
        assert!(household.battery.is_some());
        assert!(household.evse.is_some());
        assert!(household.heat_pump.is_some());
        assert!(household.dhw.is_some());
        assert!(household.dishwasher.is_some());
    }
}
