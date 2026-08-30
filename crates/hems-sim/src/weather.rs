//! The day that actually happened, as opposed to the day that was forecast.
//!
//! # Why this module exists
//!
//! Until it did, every simulated day in this workspace handed the planner the
//! *same* production and load curves the simulator was about to run. The
//! forecast could not be wrong. Every saving figure the project has ever quoted
//! was therefore a **perfect-foresight** number: an upper bound no controller in
//! the field can reach, produced by a test that could not distinguish a good
//! planner from a planner that had been shown the answer.
//!
//! It also meant that the one mechanism built specifically to absorb forecast
//! error — the arbiter tracking the plan's *energy* rather than its setpoint
//! (D19) — was never once exercised by a day. The quantity it corrects was
//! identically zero.
//!
//! So the simulator gets a **realisation** and the planner gets an
//! **expectation**, and they are not the same series. That is the whole idea:
//!
//! | Quantity | The planner is told | The house does |
//! |---|---|---|
//! | production | clear sky × (1 − mean cloud), with a band | clear sky × (1 − *this day's* cloud) |
//! | household load | the profile learned from previous days | the profile × this day's noise |
//! | outdoor temperature | the diurnal shape | the shape plus a slow error |
//! | hot water | the household's usual draw | this morning's actual draw |
//!
//! # Deterministic, and not merely repeatable
//!
//! Replaying a day and getting the same answer is a *property* of this project
//! (D23), so the realisation is a pure function of `(seed, instant)` — there is
//! no generator state, no iteration order to depend on, and no way for a
//! parallel test run to change it. `hemsd --day winter` on any machine, any
//! number of threads, produces the same cloud at 12:19.
//!
//! # The shape of the noise, and why not white
//!
//! White noise on a photovoltaic series is the wrong error entirely: it averages
//! out inside a quarter hour, so a planner working in quarter hours never sees
//! it, and the arbiter's energy tracking has nothing to catch up on. Real
//! forecast error is **correlated** — a cumulus field passes for twenty minutes,
//! a front is three hours late, a whole afternoon is hazier than the model said.
//!
//! So the process is value noise summed over four octaves (a crude fractional
//! Brownian motion) at about four hours, one hour, a quarter hour and four
//! minutes — a front, a haze, a cumulus field and one cloud. It is smooth,
//! bounded, and has energy at exactly the time scales a receding-horizon
//! controller has to correct for, including *below* the planner's own grain.

use hems_core::prelude::Slot;
use time::OffsetDateTime;

/// Golden-ratio odd constant, the usual mixing multiplier for a 64-bit hash.
const PHI64: u64 = 0x9E37_79B9_7F4A_7C15;

/// One deterministic weather and behaviour realisation.
///
/// Two realisations with different seeds are different days with the same
/// statistics — which is what a fleet backtest wants, and what a regression
/// suite must **not** do silently: the seed is part of the scenario, so a
/// changed number is a changed test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Realisation {
    /// Which day this is. Same seed, same weather, for ever.
    pub seed: u64,
}

/// The streams, so two quantities driven by the same seed are independent.
const STREAM_CLOUD: u64 = 1;
const STREAM_LOAD: u64 = 2;
const STREAM_OUTDOOR: u64 = 3;
const STREAM_DRAW: u64 = 4;

/// The `SplitMix64` finaliser — a good avalanche in five operations, and no
/// state at all, which is what makes the realisation a pure function.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(PHI64);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A hashed value in `[-1, 1]`.
fn hashed(seed: u64, stream: u64, index: i64) -> f64 {
    #[allow(clippy::cast_sign_loss)]
    let h = mix(seed ^ mix(stream.wrapping_mul(PHI64) ^ (index as u64)));
    // 53 bits into [0, 1), then centred.
    #[allow(clippy::cast_precision_loss)]
    let unit = (h >> 11) as f64 / (1u64 << 53) as f64;
    unit.mul_add(2.0, -1.0)
}

/// Smoothstep, so the interpolation between lattice points has a continuous
/// first derivative — a cloud edge is soft, and a kink would show up as a step
/// in the measured power that no inverter produces.
fn smoothstep(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// Smooth value noise in `[-1, 1]` with a lattice spacing of one unit of `x`.
fn value_noise(seed: u64, stream: u64, x: f64) -> f64 {
    let floor = x.floor();
    #[allow(clippy::cast_possible_truncation)]
    let index = floor as i64;
    let frac = smoothstep(x - floor);
    let a = hashed(seed, stream, index);
    let b = hashed(seed, stream, index + 1);
    a + (b - a) * frac
}

/// Four octaves of value noise, normalised back into `[-1, 1]`.
///
/// `slowest_hours` is the characteristic time of the first octave; each further
/// one is four times faster and half as strong. At the default four hours that
/// is a front, a haze, a cumulus field and an individual cloud — 4 h, 1 h,
/// 15 min and about 4 min.
///
/// The last octave is the one that earns its keep: without something moving
/// *inside* a quarter hour the planner's grain hides the whole error, and the
/// arbiter's energy tracking — the mechanism built for exactly this — has
/// nothing to correct.
fn fbm(seed: u64, stream: u64, hours_since_midnight: f64, slowest_hours: f64) -> f64 {
    let x = hours_since_midnight / slowest_hours;
    let raw = value_noise(seed, stream, x)
        + 0.5 * value_noise(seed, stream.wrapping_add(101), x * 4.0)
        + 0.25 * value_noise(seed, stream.wrapping_add(202), x * 16.0)
        + 0.125 * value_noise(seed, stream.wrapping_add(303), x * 64.0);
    raw / 1.875
}

impl Realisation {
    /// A realisation with a given seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// Hours since the local midnight of `at`'s day — the axis every process
    /// below runs on.
    ///
    /// Local, because the weather a household experiences keeps local time and
    /// so does its behaviour; and hours rather than slots, because the fast
    /// octave has to move *inside* a quarter hour or the arbiter has nothing to
    /// react to.
    fn hours(at: OffsetDateTime) -> f64 {
        let slot = Slot::containing(at);
        let into_slot = (at - slot.start()).as_seconds_f64() / 3600.0;
        f64::from(slot.local_minute_of_day()) / 60.0 + into_slot
    }

    /// The cloud cover at an instant, in `[0, 1]`, around a daily mean.
    ///
    /// `mean` is the day's expected cloudiness — what a weather service would
    /// have said. The realisation moves around it and is clamped, so an
    /// overcast day cannot accidentally become sunnier than clear sky.
    ///
    /// `amplitude` is how variable the day is: near zero for a settled
    /// high-pressure June day, half or more for a broken-cloud April one. It is
    /// deliberately an input rather than a constant, because "how wrong is the
    /// forecast today" is the parameter the whole exercise is about.
    #[must_use]
    pub fn cloud_cover(&self, at: OffsetDateTime, mean: f64, amplitude: f64) -> f64 {
        let n = fbm(self.seed, STREAM_CLOUD, Self::hours(at), 4.0);
        (mean + amplitude * n).clamp(0.0, 1.0)
    }

    /// A multiplier on the household's usual load at an instant.
    ///
    /// Around one, slowly varying, and never negative: somebody is at home
    /// today, or is not, and boils a kettle when the profile did not expect one.
    #[must_use]
    pub fn load_factor(&self, at: OffsetDateTime, amplitude: f64) -> f64 {
        let n = fbm(self.seed, STREAM_LOAD, Self::hours(at), 3.0);
        amplitude.mul_add(n, 1.0).max(0.05)
    }

    /// The outdoor temperature at an instant, °C.
    ///
    /// The **diurnal shape** is the forecastable part — coldest around five in
    /// the morning, warmest around three in the afternoon — and
    /// [`Realisation::forecast_outdoor_c`] returns exactly it. What this adds is
    /// the slow error a forecast makes: a front that is two hours late, an
    /// afternoon three kelvin milder than modelled.
    ///
    /// It matters more than it looks. The heat pump's coefficient of performance
    /// is linear in this number, so a plan made against a flat 2 °C prices the
    /// morning and the afternoon identically — and pre-heating into the *warm*
    /// part of the day is one of the two things a heating plan is for.
    #[must_use]
    pub fn outdoor_c(&self, at: OffsetDateTime, mean_c: f64, swing_k: f64, error_k: f64) -> f64 {
        let n = fbm(self.seed, STREAM_OUTDOOR, Self::hours(at), 8.0);
        Self::forecast_outdoor_c(at, mean_c, swing_k) + error_k * n
    }

    /// The diurnal temperature shape alone — what the weather service said.
    ///
    /// A cosine with its minimum at 05:00 and its maximum at 17:00. Crude, and
    /// far closer to a German day than the constant the scenarios used to pass.
    #[must_use]
    pub fn forecast_outdoor_c(at: OffsetDateTime, mean_c: f64, swing_k: f64) -> f64 {
        let hours = Self::hours(at);
        let phase = (hours - 5.0) / 24.0 * std::f64::consts::TAU;
        swing_k.mul_add(-phase.cos(), mean_c)
    }

    /// A multiplier on the hot water a household draws in a slot.
    ///
    /// Whole-slot rather than continuous: a shower is an event, and what varies
    /// between days is whether it happened and how long it lasted, not its
    /// second-by-second shape.
    #[must_use]
    pub fn draw_factor(&self, slot: Slot, amplitude: f64) -> f64 {
        let n = value_noise(
            self.seed,
            STREAM_DRAW,
            f64::from(slot.index_in_local_day()) / 4.0,
        );
        amplitude.mul_add(n, 1.0).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOON: OffsetDateTime = datetime!(2026-06-21 12:00:00 +02:00);

    // Bit-for-bit equality is the property, not an approximation of it: a day
    // that replays to the last cent (D23) needs the same cloud at 12:19 on every
    // machine and under any number of threads.
    #[allow(clippy::float_cmp)]
    #[test]
    fn the_same_seed_gives_the_same_day_for_ever() {
        let a = Realisation::new(7);
        let b = Realisation::new(7);
        for minute in 0..1440 {
            let at = datetime!(2026-06-21 00:00:00 +02:00) + time::Duration::minutes(minute);
            assert_eq!(a.cloud_cover(at, 0.3, 0.4), b.cloud_cover(at, 0.3, 0.4));
        }
    }

    #[test]
    fn different_seeds_are_different_days() {
        let a = Realisation::new(1);
        let b = Realisation::new(2);
        let differ = (0..96)
            .map(|k| datetime!(2026-06-21 00:00:00 +02:00) + time::Duration::minutes(k * 15))
            .filter(|at| (a.cloud_cover(*at, 0.3, 0.4) - b.cloud_cover(*at, 0.3, 0.4)).abs() > 0.01)
            .count();
        assert!(differ > 60, "only {differ} of 96 quarter hours differ");
    }

    #[test]
    fn cloud_cover_stays_a_fraction() {
        let r = Realisation::new(42);
        for minute in 0..1440 {
            let at = datetime!(2026-06-21 00:00:00 +02:00) + time::Duration::minutes(minute);
            let c = r.cloud_cover(at, 0.9, 0.8);
            assert!((0.0..=1.0).contains(&c), "{c}");
        }
    }

    #[test]
    fn the_error_moves_inside_a_quarter_hour() {
        // The whole point: a planner working in quarter hours must be given
        // something to be wrong about *within* a slot, or the arbiter's energy
        // tracking is never exercised.
        let r = Realisation::new(3);
        let steps: Vec<f64> = (0..240)
            .map(|m| {
                let at = NOON - time::Duration::hours(2) + time::Duration::minutes(m);
                (r.cloud_cover(at, 0.4, 0.5)
                    - r.cloud_cover(at + time::Duration::minutes(5), 0.4, 0.5))
                .abs()
            })
            .collect();
        let mean = steps.iter().sum::<f64>() / steps.len() as f64;
        assert!(
            mean > 0.01,
            "a mean five-minute change of {mean} is not a passing cloud"
        );
    }

    #[test]
    fn but_it_is_correlated_rather_than_white() {
        // Neighbouring seconds must be close, or the "cloud" is numerical hash
        // and every device on the site chases it.
        let r = Realisation::new(3);
        let a = r.cloud_cover(NOON, 0.4, 0.5);
        let b = r.cloud_cover(NOON + time::Duration::seconds(10), 0.4, 0.5);
        assert!((a - b).abs() < 0.02, "{a} then {b} is white noise");
    }

    #[test]
    fn a_zero_amplitude_day_is_the_forecast_exactly() {
        // The escape hatch every regression test needs: the old behaviour is
        // this one with the amplitudes at zero, so a test that wants perfect
        // foresight can still ask for it — and has to say so.
        let r = Realisation::new(11);
        assert!((r.cloud_cover(NOON, 0.55, 0.0) - 0.55).abs() < 1e-12);
        assert!((r.load_factor(NOON, 0.0) - 1.0).abs() < 1e-12);
        assert!(
            (r.outdoor_c(NOON, 2.0, 0.0, 0.0) - 2.0).abs() < 1e-12,
            "no swing and no error is the old constant"
        );
    }

    #[test]
    fn the_day_is_coldest_before_dawn_and_warmest_in_the_afternoon() {
        let midnight = datetime!(2026-01-15 00:00:00 +01:00);
        let at =
            |h: i64| Realisation::forecast_outdoor_c(midnight + time::Duration::hours(h), 2.0, 4.0);
        assert!(at(5) < at(0), "05:00 is the minimum");
        assert!(at(17) > at(12), "17:00 is the maximum");
        assert!((at(5) - (2.0 - 4.0)).abs() < 1e-9);
        assert!((at(17) - (2.0 + 4.0)).abs() < 1e-9);
    }

    #[test]
    fn the_load_factor_is_never_negative() {
        let r = Realisation::new(5);
        for minute in 0..1440 {
            let at = datetime!(2026-01-15 00:00:00 +01:00) + time::Duration::minutes(minute);
            assert!(r.load_factor(at, 3.0) > 0.0);
        }
    }
}
