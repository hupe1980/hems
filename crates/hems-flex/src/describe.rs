//! What a Resource Manager tells a Customer Energy Manager it can do.
//!
//! Each function returns the S2 description *and* the identifiers inside it,
//! because an instruction refers to an operation mode by ID and there is no way
//! to act on one you cannot name. Handing back a bare `SystemDescription` would
//! be a description you can send but not obey.
//!
//! # Fill level units
//!
//! S2 leaves the fill level unit to the Resource Manager and requires only that
//! the fill *rate* be expressed in **that unit per second**. hems uses
//! kilowatt-hours for every store — a battery, a hot water tank and a car all
//! hold energy — so a fill rate is kWh/s. The number is small; that is what the
//! standard asks for, and [`KWH_PER_S_PER_W`] converts once, in one place.

use hems_core::asset::{Battery, Evse, HeatPump, PvArray};
use hems_core::prelude::*;
use time::OffsetDateTime;

/// S2's generated types carry `chrono` timestamps; the rest of hems uses
/// `time`. Converting in one place beats letting two clocks into the domain.
fn utc(at: OffsetDateTime) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_nanos(
        i64::try_from(at.unix_timestamp_nanos()).unwrap_or(i64::MAX),
    )
}
use hems_device::SgReadyState;
use s2energy::common::{Commodity, CommodityQuantity, Duration, Id, NumberRange, PowerRange, Role};
use s2energy::{frbc, ombc, pebc};

use crate::map::{control_type_for, roles_for};

/// The namespace hems mints its S2 identifiers in.
///
/// Randomly chosen once, then fixed forever — that is what a UUID namespace is.
const HEMS_NAMESPACE: uuid::Uuid = uuid::uuid!("6f9c1f0e-4b3a-5d2e-9a71-2c8e5b4d7f10");

/// A stable S2 identifier for one named part of one asset.
///
/// **Deterministic on purpose.** S2 instructions name an operation mode by ID,
/// so a Resource Manager that re-mints its IDs on every reconnect invalidates
/// every description a Customer Energy Manager cached — and a manager that
/// replays a plan it made ten minutes ago addresses modes that no longer exist.
/// Deriving them from the asset's own identity means a restart changes nothing,
/// which is the behaviour a manager is entitled to assume.
fn stable_id(asset: &AssetId, part: &str) -> Id {
    Id(uuid::Uuid::new_v5(
        &HEMS_NAMESPACE,
        format!("{asset}/{part}").as_bytes(),
    ))
}

/// A watt sustained for a second is this many kilowatt-hours — the conversion
/// from a power the hardware quotes to the fill rate S2 wants.
pub const KWH_PER_S_PER_W: f64 = 1.0 / 3_600_000.0;

/// How long hems takes to act on an instruction, as advertised to a CEM.
///
/// One control tick. Claiming less would invite a manager to plan on a
/// responsiveness the arbiter does not have.
const PROCESSING_DELAY: Duration = Duration(1_000);

/// Which commodity a device's power is measured in, given the mode it is in.
///
/// A switchable charge point changes this when it switches, which is why the
/// mode is a parameter and not a property of the wiring: a description that
/// still says `ElectricPower3PhaseSymmetric` after the contactor has dropped two
/// conductors is describing a device that no longer exists.
fn quantity(asset_phases: PhaseConnection, mode: PhaseMode) -> CommodityQuantity {
    match asset_phases.clamp_mode(mode) {
        PhaseMode::Single => CommodityQuantity::ElectricPowerL1,
        PhaseMode::Three => CommodityQuantity::ElectricPower3PhaseSymmetric,
    }
}

/// The Resource Manager announcement for any asset — who it is, what roles it
/// plays, and which control type it will accept instructions in.
#[must_use]
pub fn resource_manager_details(
    asset: &Asset,
    mode: PhaseMode,
    has_deadline: bool,
) -> s2energy::common::ResourceManagerDetails {
    let roles = roles_for(asset)
        .into_iter()
        .map(|role| Role {
            role,
            commodity: Commodity::Electricity,
        })
        .collect();

    s2energy::common::ResourceManagerDetails::builder()
        .message_id(Id::generate())
        .resource_id(stable_id(asset.id(), "resource"))
        .name(asset.id().to_string())
        .roles(roles)
        .instruction_processing_delay(PROCESSING_DELAY)
        .available_control_types(vec![control_type_for(asset, has_deadline).into()])
        .provides_forecast(false)
        .provides_power_measurement_types(vec![quantity(asset.meta().phases, mode)])
        .build()
}

/// A battery, described as fill-rate-based control, with the IDs needed to read
/// an instruction back.
#[derive(Debug, Clone)]
pub struct BatteryDescription {
    /// The description to send.
    pub system: frbc::SystemDescription,
    /// The single actuator: the inverter.
    pub actuator: Id,
    /// Charging. Factor 0 is idle, factor 1 is full charge power.
    pub charge: Id,
    /// Discharging. Factor 0 is idle, factor 1 is full discharge power.
    pub discharge: Id,
}

/// Describe a battery as a store with a level and two directions.
///
/// Both operation modes start at idle, so an `operation_mode_factor` of zero
/// means *stop* whichever mode is active. A manager that has to switch mode to
/// stop a battery will overshoot every time it changes its mind.
#[must_use]
pub fn describe_battery(battery: &Battery, valid_from: OffsetDateTime) -> BatteryDescription {
    let capacity = battery.capacity.kwh();
    let usable = NumberRange {
        start_of_range: battery.soc_min.fraction() * capacity,
        end_of_range: battery.soc_max.fraction() * capacity,
    };
    let q = quantity(battery.meta.phases, battery.meta.phases.default_mode());

    // Round-trip losses belong in the *rate*, not the power: a battery told to
    // draw 5 kW stores less than 5 kWh per hour, and a manager that plans on the
    // electrical figure will believe the battery is full before it is.
    let charge_rate = battery.max_charge.get() * battery.efficiency_charge * KWH_PER_S_PER_W;
    let discharge_rate = battery.max_discharge.get() * KWH_PER_S_PER_W
        / battery.efficiency_discharge.max(f64::EPSILON);

    let charge = stable_id(&battery.meta.id, "battery/charge");
    let discharge = stable_id(&battery.meta.id, "battery/discharge");
    let actuator = stable_id(&battery.meta.id, "battery/inverter");

    let mode = |id: &Id, label: &str, rate_end: f64, power_end: f64| {
        frbc::OperationMode::builder()
            .id(id.clone())
            .diagnostic_label(label)
            .abnormal_condition_only(false)
            .elements(vec![frbc::OperationModeElement {
                fill_level_range: usable.clone(),
                fill_rate: NumberRange {
                    start_of_range: 0.0,
                    end_of_range: rate_end,
                },
                power_ranges: vec![PowerRange {
                    start_of_range: 0.0,
                    end_of_range: power_end,
                    commodity_quantity: q,
                }],
                running_costs: None,
            }])
            .build()
    };

    let system = frbc::SystemDescription::builder()
        .message_id(Id::generate())
        .valid_from(utc(valid_from))
        .actuators(vec![
            frbc::ActuatorDescription::builder()
                .id(actuator.clone())
                .diagnostic_label("inverter")
                .supported_commodities(vec![Commodity::Electricity])
                .operation_modes(vec![
                    mode(&charge, "charge", charge_rate, battery.max_charge.get()),
                    // Load convention: discharging is negative power, and it
                    // empties the store, so the rate is negative too.
                    mode(
                        &discharge,
                        "discharge",
                        -discharge_rate,
                        -battery.max_discharge.get(),
                    ),
                ])
                .transitions(Vec::new())
                .timers(Vec::new())
                .build(),
        ])
        .storage(
            frbc::StorageDescription::builder()
                .diagnostic_label("battery")
                .fill_level_label("kWh")
                .fill_level_range(usable.clone())
                .provides_leakage_behaviour(false)
                .provides_fill_level_target_profile(false)
                .provides_usage_forecast(false)
                .build(),
        )
        .build();

    BatteryDescription {
        system,
        actuator,
        charge,
        discharge,
    }
}

/// Describe a charge point as a power envelope.
///
/// The consequence of a tighter envelope is [`Defer`] — the car charges later,
/// nothing is lost. That single field is why a manager may curtail a wallbox
/// freely and must think twice about an inverter.
///
/// [`Defer`]: pebc::PowerEnvelopeConsequenceType::Defer
#[must_use]
pub fn describe_evse(
    evse: &Evse,
    mode: PhaseMode,
    valid_from: OffsetDateTime,
) -> pebc::PowerConstraints {
    let to_power = |c: Current| match evse.meta.phases.clamp_mode(mode) {
        PhaseMode::Single => c.to_power_1p(NOMINAL_VOLTAGE),
        PhaseMode::Three => c.to_power_3p(NOMINAL_VOLTAGE),
    };
    // The floor is the minimum charging current, not zero: between zero and
    // 6 A a charge point cannot operate at all, and a manager that believes
    // 2 kW is available will keep the car idle while thinking it is charging.
    let floor = to_power(evse.min_current).get();
    let ceiling = to_power(evse.max_current)
        .get()
        .min(evse.meta.connection_power.get());

    pebc::PowerConstraints::builder()
        .message_id(Id::generate())
        .id(stable_id(&evse.meta.id, "evse/envelope"))
        .valid_from(utc(valid_from))
        .consequence_type(pebc::PowerEnvelopeConsequenceType::Defer)
        .allowed_limit_ranges(vec![pebc::AllowedLimitRange {
            commodity_quantity: quantity(evse.meta.phases, mode),
            limit_type: pebc::PowerEnvelopeLimitType::UpperLimit,
            range_boundary: NumberRange {
                start_of_range: floor,
                end_of_range: ceiling,
            },
            abnormal_condition_only: false,
        }])
        .build()
}

/// Describe an inverter as a power envelope whose consequence is [`Vanish`].
///
/// Curtailed sunlight does not come back later. § 9 EEG and § 51 make hems ask
/// for this often enough that saying so precisely matters.
///
/// [`Vanish`]: pebc::PowerEnvelopeConsequenceType::Vanish
#[must_use]
pub fn describe_pv(pv: &PvArray, valid_from: OffsetDateTime) -> pebc::PowerConstraints {
    pebc::PowerConstraints::builder()
        .message_id(Id::generate())
        .id(stable_id(&pv.meta.id, "pv/envelope"))
        .valid_from(utc(valid_from))
        .consequence_type(pebc::PowerEnvelopeConsequenceType::Vanish)
        .allowed_limit_ranges(vec![pebc::AllowedLimitRange {
            commodity_quantity: quantity(pv.meta.phases, pv.meta.phases.default_mode()),
            limit_type: pebc::PowerEnvelopeLimitType::LowerLimit,
            // Load convention: full production is the most negative value the
            // envelope may reach, and zero is full curtailment.
            range_boundary: NumberRange {
                start_of_range: -pv.ac_nominal.get(),
                end_of_range: 0.0,
            },
            abnormal_condition_only: false,
        }])
        .build()
}

/// A heat pump's three SG Ready states, described as operation modes.
#[derive(Debug, Clone)]
pub struct HeatPumpDescription {
    /// The description to send.
    pub system: ombc::SystemDescription,
    /// Operation mode IDs, in the order [`SgReadyState`] numbers them.
    pub modes: [(Id, SgReadyState); 3],
}

impl HeatPumpDescription {
    /// The SG Ready state an operation mode ID stands for.
    #[must_use]
    pub fn state_of(&self, id: &Id) -> Option<SgReadyState> {
        self.modes
            .iter()
            .find(|(mode, _)| mode == id)
            .map(|(_, state)| *state)
    }
}

/// Describe an SG Ready heat pump as three operation modes.
///
/// Three, not four: BWP's SG Ready v1.1 defines state 1 (limited), 2 (normal)
/// and 3 (boost) for an energy manager, and the fourth is the manufacturer's
/// own forced-run signal, which no HEMS may assert.
///
/// The advertised power ranges are **not necessarily ordered**. § 14a's value
/// for state 1 is a guaranteed *minimum* — 4,2 kW, or 40 % of the grid
/// connection power above 11 kW — so a 4 kW heat pump on a 30 kW connection is
/// guaranteed more than it can draw, and its state 1 is its full rating: higher
/// than the half-load of state 2. A manager reading these ranges will do the
/// right thing; one assuming the states descend will not.
#[must_use]
pub fn describe_heat_pump(
    hp: &HeatPump,
    grid_connection_power: Power,
    valid_from: OffsetDateTime,
) -> HeatPumpDescription {
    let q = quantity(hp.meta.phases, hp.meta.phases.default_mode());
    let states = [
        SgReadyState::Limited,
        SgReadyState::Normal,
        SgReadyState::Boost,
    ]
    .map(|state| {
        let expected =
            hems_device::expected_power(state, hp.electrical_nominal, grid_connection_power);
        (Id::generate(), state, expected)
    });

    let system = ombc::SystemDescription::builder()
        .message_id(Id::generate())
        .valid_from(utc(valid_from))
        .operation_modes(
            states
                .iter()
                .map(|(id, state, expected)| {
                    ombc::OperationMode::builder()
                        .id(id.clone())
                        .diagnostic_label(format!("SG Ready {}", state.number()))
                        .abnormal_condition_only(false)
                        .power_ranges(vec![PowerRange {
                            start_of_range: 0.0,
                            end_of_range: expected.get(),
                            commodity_quantity: q,
                        }])
                        .build()
                })
                .collect(),
        )
        .transitions(Vec::new())
        .timers(Vec::new())
        .build();

    HeatPumpDescription {
        system,
        modes: states.map(|(id, state, _)| (id, state)),
    }
}
