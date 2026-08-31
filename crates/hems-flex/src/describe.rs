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

use hems_core::asset::{Battery, DhwTank, Evse, FlexibleLoad, HeatPump, Programme, PvArray};
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
use s2energy::{frbc, ombc, pebc, ppbc};

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
pub(crate) fn stable_id(asset: &AssetId, part: &str) -> Id {
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

/// A hot-water tank, described as fill-rate-based control.
#[derive(Debug, Clone)]
pub struct DhwDescription {
    /// The description to send.
    pub system: frbc::SystemDescription,
    /// The single actuator: the heater.
    pub actuator: Id,
    /// Heating. Factor 0 is idle, factor 1 is the heater at full power.
    pub heat: Id,
}

/// Describe a hot-water tank as a store with one direction.
///
/// The same shape as a battery and deliberately so — that is S2's whole
/// argument, and it is why a Customer Energy Manager that has never heard of a
/// Brauchwasserwärmepumpe can plan one. Three differences, and each is a fact
/// about water rather than about the encoding:
///
/// * **one operation mode, not two.** A tank cannot give its heat back to the
///   house's electricity, so there is no discharge to describe. It empties on
///   its own — by standing loss and by somebody having a shower — which is
///   `provides_leakage_behaviour`.
/// * **the fill rate carries the coefficient of performance.** A tank told to
///   draw 500 W of electricity gains 1,5 kW of heat, and a manager that plans on
///   the electrical figure will think the tank fills three times more slowly
///   than it does.
/// * **the fill level is heat above the lowest acceptable temperature**, in
///   kilowatt-hours like every other store here — the same quantity
///   [`DhwTank::usable_heat`] defines, so the description and the planner cannot
///   disagree about how full "full" is.
#[must_use]
pub fn describe_dhw(tank: &DhwTank, valid_from: OffsetDateTime) -> DhwDescription {
    let usable = NumberRange {
        start_of_range: 0.0,
        end_of_range: tank.usable_heat().kwh(),
    };
    let q = quantity(tank.meta.phases, tank.meta.phases.default_mode());
    let heat = stable_id(&tank.meta.id, "dhw/heat");
    let actuator = stable_id(&tank.meta.id, "dhw/heater");

    let system = frbc::SystemDescription::builder()
        .message_id(Id::generate())
        .valid_from(utc(valid_from))
        .actuators(vec![
            frbc::ActuatorDescription::builder()
                .id(actuator.clone())
                .diagnostic_label("water heater")
                .supported_commodities(vec![Commodity::Electricity])
                .operation_modes(vec![
                    frbc::OperationMode::builder()
                        .id(heat.clone())
                        .diagnostic_label("heat")
                        .abnormal_condition_only(false)
                        .elements(vec![frbc::OperationModeElement {
                            fill_level_range: usable.clone(),
                            fill_rate: NumberRange {
                                start_of_range: 0.0,
                                end_of_range: tank.heater.get()
                                    * tank.cop.max(0.0)
                                    * KWH_PER_S_PER_W,
                            },
                            power_ranges: vec![PowerRange {
                                start_of_range: 0.0,
                                end_of_range: tank.heater.get(),
                                commodity_quantity: q,
                            }],
                            running_costs: None,
                        }])
                        .build(),
                ])
                .transitions(Vec::new())
                .timers(Vec::new())
                .build(),
        ])
        .storage(
            frbc::StorageDescription::builder()
                .diagnostic_label("hot water tank")
                .fill_level_label("kWh")
                .fill_level_range(usable)
                // A tank that is not heated goes cold, and a manager that does
                // not know it will schedule the morning shower the night before.
                .provides_leakage_behaviour(true)
                .provides_fill_level_target_profile(false)
                .provides_usage_forecast(false)
                .build(),
        )
        .build();

    DhwDescription {
        system,
        actuator,
        heat,
    }
}

/// A shiftable appliance, described as power-profile-based control, with the
/// identifiers a `PPBC.ScheduleInstruction` has to name.
#[derive(Debug, Clone)]
pub struct ProgrammeDescription {
    /// The description to send.
    pub definition: ppbc::PowerProfileDefinition,
    /// The container the alternatives live in — one, here.
    pub container: Id,
    /// The sequence: the programme the appliance is loaded with.
    pub sequence: Id,
}

/// Describe a shiftable appliance as the one profile it is loaded with.
///
/// PPBC's container is a set of **alternatives** a manager may choose between —
/// eco versus intensive, 40 °C versus 60 °C. A machine reports the programme it
/// was actually set to, so hems sends one, and the manager's only decision is
/// *when*. Offering alternatives is a capability of the appliance rather than of
/// the energy manager: nothing in a house lets a HEMS change the wash.
///
/// `is_interruptible` is `false` and that is the whole point of the control
/// type. A dishwasher stopped halfway is not a dishwasher that resumes; it is a
/// dishwasher somebody has to restart, and a manager told otherwise will pause
/// one to shed a kilowatt.
///
/// The window is `[valid_from, deadline)`: `start_time` is the first moment the
/// household will let the programme begin and `end_time` the moment it must
/// already be finished — half-open, like every other window in this workspace.
#[must_use]
pub fn describe_programme(
    load: &FlexibleLoad,
    programme: &Programme,
    valid_from: OffsetDateTime,
    deadline: OffsetDateTime,
) -> ProgrammeDescription {
    let q = quantity(load.meta.phases, load.meta.phases.default_mode());
    let sequence = stable_id(&load.meta.id, "load/programme");
    let container = stable_id(&load.meta.id, "load/alternatives");

    let elements = programme
        .steps
        .iter()
        .map(|step| ppbc::PowerSequenceElement {
            duration: Duration(
                u64::try_from(hems_core::prelude::SLOT.whole_milliseconds()).unwrap_or(u64::MAX),
            ),
            power_values: vec![s2energy::common::PowerForecastValue {
                commodity_quantity: q,
                value_expected: step.get(),
                value_lower_68ppr: None,
                value_lower_95ppr: None,
                value_lower_limit: None,
                value_upper_68ppr: None,
                value_upper_95ppr: None,
                value_upper_limit: None,
            }],
        })
        .collect();

    let definition = ppbc::PowerProfileDefinition::builder()
        .message_id(Id::generate())
        .id(stable_id(&load.meta.id, "load/profile"))
        .start_time(utc(valid_from))
        .end_time(utc(deadline))
        .power_sequences_containers(vec![
            ppbc::PowerSequenceContainer::builder()
                .id(container.clone())
                .power_sequences(vec![
                    ppbc::PowerSequence::builder()
                        .id(sequence.clone())
                        .abnormal_condition_only(false)
                        // Started, never paused: see the note above.
                        .is_interruptible(false)
                        .elements(elements)
                        .build(),
                ])
                .build(),
        ])
        .build();

    ProgrammeDescription {
        definition,
        container,
        sequence,
    }
}

/// A charging session, as the store S2 says it is.
///
/// A parked car is a battery with a departure time, and everything the standard
/// needs to say about one is here: how full it is, how full it can get, and what
/// the household asked for. None of it is a property of the *charge point*,
/// which is why it is a separate type — a wallbox with no car plugged in is an
/// envelope and has nothing to fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvStorage {
    /// Energy in the car now.
    pub stored: Energy,
    /// Usable battery capacity.
    pub capacity: Energy,
    /// Charging efficiency, so the fill rate is what the car receives rather
    /// than what the meter sees.
    pub efficiency: f64,
}

/// A charge point with a car on it, described as fill-rate-based control.
#[derive(Debug, Clone)]
pub struct EvDescription {
    /// The description to send.
    pub system: frbc::SystemDescription,
    /// The single actuator: the charge point.
    pub actuator: Id,
    /// Charging.
    pub charge: Id,
    /// Discharging, for a bidirectional charge point.
    pub discharge: Option<Id>,
}

/// Describe a charge point with a car on it as a store.
///
/// This is the whole argument for S2 in one function. The same wallbox is a
/// [power envelope](describe_evse) when nothing is plugged in and a **store**
/// when something is, because what a Customer Energy Manager needs to be able to
/// say has changed — and a manager that has never heard of a car can plan this
/// one with exactly the code it plans a battery with.
///
/// Two things a battery's description does not have to carry:
///
/// * **the power range starts at the minimum, not at zero.** A charge point
///   below the 6 A of IEC 61851 is not charging slowly, it is idle, and S2's
///   `PowerRange` is where that fact belongs. A manager handed a range from zero
///   will ask for 2 kW on three conductors and believe a car is charging.
/// * **the fill level range is the whole battery**, and the household's own
///   target is not in it. A target is a `FillLevelTargetProfile`, which is a
///   message rather than a description, and folding it into the range would tell
///   a manager the car physically cannot hold more — which is how a manager
///   stops offering the surplus that would have filled it.
#[must_use]
pub fn describe_ev(
    evse: &Evse,
    session: &EvStorage,
    mode: PhaseMode,
    valid_from: OffsetDateTime,
) -> EvDescription {
    let held = evse.meta.phases.clamp_mode(mode);
    let max = evse.max_power(held);
    let min = evse.min_power(held);
    let q = quantity(evse.meta.phases, mode);
    let usable = NumberRange {
        start_of_range: 0.0,
        end_of_range: session.capacity.kwh(),
    };
    let efficiency = session.efficiency.clamp(f64::EPSILON, 1.0);

    let actuator = stable_id(&evse.meta.id, "evse/charger");
    let charge = stable_id(&evse.meta.id, "evse/charge");
    let discharge = evse
        .bidirectional
        .then(|| stable_id(&evse.meta.id, "evse/discharge"));

    let mode_of = |id: &Id, label: &str, rate: f64, from: f64, to: f64| {
        frbc::OperationMode::builder()
            .id(id.clone())
            .diagnostic_label(label)
            .abnormal_condition_only(false)
            .elements(vec![frbc::OperationModeElement {
                fill_level_range: usable.clone(),
                fill_rate: NumberRange {
                    start_of_range: 0.0,
                    end_of_range: rate,
                },
                power_ranges: vec![PowerRange {
                    start_of_range: from,
                    end_of_range: to,
                    commodity_quantity: q,
                }],
                running_costs: None,
            }])
            .build()
    };

    let mut modes = vec![mode_of(
        &charge,
        "charge",
        max.get() * efficiency * KWH_PER_S_PER_W,
        min.get(),
        max.get(),
    )];
    if let Some(id) = &discharge {
        modes.push(mode_of(
            id,
            "discharge",
            -max.get() * KWH_PER_S_PER_W / efficiency,
            -max.get(),
            -min.get(),
        ));
    }

    let system = frbc::SystemDescription::builder()
        .message_id(Id::generate())
        .valid_from(utc(valid_from))
        .actuators(vec![
            frbc::ActuatorDescription::builder()
                .id(actuator.clone())
                .diagnostic_label("charge point")
                .supported_commodities(vec![Commodity::Electricity])
                .operation_modes(modes)
                .transitions(Vec::new())
                .timers(Vec::new())
                .build(),
        ])
        .storage(
            frbc::StorageDescription::builder()
                .diagnostic_label("vehicle battery")
                .fill_level_label("kWh")
                .fill_level_range(usable)
                // A parked car does not measurably self-discharge over a night,
                // and claiming a leakage nobody can quantify is worse than
                // saying nothing.
                .provides_leakage_behaviour(false)
                // The household's departure target is a message of its own; see
                // the note above.
                .provides_fill_level_target_profile(false)
                .provides_usage_forecast(false)
                .build(),
        )
        .build();

    EvDescription {
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
        // Derived from the asset and the state, never minted: an instruction
        // names an operation mode by ID, so a Resource Manager that re-mints
        // them on reconnect invalidates every description a manager cached and
        // every instruction still in flight. This was the one description in
        // the crate that generated them, and the bug it hid is silent — a CEM
        // that replays a ten-minute-old plan addresses modes that no longer
        // exist and gets `UnknownOperationMode` for a heat pump that never
        // changed.
        let id = stable_id(
            &hp.meta.id,
            &format!("heat-pump/sg-ready-{}", state.number()),
        );
        (id, state, expected)
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
