//! What a driver reports, and how fresh it is.
//!
//! Provenance uses [`metering::QualityFlag`] — the settlement vocabulary — so a
//! value that later becomes a billing quantity does not change its meaning on
//! the way. Freshness is *not* stored: it is derived at the moment of use from
//! the timestamp and the age the consumer is willing to accept, because the same
//! reading is fresh for a 15-minute planner and stale for a 1-second arbiter.

use metering::QualityFlag;
use time::{Duration, OffsetDateTime};

use crate::units::{Current, Energy, PerPhase, Power, Soc, Voltage};

/// Whether a reading may still be acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Freshness {
    /// Young enough for the consumer that asked.
    Fresh,
    /// Older than the consumer's tolerance. The arbiter treats a stale reading
    /// as absent and falls back to the conservative assumption for that asset,
    /// which is never "assume it draws nothing".
    Stale,
}

impl Freshness {
    /// `true` when the reading may be used.
    #[must_use]
    pub fn is_fresh(self) -> bool {
        matches!(self, Freshness::Fresh)
    }
}

/// One observation of one asset.
///
/// Every field is optional because devices differ: a cheap inverter reports
/// active power only, an SMGW reports registers and per-phase values, a battery
/// adds state of charge and temperature. Consumers ask for what they need and
/// handle its absence explicitly.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Measurement {
    /// When the value was observed at the device, not when it was received.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
    /// Provenance, in the settlement vocabulary.
    pub quality: QualityFlag,
    /// Total active power, load convention.
    pub power: Option<Power>,
    /// Active power per outer conductor, load convention.
    pub power_per_phase: Option<PerPhase<Power>>,
    /// Current per outer conductor.
    pub current_per_phase: Option<PerPhase<Current>>,
    /// Voltage per outer conductor.
    pub voltage_per_phase: Option<PerPhase<Voltage>>,
    /// Cumulative energy register, direction "into the asset".
    pub energy_in: Option<Energy>,
    /// Cumulative energy register, direction "out of the asset".
    pub energy_out: Option<Energy>,
    /// State of charge, for anything that stores energy.
    pub soc: Option<Soc>,
    /// Temperature in degrees Celsius — battery cells, DHW tank, flow.
    pub temperature_c: Option<f64>,
    /// Grid frequency in hertz, where the device measures it.
    pub frequency_hz: Option<f64>,
    /// What a generator *could* produce right now if nothing limited it, as a
    /// non-negative magnitude.
    ///
    /// The one quantity a curtailed inverter cannot be asked for indirectly.
    /// Read [`Measurement::power`] instead and a controller learns only what it
    /// already commanded: curtail a roof to 5 kW and it reports 5 kW, so the
    /// next tick asks for 5 kW and the curtailment never lifts. Real hardware
    /// publishes this (SunSpec model 701's available power, EEBUS `MOI`), and
    /// where it does not the controller has to fall back on the inverter's
    /// nameplate — optimistic, and self-correcting on the next tick, which is
    /// the right way round for a quantity that only ever *relaxes* a bound.
    #[cfg_attr(feature = "serde", serde(default))]
    pub available_power: Option<Power>,
}

impl Measurement {
    /// An empty observation at `at`, to be filled in by a driver.
    #[must_use]
    pub const fn at(at: OffsetDateTime) -> Self {
        Self {
            at,
            quality: QualityFlag::Measured,
            power: None,
            power_per_phase: None,
            current_per_phase: None,
            voltage_per_phase: None,
            energy_in: None,
            energy_out: None,
            soc: None,
            temperature_c: None,
            frequency_hz: None,
            available_power: None,
        }
    }

    /// A plain active-power observation.
    #[must_use]
    pub fn power(at: OffsetDateTime, power: Power) -> Self {
        Self {
            power: Some(power),
            ..Self::at(at)
        }
    }

    /// A per-phase observation; the total is filled in from the sum.
    #[must_use]
    pub fn per_phase(at: OffsetDateTime, power: PerPhase<Power>) -> Self {
        Self {
            power: Some(power.total()),
            power_per_phase: Some(power),
            ..Self::at(at)
        }
    }

    /// Mark this observation as substituted rather than measured.
    #[must_use]
    pub fn substituted(mut self) -> Self {
        self.quality = QualityFlag::Substituted;
        self
    }

    /// How old the observation is at `now`. Negative ages (a device clock
    /// running ahead) are reported as zero.
    #[must_use]
    pub fn age(&self, now: OffsetDateTime) -> Duration {
        let age = now - self.at;
        if age.is_negative() {
            Duration::ZERO
        } else {
            age
        }
    }

    /// Whether this observation may still be acted on at `now`.
    ///
    /// A [`QualityFlag::Faulty`] reading is stale whatever its age.
    #[must_use]
    pub fn freshness(&self, now: OffsetDateTime, max_age: Duration) -> Freshness {
        if self.quality == QualityFlag::Faulty || self.age(now) > max_age {
            Freshness::Stale
        } else {
            Freshness::Fresh
        }
    }

    /// The active power if it is fresh enough at `now`, else `None`.
    #[must_use]
    pub fn fresh_power(&self, now: OffsetDateTime, max_age: Duration) -> Option<Power> {
        self.freshness(now, max_age)
            .is_fresh()
            .then_some(self.power)
            .flatten()
    }

    /// Per-phase power, derived from the total and a phase connection when the
    /// device does not report it.
    #[must_use]
    pub fn power_per_phase_or_split(
        &self,
        connection: crate::units::PhaseConnection,
        mode: crate::units::PhaseMode,
    ) -> Option<PerPhase<Power>> {
        self.power_per_phase
            .or_else(|| self.power.map(|p| connection.distribute(p, mode)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-05-01 12:00:00 UTC);

    #[test]
    fn freshness_follows_the_consumers_tolerance() {
        let m = Measurement::power(NOW - Duration::seconds(5), Power::from_kw(1.0));
        assert_eq!(m.freshness(NOW, Duration::seconds(10)), Freshness::Fresh);
        assert_eq!(m.freshness(NOW, Duration::seconds(2)), Freshness::Stale);
    }

    #[test]
    fn a_faulty_reading_is_stale_however_new_it_is() {
        let mut m = Measurement::power(NOW, Power::from_kw(1.0));
        m.quality = QualityFlag::Faulty;
        assert_eq!(m.freshness(NOW, Duration::minutes(5)), Freshness::Stale);
        assert_eq!(m.fresh_power(NOW, Duration::minutes(5)), None);
    }

    #[test]
    fn a_device_clock_running_ahead_does_not_produce_a_negative_age() {
        let m = Measurement::power(NOW + Duration::seconds(30), Power::ZERO);
        assert_eq!(m.age(NOW), Duration::ZERO);
    }

    #[test]
    fn per_phase_fills_in_the_total() {
        let m = Measurement::per_phase(
            NOW,
            PerPhase {
                l1: Power::from_kw(1.0),
                l2: Power::from_kw(2.0),
                l3: Power::ZERO,
            },
        );
        assert_eq!(m.power, Some(Power::from_kw(3.0)));
    }
}
