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
//! planner then goes on using the prior — refusing to answer in the one place where
//! guessing is genuinely worse than admitting ignorance.

use hems_core::prelude::{Rc2, Slot, ThermalState};
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
    ///
    /// The heating system's own contribution, and only that. The sun and the
    /// household are not in it: they are what [`Rc2::solar_aperture_m2`] and
    /// [`Rc2::internal_gain_kw`] are being **fitted** for, and adding them here
    /// with an assumed size would be handing the fit the answer.
    pub heat_kw: f64,
    /// Outdoor temperature over the step, °C.
    pub outdoor_c: f64,
    /// Irradiance on the building's principal glazed plane over the step, W/m².
    ///
    /// Vertical, at the façade's own azimuth — [`crate::solar::window_irradiance`]
    /// computes it from a global horizontal value. A record that has none may
    /// pass zero throughout, and [`identify`] then leaves the aperture at the
    /// prior instead of fitting a parameter nothing excites.
    pub solar_w_per_m2: f64,
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
            // The heating system plus what the candidate says the house gets for
            // nothing. The free heat is part of the *model* here, not part of
            // the record, which is what makes the aperture and the internal gain
            // identifiable at all.
            s.heat_kw + building.free_heat_kw(s.solar_w_per_m2),
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

/// How many parameters the search walks.
const PARAMS: usize = 6;

/// The parameters as a vector, in the order the search walks them.
fn to_vec(b: &Rc2) -> [f64; PARAMS] {
    [
        b.air_capacity_kwh_per_k,
        b.mass_capacity_kwh_per_k,
        b.r_air_out_k_per_kw,
        b.r_air_mass_k_per_kw,
        b.solar_aperture_m2,
        b.internal_gain_kw,
    ]
}

fn from_vec(v: [f64; PARAMS]) -> Rc2 {
    Rc2 {
        air_capacity_kwh_per_k: v[0],
        mass_capacity_kwh_per_k: v[1],
        r_air_out_k_per_kw: v[2],
        r_air_mass_k_per_kw: v[3],
        solar_aperture_m2: v[4],
        internal_gain_kw: v[5],
    }
}

/// Physically plausible bounds for a dwelling, so a fit cannot wander into a
/// house made of vacuum.
///
/// Every lower bound is strictly positive because the search is
/// **multiplicative**: a parameter that reached exactly zero could never leave
/// it again, and a dwelling with no glazing and nobody in it is not one of the
/// answers worth being able to give.
const BOUNDS: [(f64, f64); PARAMS] = [
    (0.05, 5.0),  // air capacity, kWh/K
    (1.0, 80.0),  // fabric capacity, kWh/K
    (0.5, 60.0),  // air ↔ outdoors, K/kW
    (0.02, 5.0),  // air ↔ fabric, K/kW
    (0.05, 30.0), // solar aperture, m² — 30 is a wall of glass
    (0.02, 3.0),  // internal gain, kW — 3 kW is a party, not a household
];

/// The largest relative step the search starts with, and the smallest it stops
/// at. Ratios, because the parameters are scales.
const STEP_START: f64 = 0.5;
const STEP_STOP: f64 = 0.0005;

/// The directions the pattern search tries over `active`, as exponents on a
/// step ratio.
///
/// Each active axis, then every signed pair of them. A pair moving two
/// parameters the *same* way walks along the ridge described in the module note;
/// a pair moving them opposite ways crosses it.
///
/// The axes are a parameter rather than all of them because an unexcited one
/// must not be walked: see [`excited`].
fn directions(active: &[usize]) -> Vec<[f64; PARAMS]> {
    let mut out = Vec::with_capacity(active.len() * 2 + active.len() * active.len() * 2);
    for &i in active {
        for sign in [1.0, -1.0] {
            let mut d = [0.0; PARAMS];
            d[i] = sign;
            out.push(d);
        }
    }
    for (a, &i) in active.iter().enumerate() {
        for &j in &active[a + 1..] {
            for si in [1.0, -1.0] {
                for sj in [1.0, -1.0] {
                    let mut d = [0.0; PARAMS];
                    d[i] = si;
                    d[j] = sj;
                    out.push(d);
                }
            }
        }
    }
    out
}

/// The spread of one column of the record.
fn spread(samples: &[ThermalSample], of: impl Fn(&ThermalSample) -> f64) -> f64 {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for s in samples {
        let v = of(s);
        min = min.min(v);
        max = max.max(v);
    }
    max - min
}

/// Which parameters this record can say anything about.
///
/// Identification needs **excitation**, and the two halves of the model need
/// different excitation. Without variation in the heat input the fit is watching
/// a free response and the capacities become unidentifiable from the
/// resistances — that is the refusal [`identify`] has always made. The solar
/// aperture needs the same thing from the *irradiance*: a record taken through a
/// fortnight of unbroken cloud, or one whose caller has no irradiance to give
/// and passes zero, says nothing about how much sun this house lets in.
///
/// The answer there is not to refuse the whole identification — the fabric is
/// still learnable and is the valuable half — but to leave the aperture at the
/// prior and not walk it. A parameter the data cannot constrain, left free in a
/// search, does not stay where it started: it absorbs whatever else the model
/// gets wrong.
///
/// The internal gain is always walked. It is a constant offset in the heat
/// input, so any record that excites the fabric at all constrains it.
fn excited(samples: &[ThermalSample]) -> Option<Vec<usize>> {
    // Half a kilowatt of movement in the heating, or there is no fabric to fit.
    if spread(samples, |s| s.heat_kw) <= 0.5 {
        return None;
    }
    let mut active = vec![0, 1, 2, 3, 5];
    // A hundred watts per square metre between the darkest and the brightest
    // step: less than the difference between an overcast noon and a dark one, so
    // this refuses only a record with no daylight in it at all.
    if spread(samples, |s| s.solar_w_per_m2) > 100.0 {
        active.push(4);
        active.sort_unstable();
    }
    Some(active)
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
    if samples.len() < MIN_SAMPLES || !prior.is_valid() {
        return None;
    }
    if samples.iter().any(|s| {
        ![
            s.indoor_c,
            s.mass_c,
            s.heat_kw,
            s.outdoor_c,
            s.next_indoor_c,
            s.solar_w_per_m2,
        ]
        .iter()
        .all(|v| v.is_finite())
    }) {
        return None;
    }
    let active = excited(samples)?;

    let prior_mse = one_step_mse(&prior, samples, dt);
    let mut best = to_vec(&prior);
    let mut best_mse = prior_mse;
    let mut step = STEP_START;
    let directions = directions(&active);

    while step > STEP_STOP {
        let mut improved = false;
        for direction in &directions {
            let mut candidate = best;
            for i in 0..PARAMS {
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

/// How many samples the record keeps.
///
/// A fortnight at a quarter-hour step. Long enough to hold a cold spell and a
/// mild one, short enough that a house whose windows were replaced in March is
/// re-learned by April.
pub const WINDOW: usize = 96 * 14;

/// A house watching itself.
///
/// [`identify`] takes a record and answers; this is what keeps the record. The
/// two are separate because identification is a pure function of a slice and
/// belongs to whoever wants to run it — a laboratory fit on a CSV, a simulator,
/// or this — and because the awkward part of the job is not the fit at all. It
/// is that a sample spans **two** observations: the box sees an indoor
/// temperature now and learns what it predicts only a quarter hour later.
///
/// So an observation is held open until the next one closes it, and a gap —
/// a restart, a sensor that dropped out, a box that was off for a day — closes
/// nothing and starts again. A pair stitched across a gap would teach the model
/// that four hours of cooling happened in fifteen minutes, which is a house made
/// of tissue paper.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Record {
    /// Closed samples, oldest first.
    samples: std::collections::VecDeque<ThermalSample>,
    /// The observation waiting for the next one to close it.
    open: Option<Observation>,
    /// The building in force, fitted or prior.
    building: Rc2,
    /// What the last accepted fit said, for anyone reporting on it.
    fitted: Option<Identified>,
}

/// One reading, before the next one turns it into a sample.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
struct Observation {
    /// The slot this reading covers.
    slot: Slot,
    indoor_c: f64,
    outdoor_c: f64,
    heat_kw: f64,
    solar_w_per_m2: f64,
}

impl Default for Record {
    fn default() -> Self {
        Self::new(Rc2::house())
    }
}

impl Record {
    /// A record of a house nobody has watched yet, starting from `prior`.
    #[must_use]
    pub fn new(prior: Rc2) -> Self {
        Self {
            samples: std::collections::VecDeque::new(),
            open: None,
            building: prior,
            fitted: None,
        }
    }

    /// The building to plan against — the fit if there is one, else the prior.
    #[must_use]
    pub const fn building(&self) -> Rc2 {
        self.building
    }

    /// What the last accepted identification concluded, if any.
    #[must_use]
    pub const fn fitted(&self) -> Option<&Identified> {
        self.fitted.as_ref()
    }

    /// How many closed samples the record holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// One completed slot: how warm it was inside and out, and the **thermal**
    /// power that went into the air over it.
    ///
    /// Thermal rather than electrical, because that is what the model's `heat_kw`
    /// means — a heat pump drawing a kilowatt at a coefficient of three puts
    /// three into the house, and a record that fed it the meter reading would
    /// identify a building three times as leaky as the real one.
    ///
    /// A slot that does not directly follow the open observation discards it and
    /// starts again; so does any non-finite reading.
    pub fn observe(
        &mut self,
        slot: Slot,
        indoor_c: f64,
        outdoor_c: f64,
        heat_kw: f64,
        solar_w_per_m2: f64,
    ) {
        if ![indoor_c, outdoor_c, heat_kw, solar_w_per_m2]
            .iter()
            .all(|v| v.is_finite())
        {
            self.open = None;
            return;
        }
        let now = Observation {
            slot,
            indoor_c,
            outdoor_c,
            heat_kw,
            solar_w_per_m2,
        };
        if let Some(previous) = self.open.take() {
            if previous.slot.next() == slot {
                // The fabric is a hidden state `identify` re-derives for itself,
                // so it is seeded from the air here rather than invented.
                self.push(ThermalSample {
                    indoor_c: previous.indoor_c,
                    mass_c: previous.indoor_c,
                    heat_kw: previous.heat_kw,
                    outdoor_c: previous.outdoor_c,
                    solar_w_per_m2: previous.solar_w_per_m2,
                    next_indoor_c: indoor_c,
                });
            } else {
                // A gap. Everything before it stays — it was measured — and the
                // pair that would have spanned the gap is simply never made.
                self.open = None;
            }
        }
        self.open = Some(now);
    }

    fn push(&mut self, sample: ThermalSample) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Re-identify the house from everything the record holds.
    ///
    /// Returns what was adopted, or `None` where [`identify`] refused — too
    /// little data, a heat input that never moved, or a fit that did not beat
    /// what the box is already using. A refusal leaves the building alone, which
    /// is the point: the prior is a documented default and a fit that ties with
    /// it has learned nothing worth swapping it for.
    ///
    /// The **current** building is the prior, not [`Rc2::house`], so a box that
    /// has learned its house does not have to re-earn the same improvement from
    /// scratch every time.
    pub fn refit(&mut self, dt: Duration) -> Option<Identified> {
        let samples: Vec<ThermalSample> = self.samples.iter().copied().collect();
        let found = identify(&samples, dt, self.building)?;
        self.building = found.building;
        self.fitted = Some(found);
        Some(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::SLOT;

    /// Generate a record from a known house, with a heat input that actually
    /// moves — which is what makes the parameters identifiable.
    /// A day's worth of daylight on a vertical south wall, W/m² — a half-sine
    /// over the middle of the day and nothing at night.
    fn daylight(k: usize) -> f64 {
        let hour = (k % 96) as f64 / 4.0;
        if !(7.0..19.0).contains(&hour) {
            return 0.0;
        }
        450.0 * ((hour - 7.0) / 12.0 * std::f64::consts::PI).sin()
    }

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
            // Not every day is clear, or the sun and the clock would be the same
            // signal and the aperture would be unidentifiable from the daily
            // temperature swing.
            let solar_w_per_m2 = daylight(k) * if (k / 96) % 3 == 0 { 0.2 } else { 1.0 };
            let next = d.step(
                state,
                heat_kw + truth.free_heat_kw(solar_w_per_m2),
                outdoor_c,
            );
            out.push(ThermalSample {
                indoor_c: state.indoor_c,
                mass_c: state.mass_c,
                heat_kw,
                outdoor_c,
                solar_w_per_m2,
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
            solar_aperture_m2: 7.0,
            internal_gain_kw: 0.8,
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
    fn the_sun_a_house_lets_in_is_recovered_from_its_own_record() {
        // The aperture is the parameter this model gained, and a parameter a fit
        // cannot recover is a parameter that has become a place for the fit to
        // put its other errors. A house with half again the prior's glazing.
        let truth = Rc2 {
            solar_aperture_m2: 7.0,
            internal_gain_kw: 0.8,
            ..Rc2::house()
        };
        let samples = record(truth, 6 * 96);
        let fit = identify(&samples, SLOT, Rc2::house()).expect("six days of an excited house");
        assert!(
            (fit.building.solar_aperture_m2 - 7.0).abs() < 1.0,
            "aperture {} m² against 7,0",
            fit.building.solar_aperture_m2
        );
        assert!(
            (fit.building.internal_gain_kw - 0.8).abs() < 0.25,
            "internal gain {} kW against 0,8",
            fit.building.internal_gain_kw
        );
    }

    #[test]
    fn a_fortnight_of_darkness_leaves_the_aperture_alone_rather_than_inventing_one() {
        // Excitation is per parameter. A record with no daylight in it still
        // says everything about the fabric and nothing about the glazing, and a
        // parameter the data cannot constrain must not be walked: left free, it
        // absorbs whatever else the model gets wrong.
        let truth = Rc2 {
            air_capacity_kwh_per_k: 0.35,
            mass_capacity_kwh_per_k: 25.0,
            r_air_out_k_per_kw: 3.5,
            r_air_mass_k_per_kw: 0.25,
            solar_aperture_m2: 7.0,
            internal_gain_kw: 0.8,
        };
        let mut samples = record(truth, 4 * 96);
        // The same house, re-derived with the sun switched off — polar night,
        // or a caller with no irradiance to give.
        let dark = truth.discretise(SLOT);
        let mut state = ThermalState::uniform(20.0);
        for s in &mut samples {
            let next = dark.step(state, s.heat_kw + truth.internal_gain_kw, s.outdoor_c);
            s.indoor_c = state.indoor_c;
            s.mass_c = state.mass_c;
            s.solar_w_per_m2 = 0.0;
            s.next_indoor_c = next.indoor_c;
            state = next;
        }
        let prior = Rc2::house();
        let fit = identify(&samples, SLOT, prior).expect("the fabric is still learnable");
        assert_eq!(
            fit.building.solar_aperture_m2, prior.solar_aperture_m2,
            "an unexcited parameter stays where the prior put it"
        );
        // …and the fabric was learned anyway, which is the reason not to refuse
        // the whole identification.
        assert!(fit.improvement() > 0.5, "improvement {}", fit.improvement());
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
                    solar_w_per_m2: 0.0,
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
            solar_aperture_m2: 7.0,
            internal_gain_kw: 0.8,
        };
        let samples = record(truth, 3 * 96);
        let fit = identify(&samples, SLOT, Rc2::house()).expect("a fit");
        assert!(fit.building.is_valid());
        for (v, (lo, hi)) in to_vec(&fit.building).iter().zip(BOUNDS) {
            assert!((lo..=hi).contains(v), "{v} outside [{lo}, {hi}]");
        }
    }

    /// Play a house into a [`Record`] the way the control loop does — one slot
    /// at a time, indoor temperature only, no fabric.
    fn watched(truth: Rc2, slots: usize, from: Slot) -> Record {
        let d = truth.discretise(SLOT);
        let mut state = ThermalState::uniform(20.0);
        let mut record = Record::new(Rc2::house());
        let mut slot = from;
        for k in 0..slots {
            #[allow(clippy::cast_precision_loss)]
            let t = k as f64;
            let heat_kw = if (k / 6) % 2 == 0 { 4.0 } else { 0.0 };
            let outdoor_c = 2.0 + 4.0 * (t / 96.0 * std::f64::consts::TAU).sin();
            let solar_w_per_m2 = daylight(k) * if (k / 96) % 3 == 0 { 0.2 } else { 1.0 };
            record.observe(slot, state.indoor_c, outdoor_c, heat_kw, solar_w_per_m2);
            state = d.step(
                state,
                heat_kw + truth.free_heat_kw(solar_w_per_m2),
                outdoor_c,
            );
            slot = slot.next();
        }
        record
    }

    fn midnight() -> Slot {
        Slot::containing(time::macros::datetime!(2026-01-12 00:00:00 UTC))
    }

    #[test]
    fn a_box_watching_its_own_house_learns_it() {
        // The whole chain the daemon runs: one reading a quarter hour, each one
        // closing the last, and a fit at the end of it.
        let truth = Rc2 {
            air_capacity_kwh_per_k: 0.35,
            mass_capacity_kwh_per_k: 25.0,
            r_air_out_k_per_kw: 3.5,
            r_air_mass_k_per_kw: 0.25,
            solar_aperture_m2: 7.0,
            internal_gain_kw: 0.8,
        };
        let mut record = watched(truth, 4 * 96, midnight());
        assert_eq!(
            record.len(),
            4 * 96 - 1,
            "each reading closes the one before"
        );
        assert_eq!(
            record.building(),
            Rc2::house(),
            "nothing adopted until asked"
        );

        let fit = record
            .refit(SLOT)
            .expect("a house this different is learnable");
        assert!(fit.improvement() > MIN_IMPROVEMENT);
        assert_eq!(record.building(), fit.building);
        // The fabric capacity is what decides whether pre-heating pays, and the
        // prior is out by a factor of two on this house.
        let learned = record.building().mass_capacity_kwh_per_k;
        assert!(
            (learned - truth.mass_capacity_kwh_per_k).abs()
                < (Rc2::house().mass_capacity_kwh_per_k - truth.mass_capacity_kwh_per_k).abs(),
            "the fit moved towards the real fabric, not away from it: {learned}"
        );
    }

    #[test]
    fn a_gap_does_not_become_a_sample() {
        // The one mistake that would poison the fit rather than merely slow it:
        // a box that was off for four hours pairing the reading before with the
        // reading after teaches a house that cools sixteen times as fast as it
        // does.
        let mut record = Record::new(Rc2::house());
        let first = midnight();
        record.observe(first, 21.0, 0.0, 4.0, 0.0);
        record.observe(first.next(), 21.1, 0.0, 4.0, 0.0);
        assert_eq!(record.len(), 1);

        // Four hours later.
        let after = (0..16).fold(first, |s, _| s.next());
        record.observe(after, 18.0, 0.0, 0.0, 0.0);
        assert_eq!(record.len(), 1, "the pair spanning the gap was never made");

        record.observe(after.next(), 17.9, 0.0, 0.0, 0.0);
        assert_eq!(record.len(), 2, "and the record picks straight back up");
    }

    #[test]
    fn a_sensor_that_drops_out_breaks_the_chain_rather_than_poisoning_it() {
        let mut record = Record::new(Rc2::house());
        let s = midnight();
        record.observe(s, 21.0, 0.0, 4.0, 0.0);
        record.observe(s.next(), f64::NAN, 0.0, 4.0, 0.0);
        record.observe(s.next().next(), 21.2, 0.0, 4.0, 0.0);
        assert!(
            record.is_empty(),
            "neither pair touches a reading nobody took"
        );
    }

    #[test]
    fn a_house_nobody_can_learn_keeps_the_prior() {
        // No excitation: the heating never moved. `identify` refuses, and the
        // refusal has to leave the planner's building alone rather than adopt
        // whatever the search wandered into.
        let mut record = Record::new(Rc2::house());
        let mut slot = midnight();
        for k in 0..(3 * 96) {
            let drift = 20.0 + f64::from(k) * 0.001;
            record.observe(slot, drift, 5.0, 2.0, 0.0);
            slot = slot.next();
        }
        assert!(record.refit(SLOT).is_none());
        assert_eq!(record.building(), Rc2::house());
        assert!(record.fitted().is_none());
    }

    #[test]
    fn the_record_forgets_the_oldest_quarter_hour_rather_than_growing() {
        let record = watched(Rc2::house(), WINDOW + 200, midnight());
        assert_eq!(record.len(), WINDOW);
    }
}
