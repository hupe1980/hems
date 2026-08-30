//! Turning a decision into something the device will accept.
//!
//! The planner and the arbiter think in watts, because watts are what the
//! physics and the regulation are written in. Almost no device does.
//!
//! * A charge point takes **amperes per conductor**, and refuses anything below
//!   the 6 A of IEC 61851 by simply not charging.
//! * An SG Ready heat pump takes **one of three contact states**.
//! * A photovoltaic inverter takes a **ceiling**, not a target.
//! * A battery takes a signed **power**.
//!
//! Emitting active power to all of them, which is the obvious first
//! implementation, leaves most of a household undriveable. This module is the
//! translation, and it is a pure function of the asset and the decision, so it
//! is testable without a device on the desk.

use hems_core::prelude::*;

use crate::sg_ready::{SgReadyState, state_for};

/// What the arbiter decided, and the circumstances a device needs to know about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// The active power the arbiter settled on, load convention.
    pub power: Power,
    /// Whether a grid or safety rule is what is holding that value.
    ///
    /// It changes the translation: an SG Ready heat pump held down by the
    /// network operator belongs in state 1 even when the arithmetic would have
    /// put it in state 2, because state 1 is the state the operator's signal
    /// means.
    pub guard_limited: bool,
    /// Which conductors the asset should be using.
    ///
    /// For a switchable charge point this is the mode the arbiter has decided
    /// on, which may differ from the one the contactor is in — the command that
    /// closes the gap is the first one emitted.
    pub mode: PhaseMode,
}

impl Decision {
    /// A decision with no guard rule behind it.
    #[must_use]
    pub const fn new(power: Power) -> Self {
        Self {
            power,
            guard_limited: false,
            mode: PhaseMode::Three,
        }
    }

    /// Say whether a guard rule is what produced the value.
    #[must_use]
    pub const fn guard_limited(mut self, limited: bool) -> Self {
        self.guard_limited = limited;
        self
    }

    /// Say which conductors the asset should be using.
    #[must_use]
    pub const fn in_phase_mode(mut self, mode: PhaseMode) -> Self {
        self.mode = mode;
        self
    }
}

/// The power `asset` will actually take if it is asked for `wanted`.
///
/// Some devices are **semi-continuous**: a charge point is off, or somewhere
/// between the 6 A of IEC 61851 and its maximum, with nothing in between. Asking
/// one for 3,7 kW on three conductors is asking for 5,3 A, and it answers by
/// charging nothing at all.
///
/// Every layer above has to know that, and until this existed none of them did:
/// the arbiter commanded the value, the energy tracker counted it as delivered,
/// the plan fell behind by exactly that much and compensated in the next slot,
/// and the only place the truth appeared was the meter.
///
/// A request below the minimum therefore resolves to **zero**, not up to the
/// minimum. Up would be the tempting choice — a semi-continuous device can
/// deliver a fractional average by running at its minimum for part of a slot —
/// and it is wrong twice over. The planner's own semi-continuous constraint
/// already guarantees it never *asks* for a fraction, so the only requests below
/// the minimum are the two where more power is the wrong answer: the tail of a
/// slot whose energy has already been delivered, and a guard that has cut the
/// device below what it can start on. Rounding either of them up buys energy
/// nobody wanted. On the reference winter day it bought 2,2 kWh of it, at the
/// evening price.
#[must_use]
pub fn realisable(asset: &Asset, wanted: Power, mode: PhaseMode) -> Power {
    match asset {
        Asset::Evse(evse) => {
            let held = evse.meta.phases.clamp_mode(mode);
            if wanted < evse.min_power(held) {
                Power::ZERO
            } else {
                wanted.min(evse.max_power(held))
            }
        }
        // Everything else in the model takes a continuous value, or is mapped to
        // a discrete state by `commands_for` in a way that loses nothing.
        _ => wanted,
    }
}

/// The commands to send to `asset` for `decision`.
///
/// Usually one. A charge point that has to change its phase count gets two, in
/// the order the hardware needs them: the phase count first, because changing it
/// while current is flowing is what damages contactors.
#[must_use]
pub fn commands_for(asset: &Asset, decision: Decision) -> Vec<Command> {
    match asset {
        Asset::Evse(evse) => evse_commands(evse, decision),
        Asset::HeatPump(hp) => heat_pump_commands(hp, decision),
        Asset::Pv(_) => {
            // An inverter is told what it may feed in, never what to produce.
            vec![Command::ProductionCeiling(decision.power.outflow())]
        }
        Asset::Battery(_) => vec![Command::ActivePower(decision.power)],
        Asset::Relay(_) => vec![Command::OnOff(decision.power > Power::ZERO)],
        Asset::Dhw(tank) => {
            // A hot-water tank is a resistive heater: on, or off.
            vec![Command::OnOff(decision.power >= tank.heater * 0.5)]
        }
        Asset::Load(_) | Asset::Meter(_) => {
            if asset
                .capabilities()
                .contains(Capabilities::LIMIT_CONSUMPTION)
            {
                vec![Command::ConsumptionCeiling(decision.power.inflow())]
            } else {
                Vec::new()
            }
        }
    }
}

fn evse_commands(evse: &Evse, decision: Decision) -> Vec<Command> {
    let mode = evse.meta.phases.clamp_mode(decision.mode);
    let phases = evse.phase_count(mode);
    let current =
        Current::new(decision.power.inflow().get() / (f64::from(phases) * NOMINAL_VOLTAGE.get()));

    // Below the standard's minimum a charge point is not charging slowly, it is
    // idle — so ask for nothing rather than for something it will ignore.
    let commanded = if current < evse.min_current {
        Current::ZERO
    } else {
        current.min(evse.max_current)
    };

    // The phase count goes first. Changing it while current is flowing is what
    // welds contactors, so a driver that applies these in order gets the pause
    // the hardware needs for free.
    let mut out = Vec::with_capacity(2);
    if evse.meta.phases.is_switchable() {
        out.push(Command::PhaseCount(phases));
    }
    out.push(Command::ChargingCurrent(commanded));
    out
}

fn heat_pump_commands(hp: &HeatPump, decision: Decision) -> Vec<Command> {
    match hp.control {
        HeatPumpControl::SgReady => {
            let state = if decision.guard_limited {
                // The network operator's signal *is* state 1. Translating it
                // into anything else loses the only thing the contact means.
                SgReadyState::Limited
            } else {
                state_for(
                    decision.power,
                    hp.electrical_nominal,
                    hp.meta.connection_power,
                )
            };
            vec![Command::OperationMode(state.number())]
        }
        HeatPumpControl::PowerCeiling => {
            vec![Command::ConsumptionCeiling(decision.power.inflow())]
        }
        HeatPumpControl::OperationModes => {
            // A digital interface with named modes, ordered by how much the unit
            // is being asked to do. Without a device profile this is the same
            // three-way split SG Ready makes, which is the safe default.
            let state = state_for(
                decision.power,
                hp.electrical_nominal,
                hp.meta.connection_power,
            );
            vec![Command::OperationMode(state.number())]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{AssetMeta, Battery, Chemistry, DhwTank, PvArray, Relay};

    fn meta(id: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            CircuitId::new("main").unwrap(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
        .with_capabilities(Capabilities::MEASURE | Capabilities::SET_POWER)
    }

    fn evse(switchable: bool) -> Asset {
        let mut m = meta("wallbox", 11.0);
        if switchable {
            m.phases = PhaseConnection::Switchable { phase: Phase::L1 };
        }
        Asset::Evse(Evse {
            meta: m,
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            bidirectional: false,
            public: false,
        })
    }

    fn heat_pump(control: HeatPumpControl) -> Asset {
        Asset::HeatPump(HeatPump {
            meta: meta("wp", 9.0),
            electrical_nominal: Power::from_kw(5.0),
            heating_rod: None,
            control,
            modulating: true,
        })
    }

    #[test]
    fn a_charge_point_is_told_amperes_not_watts() {
        // 6,9 kW on three phases is 10 A per conductor.
        let cmds = commands_for(&evse(false), Decision::new(Power::from_kw(6.9)));
        assert_eq!(cmds.len(), 1);
        match cmds[0] {
            Command::ChargingCurrent(c) => assert!((c.get() - 10.0).abs() < 1e-9, "{c}"),
            other => panic!("expected a current, got {other}"),
        }
    }

    #[test]
    fn a_charge_point_below_its_minimum_is_told_zero_not_a_trickle() {
        // 2 kW on three phases is under 3 A, and a charge point will simply not
        // start. Asking for it anyway leaves the arbiter believing power is
        // flowing that is not.
        let cmds = commands_for(&evse(false), Decision::new(Power::from_kw(2.0)));
        assert_eq!(cmds[0], Command::ChargingCurrent(Current::ZERO));
    }

    #[test]
    fn a_charge_point_is_never_told_more_than_its_rating() {
        let cmds = commands_for(&evse(false), Decision::new(Power::from_kw(30.0)));
        assert_eq!(cmds[0], Command::ChargingCurrent(Current::new(16.0)));
    }

    #[test]
    fn a_switchable_charge_point_is_told_the_phase_count_first() {
        // Changing the contactor while current flows is what destroys it.
        let cmds = commands_for(
            &evse(true),
            Decision::new(Power::from_kw(3.6)).in_phase_mode(PhaseMode::Single),
        );
        assert_eq!(cmds[0], Command::PhaseCount(1));
        match cmds[1] {
            // 3,6 kW on one conductor is about 15,6 A.
            Command::ChargingCurrent(c) => assert!(c.get() > 15.0 && c.get() <= 16.0, "{c}"),
            other => panic!("expected a current, got {other}"),
        }
    }

    #[test]
    fn an_sg_ready_heat_pump_is_told_a_state() {
        let hp = heat_pump(HeatPumpControl::SgReady);
        assert_eq!(
            commands_for(&hp, Decision::new(Power::ZERO)),
            vec![Command::OperationMode(1)]
        );
        assert_eq!(
            commands_for(&hp, Decision::new(Power::from_kw(3.0))),
            vec![Command::OperationMode(2)]
        );
        assert_eq!(
            commands_for(&hp, Decision::new(Power::from_kw(5.0))),
            vec![Command::OperationMode(3)]
        );
    }

    #[test]
    fn a_grid_limited_heat_pump_goes_to_state_one_whatever_the_arithmetic_says() {
        let hp = heat_pump(HeatPumpControl::SgReady);
        // 3 kW would ordinarily be state 2 — but the value is there because the
        // network operator put it there, and state 1 is what that means.
        let cmds = commands_for(&hp, Decision::new(Power::from_kw(3.0)).guard_limited(true));
        assert_eq!(cmds, vec![Command::OperationMode(1)]);
    }

    #[test]
    fn a_modern_heat_pump_gets_a_ceiling_in_watts() {
        let hp = heat_pump(HeatPumpControl::PowerCeiling);
        assert_eq!(
            commands_for(&hp, Decision::new(Power::from_kw(3.0))),
            vec![Command::ConsumptionCeiling(Power::from_kw(3.0))]
        );
    }

    #[test]
    fn an_inverter_is_told_a_ceiling_as_a_magnitude() {
        let pv = Asset::Pv(PvArray {
            meta: meta("pv", 9.8),
            kwp_dc: Power::from_kw(9.8),
            ac_nominal: Power::from_kw(8.0),
            tilt_deg: 35.0,
            azimuth_deg: 180.0,
            cap_relief: CapRelief::None,
        });
        // Production is negative in the load convention; a ceiling is positive.
        assert_eq!(
            commands_for(&pv, Decision::new(Power::from_kw(-5.88))),
            vec![Command::ProductionCeiling(Power::from_kw(5.88))]
        );
    }

    #[test]
    fn a_battery_keeps_its_sign() {
        let battery = Asset::Battery(Battery {
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
        });
        assert_eq!(
            commands_for(&battery, Decision::new(Power::from_kw(-3.0))),
            vec![Command::ActivePower(Power::from_kw(-3.0))]
        );
    }

    #[test]
    fn a_relay_and_a_tank_are_switched_not_modulated() {
        let relay = Asset::Relay(Relay {
            meta: meta("heizstab", 3.0),
            purpose: "Heizstab".into(),
        });
        assert_eq!(
            commands_for(&relay, Decision::new(Power::from_kw(3.0))),
            vec![Command::OnOff(true)]
        );
        assert_eq!(
            commands_for(&relay, Decision::new(Power::ZERO)),
            vec![Command::OnOff(false)]
        );

        let tank = Asset::Dhw(DhwTank {
            cop: 3.0,
            standing_loss: Power::new(45.0),
            meta: meta("brauchwasser", 3.0),
            volume_l: 300.0,
            heater: Power::from_kw(3.0),
            t_min_c: 45.0,
            t_set_c: 55.0,
            t_max_c: 65.0,
        });
        assert_eq!(
            commands_for(&tank, Decision::new(Power::from_kw(2.0))),
            vec![Command::OnOff(true)]
        );
    }

    #[test]
    fn an_uncontrollable_load_gets_no_command_at_all() {
        let load = Asset::Load(hems_core::asset::FlexibleLoad {
            meta: AssetMeta::new(
                AssetId::new("haushalt").unwrap(),
                CircuitId::new("main").unwrap(),
                PhaseConnection::Three,
                Power::from_kw(3.0),
            ),
            nominal: Power::from_kw(0.5),
            kind: hems_core::asset::LoadKind::Fixed,
        });
        assert!(commands_for(&load, Decision::new(Power::from_kw(1.0))).is_empty());
    }

    #[test]
    fn every_command_produced_is_finite() {
        // The gate in `Setpoint::new` should never have to fire on our own
        // output.
        for asset in [evse(true), heat_pump(HeatPumpControl::SgReady)] {
            for kw in [-100.0, -1.0, 0.0, 0.5, 11.0, 1e6] {
                for cmd in commands_for(&asset, Decision::new(Power::from_kw(kw))) {
                    assert!(cmd.is_finite(), "{asset:?} at {kw} kW produced {cmd}");
                }
            }
        }
    }
}
