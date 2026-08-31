//! The things behind the grid connection.
//!
//! Each variant carries the *facts* about a device — ratings, geometry,
//! capabilities, when it was commissioned. It carries no rules: whether a heat
//! pump is a steuerbare Verbrauchseinrichtung, and what minimum power it is
//! owed, is decided by `hems-grid` from these facts. Keeping facts and rules
//! apart is what lets the same site description outlive a change in the
//! Festlegung.

use time::Date;

use crate::envelope::Envelope;
use crate::ids::{AssetId, CircuitId};
use crate::units::{Current, Energy, NOMINAL_VOLTAGE, PhaseConnection, PhaseMode, Power, Soc};

/// What a driver can do with an asset.
///
/// A bitset rather than a set of `Option` fields, because the arbiter asks
/// "can this be limited?" on every tick and the answer must be a branch, not an
/// allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Capabilities(u16);

impl Capabilities {
    /// Reports measurements.
    pub const MEASURE: Self = Self(1 << 0);
    /// Accepts a consumption ceiling.
    pub const LIMIT_CONSUMPTION: Self = Self(1 << 1);
    /// Accepts a production ceiling (curtailment).
    pub const LIMIT_PRODUCTION: Self = Self(1 << 2);
    /// Accepts an active-power target, both signs where the device allows it.
    pub const SET_POWER: Self = Self(1 << 3);
    /// Accepts a discrete operating mode (SG Ready, OMBC).
    pub const SET_MODE: Self = Self(1 << 4);
    /// Accepts a schedule for later execution.
    pub const SCHEDULE: Self = Self(1 << 5);
    /// Can export as well as import (bidirectional).
    pub const BIDIRECTIONAL: Self = Self(1 << 7);
    /// Can be identified physically (blink, beep) during commissioning.
    pub const IDENTIFY: Self = Self(1 << 8);

    // Note there is no `SWITCH_PHASES`. Whether a device can change its
    // conductor count is [`PhaseConnection::Switchable`], and one fact with two
    // representations is one fact that can contradict itself — a capability
    // declared on a fixed three-phase connection is a charge point asked to
    // switch that cannot.

    /// No capabilities.
    pub const NONE: Self = Self(0);

    /// The union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// `true` when every capability in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for Capabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// What a device brings with it from before 2024, `[BK6-22-300 A1 10]`.
///
/// A fact about the device and its contract, not a rule: `hems-grid` turns it
/// into a regime. It has to live here because the guard needs it on every tick
/// and the answer is not derivable from anything else on the asset — whether a
/// reduced network fee was ever granted is in a contract, not in a datasheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LegacyStatus {
    /// Nothing: no reduced network fee was ever granted for it.
    #[default]
    None,
    /// A reduced network fee under the old § 14a Abs. 2 Satz 1 EnWG or its
    /// predecessor was granted, `[A1 10.1]`.
    ReducedNetworkFee,
    /// It is a night-storage heater on the old rule, `[A1 10.2.b]`.
    Nachtspeicher,
}

/// What has taken a photovoltaic system out of the 60 % feed-in cap of
/// § 9 Abs. 2 EEG.
///
/// A fact about the installation, not a rule — `hems_grid::para9` turns it into
/// a ceiling. It lives here for the same reason [`LegacyStatus`] does: the guard
/// needs it on every tick, and the answer is in an installation record rather
/// than in a datasheet.
///
/// The cap is lifted by a **technical fact**, not by a commercial arrangement,
/// and the distinction is worth a type rather than a `direktvermarktung`
/// boolean: a system whose market contract is signed would otherwise lose its
/// cap whether or not the control path existed. Direktvermarktung *requires*
/// Fernsteuerbarkeit (§ 10b EEG), so the two normally travel together — but "the
/// contract is signed" and "the control path works" are different days, and on
/// the days between them the cap still applies.
///
/// [`Default`]s to [`CapRelief::None`] on purpose: the cap staying on costs a
/// household some feed-in, lifting it wrongly means feeding in above a statutory
/// limit, and only one of those is the operator's problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum CapRelief {
    /// Nothing. The cap applies if the system is otherwise in scope.
    #[default]
    None,
    /// An intelligent metering system with a control device is **in operation**
    /// — the lift § 9 Abs. 2 EEG names.
    ImsysWithControl,
    /// The output is sold on the market and the Fernsteuerbarkeit § 10b EEG
    /// demands is working. The commercial arrangement alone is not enough.
    DirektvermarktungFernsteuerbar,
}

impl CapRelief {
    /// Whether this lifts the cap.
    #[must_use]
    pub const fn lifts_cap(self) -> bool {
        !matches!(self, CapRelief::None)
    }
}

/// Why an asset that looks like a steuerbare Verbrauchseinrichtung is not one.
///
/// `[BK6-22-300 Anlage 1 Ziff. 3.1.b]`. The list is closed: anything not on it
/// participates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SteuVeExemption {
    /// A publicly accessible charge point (§ 2 Nr. 5 LSV) — outside the scope of
    /// Ziff. 2.4.1.a from the start.
    PublicChargePoint,
    /// Operated by an institution with Sonderrechte under § 35 Abs. 1, 5a StVO
    /// — fire service, ambulance, police `[A1 3.1.b aa]`.
    EmergencyServices,
    /// Heating or cooling that does not serve living, office or common rooms —
    /// process heat, and equipment serving critical infrastructure
    /// `[A1 3.1.b bb]`.
    NonResidentialHeatingOrCooling,
}

/// Facts every asset carries.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetMeta {
    /// The name used in configuration, topics and the UI.
    pub id: AssetId,
    /// A human label for the UI. Falls back to the id when empty.
    #[cfg_attr(feature = "serde", serde(default))]
    pub label: String,
    /// The circuit this asset hangs off.
    pub circuit: CircuitId,
    /// How it is connected to the outer conductors.
    pub phases: PhaseConnection,
    /// Netzanschlussleistung — the nameplate power the network operator sees.
    ///
    /// This is the number the 4,2 kW threshold and the 0,4 scaling factor of
    /// `[BK6-22-300 A1 2.4.1 / 4.5.1]` are applied to, so it is a declared
    /// value, not a measured one.
    pub connection_power: Power,
    /// The date of technical commissioning.
    ///
    /// `[A1 3.1.b]` makes participation in the netzorientierte Steuerung
    /// mandatory for devices commissioned **after 31.12.2023**; `[A1 10]` puts
    /// everything older into one of the transitional regimes. An unknown date
    /// leaves the device **in** the § 14a group: dropping a device out of the
    /// group is how a site exceeds a network operator's limit, and it also
    /// lowers the minimum power the customer is owed. See
    /// `hems_grid::para14a::participation`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub commissioned_at: Option<Date>,
    /// Why this asset does not participate, if it does not.
    #[cfg_attr(feature = "serde", serde(default))]
    pub steuve_exemption: Option<SteuVeExemption>,
    /// What the device brings with it from before 2024, `[A1 10]`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub legacy_status: LegacyStatus,
    /// Whether the operator moved this device into the netzorientierte Steuerung
    /// voluntarily, `[A1 10.4]`.
    ///
    /// The network operator may not refuse and there is no way back, so this is
    /// a contractual fact that is stored rather than derived.
    #[cfg_attr(feature = "serde", serde(default))]
    pub switched_voluntarily: bool,
    /// What the driver can do with it.
    pub capabilities: Capabilities,
}

impl AssetMeta {
    /// A minimal description: identity, circuit, phases and rating.
    #[must_use]
    pub fn new(
        id: AssetId,
        circuit: CircuitId,
        phases: PhaseConnection,
        connection_power: Power,
    ) -> Self {
        Self {
            label: id.to_string(),
            id,
            circuit,
            phases,
            connection_power,
            commissioned_at: None,
            steuve_exemption: None,
            legacy_status: LegacyStatus::None,
            switched_voluntarily: false,
            capabilities: Capabilities::MEASURE,
        }
    }

    /// Replace the capability set.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Set the commissioning date.
    #[must_use]
    pub fn commissioned(mut self, date: Date) -> Self {
        self.commissioned_at = Some(date);
        self
    }

    /// Declare an exemption from § 14a participation.
    #[must_use]
    pub fn exempt(mut self, reason: SteuVeExemption) -> Self {
        self.steuve_exemption = Some(reason);
        self
    }

    /// Record what the device brings with it from before 2024, `[A1 10]`.
    #[must_use]
    pub fn with_legacy_status(mut self, legacy_status: LegacyStatus) -> Self {
        self.legacy_status = legacy_status;
        self
    }

    /// Record the irreversible move into the netzorientierte Steuerung,
    /// `[A1 10.4]`.
    #[must_use]
    pub fn switched_voluntarily(mut self) -> Self {
        self.switched_voluntarily = true;
        self
    }
}

/// Which Fallgruppe of `[BK6-22-300 A1 2.4.1]` an asset belongs to.
///
/// This is `metering`'s type, not a copy of it. The grouping matters — heat
/// pumps and cooling are each summed **per Fallgruppe** behind one connection
/// before the 4,2 kW threshold is applied `[A1 2.4.2]`, charge points and
/// storage are not — and `metering::para14a` is where the arithmetic that
/// depends on it lives. Two enumerations of the same four Fallgruppen are two
/// things that can disagree about a regulation, which is the one kind of
/// duplication this workspace cannot afford.
pub use metering::para14a::SteuVeFallgruppe as Fallgruppe;

/// How a heat pump can be told what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum HeatPumpControl {
    /// Two relay contacts carrying the four SG Ready states.
    ///
    /// Coarse — the states are "blocked", "normal", "recommended on",
    /// "commanded on", not a power value — and from 1 July 2027 no longer enough
    /// for a BEG-funded heat pump, which needs an interoperable digital
    /// interface in a Code-of-Conduct format.
    SgReady,
    /// A continuous electrical power ceiling (EEBUS LPC, or a vendor register).
    PowerCeiling,
    /// Discrete modes over a digital interface (EEBUS OMBC-style).
    OperationModes,
}

/// A photovoltaic array with its inverter.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PvArray {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// Installed DC power in watts — the reference the § 9 EEG 60 % cap and the
    /// EEBUS `MGCP` feed-in factor are percentages *of*.
    pub kwp_dc: Power,
    /// The inverter's AC limit.
    pub ac_nominal: Power,
    /// Module tilt from horizontal, degrees.
    #[cfg_attr(feature = "serde", serde(default))]
    pub tilt_deg: f64,
    /// Module azimuth, degrees east of north (180 = due south).
    #[cfg_attr(feature = "serde", serde(default))]
    pub azimuth_deg: f64,
    /// What, if anything, has lifted the § 9 Abs. 2 EEG 60 % feed-in cap for
    /// this system.
    ///
    /// Together with [`AssetMeta::commissioned_at`] and [`PvArray::kwp_dc`] this
    /// is everything `hems_grid::para9` needs to decide whether the cap applies.
    #[cfg_attr(feature = "serde", serde(default))]
    pub cap_relief: CapRelief,
}

/// Battery chemistry, because degradation behaves differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Chemistry {
    /// Lithium iron phosphate — cheap cycles, the home-storage default.
    #[default]
    Lfp,
    /// Nickel manganese cobalt — denser, markedly more cycle-sensitive.
    Nmc,
    /// Anything else; the optimiser uses conservative defaults.
    Other,
}

/// A stationary battery.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Battery {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// Usable capacity.
    pub capacity: Energy,
    /// Maximum charging power.
    pub max_charge: Power,
    /// Maximum discharging power, as a positive magnitude.
    pub max_discharge: Power,
    /// One-way charging efficiency in `(0, 1]`.
    pub efficiency_charge: f64,
    /// One-way discharging efficiency in `(0, 1]`.
    pub efficiency_discharge: f64,
    /// Lowest state of charge the system will use in normal operation.
    pub soc_min: Soc,
    /// Highest state of charge the system will use.
    pub soc_max: Soc,
    /// Energy held back for a power cut. Neither planner nor arbiter may plan
    /// below it; only an islanded system may use it.
    #[cfg_attr(feature = "serde", serde(default))]
    pub reserve_soc: Soc,
    /// Cell chemistry.
    #[cfg_attr(feature = "serde", serde(default))]
    pub chemistry: Chemistry,
    /// Whether the system can charge from the grid at all. A storage system that
    /// cannot is outside MiSpeL entirely and always "green".
    #[cfg_attr(feature = "serde", serde(default))]
    pub grid_charging_allowed: bool,
}

/// A charge point for electric vehicles.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Evse {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// Lowest current the standard allows a session to run at — 6 A in
    /// IEC 61851, which is why a "small" limit still means 1,4 kW single-phase.
    pub min_current: Current,
    /// Highest current the hardware allows.
    pub max_current: Current,
    /// Whether the charge point can discharge the vehicle (V2H/V2G).
    #[cfg_attr(feature = "serde", serde(default))]
    pub bidirectional: bool,
    /// Whether it is publicly accessible in the sense of § 2 Nr. 5 LSV.
    #[cfg_attr(feature = "serde", serde(default))]
    pub public: bool,
    /// The state of charge the household asked the car to reach — evcc's and
    /// openWB's *Ladelimit*.
    ///
    /// The **planner** does not read it: it is given an energy target and a
    /// departure, which say the same thing more precisely. The real-time
    /// fallback does, because it has neither — and a surplus tracker with no
    /// notion of *enough* pushes production into a car that already has what it
    /// was asked for, in preference to exporting it for money.
    ///
    /// `None` means "fill it". Read against
    /// [`crate::measurement::Measurement::soc`] on the charge point, which is
    /// where a vehicle's charge reaches the box (EEBUS `EVSOC`, ISO 15118 or an
    /// OEM API, in that order of trust).
    #[cfg_attr(feature = "serde", serde(default))]
    pub charge_limit: Option<Soc>,
}

/// A heat pump, with whatever auxiliary heater is bound to it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HeatPump {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// Nominal electrical input power of the compressor.
    pub electrical_nominal: Power,
    /// Electrical power of the auxiliary or emergency heater, if fitted.
    ///
    /// `[A1 2.4.1.b]` folds it into the same Fallgruppe as the compressor, so it
    /// belongs to this asset rather than being a load of its own.
    #[cfg_attr(feature = "serde", serde(default))]
    pub heating_rod: Option<Power>,
    /// How it takes commands.
    pub control: HeatPumpControl,
    /// Whether the pump modulates or only starts and stops.
    #[cfg_attr(feature = "serde", serde(default))]
    pub modulating: bool,
}

impl Evse {
    /// The number of outer conductors a session uses in `mode`.
    #[must_use]
    pub fn phase_count(&self, mode: PhaseMode) -> u8 {
        self.meta.phases.count(mode).max(1)
    }

    /// The least power at which a session runs at all, in `mode`.
    ///
    /// IEC 61851 puts the floor at 6 A per conductor. Below it a charge point is
    /// not charging slowly, it is idle — which is why allocating it 2 kW wastes
    /// the 2 kW rather than charging the car slowly.
    ///
    /// The mode is the whole point: 6 A on three conductors is 4,1 kW, and on
    /// one it is 1,4 kW. A household with 2 kW of surplus can charge in the
    /// second and not in the first, and that gap is why a switchable charge
    /// point is worth the contactor.
    #[must_use]
    pub fn min_power(&self, mode: PhaseMode) -> Power {
        Power::new(
            self.min_current.get() * NOMINAL_VOLTAGE.get() * f64::from(self.phase_count(mode)),
        )
    }

    /// The most a session can draw in `mode`.
    #[must_use]
    pub fn max_power(&self, mode: PhaseMode) -> Power {
        Power::new(
            self.max_current.get() * NOMINAL_VOLTAGE.get() * f64::from(self.phase_count(mode)),
        )
        .min(self.meta.connection_power.abs())
    }

    /// The smallest power at which this charge point could charge in **any**
    /// mode it is wired for.
    ///
    /// What an allocator needs: switching a device off because it cannot run on
    /// 2 kW three-phase, when it could run on 2 kW single-phase, wastes the
    /// 2 kW *and* the switch the hardware came with.
    #[must_use]
    pub fn lowest_useful_power(&self) -> Power {
        [PhaseMode::Single, PhaseMode::Three]
            .into_iter()
            .filter(|m| self.meta.phases.supports(*m))
            .map(|m| self.min_power(m))
            .reduce(Power::min)
            .unwrap_or_else(|| self.min_power(self.meta.phases.default_mode()))
    }

    /// The largest power at which it could charge in any mode it is wired for.
    #[must_use]
    pub fn highest_power(&self) -> Power {
        [PhaseMode::Single, PhaseMode::Three]
            .into_iter()
            .filter(|m| self.meta.phases.supports(*m))
            .map(|m| self.max_power(m))
            .reduce(Power::max)
            .unwrap_or_else(|| self.max_power(self.meta.phases.default_mode()))
    }
}

impl Battery {
    /// The lowest state of charge normal operation may reach: the operating
    /// floor, or the backup reserve where that is higher.
    ///
    /// A reserve is a promise to the household — "there will be something left
    /// when the street goes dark" — so it binds the guard, not only the planner.
    #[must_use]
    pub fn discharge_floor(&self) -> Soc {
        if self.reserve_soc > self.soc_min {
            self.reserve_soc
        } else {
            self.soc_min
        }
    }
}

impl HeatPump {
    /// Compressor plus auxiliary heater — the Fallgruppe's summed power.
    #[must_use]
    pub fn group_power(&self) -> Power {
        self.electrical_nominal + self.heating_rod.unwrap_or(Power::ZERO)
    }
}

/// Specific heat of water, kWh per litre and kelvin.
///
/// 4,186 kJ/(kg·K) at one kilogram per litre, in the unit the rest of this
/// workspace counts energy in.
pub const WATER_KWH_PER_LITRE_KELVIN: f64 = 4.186 / 3600.0;

/// A domestic hot water tank with an electric heater.
///
/// The cheapest store in most German houses and the one nobody plans with. Three
/// hundred litres between 45 and 60 °C hold about 5 kWh of heat; a hot-water heat
/// pump puts it there at a coefficient of performance around three, so shifting
/// a day's washing into the sunny hours is worth a couple of kilowatt-hours of
/// import at the retail price for a device that costs nothing to control.
///
/// It is deliberately **not** a steuerbare Verbrauchseinrichtung. `[A1 2.4.1]`
/// lists exactly four Fallgruppen — charge point, heat-pump heating including
/// its auxiliary heaters, space cooling, and storage while charging — and a
/// water heater is in none of them. A Heizstab that is the *heat pump's*
/// Zusatzheizung is another matter and is already counted there, through
/// [`HeatPump::heating_rod`]. So a § 14a reduction does not bind a tank, while
/// the fuse above it and the connection behind it still do, and its consumption
/// still spends the surplus that would otherwise have raised the § 14a budget.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DhwTank {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// Tank volume in litres.
    pub volume_l: f64,
    /// Electrical heating power.
    pub heater: Power,
    /// Thermal kilowatt-hours delivered per electrical kilowatt-hour.
    ///
    /// One for an immersion heater, around three for a hot-water heat pump. It
    /// is what decides whether the tank is worth planning with at all.
    #[cfg_attr(feature = "serde", serde(default = "one"))]
    pub cop: f64,
    /// Standing loss — the reason a tank left alone is cold in the morning.
    #[cfg_attr(feature = "serde", serde(default))]
    pub standing_loss: Power,
    /// Lowest acceptable temperature, °C.
    pub t_min_c: f64,
    /// Target temperature, °C.
    pub t_set_c: f64,
    /// Highest safe temperature, °C.
    pub t_max_c: f64,
}

fn one() -> f64 {
    1.0
}

impl DhwTank {
    /// Heat capacity of the water in the tank, kWh per kelvin.
    #[must_use]
    pub fn heat_capacity_kwh_per_k(&self) -> f64 {
        self.volume_l.max(0.0) * WATER_KWH_PER_LITRE_KELVIN
    }

    /// The heat the tank may hold between its lowest acceptable and its highest
    /// safe temperature — the store the planner is allowed to move.
    #[must_use]
    pub fn usable_heat(&self) -> Energy {
        Energy::from_kwh(self.heat_capacity_kwh_per_k() * (self.t_max_c - self.t_min_c).max(0.0))
    }

    /// The heat stored at `temperature_c`, measured from the lowest acceptable
    /// temperature and clamped to what the tank can hold.
    #[must_use]
    pub fn stored_heat(&self, temperature_c: f64) -> Energy {
        let above = (temperature_c - self.t_min_c).max(0.0);
        Energy::from_kwh(self.heat_capacity_kwh_per_k() * above).min(self.usable_heat())
    }

    /// The temperature `stored` corresponds to, °C — the number to show a
    /// household, which does not think in kilowatt-hours of water.
    #[must_use]
    pub fn temperature_at(&self, stored: Energy) -> f64 {
        let c = self.heat_capacity_kwh_per_k();
        if c <= 0.0 {
            return self.t_min_c;
        }
        self.t_min_c + stored.kwh() / c
    }
}

/// How much freedom a load gives the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LoadKind {
    /// Cannot be influenced. The household base load.
    #[default]
    Fixed,
    /// Can be started later, but not interrupted once running — a dishwasher.
    Shiftable,
    /// Can be interrupted and resumed — a pool pump, a dehumidifier.
    Interruptible,
}

/// A load other than the modelled appliances.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FlexibleLoad {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// Nominal power while running.
    pub nominal: Power,
    /// How much freedom the planner has.
    pub kind: LoadKind,
}

/// What a meter measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MeterRole {
    /// The grid connection point — the meter every decision starts from.
    GridConnection,
    /// Production of one generator.
    Production,
    /// A sub-meter for one asset, named by `subject`.
    Submeter,
    /// Total household consumption.
    Consumption,
}

/// A meter.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Meter {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// What it measures.
    pub role: MeterRole,
    /// The asset this meter is attached to, for a sub-meter.
    #[cfg_attr(feature = "serde", serde(default))]
    pub subject: Option<AssetId>,
}

/// A switched output — an SG Ready contact, a heating-rod contactor.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Relay {
    /// Identity and connection.
    pub meta: AssetMeta,
    /// What closing the contact does, for the UI and the evidence record.
    pub purpose: String,
}

/// Anything that consumes, produces, stores or measures.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "type"))]
pub enum Asset {
    /// A photovoltaic array.
    Pv(PvArray),
    /// A stationary battery.
    Battery(Battery),
    /// A charge point.
    Evse(Evse),
    /// A heat pump.
    HeatPump(HeatPump),
    /// A hot water tank.
    Dhw(DhwTank),
    /// Any other load.
    Load(FlexibleLoad),
    /// A meter.
    Meter(Meter),
    /// A switched output.
    Relay(Relay),
}

impl Asset {
    /// The facts common to every asset.
    #[must_use]
    pub fn meta(&self) -> &AssetMeta {
        match self {
            Asset::Pv(a) => &a.meta,
            Asset::Battery(a) => &a.meta,
            Asset::Evse(a) => &a.meta,
            Asset::HeatPump(a) => &a.meta,
            Asset::Dhw(a) => &a.meta,
            Asset::Load(a) => &a.meta,
            Asset::Meter(a) => &a.meta,
            Asset::Relay(a) => &a.meta,
        }
    }

    /// The identifier.
    #[must_use]
    pub fn id(&self) -> &AssetId {
        &self.meta().id
    }

    /// What the driver can do with it.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        self.meta().capabilities
    }

    /// Which § 14a Fallgruppe the asset falls into, ignoring thresholds,
    /// exemptions and commissioning dates — those are `hems-grid`'s decision.
    ///
    /// A hot water tank's heater is *not* a Fallgruppe of its own: `[A1 2.4.1.b]`
    /// counts auxiliary and emergency heaters with the heat pump, and a tank
    /// without a heat pump behind it is an ordinary load.
    #[must_use]
    pub fn fallgruppe(&self) -> Option<Fallgruppe> {
        match self {
            Asset::Evse(e) if !e.public => Some(Fallgruppe::Ladepunkt),
            Asset::HeatPump(_) => Some(Fallgruppe::Waermepumpe),
            Asset::Battery(_) => Some(Fallgruppe::Stromspeicher),
            _ => None,
        }
    }

    /// Whether VDE-AR-N 4100's symmetry requirement applies to this device.
    ///
    /// Not to everything behind the connection, which is the reading that looks
    /// obvious and is wrong. The VDE FNN Hinweis *Symmetrischer Anschluss und
    /// Betrieb in Kundenanlagen* is explicit about the scope of Abschnitt
    /// 5.5.2: *"Die Anforderungen zum symmetrischen Betrieb gelten nur für
    /// Geräte die elektrische Energie einspeisen oder speichern können, also
    /// Erzeugungsanlagen, Speicher, Ladeeinrichtungen für Elektrofahrzeuge."*
    ///
    /// So an inverter, a battery and a charge point are in scope; a heat pump,
    /// a hot-water tank and the household's own single-phase load are not.
    /// Counting them made the site look more unbalanced than the rule says it
    /// is, and spent the difference on the one device the manager could still
    /// move.
    #[must_use]
    pub fn symmetry_relevant(&self) -> bool {
        matches!(self, Asset::Pv(_) | Asset::Battery(_) | Asset::Evse(_))
    }

    /// The power the § 14a threshold is applied to.
    ///
    /// For a heat pump this is compressor **plus** heating rod `[A1 2.4.1.b]`.
    #[must_use]
    pub fn steuve_power(&self) -> Power {
        match self {
            Asset::HeatPump(hp) => hp.group_power(),
            other => other.meta().connection_power,
        }
    }

    /// What the hardware itself can do, before any rule applies.
    ///
    /// The nameplate `connection_power` is what the network operator sees, and
    /// it is symmetric — which is wrong for almost every asset. A photovoltaic
    /// array cannot consume, a charge point without bidirectional hardware
    /// cannot export, and a battery's charging and discharging ratings are
    /// routinely different. Handing the guard a symmetric interval invites it to
    /// command a value the device will silently ignore, and an ignored command
    /// is indistinguishable from a driver fault in the log.
    #[must_use]
    pub fn ratings(&self) -> Envelope {
        match self {
            // An inverter produces; the load convention makes that negative. The
            // small standby draw at night is not worth modelling as a floor.
            Asset::Pv(pv) => Envelope::new(-pv.ac_nominal.max(Power::ZERO), Power::ZERO),
            Asset::Battery(b) => {
                Envelope::new(-b.max_discharge.abs(), b.max_charge.max(Power::ZERO))
            }
            Asset::Evse(e) => {
                let ceiling = e.highest_power();
                Envelope::new(
                    if e.bidirectional {
                        -ceiling
                    } else {
                        Power::ZERO
                    },
                    ceiling,
                )
            }
            Asset::HeatPump(hp) => Envelope::new(Power::ZERO, hp.group_power()),
            Asset::Dhw(t) => Envelope::new(Power::ZERO, t.heater.max(Power::ZERO)),
            // A load consumes. A symmetric envelope would make a dishwasher
            // look like a generator, and the guard would offer it a share of the
            // connection's export capacity and of the § 9 EEG cap — capacity a
            // device that cannot produce a watt then holds against the inverter
            // that can.
            Asset::Load(l) => Envelope::new(Power::ZERO, l.meta.connection_power.max(Power::ZERO)),
            Asset::Relay(r) => Envelope::new(Power::ZERO, r.meta.connection_power.max(Power::ZERO)),
            // A meter is never commanded; its envelope exists only so that the
            // map has an entry for every asset, and it is symmetric because a
            // meter sees current in both directions.
            Asset::Meter(m) => {
                let p = m.meta.connection_power.abs();
                Envelope::new(-p, p)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Phase;

    fn meta(id: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            CircuitId::new("main").unwrap(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
    }

    #[test]
    fn a_heat_pumps_steuve_power_includes_its_heating_rod() {
        let hp = HeatPump {
            meta: meta("wp", 5.0),
            electrical_nominal: Power::from_kw(5.0),
            heating_rod: Some(Power::from_kw(6.0)),
            control: HeatPumpControl::PowerCeiling,
            modulating: true,
        };
        // 5 + 6 = 11 kW: exactly the threshold above which the 0,4 scaling of
        // [A1 4.5.1] applies, so getting this sum wrong changes the minimum power.
        assert_eq!(Asset::HeatPump(hp).steuve_power(), Power::from_kw(11.0));
    }

    #[test]
    fn a_public_charge_point_is_not_a_fallgruppe() {
        let mk = |public| {
            Asset::Evse(Evse {
                meta: meta("wallbox", 11.0),
                min_current: Current::new(6.0),
                max_current: Current::new(16.0),
                bidirectional: false,
                public,
                charge_limit: None,
            })
        };
        assert_eq!(mk(false).fallgruppe(), Some(Fallgruppe::Ladepunkt));
        assert_eq!(mk(true).fallgruppe(), None);
    }

    #[test]
    fn ratings_are_asymmetric_because_hardware_is() {
        let pv = Asset::Pv(PvArray {
            meta: meta("pv", 9.8),
            kwp_dc: Power::from_kw(9.8),
            ac_nominal: Power::from_kw(8.0),
            tilt_deg: 35.0,
            azimuth_deg: 180.0,
            cap_relief: CapRelief::None,
        });
        // An inverter cannot consume, so its ceiling is zero, not +8 kW.
        assert_eq!(pv.ratings().ceiling, Power::ZERO);
        assert_eq!(pv.ratings().floor, Power::from_kw(-8.0));

        let wallbox = Asset::Evse(Evse {
            meta: meta("wallbox", 11.0),
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
            charge_limit: None,
        });
        // A one-way charge point cannot export whatever the nameplate says.
        assert_eq!(wallbox.ratings().floor, Power::ZERO);
        assert!((wallbox.ratings().ceiling.kw() - 11.0).abs() < 1e-9);
    }

    #[test]
    fn a_batterys_backup_reserve_beats_its_operating_floor() {
        let b = Battery {
            meta: meta("battery", 5.0),
            capacity: Energy::from_kwh(10.0),
            max_charge: Power::from_kw(5.0),
            max_discharge: Power::from_kw(4.0),
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.05).unwrap(),
            soc_max: Soc::new(0.95).unwrap(),
            reserve_soc: Soc::new(0.30).unwrap(),
            chemistry: Chemistry::Lfp,
            grid_charging_allowed: true,
        };
        assert_eq!(b.discharge_floor(), Soc::new(0.30).unwrap());
        let ratings = Asset::Battery(b).ratings();
        assert_eq!(ratings.floor, Power::from_kw(-4.0));
        assert_eq!(ratings.ceiling, Power::from_kw(5.0));
    }

    #[test]
    fn capabilities_compose_and_answer_questions() {
        let caps = Capabilities::MEASURE | Capabilities::LIMIT_CONSUMPTION | Capabilities::IDENTIFY;
        assert!(caps.contains(Capabilities::LIMIT_CONSUMPTION));
        assert!(!caps.contains(Capabilities::BIDIRECTIONAL));
        assert!(caps.contains(Capabilities::MEASURE | Capabilities::IDENTIFY));
    }

    #[test]
    fn a_single_phase_asset_names_its_conductor() {
        let m = AssetMeta::new(
            AssetId::new("heizstab").unwrap(),
            CircuitId::new("main").unwrap(),
            PhaseConnection::Single { phase: Phase::L2 },
            Power::from_kw(3.0),
        );
        assert_eq!(m.phases.count(PhaseMode::Three), 1, "it cannot be switched");
        assert_eq!(m.phases.single_phase_conductor(), Some(Phase::L2));
    }

    #[test]
    fn a_switchable_charge_point_has_two_minimums_and_the_lower_one_matters() {
        let mut m = meta("wallbox", 11.0);
        m.phases = PhaseConnection::Switchable { phase: Phase::L1 };
        let e = Evse {
            meta: m,
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
            charge_limit: None,
        };
        // 6 A × 230 V × 3 = 4,14 kW three-phase; a third of it on one conductor.
        assert!((e.min_power(PhaseMode::Three).kw() - 4.14).abs() < 1e-9);
        assert!((e.min_power(PhaseMode::Single).kw() - 1.38).abs() < 1e-9);
        assert_eq!(e.lowest_useful_power(), e.min_power(PhaseMode::Single));
        assert_eq!(e.highest_power(), e.max_power(PhaseMode::Three));

        // A fixed three-phase charge point has only the one.
        let mut m = meta("fixed", 11.0);
        m.phases = PhaseConnection::Three;
        let fixed = Evse { meta: m, ..e };
        assert_eq!(
            fixed.lowest_useful_power(),
            fixed.min_power(PhaseMode::Three)
        );
    }
}
