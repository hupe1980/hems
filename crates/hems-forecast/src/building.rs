//! Which house this actually is.
//!
//! The planner solves a heating schedule against [`hems_core::thermal::Rc2`],
//! and until now it solved it against `Rc2::house()` — a well-insulated 150 m²
//! German single-family house, which is a reasonable *prior* and is wrong for
//! every specific building. The error is not a rounding one: the fabric capacity
//! is what decides whether pre-heating into a cheap hour pays at all, and it
//! differs by a factor of three between a 1970s solid-wall house and a new
//! timber frame. A plan that pre-heats a building with a third of the assumed
//! inertia over-heats it and then pays the comfort slack for the overshoot.
//!
//! A house tells you what it is if you watch it. Indoor temperature, outdoor
//! temperature and the heat put in are all measured on any site with a heat pump
//! the manager can talk to, and four parameters is few enough to identify from a
//! week of them.
//!
//! # How the fit works, and why this way
//!
//! The criterion is **one-step-ahead** prediction error on the air node under
//! the same exact zero-order-hold step the planner will use
//! ([`Rc2::discretise`]). One step ahead rather than a whole simulated
//! trajectory, because a multi-step criterion is dominated by the slow mode and
//! will happily accept an air capacity that is badly wrong; and the *same* step,
//! because a model identified under one discretisation and deployed under
//! another is fitted to the discretisation error as much as to the house.
//!
//! The search is a deterministic **pattern search** in log space: a step is
//! tried along each of the four axes and along every signed pair of them, and
//! the step is halved whenever no direction improves. Log space because every
//! parameter is a positive scale, so a 10 % change means the same thing at
//! either end of its range. The pairs are what make it work rather than a
//! nicety: the error surface of an RC pair has a long **ridge** — a heavier
//! fabric with a tighter coupling to the air predicts almost the same next
//! quarter hour as a lighter one with a looser coupling — and pure coordinate
//! descent walks up to the ridge and then stops, three quarters of the way to
//! the answer. Four parameters, thirty-two directions and no dependencies: a
//! household box has no business linking a nonlinear least-squares library for
//! this.
//!
//! # What it refuses to do
//!
//! Identification needs **excitation**: a week in which the heating never
//! changed says nothing about the response to heating. [`identify`] returns
//! `None` where the record is too short, where the heat input never moved, or
//! where the fit is not clearly better than the prior it started from — and the
//! planner then goes on using the prior, which is P5 in the one place where
//! guessing is genuinely worse than admitting ignorance.

use hems_core::prelude::{Rc2, ThermalState};
use time::Duration;

/// One measured step of the building's life.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ThermalSample {
    /// Indoor air temperature at the start of the step, °C.
    pub indoor_c: f64,
    /// Fabric temperature at the start of the step, °C.
    ///
    /// Almost never measured directly. A site with one sensor should pass the
    /// indoor temperature here for the *first* sample and let [`identify`]
    /// propagate the fabric state through the record, which is what
    /// [`identify`] does: it re-derives the fabric from the model rather than
    /// asking for a number nobody has.
    pub mass_c: f64,
    /// Heat delivered into the air during the step, kW.
    pub heat_kw: f64,
    /// Outdoor temperature over the step, °C.
    pub outdoor_c: f64,
    /// Indoor air temperature at the *end* of the step, °C — the thing being
    /// predicted.
    pub next_indoor_c: f64,
}

/// The fewest samples an identification will run on.
///
/// A day at a quarter-hour step. Fewer than that and the fit is describing the
/// weather rather than the house.
pub const MIN_SAMPLES: usize = 96;

/// How much better than the prior a fit has to be before it is adopted.
///
/// Five per cent of the mean squared one-step error. A fit that only ties with
/// the prior is a fit that learned nothing, and swapping a documented default
/// for an undocumented coincidence is a bad trade.
pub const MIN_IMPROVEMENT: f64 = 0.05;

/// What an identification concluded.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Identified {
    /// The building the data describes.
    pub building: Rc2,
    /// Root-mean-square one-step error of the fitted model, K.
    pub rmse_k: f64,
    /// The same for the prior it started from, so the improvement is visible.
    pub prior_rmse_k: f64,
    /// How many samples it rests on.
    pub samples: usize,
}

impl Identified {
    /// How much of the prior's error the fit removed, as a fraction.
    #[must_use]
    pub fn improvement(&self) -> f64 {
        if self.prior_rmse_k <= 0.0 {
            return 0.0;
        }
        1.0 - self.rmse_k / self.prior_rmse_k
    }
}

/// Mean squared one-step error of `building` over `samples`, propagating the
/// fabric state through the record.
///
/// The fabric is a hidden state: it is initialised from the first sample and
/// then carried by the model itself, so a candidate whose fabric drifts away
/// from reality is punished by the air-node errors that follow — which is the
/// only evidence about the fabric a household site ever produces.
fn one_step_mse(building: &Rc2, samples: &[ThermalSample], dt: Duration) -> f64 {
    let d = building.discretise(dt);
    let mut mass = samples[0].mass_c;
    let mut total = 0.0;
    for s in samples {
        let next = d.step(
            ThermalState {
                indoor_c: s.indoor_c,
                mass_c: mass,
            },
            s.heat_kw,
            s.outdoor_c,
        );
        let error = next.indoor_c - s.next_indoor_c;
        total += error * error;
        mass = next.mass_c;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = samples.len() as f64;
    total / n
}

/// The four parameters as a vector, in the order the search walks them.
fn to_vec(b: &Rc2) -> [f64; 4] {
    [
        b.air_capacity_kwh_per_k,
        b.mass_capacity_kwh_per_k,
        b.r_air_out_k_per_kw,
        b.r_air_mass_k_per_kw,
    ]
}

fn from_vec(v: [f64; 4]) -> Rc2 {
    Rc2 {
        air_capacity_kwh_per_k: v[0],
        mass_capacity_kwh_per_k: v[1],
        r_air_out_k_per_kw: v[2],
        r_air_mass_k_per_kw: v[3],
    }
}

/// Physically plausible bounds for a dwelling, so a fit cannot wander into a
/// house made of vacuum.
const BOUNDS: [(f64, f64); 4] = [
    (0.05, 5.0), // air capacity, kWh/K
    (1.0, 80.0), // fabric capacity, kWh/K
    (0.5, 60.0), // air ↔ outdoors, K/kW
    (0.02, 5.0), // air ↔ fabric, K/kW
];

/// The largest relative step the search starts with, and the smallest it stops
/// at. Ratios, because the parameters are scales.
const STEP_START: f64 = 0.5;
const STEP_STOP: f64 = 0.0005;

/// The directions the pattern search tries, as exponents on a step ratio.
///
/// The four axes, then every signed pair. A pair moving two parameters the
/// *same* way walks along the ridge described in the module note; a pair moving
/// them opposite ways crosses it.
fn directions() -> Vec<[f64; 4]> {
    let mut out = Vec::with_capacity(32);
    for i in 0..4 {
        for sign in [1.0, -1.0] {
            let mut d = [0.0; 4];
            d[i] = sign;
            out.push(d);
        }
    }
    for i in 0..4 {
        for j in (i + 1)..4 {
            for si in [1.0, -1.0] {
                for sj in [1.0, -1.0] {
                    let mut d = [0.0; 4];
                    d[i] = si;
                    d[j] = sj;
                    out.push(d);
                }
            }
        }
    }
    out
}

/// Whether the record says anything about how the house responds to heat.
///
/// Without variation in the heat input the identification is fitting a free
/// response, and the two capacities become unidentifiable from the resistances.
fn is_excited(samples: &[ThermalSample]) -> bool {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for s in samples {
        min = min.min(s.heat_kw);
        max = max.max(s.heat_kw);
    }
    max - min > 0.5
}

/// Identify a building from its own record.
///
/// `dt` is the step the samples are spaced at *and* the step the result will be
/// used at. `prior` is what to start from and what to fall back on — normally
/// [`Rc2::house`].
///
/// Returns `None` when there is too little data, when the heat input never
/// moved, or when the fit does not beat the prior by [`MIN_IMPROVEMENT`].
#[must_use]
pub fn identify(samples: &[ThermalSample], dt: Duration, prior: Rc2) -> Option<Identified> {
    if samples.len() < MIN_SAMPLES || !prior.is_valid() || !is_excited(samples) {
        return None;
    }
    if samples.iter().any(|s| {
        ![
            s.indoor_c,
            s.mass_c,
            s.heat_kw,
            s.outdoor_c,
            s.next_indoor_c,
        ]
        .iter()
        .all(|v| v.is_finite())
    }) {
        return None;
    }

    let prior_mse = one_step_mse(&prior, samples, dt);
    let mut best = to_vec(&prior);
    let mut best_mse = prior_mse;
    let mut step = STEP_START;
    let directions = directions();

    while step > STEP_STOP {
        let mut improved = false;
        for direction in &directions {
            let mut candidate = best;
            for i in 0..4 {
                if direction[i] != 0.0 {
                    candidate[i] = (candidate[i] * (1.0 + step).powf(direction[i]))
                        .clamp(BOUNDS[i].0, BOUNDS[i].1);
                }
            }
            if candidate == best {
                continue;
            }
            let mse = one_step_mse(&from_vec(candidate), samples, dt);
            if mse < best_mse {
                best = candidate;
                best_mse = mse;
                improved = true;
            }
        }
        if !improved {
            step /= 2.0;
        }
    }

    let identified = Identified {
        building: from_vec(best),
        rmse_k: best_mse.sqrt(),
        prior_rmse_k: prior_mse.sqrt(),
        samples: samples.len(),
    };
    (identified.improvement() >= MIN_IMPROVEMENT).then_some(identified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::SLOT;

    /// Generate a record from a known house, with a heat input that actually
    /// moves — which is what makes the parameters identifiable.
    fn record(truth: Rc2, n: usize) -> Vec<ThermalSample> {
        let d = truth.discretise(SLOT);
        let mut state = ThermalState::uniform(20.0);
        let mut out = Vec::with_capacity(n);
        for k in 0..n {
            #[allow(clippy::cast_precision_loss)]
            let t = k as f64;
            // A thermostat-ish duty cycle plus a slow drift, so the input has
            // content at more than one frequency.
            let heat_kw = if (k / 6) % 2 == 0 { 4.0 } else { 0.0 };
            let outdoor_c = 2.0 + 4.0 * (t / 96.0 * std::f64::consts::TAU).sin();
            let next = d.step(state, heat_kw, outdoor_c);
            out.push(ThermalSample {
                indoor_c: state.indoor_c,
                mass_c: state.mass_c,
                heat_kw,
                outdoor_c,
                next_indoor_c: next.indoor_c,
            });
            state = next;
        }
        out
    }

    #[test]
    fn a_house_is_recovered_from_its_own_record() {
        // A heavier, leakier building than the prior: solid walls, worse
        // windows — the case the prior is most wrong about.
        let truth = Rc2 {
            air_capacity_kwh_per_k: 0.35,
            mass_capacity_kwh_per_k: 25.0,
            r_air_out_k_per_kw: 3.5,
            r_air_mass_k_per_kw: 0.25,
        };
        let samples = record(truth, 4 * 96);
        let fit = identify(&samples, SLOT, Rc2::house()).expect("four days of an excited house");

        assert!(
            fit.rmse_k < 0.01,
            "one-step error {} K is not a fit",
            fit.rmse_k
        );
        assert!(fit.improvement() > 0.9, "improvement {}", fit.improvement());

        // The parameters themselves need only be close enough that the *step*
        // agrees: an RC pair has a mild ridge along it, and what the planner
        // consumes is the discretisation, not the four numbers.
        let a = fit.building.discretise(SLOT);
        let b = truth.discretise(SLOT);
        assert!((a.b_heat[0] - b.b_heat[0]).abs() < 0.01, "{a:?} vs {b:?}");
        assert!((a.a[0][0] - b.a[0][0]).abs() < 0.01, "{a:?} vs {b:?}");
    }

    #[test]
    fn a_house_that_matches_the_prior_is_left_alone() {
        let samples = record(Rc2::house(), 2 * 96);
        assert!(
            identify(&samples, SLOT, Rc2::house()).is_none(),
            "nothing to learn is not a fit worth adopting"
        );
    }

    #[test]
    fn a_record_with_no_excitation_is_refused() {
        let truth = Rc2 {
            air_capacity_kwh_per_k: 0.35,
            ..Rc2::house()
        };
        let d = truth.discretise(SLOT);
        let mut state = ThermalState::uniform(20.0);
        let samples: Vec<_> = (0..2 * 96)
            .map(|_| {
                let next = d.step(state, 0.0, 5.0);
                let s = ThermalSample {
                    indoor_c: state.indoor_c,
                    mass_c: state.mass_c,
                    heat_kw: 0.0,
                    outdoor_c: 5.0,
                    next_indoor_c: next.indoor_c,
                };
                state = next;
                s
            })
            .collect();
        assert!(identify(&samples, SLOT, Rc2::house()).is_none());
    }

    #[test]
    fn too_short_a_record_is_refused() {
        let samples = record(Rc2::house(), 20);
        assert!(identify(&samples, SLOT, Rc2::house()).is_none());
    }

    #[test]
    fn the_fit_stays_inside_physics() {
        let truth = Rc2 {
            air_capacity_kwh_per_k: 0.35,
            mass_capacity_kwh_per_k: 25.0,
            r_air_out_k_per_kw: 3.5,
            r_air_mass_k_per_kw: 0.25,
        };
        let samples = record(truth, 3 * 96);
        let fit = identify(&samples, SLOT, Rc2::house()).expect("a fit");
        assert!(fit.building.is_valid());
        for (v, (lo, hi)) in to_vec(&fit.building).iter().zip(BOUNDS) {
            assert!((lo..=hi).contains(v), "{v} outside [{lo}, {hi}]");
        }
    }
}
