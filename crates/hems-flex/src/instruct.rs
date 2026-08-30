//! Reading an S2 instruction back into something the site can execute.
//!
//! An instruction is not a command. It names an operation mode by ID and gives
//! a factor in `[0, 1]`; turning that into watts needs the description that was
//! sent, which is why every function here takes one. A Resource Manager that
//! interprets an instruction without reference to what it advertised is
//! guessing.

use hems_core::asset::Battery;
use hems_core::prelude::*;
use hems_device::SgReadyState;
use s2energy::{frbc, ombc, pebc};

use crate::describe::{BatteryDescription, HeatPumpDescription};

/// Why an instruction could not be executed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstructError {
    /// The instruction names an actuator this Resource Manager never described.
    #[error("instruction addresses an unknown actuator")]
    UnknownActuator,
    /// The instruction names an operation mode this Resource Manager never
    /// described. Refusing is the only safe answer: a mode we cannot name is a
    /// mode whose power range we do not know.
    #[error("instruction names an unknown operation mode")]
    UnknownOperationMode,
    /// The factor was outside `[0, 1]`, or not a number.
    #[error("operation mode factor {0} is outside [0, 1]")]
    FactorOutOfRange(String),
    /// The instruction carried no envelope element to read a limit from.
    #[error("power envelope contains no elements")]
    EmptyEnvelope,
    /// No envelope in the instruction covers the commodity the asset consumes.
    #[error("no power envelope for this asset's commodity")]
    NoMatchingEnvelope,
}

fn factor(value: f64) -> Result<f64, InstructError> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(value)
    } else {
        Err(InstructError::FactorOutOfRange(value.to_string()))
    }
}

/// The active power an FRBC instruction asks a battery for.
///
/// # Errors
/// [`InstructError`] when the instruction names something that was never
/// described, or carries a factor outside `[0, 1]`.
pub fn battery_power(
    description: &BatteryDescription,
    instruction: &frbc::Instruction,
    battery: &Battery,
) -> Result<Power, InstructError> {
    if instruction.actuator_id != description.actuator {
        return Err(InstructError::UnknownActuator);
    }
    let f = factor(instruction.operation_mode_factor)?;

    if instruction.operation_mode == description.charge {
        Ok(Power::new(battery.max_charge.get() * f))
    } else if instruction.operation_mode == description.discharge {
        // Load convention: discharging is negative.
        Ok(Power::new(-battery.max_discharge.get() * f))
    } else {
        Err(InstructError::UnknownOperationMode)
    }
}

/// The SG Ready state an OMBC instruction selects.
///
/// The factor is ignored: SG Ready has no continuum, and an instruction that
/// asks for half of "boost" is asking for boost.
///
/// # Errors
/// [`InstructError::UnknownOperationMode`] when the mode was never described.
pub fn heat_pump_state(
    description: &HeatPumpDescription,
    instruction: &ombc::Instruction,
) -> Result<SgReadyState, InstructError> {
    description
        .state_of(&instruction.operation_mode_id)
        .ok_or(InstructError::UnknownOperationMode)
}

/// The nearest-term ceiling a PEBC instruction imposes, for the given commodity.
///
/// S2 sends a whole envelope over time; the arbiter runs now, so the element in
/// force now is the one that matters. The rest belongs to the optimiser, which
/// takes it as a forecast rather than a command.
///
/// # Errors
/// [`InstructError::NoMatchingEnvelope`] or [`InstructError::EmptyEnvelope`].
pub fn envelope_now(
    instruction: &pebc::Instruction,
    quantity: s2energy::common::CommodityQuantity,
) -> Result<(Power, Power), InstructError> {
    let envelope = instruction
        .power_envelopes
        .iter()
        .find(|e| e.commodity_quantity == quantity)
        .ok_or(InstructError::NoMatchingEnvelope)?;
    let element = envelope
        .power_envelope_elements
        .first()
        .ok_or(InstructError::EmptyEnvelope)?;
    Ok((
        Power::new(element.lower_limit),
        Power::new(element.upper_limit),
    ))
}

/// The command that carries out a PEBC instruction on `asset`.
///
/// A producer is bounded from below (curtailment), a consumer from above. Which
/// end of the envelope is the operative one is a property of the asset, not of
/// the message — an inverter handed a `ConsumptionCeiling` would ignore it.
///
/// # Errors
/// [`InstructError`] when the instruction carries nothing usable for the asset.
pub fn envelope_command(
    instruction: &pebc::Instruction,
    asset: &Asset,
    mode: PhaseMode,
) -> Result<Command, InstructError> {
    let quantity = match asset.meta().phases.clamp_mode(mode) {
        PhaseMode::Single => s2energy::common::CommodityQuantity::ElectricPowerL1,
        PhaseMode::Three => s2energy::common::CommodityQuantity::ElectricPower3PhaseSymmetric,
    };
    let (lower, upper) = envelope_now(instruction, quantity)?;

    Ok(match asset {
        Asset::Pv(_) => Command::ProductionCeiling(Power::new(-lower.get()).max(Power::ZERO)),
        _ => Command::ConsumptionCeiling(upper.max(Power::ZERO)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::{
        HeatPumpDescription, describe_battery, describe_evse, describe_heat_pump, describe_pv,
    };
    use hems_core::asset::{AssetMeta, Chemistry, Evse, HeatPump, PvArray};
    use s2energy::common::{Duration, Id};
    use time::OffsetDateTime;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 12:00 UTC);

    fn meta(id: &str, kw: f64, phases: PhaseConnection) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            CircuitId::new("main").unwrap(),
            phases,
            Power::from_kw(kw),
        )
    }

    fn battery() -> Battery {
        Battery {
            meta: meta("battery", 5.0, PhaseConnection::Three),
            capacity: Energy::from_kwh(10.0),
            max_charge: Power::from_kw(5.0),
            max_discharge: Power::from_kw(5.0),
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.1).unwrap(),
            soc_max: Soc::FULL,
            reserve_soc: Soc::EMPTY,
            chemistry: Chemistry::Lfp,
            grid_charging_allowed: true,
        }
    }

    fn frbc_instruction(actuator: Id, mode: Id, f: f64) -> frbc::Instruction {
        frbc::Instruction::builder()
            .message_id(Id::generate())
            .id(Id::generate())
            .actuator_id(actuator)
            .operation_mode(mode)
            .operation_mode_factor(f)
            .execution_time(chrono::DateTime::from_timestamp_nanos(
                i64::try_from(T0.unix_timestamp_nanos()).unwrap(),
            ))
            .abnormal_condition(false)
            .build()
    }

    #[test]
    fn a_charge_instruction_becomes_positive_power_and_a_discharge_one_negative() {
        let b = battery();
        let d = describe_battery(&b, T0);

        let charge = frbc_instruction(d.actuator.clone(), d.charge.clone(), 1.0);
        assert_eq!(battery_power(&d, &charge, &b).unwrap(), Power::from_kw(5.0));

        let discharge = frbc_instruction(d.actuator.clone(), d.discharge.clone(), 1.0);
        assert_eq!(
            battery_power(&d, &discharge, &b).unwrap(),
            Power::from_kw(-5.0)
        );
    }

    #[test]
    fn factor_zero_is_idle_in_either_mode() {
        // The reason both modes start at idle: a manager can stop the battery
        // without first switching mode, so it never overshoots on a change of
        // mind.
        let b = battery();
        let d = describe_battery(&b, T0);
        for mode in [d.charge.clone(), d.discharge.clone()] {
            let idle = frbc_instruction(d.actuator.clone(), mode, 0.0);
            assert_eq!(battery_power(&d, &idle, &b).unwrap(), Power::ZERO);
        }
    }

    #[test]
    fn an_unknown_mode_or_actuator_is_refused_rather_than_guessed() {
        let b = battery();
        let d = describe_battery(&b, T0);
        assert_eq!(
            battery_power(
                &d,
                &frbc_instruction(d.actuator.clone(), Id::generate(), 1.0),
                &b
            ),
            Err(InstructError::UnknownOperationMode)
        );
        assert_eq!(
            battery_power(
                &d,
                &frbc_instruction(Id::generate(), d.charge.clone(), 1.0),
                &b
            ),
            Err(InstructError::UnknownActuator)
        );
    }

    #[test]
    fn a_factor_outside_the_unit_interval_is_refused() {
        let b = battery();
        let d = describe_battery(&b, T0);
        for bad in [1.5, -0.1, f64::NAN] {
            assert!(matches!(
                battery_power(
                    &d,
                    &frbc_instruction(d.actuator.clone(), d.charge.clone(), bad),
                    &b
                ),
                Err(InstructError::FactorOutOfRange(_))
            ));
        }
    }

    #[test]
    fn the_described_fill_rate_accounts_for_the_round_trip_loss() {
        // 5 kW into a 95 %-efficient battery stores 4.75 kWh per hour. A manager
        // planning on 5 would think the battery full a quarter of an hour early.
        let d = describe_battery(&battery(), T0);
        let charge = &d.system.actuators[0].operation_modes[0].elements[0];
        let stored_per_hour = charge.fill_rate.end_of_range * 3600.0;
        assert!((stored_per_hour - 4.75).abs() < 1e-9, "{stored_per_hour}");
    }

    #[test]
    fn describing_the_same_asset_twice_yields_the_same_identifiers() {
        // The whole reason the IDs are derived rather than generated: an
        // instruction names an operation mode by ID, so a Resource Manager that
        // re-mints them on reconnect invalidates every description the manager
        // cached — and a manager replaying a ten-minute-old plan addresses modes
        // that no longer exist.
        let b = battery();
        let first = describe_battery(&b, T0);
        let second = describe_battery(&b, T0 + time::Duration::hours(3));
        assert_eq!(first.charge, second.charge);
        assert_eq!(first.discharge, second.discharge);
        assert_eq!(first.actuator, second.actuator);

        // An instruction issued against the first description is still
        // executable against the second.
        let instruction = frbc_instruction(first.actuator.clone(), first.charge.clone(), 1.0);
        assert_eq!(
            battery_power(&second, &instruction, &b).unwrap(),
            Power::from_kw(5.0)
        );
    }

    #[test]
    fn two_batteries_do_not_share_identifiers() {
        let mut other = battery();
        other.meta = meta("battery-2", 5.0, PhaseConnection::Three);
        assert_ne!(
            describe_battery(&battery(), T0).charge,
            describe_battery(&other, T0).charge
        );
    }

    #[test]
    fn the_usable_fill_level_range_excludes_the_reserved_bottom() {
        let d = describe_battery(&battery(), T0);
        let range = &d.system.storage.fill_level_range;
        assert!((range.start_of_range - 1.0).abs() < 1e-9);
        assert!((range.end_of_range - 10.0).abs() < 1e-9);
    }

    fn evse() -> Evse {
        Evse {
            meta: meta("wallbox", 11.0, PhaseConnection::Three),
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
        }
    }

    #[test]
    fn a_charge_points_envelope_floor_is_its_minimum_current_not_zero() {
        // Below 6 A a wallbox cannot operate. An envelope that says 0 invites a
        // manager to allocate 2 kW and wonder why nothing charges.
        let c = describe_evse(&evse(), PhaseMode::Three, T0);
        let range = &c.allowed_limit_ranges[0].range_boundary;
        assert!((range.start_of_range - 4140.0).abs() < 1.0, "{range:?}");
        assert!((range.end_of_range - 11000.0).abs() < 1.0, "{range:?}");
    }

    #[test]
    fn deferring_a_car_and_losing_the_sun_are_different_consequences() {
        // The one field that tells a manager curtailing PV costs energy for good
        // while curtailing a wallbox only moves it.
        assert_eq!(
            describe_evse(&evse(), PhaseMode::Three, T0).consequence_type,
            pebc::PowerEnvelopeConsequenceType::Defer
        );
        assert_eq!(
            describe_pv(&pv(), T0).consequence_type,
            pebc::PowerEnvelopeConsequenceType::Vanish
        );
    }

    fn pv() -> PvArray {
        PvArray {
            meta: meta("pv", 9.8, PhaseConnection::Three),
            kwp_dc: Power::from_kw(9.8),
            ac_nominal: Power::from_kw(8.0),
            tilt_deg: 35.0,
            azimuth_deg: 180.0,
            cap_relief: CapRelief::None,
        }
    }

    fn pebc_instruction(
        quantity: s2energy::common::CommodityQuantity,
        lower: f64,
        upper: f64,
    ) -> pebc::Instruction {
        pebc::Instruction::builder()
            .message_id(Id::generate())
            .id(Id::generate())
            .execution_time(chrono::DateTime::from_timestamp_nanos(
                i64::try_from(T0.unix_timestamp_nanos()).unwrap(),
            ))
            .abnormal_condition(false)
            .power_constraints_id(Id::generate())
            .power_envelopes(vec![pebc::PowerEnvelope {
                id: Id::generate(),
                commodity_quantity: quantity,
                power_envelope_elements: vec![pebc::PowerEnvelopeElement {
                    duration: Duration(900_000),
                    lower_limit: lower,
                    upper_limit: upper,
                }],
            }])
            .build()
    }

    #[test]
    fn an_envelope_bounds_a_consumer_from_above_and_a_producer_from_below() {
        let q = s2energy::common::CommodityQuantity::ElectricPower3PhaseSymmetric;
        let instruction = pebc_instruction(q, -4000.0, 4200.0);

        let wallbox = Asset::Evse(evse());
        assert_eq!(
            envelope_command(&instruction, &wallbox, PhaseMode::Three).unwrap(),
            Command::ConsumptionCeiling(Power::from_kw(4.2))
        );

        // Same message, opposite end read — because an inverter handed a
        // consumption ceiling would ignore it.
        let inverter = Asset::Pv(pv());
        assert_eq!(
            envelope_command(&instruction, &inverter, PhaseMode::Three).unwrap(),
            Command::ProductionCeiling(Power::from_kw(4.0))
        );
    }

    #[test]
    fn an_envelope_for_another_commodity_is_not_silently_applied() {
        let instruction = pebc_instruction(
            s2energy::common::CommodityQuantity::HeatThermalPower,
            0.0,
            4200.0,
        );
        assert_eq!(
            envelope_command(&instruction, &Asset::Evse(evse()), PhaseMode::Three),
            Err(InstructError::NoMatchingEnvelope)
        );
    }

    #[test]
    fn a_switchable_charge_point_reads_a_different_envelope_in_each_mode() {
        // The description a Customer Energy Manager is sent depends on the mode
        // the contactor is actually in, not on the wiring: an envelope for
        // `ElectricPower3PhaseSymmetric` says nothing about a device currently
        // drawing on one conductor.
        let mut switchable = evse();
        switchable.meta = meta(
            "wallbox",
            11.0,
            PhaseConnection::Switchable { phase: Phase::L1 },
        );
        let one_phase = pebc_instruction(
            s2energy::common::CommodityQuantity::ElectricPowerL1,
            0.0,
            3000.0,
        );
        let asset = Asset::Evse(switchable);
        assert_eq!(
            envelope_command(&one_phase, &asset, PhaseMode::Single).unwrap(),
            Command::ConsumptionCeiling(Power::from_kw(3.0))
        );
        assert_eq!(
            envelope_command(&one_phase, &asset, PhaseMode::Three),
            Err(InstructError::NoMatchingEnvelope)
        );
    }

    #[test]
    fn a_single_phase_asset_reads_its_own_conductors_envelope() {
        let mut single = evse();
        single.meta = meta("wallbox", 3.7, PhaseConnection::Single { phase: Phase::L1 });
        let instruction = pebc_instruction(
            s2energy::common::CommodityQuantity::ElectricPowerL1,
            0.0,
            3000.0,
        );
        assert_eq!(
            envelope_command(&instruction, &Asset::Evse(single), PhaseMode::Single).unwrap(),
            Command::ConsumptionCeiling(Power::from_kw(3.0))
        );
    }

    fn heat_pump() -> HeatPump {
        HeatPump {
            meta: meta("wp", 9.0, PhaseConnection::Three),
            electrical_nominal: Power::from_kw(4.0),
            heating_rod: None,
            control: HeatPumpControl::SgReady,
            modulating: true,
        }
    }

    #[test]
    fn a_heat_pump_is_described_with_three_modes_and_each_maps_back() {
        let d = describe_heat_pump(&heat_pump(), Power::from_kw(30.0), T0);
        assert_eq!(d.system.operation_modes.len(), 3);
        for (id, state) in &d.modes {
            let instruction = ombc::Instruction::builder()
                .message_id(Id::generate())
                .id(Id::generate())
                .execution_time(chrono::DateTime::from_timestamp_nanos(
                    i64::try_from(T0.unix_timestamp_nanos()).unwrap(),
                ))
                .operation_mode_id(id.clone())
                .operation_mode_factor(1.0)
                .abnormal_condition(false)
                .build();
            assert_eq!(heat_pump_state(&d, &instruction).unwrap(), *state);
        }
    }

    #[test]
    fn a_heat_pump_mode_we_never_described_is_refused() {
        let d = describe_heat_pump(&heat_pump(), Power::from_kw(30.0), T0);
        let instruction = ombc::Instruction::builder()
            .message_id(Id::generate())
            .id(Id::generate())
            .execution_time(chrono::DateTime::from_timestamp_nanos(
                i64::try_from(T0.unix_timestamp_nanos()).unwrap(),
            ))
            .operation_mode_id(Id::generate())
            .operation_mode_factor(1.0)
            .abnormal_condition(false)
            .build();
        assert_eq!(
            heat_pump_state(&d, &instruction),
            Err(InstructError::UnknownOperationMode)
        );
    }

    #[test]
    fn the_limited_mode_is_not_always_the_quietest_one() {
        // A fact worth encoding, because it surprises everyone: § 14a's state-1
        // value is a *guaranteed minimum*, not a reduction. A 4 kW heat pump on
        // a 30 kW connection is guaranteed 12 kW it cannot use, so its state 1
        // is its full rating — higher than the half-load of "normal".
        let small = describe_heat_pump(&heat_pump(), Power::from_kw(30.0), T0);
        let power_of = |d: &HeatPumpDescription, i: usize| {
            d.system.operation_modes[i].power_ranges[0].end_of_range
        };
        assert!(
            (power_of(&small, 0) - 4000.0).abs() < 1.0,
            "state 1 does not limit this unit"
        );
        assert!(power_of(&small, 1) < power_of(&small, 0));

        // A unit large enough for the value to bite orders the way one expects.
        let mut big = heat_pump();
        big.electrical_nominal = Power::from_kw(12.0);
        let large = describe_heat_pump(&big, Power::from_kw(11.0), T0);
        assert!(power_of(&large, 0) < power_of(&large, 1));
        assert!(power_of(&large, 1) < power_of(&large, 2));
    }
}
