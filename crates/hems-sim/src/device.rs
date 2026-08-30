//! Physical models that answer a setpoint the way real hardware does.
//!
//! Real hardware is slower than a controller thinks, refuses values it cannot
//! reach, and reports what it actually did rather than what it was told. A
//! simulator that ignores that flatters the controller and hides exactly the
//! bugs worth finding, so every model here has at least one way of saying no.

use hems_core::prelude::{CopCurve, Current, Energy, PhaseMode, Power, Rc2, Soc, ThermalState};
use time::Duration;

/// A stationary battery with efficiency, limits and a state of charge.
#[derive(Debug, Clone, PartialEq)]
pub struct BatterySim {
    /// Usable capacity.
    pub capacity: Energy,
    /// Energy currently stored.
    pub stored: Energy,
    /// Maximum charging power.
    pub max_charge: Power,
    /// Maximum discharging power as a positive magnitude.
    pub max_discharge: Power,
    /// One-way charging efficiency.
    pub efficiency_charge: f64,
    /// One-way discharging efficiency.
    pub efficiency_discharge: f64,
    /// Lowest usable state of charge.
    pub soc_min: Soc,
    /// Highest usable state of charge.
    pub soc_max: Soc,
    /// Standing loss, watts. Small and always there — the reason a battery left
    /// alone is not full a week later.
    pub standby_loss: Power,
}

impl BatterySim {
    /// A battery of `capacity`, half full.
    #[must_use]
    pub fn new(capacity: Energy, max_power: Power) -> Self {
        Self {
            capacity,
            stored: capacity * 0.5,
            max_charge: max_power,
            max_discharge: max_power,
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.05).unwrap_or(Soc::EMPTY),
            soc_max: Soc::FULL,
            standby_loss: Power::new(15.0),
        }
    }

    /// The current state of charge.
    #[must_use]
    pub fn soc(&self) -> Soc {
        Soc::clamped(self.stored / self.capacity)
    }

    /// Apply `commanded` for `dt` and report the power actually taken.
    ///
    /// The value returned is what a meter would see: bounded by the hardware,
    /// by the state of charge, and reduced by the standing loss.
    pub fn step(&mut self, commanded: Power, dt: Duration) -> Power {
        let hours = dt.as_seconds_f64() / 3600.0;
        let ceiling = self.capacity * self.soc_max.fraction();
        let floor = self.capacity * self.soc_min.fraction();

        let wanted = commanded.clamp(-self.max_discharge, self.max_charge);
        let actual = if wanted > Power::ZERO {
            // Charging: bounded by the room left, after losses.
            let room = (ceiling - self.stored).max(Energy::ZERO);
            let max_by_room = Power::new(room.get() / hours.max(1e-9) / self.efficiency_charge);
            let p = wanted.min(max_by_room);
            self.stored += Energy::new(p.get() * hours * self.efficiency_charge);
            p
        } else if wanted < Power::ZERO {
            // Discharging: bounded by what is stored, before losses.
            let available = (self.stored - floor).max(Energy::ZERO);
            let max_by_charge =
                Power::new(available.get() / hours.max(1e-9) * self.efficiency_discharge);
            let p = wanted.abs().min(max_by_charge);
            self.stored -= Energy::new(p.get() * hours / self.efficiency_discharge);
            -p
        } else {
            Power::ZERO
        };

        self.stored = (self.stored - Energy::new(self.standby_loss.get() * hours))
            .max(Energy::ZERO)
            .min(self.capacity);
        actual
    }
}

/// A hot-water tank that answers a heater command and loses heat while it waits.
///
/// Modelled the way [`hems_optimizer`](https://docs.rs/hems-optimizer) plans it
/// and the way S2 describes it — a fill level in kilowatt-hours of heat above
/// the lowest acceptable temperature — so the plan and the tank cannot disagree
/// about physics. Two ways of saying no: it stops at full, and it cannot deliver
/// water it does not have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TankSim {
    /// Heat the tank holds between its lowest acceptable and its highest safe
    /// temperature.
    pub capacity: Energy,
    /// Heat stored now, above the lowest acceptable temperature.
    pub stored: Energy,
    /// Electrical power of the heater.
    pub heater: Power,
    /// Thermal kilowatt-hours per electrical kilowatt-hour.
    pub cop: f64,
    /// Standing loss.
    pub standing_loss: Power,
}

impl TankSim {
    /// A tank of `capacity` on a heater of `heater`, half charged.
    #[must_use]
    pub fn new(capacity: Energy, heater: Power) -> Self {
        Self {
            capacity,
            stored: capacity * 0.5,
            heater,
            cop: 3.0,
            standing_loss: Power::new(45.0),
        }
    }

    /// The fraction of its usable heat the tank is holding.
    #[must_use]
    pub fn fill(&self) -> f64 {
        if self.capacity > Energy::ZERO {
            (self.stored / self.capacity).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Apply `commanded` for `dt` while the household draws `draw` of heat.
    ///
    /// Returns the electrical power a meter would see and the heat the household
    /// asked for and did not get — a cold shower, which is a real outcome and
    /// has to be counted rather than clamped away.
    pub fn step(&mut self, commanded: Power, draw: Energy, dt: Duration) -> (Power, Energy) {
        let hours = dt.as_seconds_f64() / 3600.0;
        // The draw and the standing loss happen whatever the controller does.
        let wanted = (draw + Energy::new(self.standing_loss.get() * hours)).max(Energy::ZERO);
        let taken = wanted.min(self.stored);
        self.stored -= taken;
        let short = (wanted - taken).max(Energy::ZERO);

        let room = (self.capacity - self.stored).max(Energy::ZERO);
        let by_room = if hours > 0.0 && self.cop > 0.0 {
            Power::new(room.get() / hours / self.cop)
        } else {
            Power::ZERO
        };
        let actual = commanded.clamp(Power::ZERO, self.heater).min(by_room);
        self.stored = (self.stored + Energy::new(actual.get() * self.cop * hours))
            .min(self.capacity)
            .max(Energy::ZERO);
        (actual, short)
    }
}

/// An inverter that answers a curtailment command.
///
/// Small, and it closes the loop on every feed-in limit: the guard, the planner
/// and the § 9 EEG module can all decide that a roof must be curtailed, and
/// without something on the other end of that decision none of them is testable
/// end to end and the day's own `curtailed` figure is structurally zero.
///
/// A real inverter follows a new ceiling in a second or two rather than
/// instantly (the DC/DC stage has to walk off the maximum power point), so this
/// one does too: `response` is the fraction of the remaining gap it closes per
/// step. It is the smallest way of saying no that keeps a controller honest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PvSim {
    /// What the inverter is feeding in now, as a non-negative magnitude.
    pub producing: Power,
    /// The fraction of the gap to a new ceiling closed in one step.
    ///
    /// `1.0` follows instantly. The default of 0,4 is about two seconds of
    /// settling at a one-second tick, which is what a string inverter does.
    pub response: f64,
}

impl Default for PvSim {
    fn default() -> Self {
        Self {
            producing: Power::ZERO,
            response: 0.4,
        }
    }
}

impl PvSim {
    /// Apply a production `ceiling` against what the weather is offering.
    ///
    /// Both arguments are non-negative magnitudes. Returns the power the meter
    /// would see, in the **load convention** — so a negative number — and the
    /// production the ceiling **refused**.
    ///
    /// The second value is deliberately the ceiling's own doing and not the
    /// difference between the weather and the meter. The two differ by the ramp,
    /// and reporting the ramp as curtailment would make a controller that never
    /// curtails anything print a curtailment figure every sunrise — a KPI that
    /// moves when nothing was decided is worse than one that is always zero.
    pub fn step(&mut self, available: Power, ceiling: Power) -> (Power, Power) {
        let available = available.max(Power::ZERO);
        let ceiling = ceiling.max(Power::ZERO);
        let target = available.min(ceiling);
        let gap = target - self.producing;
        self.producing = (self.producing + gap * self.response.clamp(0.0, 1.0)).max(Power::ZERO);
        // Never above what the weather is offering, whatever the ramp says: an
        // inverter cannot overshoot into sunshine that is not there.
        self.producing = self.producing.min(available);
        (-self.producing, (available - ceiling).max(Power::ZERO))
    }
}

/// A charge point with a car plugged into it, or not.
#[derive(Debug, Clone, PartialEq)]
pub struct EvseSim {
    /// Lowest current a session can run at — 6 A by IEC 61851.
    pub min_current: Current,
    /// Highest current the hardware allows.
    pub max_current: Current,
    /// Which conductors are in use right now.
    pub mode: PhaseMode,
    /// Whether the hardware can change that.
    pub switchable: bool,
    /// The car, when one is connected.
    pub vehicle: Option<VehicleSim>,
    /// Ticks left before a commanded phase change has taken effect.
    ///
    /// Real hardware opens a contactor and lets the vehicle re-negotiate, and
    /// IEC 61851 gives that seconds rather than milliseconds. A simulator that
    /// switched instantly would hide every controller bug that chatters.
    switch_delay_ticks: u8,
}

/// The car.
#[derive(Debug, Clone, PartialEq)]
pub struct VehicleSim {
    /// Usable battery capacity.
    pub capacity: Energy,
    /// Energy currently in the car.
    pub stored: Energy,
    /// Charging efficiency on three conductors.
    pub efficiency: f64,
    /// Charging efficiency on one.
    ///
    /// Lower, because the onboard charger's standing overhead is a constant few
    /// hundred watts: noise at 11 kW, a tenth of the throughput at 1,4. A
    /// simulator that used one figure for both would let a controller switch
    /// conductors for free and report a saving nobody would see.
    pub efficiency_single_phase: f64,
    /// The most the car itself will take.
    pub max_charge: Power,
}

impl EvseSim {
    /// An 11 kW three-phase charge point with nothing plugged in.
    #[must_use]
    pub fn new() -> Self {
        Self {
            min_current: Current::new(6.0),
            max_current: Current::new(16.0),
            mode: PhaseMode::Three,
            switchable: false,
            vehicle: None,
            switch_delay_ticks: 0,
        }
    }

    /// Plug a car in.
    #[must_use]
    pub fn with_vehicle(mut self, vehicle: VehicleSim) -> Self {
        self.vehicle = Some(vehicle);
        self
    }

    /// Give it a contactor that can drop two conductors.
    #[must_use]
    pub fn switchable(mut self) -> Self {
        self.switchable = true;
        self
    }

    /// Command a phase mode, the way a driver would.
    ///
    /// Returns `true` when the mode changed. The change costs a tick, because a
    /// real charge point opens its contactor and lets the vehicle re-negotiate:
    /// during that tick the session delivers nothing, which is the cost a
    /// controller has to be worth paying.
    pub fn set_mode(&mut self, mode: PhaseMode) -> bool {
        if !self.switchable || mode == self.mode {
            return false;
        }
        self.mode = mode;
        self.switch_delay_ticks = 1;
        true
    }

    /// The least power a session can draw, given the conductors in use.
    #[must_use]
    pub fn minimum_power(&self) -> Power {
        self.min_current
            .to_power_1p(hems_core::prelude::NOMINAL_VOLTAGE)
            * f64::from(self.mode.count())
    }

    /// The most it can draw.
    #[must_use]
    pub fn maximum_power(&self) -> Power {
        self.max_current
            .to_power_1p(hems_core::prelude::NOMINAL_VOLTAGE)
            * f64::from(self.mode.count())
    }

    /// Apply `commanded` for `dt` and report what was actually drawn.
    ///
    /// A charge point below its minimum current does not charge slowly — it does
    /// not charge. Controllers that forget this spend a whole afternoon
    /// commanding 1 kW into a car that never wakes up.
    pub fn step(&mut self, commanded: Power, dt: Duration) -> Power {
        if self.switch_delay_ticks > 0 {
            // The contactor is open and the vehicle is re-negotiating.
            self.switch_delay_ticks -= 1;
            return Power::ZERO;
        }
        let (minimum, maximum) = (self.minimum_power(), self.maximum_power());
        let mode = self.mode;
        let Some(vehicle) = self.vehicle.as_mut() else {
            return Power::ZERO;
        };
        let hours = dt.as_seconds_f64() / 3600.0;

        let wanted = commanded
            .max(Power::ZERO)
            .min(maximum)
            .min(vehicle.max_charge);
        if wanted < minimum {
            return Power::ZERO;
        }
        let efficiency = match mode {
            PhaseMode::Three => vehicle.efficiency,
            PhaseMode::Single => vehicle.efficiency_single_phase,
        };
        let room = (vehicle.capacity - vehicle.stored).max(Energy::ZERO);
        let max_by_room = Power::new(room.get() / hours.max(1e-9) / efficiency);
        let actual = wanted.min(max_by_room);
        vehicle.stored += Energy::new(actual.get() * hours * efficiency);
        // Below the minimum current the session simply stops.
        if actual < minimum {
            Power::ZERO
        } else {
            actual
        }
    }
}

impl Default for EvseSim {
    fn default() -> Self {
        Self::new()
    }
}

/// A heat pump heating a building modelled as two thermal masses.
///
/// The two-mass RC model — indoor air against the building fabric, both against
/// outdoors — is the smallest one that reproduces what matters for control: a
/// house does not cool down the moment the heating stops, and it cannot be
/// reheated instantly either. Everything a plan does with a heat pump depends on
/// that inertia.
///
/// The physics is [`hems_core::thermal::Rc2`], the same type the planner builds
/// its constraints from and the same exact discretisation. A simulator that
/// disagreed with the plan about the house would make every planning error
/// indistinguishable from a modelling one.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildingSim {
    /// Where the two masses are.
    pub state: ThermalState,
    /// The building.
    pub building: Rc2,
    /// Electrical power of the heat pump at full output.
    pub nominal_electrical: Power,
    /// How the coefficient of performance moves with the weather.
    pub cop: CopCurve,
    /// The indoor temperature the unit's own thermostat aims for, °C.
    pub thermostat_set_c: f64,
    /// The indoor temperature it will not heat past whatever it is told, °C.
    pub thermostat_max_c: f64,
    /// Whether the unit's own thermostat is currently calling for heat.
    pub calling: bool,
}

impl BuildingSim {
    /// A well-insulated single-family house with a 5 kW heat pump.
    #[must_use]
    pub fn new(indoor_c: f64) -> Self {
        Self {
            state: ThermalState::uniform(indoor_c),
            building: Rc2::house(),
            nominal_electrical: Power::from_kw(5.0),
            cop: CopCurve::air_source(),
            thermostat_set_c: 20.5,
            thermostat_max_c: 23.0,
            calling: false,
        }
    }

    /// Indoor air temperature, °C.
    #[must_use]
    pub fn indoor_c(&self) -> f64 {
        self.state.indoor_c
    }

    /// Building-fabric temperature, °C.
    #[must_use]
    pub fn mass_c(&self) -> f64 {
        self.state.mass_c
    }

    /// The coefficient of performance at an outdoor temperature.
    ///
    /// Falls as it gets colder, which is why a plan that pre-heats in the
    /// afternoon beats one that waits for the coldest hour of the night.
    #[must_use]
    pub fn cop(&self, outdoor_c: f64) -> f64 {
        self.cop.at(outdoor_c)
    }

    /// Run the building for `dt` with the heat pump given a ceiling of
    /// `electrical`.
    ///
    /// Returns the electrical power actually drawn, which is **not** always what
    /// was asked for. A heat pump is a device an energy manager can only
    /// *limit*: EEBUS LPC sends it a consumption ceiling and the unit's own
    /// controller decides what to do underneath it. So this one does too, and
    /// the boundary between the two is where a real ceiling stops being one:
    ///
    /// * a ceiling **at or above the nameplate** is not a ceiling. The unit runs
    ///   its own thermostat — on below [`BuildingSim::thermostat_set_c`], off
    ///   half a kelvin above it — which is what an absent instruction means for
    ///   a device like this. Reading it as "draw your full rating" cooks the
    ///   house to 64 °C on a June day.
    /// * a ceiling **below the nameplate** is the manager asking for something,
    ///   and the unit takes it — which is what lets a plan pre-heat into a cheap
    ///   hour rather than waiting for the thermostat to notice the cold.
    ///
    /// Above [`BuildingSim::thermostat_max_c`] nothing can make it hotter. That
    /// is a safety limit rather than a control one, and it is the third way this
    /// simulator says no.
    pub fn step(&mut self, electrical: Power, outdoor_c: f64, dt: Duration) -> Power {
        let unlimited = electrical >= self.nominal_electrical;
        // The unit's own hysteresis, kept across ticks so it does not chatter.
        if self.state.indoor_c < self.thermostat_set_c {
            self.calling = true;
        } else if self.state.indoor_c > self.thermostat_set_c + 0.5 {
            self.calling = false;
        }
        let asked = if unlimited {
            if self.calling {
                self.nominal_electrical
            } else {
                Power::ZERO
            }
        } else {
            electrical
        };
        let ceiling = if self.state.indoor_c >= self.thermostat_max_c {
            Power::ZERO
        } else {
            self.nominal_electrical
        };
        let drawn = asked.clamp(Power::ZERO, ceiling);
        let heat_kw = drawn.kw() * self.cop(outdoor_c);
        self.state = self.building.step(self.state, heat_kw, outdoor_c, dt);
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUARTER: Duration = Duration::minutes(15);

    #[test]
    fn a_battery_reports_what_it_took_not_what_it_was_told() {
        let mut b = BatterySim::new(Energy::from_kwh(10.0), Power::from_kw(5.0));
        // Commanded far beyond its rating.
        let actual = b.step(Power::from_kw(20.0), QUARTER);
        assert_eq!(actual, Power::from_kw(5.0));
    }

    #[test]
    fn a_full_battery_stops_taking_charge() {
        let mut b = BatterySim::new(Energy::from_kwh(10.0), Power::from_kw(5.0));
        b.stored = b.capacity;
        assert!(b.step(Power::from_kw(5.0), QUARTER) < Power::new(1.0));
    }

    #[test]
    fn an_empty_battery_stops_discharging_at_its_floor() {
        let mut b = BatterySim::new(Energy::from_kwh(10.0), Power::from_kw(5.0));
        b.stored = Energy::from_kwh(0.5);
        for _ in 0..20 {
            b.step(Power::from_kw(-5.0), QUARTER);
        }
        assert!(b.soc().fraction() <= 0.06, "stopped at {}", b.soc());
    }

    #[test]
    fn a_round_trip_loses_energy() {
        let mut b = BatterySim::new(Energy::from_kwh(10.0), Power::from_kw(5.0));
        let before = b.stored;
        b.step(Power::from_kw(4.0), QUARTER);
        b.step(Power::from_kw(-4.0), QUARTER);
        assert!(b.stored < before, "a lossless battery is not a battery");
    }

    #[test]
    fn a_charge_point_below_its_minimum_current_does_nothing_at_all() {
        let mut e = EvseSim::new().with_vehicle(VehicleSim {
            capacity: Energy::from_kwh(60.0),
            stored: Energy::from_kwh(20.0),
            efficiency: 0.92,
            efficiency_single_phase: 0.85,
            max_charge: Power::from_kw(11.0),
        });
        // 6 A on three phases is 4,14 kW; anything below is not slow charging.
        assert_eq!(e.step(Power::from_kw(2.0), QUARTER), Power::ZERO);
        assert!(e.step(Power::from_kw(5.0), QUARTER) > Power::ZERO);
    }

    #[test]
    fn an_unplugged_charge_point_draws_nothing_whatever_it_is_told() {
        let mut e = EvseSim::new();
        assert_eq!(e.step(Power::from_kw(11.0), QUARTER), Power::ZERO);
    }

    #[test]
    fn a_house_cools_down_slowly_when_the_heating_stops() {
        let mut b = BuildingSim::new(21.0);
        for _ in 0..4 {
            b.step(Power::ZERO, -5.0, QUARTER);
        }
        assert!(b.indoor_c() < 21.0, "it should be cooling");
        assert!(b.indoor_c() > 17.0, "but not that fast: {}", b.indoor_c());
    }

    #[test]
    fn heating_warms_the_house_and_then_the_fabric() {
        let mut b = BuildingSim::new(18.0);
        for _ in 0..8 {
            b.step(Power::from_kw(5.0), 0.0, QUARTER);
        }
        assert!(b.indoor_c() > 18.0);
        assert!(b.mass_c() > 18.0, "the fabric follows the air");
        assert!(b.mass_c() < b.indoor_c(), "but lags behind it");
    }

    #[test]
    fn the_coefficient_of_performance_falls_with_the_temperature() {
        let b = BuildingSim::new(21.0);
        assert!(
            b.cop(10.0) > b.cop(-10.0),
            "heat pumps are worse when it is cold"
        );
    }
}
