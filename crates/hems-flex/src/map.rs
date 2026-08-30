//! Which control type an asset belongs to.
//!
//! The choice is not arbitrary and it is not per-device-class either: it follows
//! from **what the energy manager needs to be able to say** to the thing.
//!
//! A charge point with no departure time is a power envelope — tell it a bound
//! and it charges. The same charge point with a car that must be full by seven
//! is *storage*: it has a fill level, a rate and a target, and describing it as
//! an envelope throws all three away. So the mapping depends on the situation,
//! not only on the hardware, which is exactly the distinction S2 draws and
//! use-case-organised protocols cannot.

use hems_core::prelude::*;
use s2energy::common::{ControlType as S2ControlType, RoleType};

/// The S2 control types, as hems uses them.
///
/// A thin mirror of [`s2energy::common::ControlType`] so that the mapping can be
/// matched on and tested without constructing wire messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlType {
    /// Fill Rate Based Control — a store with a level and a rate.
    Frbc,
    /// Power Envelope Based Control — a bound is all that is needed.
    Pebc,
    /// Operation Mode Based Control — discrete states.
    Ombc,
    /// Power Profile Based Control — a fixed sequence started in a window.
    Ppbc,
    /// Demand Driven Based Control — actuators serving a reported demand.
    Ddbc,
    /// The device takes no instruction at all; it can only be measured.
    NotControllable,
}

impl From<ControlType> for S2ControlType {
    fn from(value: ControlType) -> Self {
        match value {
            ControlType::Frbc => S2ControlType::FillRateBasedControl,
            ControlType::Pebc => S2ControlType::PowerEnvelopeBasedControl,
            ControlType::Ombc => S2ControlType::OperationModeBasedControl,
            ControlType::Ppbc => S2ControlType::PowerProfileBasedControl,
            ControlType::Ddbc => S2ControlType::DemandDrivenBasedControl,
            ControlType::NotControllable => S2ControlType::NotControlable,
        }
    }
}

/// The control type that best describes `asset`.
///
/// `has_deadline` says whether a charge point currently has a car with a
/// departure time — the one case where the same hardware is described
/// differently, and the reason this takes an argument at all.
#[must_use]
pub fn control_type_for(asset: &Asset, has_deadline: bool) -> ControlType {
    match asset {
        // A store, whatever it stores. Fill level, rate, and a range to stay in.
        Asset::Battery(_) | Asset::Dhw(_) => ControlType::Frbc,

        // With a departure time the car is a store with a target; without one,
        // a bound is all the manager can usefully say.
        Asset::Evse(_) => {
            if has_deadline {
                ControlType::Frbc
            } else {
                ControlType::Pebc
            }
        }

        // Three contacts are three operation modes. A heat pump that takes a
        // power ceiling is an envelope; one that heats a buffer the manager can
        // see is really storage, but S2 wants the *store* described by whoever
        // owns it, and a heat pump rarely exposes its buffer.
        Asset::HeatPump(hp) => match hp.control {
            HeatPumpControl::SgReady | HeatPumpControl::OperationModes => ControlType::Ombc,
            HeatPumpControl::PowerCeiling => ControlType::Pebc,
        },

        // Curtailment is a ceiling and nothing else.
        Asset::Pv(_) => ControlType::Pebc,

        Asset::Load(load) => match load.kind {
            // A washing machine runs a programme; it can be started later but
            // not turned down, which is precisely PPBC.
            LoadKind::Shiftable => ControlType::Ppbc,
            LoadKind::Interruptible => ControlType::Ombc,
            LoadKind::Fixed => ControlType::NotControllable,
        },

        Asset::Relay(_) => ControlType::Ombc,
        Asset::Meter(_) => ControlType::NotControllable,
    }
}

/// The energy roles an asset plays, as S2 reports them.
///
/// A battery is both a consumer and a producer *and* a store — S2 lets a
/// Resource Manager declare several, and a manager that assumes one will plan a
/// battery as a load.
#[must_use]
pub fn roles_for(asset: &Asset) -> Vec<RoleType> {
    match asset {
        Asset::Battery(_) => {
            vec![
                RoleType::EnergyStorage,
                RoleType::EnergyConsumer,
                RoleType::EnergyProducer,
            ]
        }
        Asset::Evse(evse) => {
            if evse.bidirectional {
                vec![
                    RoleType::EnergyStorage,
                    RoleType::EnergyConsumer,
                    RoleType::EnergyProducer,
                ]
            } else {
                vec![RoleType::EnergyConsumer]
            }
        }
        Asset::Pv(_) => vec![RoleType::EnergyProducer],
        Asset::Dhw(_) => vec![RoleType::EnergyStorage, RoleType::EnergyConsumer],
        Asset::HeatPump(_) | Asset::Load(_) | Asset::Relay(_) => vec![RoleType::EnergyConsumer],
        Asset::Meter(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{AssetMeta, Battery, Chemistry, Evse, FlexibleLoad, HeatPump, PvArray};

    fn meta(id: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            CircuitId::new("main").unwrap(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
    }

    fn evse(bidirectional: bool) -> Asset {
        Asset::Evse(Evse {
            meta: meta("wallbox", 11.0),
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional,
            public: false,
        })
    }

    fn battery() -> Asset {
        Asset::Battery(Battery {
            meta: meta("battery", 5.0),
            capacity: Energy::from_kwh(10.0),
            max_charge: Power::from_kw(5.0),
            max_discharge: Power::from_kw(5.0),
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.05).unwrap(),
            soc_max: Soc::FULL,
            reserve_soc: Soc::EMPTY,
            chemistry: Chemistry::Lfp,
            grid_charging_allowed: true,
        })
    }

    #[test]
    fn a_battery_is_a_store() {
        assert_eq!(control_type_for(&battery(), false), ControlType::Frbc);
    }

    #[test]
    fn a_charge_point_changes_control_type_when_the_car_has_a_deadline() {
        // The point of the whole mapping: the same hardware, described
        // differently because what the manager needs to say has changed.
        assert_eq!(control_type_for(&evse(false), false), ControlType::Pebc);
        assert_eq!(control_type_for(&evse(false), true), ControlType::Frbc);
    }

    #[test]
    fn an_sg_ready_heat_pump_is_modes_and_a_modern_one_is_an_envelope() {
        let hp = |control| {
            Asset::HeatPump(HeatPump {
                meta: meta("wp", 9.0),
                electrical_nominal: Power::from_kw(5.0),
                heating_rod: None,
                control,
                modulating: true,
            })
        };
        assert_eq!(
            control_type_for(&hp(HeatPumpControl::SgReady), false),
            ControlType::Ombc
        );
        assert_eq!(
            control_type_for(&hp(HeatPumpControl::PowerCeiling), false),
            ControlType::Pebc
        );
    }

    #[test]
    fn a_washing_machine_is_a_profile_and_a_pool_pump_is_modes() {
        let load = |kind| {
            Asset::Load(FlexibleLoad {
                meta: meta("geraet", 2.0),
                nominal: Power::from_kw(2.0),
                kind,
            })
        };
        assert_eq!(
            control_type_for(&load(LoadKind::Shiftable), false),
            ControlType::Ppbc
        );
        assert_eq!(
            control_type_for(&load(LoadKind::Interruptible), false),
            ControlType::Ombc
        );
        assert_eq!(
            control_type_for(&load(LoadKind::Fixed), false),
            ControlType::NotControllable
        );
    }

    #[test]
    fn an_inverter_is_only_ever_an_envelope() {
        let pv = Asset::Pv(PvArray {
            meta: meta("pv", 9.8),
            kwp_dc: Power::from_kw(9.8),
            ac_nominal: Power::from_kw(8.0),
            tilt_deg: 35.0,
            azimuth_deg: 180.0,
            cap_relief: CapRelief::None,
        });
        assert_eq!(control_type_for(&pv, false), ControlType::Pebc);
        assert_eq!(roles_for(&pv), vec![RoleType::EnergyProducer]);
    }

    #[test]
    fn a_battery_declares_three_roles_not_one() {
        // A manager that assumes a single role plans a battery as a load.
        let roles = roles_for(&battery());
        assert!(roles.contains(&RoleType::EnergyStorage));
        assert!(roles.contains(&RoleType::EnergyProducer));
        assert!(roles.contains(&RoleType::EnergyConsumer));
    }

    #[test]
    fn a_bidirectional_charge_point_can_produce_and_an_ordinary_one_cannot() {
        assert!(roles_for(&evse(true)).contains(&RoleType::EnergyProducer));
        assert_eq!(roles_for(&evse(false)), vec![RoleType::EnergyConsumer]);
    }

    #[test]
    fn every_control_type_maps_onto_the_standards_own_enum() {
        for ct in [
            ControlType::Frbc,
            ControlType::Pebc,
            ControlType::Ombc,
            ControlType::Ppbc,
            ControlType::Ddbc,
            ControlType::NotControllable,
        ] {
            let _: S2ControlType = ct.into();
        }
    }
}
