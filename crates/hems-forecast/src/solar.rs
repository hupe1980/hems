//! Where the sun is, and what a roof does with it.
//!
//! A photovoltaic forecast has two halves that fail differently. The geometry —
//! where the sun stands over a given roof at a given minute — is exact,
//! deterministic and free: it needs no weather service and no internet, and it
//! is the same next January as it was last January. The weather is neither.
//!
//! So hems computes the geometry itself and treats a cloud forecast as an
//! optional multiplier on top. A box that loses its internet connection keeps a
//! usable clear-sky expectation instead of falling back to nothing, which is the
//! difference between a house that still plans and one that only reacts.
//!
//! The model is the standard chain: solar position from the day of the year and
//! the equation of time, clear-sky global irradiance after Haurwitz, a
//! decomposition of the global value into its direct and diffuse halves, an
//! anisotropic transposition onto the module plane, the usual cell-temperature
//! correction, and the inverter's clipping limit.
//!
//! # The decomposition is the step that used to be a constant
//!
//! A plane of glass tilted 35° to the south does not see the horizontal
//! irradiance a weather model publishes. It sees a *beam* component projected
//! through the angle of incidence — which at a German winter noon is nearly
//! three times the horizontal projection — plus the share of the sky dome it
//! can see, plus a little off the ground. Turning one number into three is the
//! decomposition, and the number that decides it is the **diffuse fraction**.
//!
//! It was a constant here, 0,25, on the argument that the global value being
//! transposed *was* the clear-sky model, so the clearness index was one by
//! construction and every correlation collapsed to its clear-sky end. That
//! argument was wrong twice.
//!
//! It was wrong about the **call site**: [`crate::WeatherSeries::modelled_production`]
//! — the path a real box runs, on a real sky from `forecastd` — passes the
//! *measured* global irradiance, whose clearness index on an overcast December
//! day is about 0,09. The true diffuse fraction there is essentially one; the
//! constant claimed three quarters of it was beam and projected that onto the
//! roof at a factor of three, so the modelled production came out **2,7 times**
//! what the roof could make. That is the shape of error the residual corrector
//! cannot absorb, because it is bucketed by hour of day and this error is a
//! function of the *weather*: fitted across a fortnight it splits the difference
//! and is wrong in both directions.
//!
//! And it was wrong about the **arithmetic**: the clearness index of the
//! Haurwitz clear sky is not one either. It is about 0,78 at a German midsummer
//! noon and 0,61 in December, because a clear sky at an air mass of four is
//! genuinely hazier than one overhead. So the constant was 17 % optimistic even
//! on the clear-sky path it was written for.
//!
//! It is now the **Erbs correlation** on the actual clearness index, which is
//! the standard answer, has no fitted parameter of ours in it, and is right at
//! both ends by construction. The transposition that follows it is **HDKR**
//! (Hay–Davies–Klucher–Reindl) rather than isotropic: the same three terms plus
//! a circumsolar one and a horizon band, which is what makes a tilted plane come
//! out right under a clear sky instead of 10 % low.
//!
//! Neither introduces a knob. Both are in Duffie & Beckman, both are in `pvlib`,
//! and the reason to prefer them to something newer is that a household box has
//! to run them every quarter hour with no lookup table and no turbidity
//! climatology. D172 has the alternatives and what the change is worth on the
//! reference days.
//!
//! # The inverter is a step, not a share of the system losses
//!
//! `system_loss` is PVWatts' 0,14, and it was being asked to cover a loss it
//! does not contain. PVWatts' ten default categories — soiling, shading, snow,
//! mismatch, wiring, connections, light-induced degradation, nameplate
//! tolerance, age, availability — are every one of them on the **direct-current**
//! side; NREL models the inverter separately, and `pvlib` splits the same seam
//! into `pvwatts_dc` and `pvwatts_ac`. This module had the first and not the
//! second: it computed a direct current, clipped it at the inverter's
//! alternating-current limit, and called the result AC. The doc comment on
//! `system_loss` even said "everything … that is **not** the inverter", so the
//! gap was written down beside the code that had it.
//!
//! That is worth about 4 % at full sun and far more than 4 % where a German
//! roof actually lives: the published curve is
//! `η = (η_nom/η_ref)(−0,0162·ζ − 0,0059/ζ + 0,9858)`, and the reciprocal term
//! is an inverter's fixed housekeeping draw. At a twentieth of rated power it
//! is 86 %, at a fiftieth 69 %, and below about 0,6 % of rated the inverter
//! yields nothing at all — a start-up threshold that falls out of the
//! correlation rather than being a constant somebody chose.
//!
//! **No reference day could have found it**, and that is the part worth
//! recording. The simulated roof's *truth* is
//! `hemsd`'s `Weather::production_at`, which calls
//! [`ArrayModel::clear_sky_power`] — the same function the forecast is built
//! from. Simulator and model therefore shared the missing step exactly, it
//! cancelled in every comparison, and seven days that check a plan against a
//! realisation drawn from the plan's own physics are blind to any error in that
//! physics by construction. The only defence against that is an **external**
//! bound — a published correlation, transcribed and pinned (`inverter_tests`),
//! which is what R23 has been saying about every figure this workspace
//! generates.
//!
//! On a real roof the error does not cancel, and
//! [`crate::residual::ResidualModel`] is the only thing that would have stood in
//! for it. That is D172's defect one layer down and it fails the same two ways:
//! a box on its **first day** has no correction and is optimistic by the whole
//! amount (D187's cold start), and the residual is a function of **how bright it
//! is** while the corrector is bucketed by hour of day, so a fortnight's fit
//! splits the difference and is wrong in both directions. D194.

use hems_core::prelude::{GeoPoint, Power, Slot};

/// The solar constant, W/m².
///
/// Duffie & Beckman's 1367, deliberately, rather than the 1361 the satellite
/// record has since settled on. It is the value the Erbs correlation's own
/// clearness indices were computed against, and a decomposition is only as
/// meaningful as the normalisation it was fitted under. The 0,4 % difference
/// moves a diffuse fraction by well under a percentage point either way.
pub const SOLAR_CONSTANT_W_PER_M2: f64 = 1367.0;

/// Where the sun is, seen from one place at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPosition {
    /// Degrees above the horizon. Negative when the sun is down.
    pub elevation_deg: f64,
    /// Degrees clockwise from north; 180 is due south.
    pub azimuth_deg: f64,
    /// Extraterrestrial irradiance on a surface normal to the beam, W/m².
    ///
    /// The solar constant corrected for where the earth is in its orbit — about
    /// 3,3 % either side over a year. It is carried on the position rather than
    /// recomputed because it is the **denominator of the clearness index**, and
    /// a decomposition that had to be handed the day of the year separately
    /// would be one a caller could get wrong.
    pub dni_extra_w_per_m2: f64,
}

impl SunPosition {
    /// Whether the sun is above the horizon.
    #[must_use]
    pub fn is_up(&self) -> bool {
        self.elevation_deg > 0.0
    }

    /// The cosine of the zenith angle, clamped at zero.
    #[must_use]
    pub fn cos_zenith(&self) -> f64 {
        self.elevation_deg.to_radians().sin().max(0.0)
    }

    /// Extraterrestrial irradiance on the *horizontal*, W/m² — the denominator
    /// of the clearness index.
    #[must_use]
    pub fn extraterrestrial_ghi(&self) -> f64 {
        self.dni_extra_w_per_m2 * self.cos_zenith()
    }

    /// The clearness index: what fraction of the light at the top of the
    /// atmosphere reached the ground.
    ///
    /// Zero when the sun is too low for the ratio to mean anything — see
    /// [`MIN_DECOMPOSITION_ELEVATION_DEG`].
    #[must_use]
    pub fn clearness_index(&self, ghi: f64) -> f64 {
        let extraterrestrial = self.extraterrestrial_ghi();
        if self.elevation_deg < MIN_DECOMPOSITION_ELEVATION_DEG || extraterrestrial <= 0.0 {
            return 0.0;
        }
        (ghi / extraterrestrial).clamp(0.0, 1.0)
    }
}

/// Below this elevation the clearness index is not a number worth believing.
///
/// Its denominator carries a `cos z` that is heading for zero, so a few watts
/// of measurement or model noise in the numerator swing it across its whole
/// range — the well-known failure of every `kt`-based correlation at sunrise and
/// sunset. Below the floor the light is taken as **entirely diffuse**, which is
/// both the safe answer and very nearly the true one: at five degrees the air
/// mass is above ten and there is almost no beam left to project.
pub const MIN_DECOMPOSITION_ELEVATION_DEG: f64 = 5.0;

/// The diffuse share of a global horizontal irradiance, after Erbs.
///
/// Erbs, Klein and Duffie (1982), as presented in Duffie & Beckman and
/// implemented in `pvlib` as `erbs`. One input — the clearness index — no site
/// parameter, and no state, which is what a box that has to evaluate it ninety-
/// six times a plan needs.
///
/// It returns 1 for an overcast sky (all of very little light is diffuse) and
/// 0,165 for the clearest one the correlation admits.
///
/// Transcribed as published, wiggle included: the quartic bottoms out at
/// `kt = 0,792` and turns back up by 7 x 10⁻⁴ before the constant branch takes
/// over. Smoothing that would make this something other than the correlation it
/// cites, for a difference no roof can tell.
#[must_use]
pub fn erbs_diffuse_fraction(clearness_index: f64) -> f64 {
    let kt = clearness_index.clamp(0.0, 1.0);
    let kd = if kt <= 0.22 {
        1.0 - 0.09 * kt
    } else if kt <= 0.80 {
        0.9511 - 0.1604 * kt + 4.388 * kt.powi(2) - 16.638 * kt.powi(3) + 12.336 * kt.powi(4)
    } else {
        0.165
    };
    kd.clamp(0.0, 1.0)
}

/// The sun's position over `at` at the middle of `slot`.
///
/// Uses the middle rather than the start: over a quarter hour the sun moves
/// almost four degrees, and sampling at the edge biases every number in the
/// same direction all day.
#[must_use]
pub fn sun_position(at: GeoPoint, slot: Slot) -> SunPosition {
    let middle = slot.start() + hems_core::slot::SLOT / 2_i32;
    let day_of_year = f64::from(middle.ordinal());
    // Fractional hour in UTC — the geometry is in solar time, so the time zone
    // never enters. This is why a DST transition cannot move the sun.
    let hour_utc = f64::from(middle.hour())
        + f64::from(middle.minute()) / 60.0
        + f64::from(middle.second()) / 3600.0;

    // Spencer's Fourier expansion for the equation of time, minutes.
    let gamma = 2.0 * std::f64::consts::PI * (day_of_year - 1.0) / 365.0;
    // Spencer's eccentricity correction, from the same expansion as the
    // declination and the equation of time below — one day angle, three series,
    // so nothing here can disagree with anything else here about what day it is.
    let eccentricity = 1.000_110
        + 0.034_221 * gamma.cos()
        + 0.001_280 * gamma.sin()
        + 0.000_719 * (2.0 * gamma).cos()
        + 0.000_077 * (2.0 * gamma).sin();
    let eot = 229.18
        * (0.000_075 + 0.001_868 * gamma.cos()
            - 0.032_077 * gamma.sin()
            - 0.014_615 * (2.0 * gamma).cos()
            - 0.040_849 * (2.0 * gamma).sin());
    // Declination, radians (Spencer).
    let declination = 0.006_918 - 0.399_912 * gamma.cos() + 0.070_257 * gamma.sin()
        - 0.006_758 * (2.0 * gamma).cos()
        + 0.000_907 * (2.0 * gamma).sin()
        - 0.002_697 * (3.0 * gamma).cos()
        + 0.001_480 * (3.0 * gamma).sin();

    let solar_time = hour_utc + at.longitude / 15.0 + eot / 60.0;
    let hour_angle = ((solar_time - 12.0) * 15.0).to_radians();

    let lat = at.latitude.to_radians();
    let sin_elevation =
        lat.sin() * declination.sin() + lat.cos() * declination.cos() * hour_angle.cos();
    let elevation = sin_elevation.clamp(-1.0, 1.0).asin();

    // Azimuth measured clockwise from north.
    let cos_azimuth = (declination.sin() * lat.cos()
        - declination.cos() * lat.sin() * hour_angle.cos())
        / elevation.cos().max(1e-9);
    let azimuth = cos_azimuth.clamp(-1.0, 1.0).acos();
    let azimuth_deg = if hour_angle > 0.0 {
        360.0 - azimuth.to_degrees()
    } else {
        azimuth.to_degrees()
    };

    SunPosition {
        elevation_deg: elevation.to_degrees(),
        azimuth_deg,
        dni_extra_w_per_m2: SOLAR_CONSTANT_W_PER_M2 * eccentricity,
    }
}

/// Clear-sky global horizontal irradiance after Haurwitz, W/m².
///
/// One parameter, no tuning, and within a few percent of the measured clear-sky
/// value across the middle latitudes — which is more precision than a household
/// forecast can use.
#[must_use]
pub fn clear_sky_ghi(sun: SunPosition) -> f64 {
    let cos_z = sun.cos_zenith();
    if cos_z <= 0.0 {
        return 0.0;
    }
    (1098.0 * cos_z * (-0.059 / cos_z).exp()).max(0.0)
}

/// The cosine of the angle of incidence of the beam on an arbitrary plane.
///
/// `tilt_deg` is from horizontal, `azimuth_deg` clockwise from north. Negative
/// where the sun is behind the plane.
#[must_use]
pub fn cos_incidence(sun: SunPosition, tilt_deg: f64, azimuth_deg: f64) -> f64 {
    let tilt = tilt_deg.to_radians();
    let sun_el = sun.elevation_deg.to_radians();
    let delta_azimuth = (sun.azimuth_deg - azimuth_deg).to_radians();
    sun_el.sin() * tilt.cos() + sun_el.cos() * tilt.sin() * delta_azimuth.cos()
}

/// Irradiance on any plane, W/m², from the global horizontal value.
///
/// `ghi` may be a clear-sky model or a weather service's forecast; the split
/// into beam and diffuse comes from the **clearness index** of whatever it is,
/// so both are handled by the same arithmetic and neither is assumed. See the
/// module note for what assuming one cost.
///
/// The transposition is HDKR: the beam through the angle of incidence, the
/// circumsolar part of the diffuse through the same angle, the rest of the sky
/// dome through the plane's view factor with Klucher's horizon band, and the
/// ground reflection.
///
/// It is a free function because a roof is not the only plane a household has.
/// [`hems_core::thermal::Rc2::solar_aperture_m2`] is driven by the same
/// transposition onto a **vertical** one — the windows — and a second
/// implementation of it would be a second thing that could disagree about how
/// much sun a building gets.
#[must_use]
pub fn plane_of_array(sun: SunPosition, ghi: f64, tilt_deg: f64, azimuth_deg: f64) -> f64 {
    if ghi <= 0.0 || !sun.is_up() {
        return 0.0;
    }
    let cos_zenith = sun.cos_zenith();
    if cos_zenith <= 0.0 {
        return 0.0;
    }

    // ── Decomposition ───────────────────────────────────────────────────────
    // Below the elevation floor `clearness_index` returns zero, which Erbs maps
    // to a diffuse fraction of one: no beam to project, which is what makes the
    // low-sun singularity a non-event rather than a clamp.
    let dhi = ghi * erbs_diffuse_fraction(sun.clearness_index(ghi));
    let bhi = ghi - dhi;
    // The beam normal to itself. Capped at the top of the atmosphere, because a
    // horizontal beam divided by a small `cos z` is how a decomposition invents
    // light that is not there — and the cap is a physical bound rather than a
    // tuning constant.
    let dni = (bhi / cos_zenith).min(sun.dni_extra_w_per_m2);
    // Re-derived from the capped beam, so that what is transposed is the light
    // the cap left rather than the light the decomposition asked for. Where the
    // cap binds — a low sun under a clearness index the correlation reads as
    // mostly beam — the components then sum to *less* than `ghi`, and
    // deliberately: the alternative is to hand the difference to the diffuse
    // term, which would transpose invented light onto the plane through the sky
    // view factor. Under-reading a roof is the safe direction and the cap is
    // rare; over-reading it is D172.
    let bhi = dni * cos_zenith;

    // ── Transposition (HDKR) ────────────────────────────────────────────────
    let tilt = tilt_deg.to_radians();
    let cos_incidence = cos_incidence(sun, tilt_deg, azimuth_deg).max(0.0);
    // The beam ratio: what one square metre of plane sees against one square
    // metre of ground.
    let beam_ratio = cos_incidence / cos_zenith;
    // Hay's anisotropy index — how much of the diffuse light is really
    // forward-scattered sunlight travelling with the beam. Zero under an
    // overcast sky, which is what collapses HDKR back to isotropic exactly where
    // isotropic is right.
    let anisotropy = (dni / sun.dni_extra_w_per_m2).clamp(0.0, 1.0);
    // Klucher's modulating factor for the brighter band near the horizon.
    let horizon = (bhi / ghi).max(0.0).sqrt();

    let sky_view = f64::midpoint(1.0, tilt.cos());
    let ground_view = f64::midpoint(1.0, -tilt.cos());

    let beam = dni * cos_incidence;
    let circumsolar = dhi * anisotropy * beam_ratio;
    let sky = dhi * (1.0 - anisotropy) * sky_view * (1.0 + horizon * (tilt / 2.0).sin().powi(3));
    let ground = ghi * GROUND_ALBEDO * ground_view;

    beam + circumsolar + sky + ground
}

/// Irradiance on the vertical plane a building's windows are mostly in, W/m².
///
/// The input [`hems_core::thermal::Rc2::free_heat_kw`] wants. `azimuth_deg` is
/// the façade's own, clockwise from north; 180 is a south-facing front.
///
/// Vertical rather than horizontal, and that is the whole reason this exists
/// rather than the caller passing the global value through: at 52° north a
/// vertical south plane sees about 1,6 times the horizontal irradiance at a
/// December noon and about half of it at a June one. An aperture fitted against
/// the horizontal would be a different constant in every season, and a house
/// identified in the heating months would then be wrong all summer by a factor
/// of three.
#[must_use]
pub fn window_irradiance(sun: SunPosition, ghi: f64, azimuth_deg: f64) -> f64 {
    plane_of_array(sun, ghi, 90.0, azimuth_deg)
}

/// Reflectance of the ground in front of the array.
///
/// Two tenths: grass, gravel, a tiled roof below — the value every transposition
/// model uses when nobody has measured the site. It matters least of the four
/// terms (a 35° plane sees about 9 % of the ground hemisphere) and it is a
/// site constant, which is exactly the kind of error
/// [`crate::residual::ResidualModel`] absorbs. Snow is the case that would
/// justify making it a parameter, and it is not modelled anywhere else here
/// either.
const GROUND_ALBEDO: f64 = 0.2;

/// The reference inverter efficiency the PVWatts part-load curve is normalised
/// against.
///
/// NREL fitted the curve to the California Energy Commission's weighted
/// efficiencies for inverters built since 2010, and 0,9637 is the efficiency of
/// the unit it came out describing. It is a property of the **correlation**
/// rather than of any array here, which is why it is a constant and
/// [`ArrayModel::inverter_nominal_efficiency`] is a field: the ratio
/// `η_nom / η_ref` is what re-scales the published shape onto a particular
/// inverter's own datasheet figure.
const INVERTER_REFERENCE_EFFICIENCY: f64 = 0.9637;

/// The PVWatts inverter curve: efficiency as a function of how hard the
/// inverter is being driven.
///
/// ```text
/// η = (η_nom / η_ref) · (−0,0162·ζ − 0,0059/ζ + 0,9858),   ζ = P_dc / P_dc0
/// ```
///
/// The term that matters is `−0,0059/ζ`, and it is a **reciprocal**: an inverter
/// draws roughly the same housekeeping power whatever it is converting, so that
/// draw is a constant share of nothing at full load and a large share of very
/// little at dawn. A curve without it — a flat 96 % — is right exactly once a
/// day and wrong all winter, which is the regime a German roof spends most of
/// its year in.
///
/// Returns zero below the load ratio where the curve crosses the axis (about
/// 0,6 % of rated direct current), which is the inverter's own start-up
/// threshold falling out of the arithmetic rather than being a second constant.
#[must_use]
pub fn inverter_efficiency(load_ratio: f64, nominal_efficiency: f64) -> f64 {
    if !load_ratio.is_finite() || load_ratio <= 0.0 {
        return 0.0;
    }
    let shape = 0.9858 - 0.0162 * load_ratio - 0.0059 / load_ratio;
    ((nominal_efficiency / INVERTER_REFERENCE_EFFICIENCY) * shape).clamp(0.0, 1.0)
}

/// A photovoltaic array's geometry and electrical limits.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ArrayModel {
    /// Installed DC power.
    pub kwp_dc: Power,
    /// The inverter's AC limit.
    pub ac_nominal: Power,
    /// Tilt from horizontal, degrees.
    pub tilt_deg: f64,
    /// Azimuth clockwise from north; 180 is due south.
    pub azimuth_deg: f64,
    /// Everything between the modules and the meter that is not the inverter:
    /// soiling, mismatch, wiring, and the modules' own tolerance.
    ///
    /// PVWatts' own default, and taken with PVWatts' own **scope**: the ten
    /// categories it multiplies together — soiling, shading, snow, mismatch,
    /// wiring, connections, light-induced degradation, nameplate tolerance, age
    /// and availability — are all on the direct-current side, and the inverter
    /// is not among them. It is modelled separately, by
    /// [`ArrayModel::inverter_nominal_efficiency`] and
    /// [`inverter_efficiency`], because its loss is the one that depends on how
    /// hard it is being driven rather than on the installation.
    pub system_loss: f64,
    /// Relative power change per kelvin of cell temperature above 25 °C.
    /// Negative; −0,004 is typical for silicon.
    pub temperature_coefficient: f64,
    /// The inverter's efficiency at its own rated power, from its datasheet.
    ///
    /// It sets the *height* of [`inverter_efficiency`]'s curve; the curve's
    /// shape is the published one. 0,96 is PVWatts' default and a fair figure
    /// for a modern residential string inverter.
    pub inverter_nominal_efficiency: f64,
}

impl ArrayModel {
    /// A south-facing array at a plausible German roof pitch.
    #[must_use]
    pub fn new(kwp_dc: Power, ac_nominal: Power, tilt_deg: f64, azimuth_deg: f64) -> Self {
        Self {
            kwp_dc,
            ac_nominal,
            tilt_deg,
            azimuth_deg,
            system_loss: 0.14,
            temperature_coefficient: -0.004,
            inverter_nominal_efficiency: 0.96,
        }
    }

    /// The direct-current power at which the inverter reaches its alternating-
    /// current limit — PVWatts' `P_dc0`, and the denominator of the load ratio.
    ///
    /// Derived from the AC limit and the nominal efficiency rather than from the
    /// array's peak, because it is a fact about the **inverter**: an array
    /// deliberately oversized against its inverter (which most German roofs now
    /// are) still drives that inverter to exactly this much before it clips.
    #[must_use]
    pub fn inverter_dc_limit(&self) -> f64 {
        if self.inverter_nominal_efficiency > 0.0 {
            self.ac_nominal.get() / self.inverter_nominal_efficiency
        } else {
            self.ac_nominal.get()
        }
    }

    /// The cosine of the angle of incidence of the beam on this plane.
    ///
    /// Negative where the sun is behind the array; callers want
    /// [`f64::max`] against zero before using it as a projection.
    #[must_use]
    pub fn cos_incidence(&self, sun: SunPosition) -> f64 {
        cos_incidence(sun, self.tilt_deg, self.azimuth_deg)
    }

    /// Irradiance on the module plane, W/m², from the global horizontal value.
    ///
    /// [`plane_of_array`] for this array's own tilt and azimuth.
    #[must_use]
    pub fn plane_of_array(&self, sun: SunPosition, ghi: f64) -> f64 {
        plane_of_array(sun, ghi, self.tilt_deg, self.azimuth_deg)
    }

    /// Alternating-current power for a given plane irradiance and air
    /// temperature, as a **negative** value in the load convention.
    ///
    /// The chain is PVWatts' own, in PVWatts' order: plane irradiance → cell
    /// temperature → direct current after the system losses → **the inverter** →
    /// the alternating-current clip. The inverter step is the one this model
    /// went four versions without, and leaving it out is not a rounding error —
    /// see the module note.
    #[must_use]
    pub fn ac_power(&self, poa: f64, ambient_c: f64) -> Power {
        if poa <= 0.0 {
            return Power::ZERO;
        }
        // Nominal operating cell temperature model: the cell runs about 25 K
        // above ambient at full sun.
        let cell_c = ambient_c + poa / 800.0 * 25.0;
        let temperature_factor = 1.0 + self.temperature_coefficient * (cell_c - 25.0);
        let dc =
            (self.kwp_dc.get() * (poa / 1000.0) * temperature_factor * (1.0 - self.system_loss))
                .max(0.0);
        let efficiency = inverter_efficiency(
            dc / self.inverter_dc_limit(),
            self.inverter_nominal_efficiency,
        );
        // The inverter clips, which is why an oversized array is not wasted.
        -Power::new((dc * efficiency).min(self.ac_nominal.get()))
    }

    /// Expected production in `slot` under a clear sky, load convention.
    #[must_use]
    pub fn clear_sky_power(&self, at: GeoPoint, slot: Slot, ambient_c: f64) -> Power {
        let sun = sun_position(at, slot);
        let poa = self.plane_of_array(sun, clear_sky_ghi(sun));
        self.ac_power(poa, ambient_c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const BERLIN: GeoPoint = GeoPoint {
        latitude: 52.52,
        longitude: 13.40,
        altitude_m: 34.0,
    };

    fn slot(t: time::OffsetDateTime) -> Slot {
        Slot::containing(t)
    }

    #[test]
    fn at_the_equinox_the_noon_sun_stands_at_ninety_less_the_latitude() {
        // 2026-03-20 is the equinox; solar noon in Berlin is about 11:08 UTC.
        let sun = sun_position(BERLIN, slot(datetime!(2026-03-20 11:00:00 UTC)));
        assert!(
            (sun.elevation_deg - (90.0 - BERLIN.latitude)).abs() < 1.5,
            "elevation {} at the equinox",
            sun.elevation_deg
        );
        assert!(
            (sun.azimuth_deg - 180.0).abs() < 5.0,
            "azimuth {}",
            sun.azimuth_deg
        );
    }

    #[test]
    fn midsummer_beats_midwinter_by_about_forty_seven_degrees() {
        let summer = sun_position(BERLIN, slot(datetime!(2026-06-21 11:00:00 UTC)));
        let winter = sun_position(BERLIN, slot(datetime!(2026-12-21 11:00:00 UTC)));
        let spread = summer.elevation_deg - winter.elevation_deg;
        assert!((spread - 46.8).abs() < 2.0, "spread {spread}");
    }

    #[test]
    fn the_sun_is_down_at_night_and_the_model_produces_nothing() {
        let array = ArrayModel::new(Power::from_kw(9.8), Power::from_kw(8.0), 35.0, 180.0);
        let midnight = slot(datetime!(2026-06-21 23:00:00 UTC));
        assert!(!sun_position(BERLIN, midnight).is_up());
        assert_eq!(array.clear_sky_power(BERLIN, midnight, 15.0), Power::ZERO);
    }

    #[test]
    fn a_south_facing_roof_beats_a_north_facing_one() {
        let noon = slot(datetime!(2026-06-21 11:00:00 UTC));
        let south = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0);
        let north = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 0.0);
        let s = south.clear_sky_power(BERLIN, noon, 20.0).outflow();
        let n = north.clear_sky_power(BERLIN, noon, 20.0).outflow();
        assert!(s > n * 1.3, "south {s} should clearly beat north {n}");
    }

    #[test]
    fn production_is_negative_in_the_load_convention() {
        let array = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0);
        let p = array.clear_sky_power(BERLIN, slot(datetime!(2026-06-21 11:00:00 UTC)), 20.0);
        assert!(p < Power::ZERO, "got {p}");
    }

    #[test]
    fn the_inverter_clips_an_oversized_array() {
        let array = ArrayModel::new(Power::from_kw(20.0), Power::from_kw(8.0), 35.0, 180.0);
        let p = array.clear_sky_power(BERLIN, slot(datetime!(2026-06-21 11:00:00 UTC)), 20.0);
        assert_eq!(p, Power::from_kw(-8.0), "clipped at the inverter's limit");
    }

    #[test]
    fn heat_costs_output() {
        let array = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0);
        let noon = slot(datetime!(2026-06-21 11:00:00 UTC));
        let cool = array.clear_sky_power(BERLIN, noon, 10.0).outflow();
        let hot = array.clear_sky_power(BERLIN, noon, 35.0).outflow();
        assert!(
            cool > hot,
            "a cool day should out-produce a hot one: {cool} vs {hot}"
        );
    }

    #[test]
    fn a_summer_day_produces_more_than_a_winter_day() {
        let array = ArrayModel::new(Power::from_kw(9.8), Power::from_kw(8.0), 35.0, 180.0);
        let day_energy = |date: time::Date, temp: f64| -> f64 {
            let start = metering::calendar::day_start_utc(date);
            (0..96)
                .map(|i| {
                    array
                        .clear_sky_power(BERLIN, Slot::containing(start).offset(i), temp)
                        .outflow()
                        .kw()
                        * 0.25
                })
                .sum()
        };
        let june = day_energy(time::macros::date!(2026 - 06 - 21), 22.0);
        let december = day_energy(time::macros::date!(2026 - 12 - 21), 2.0);
        assert!(
            june > 40.0 && june < 80.0,
            "June clear-sky yield {june} kWh"
        );
        assert!(
            december > 5.0 && december < 25.0,
            "December clear-sky yield {december} kWh"
        );
        assert!(june > december * 2.5);
    }

    #[test]
    fn the_geometry_does_not_move_when_the_clocks_do() {
        // The same solar time either side of the March transition gives almost
        // the same elevation — because the model never touches local time.
        let before = sun_position(BERLIN, slot(datetime!(2026-03-28 11:00:00 UTC)));
        let after = sun_position(BERLIN, slot(datetime!(2026-03-30 11:00:00 UTC)));
        assert!((before.elevation_deg - after.elevation_deg).abs() < 1.5);
    }
}

#[cfg(test)]
mod decomposition_tests {
    use super::*;
    use time::macros::datetime;

    const BERLIN: super::GeoPoint = super::GeoPoint {
        latitude: 52.52,
        longitude: 13.40,
        altitude_m: 34.0,
    };

    fn slot(t: time::OffsetDateTime) -> Slot {
        Slot::containing(t)
    }

    /// The regression this whole module was rewritten for.
    ///
    /// An overcast December noon: about a tenth of the light at the top of the
    /// atmosphere reaches the ground, and essentially none of it is beam. The
    /// constant that used to sit here called three quarters of it beam and
    /// projected that onto a 35° plane at a factor of nearly three.
    #[test]
    fn an_overcast_winter_noon_does_not_out_produce_the_horizontal() {
        let array = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0);
        let noon = slot(datetime!(2026-12-21 11:00:00 UTC));
        let sun = sun_position(BERLIN, noon);
        let overcast = clear_sky_ghi(sun) * 0.15;
        let poa = array.plane_of_array(sun, overcast);
        assert!(
            poa < overcast * 1.15,
            "an overcast sky is diffuse: {poa} W/m² on the plane from {overcast} W/m² horizontal"
        );
    }

    #[test]
    fn a_clear_sky_still_gains_on_the_horizontal_in_winter() {
        let array = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0);
        let noon = slot(datetime!(2026-12-21 11:00:00 UTC));
        let sun = sun_position(BERLIN, noon);
        let ghi = clear_sky_ghi(sun);
        let poa = array.plane_of_array(sun, ghi);
        // A steeply tilted plane against a low sun is the case the tilt exists
        // for: it should be worth more than the horizontal, and not three times
        // more.
        assert!(
            poa > ghi * 1.5 && poa < ghi * 2.6,
            "clear winter noon: {poa} W/m² on the plane from {ghi} W/m² horizontal"
        );
    }

    #[test]
    fn the_three_components_never_exceed_the_light_that_arrived() {
        // Beam plus diffuse is the global value, by construction and after the
        // cap at the top of the atmosphere. A plane lying flat sees exactly it,
        // plus nothing from a ground it cannot see.
        let flat = ArrayModel::new(Power::from_kw(1.0), Power::from_kw(1.0), 0.0, 180.0);
        let start = metering::calendar::day_start_utc(time::macros::date!(2026 - 06 - 21));
        for i in 0..96 {
            let s = Slot::containing(start).offset(i);
            let sun = sun_position(BERLIN, s);
            for factor in [0.05, 0.3, 0.7, 1.0] {
                let ghi = clear_sky_ghi(sun) * factor;
                let poa = flat.plane_of_array(sun, ghi);
                assert!(
                    poa <= ghi + 1e-6,
                    "a horizontal plane saw {poa} W/m² of {ghi} W/m² at slot {i}"
                );
            }
        }
    }

    #[test]
    fn erbs_runs_from_overcast_to_clear() {
        assert!((erbs_diffuse_fraction(0.0) - 1.0).abs() < 1e-12);
        assert!((erbs_diffuse_fraction(1.0) - 0.165).abs() < 1e-12);
        // Monotone downwards across the range, to within the published
        // quartic's own wiggle: it bottoms out at kt = 0,792 and turns up by
        // 7 x 10^-4 before the constant branch takes over at 0,80. That is an
        // artefact of the fit rather than a transcription error, it is what
        // `pvlib` evaluates too, and smoothing it here would make this
        // something other than Erbs for a difference no roof can tell.
        let mut previous = f64::INFINITY;
        for i in 0..=1000 {
            let kt = f64::from(i) / 1000.0;
            let kd = erbs_diffuse_fraction(kt);
            assert!(kd <= previous + 1e-3, "not monotone at kt={kt}");
            previous = kd;
        }
        // And the wiggle is where the correlation puts it, not somewhere a
        // typo would put it.
        assert!(erbs_diffuse_fraction(0.5) > erbs_diffuse_fraction(0.7));
        assert!(erbs_diffuse_fraction(0.22) > erbs_diffuse_fraction(0.5));
    }

    #[test]
    fn the_clearness_index_of_a_clear_german_sky_is_not_one() {
        // The premise the old constant rested on, measured. A clear sky at an
        // air mass of four is genuinely hazier than one overhead, so the
        // clear-sky end of the correlation is not its `kt = 1` end.
        let summer = sun_position(BERLIN, slot(datetime!(2026-06-21 11:00:00 UTC)));
        let winter = sun_position(BERLIN, slot(datetime!(2026-12-21 11:00:00 UTC)));
        let kt_summer = summer.clearness_index(clear_sky_ghi(summer));
        let kt_winter = winter.clearness_index(clear_sky_ghi(winter));
        assert!(
            (0.74..0.82).contains(&kt_summer),
            "midsummer clearness index {kt_summer}"
        );
        assert!(
            (0.56..0.66).contains(&kt_winter),
            "midwinter clearness index {kt_winter}"
        );
        assert!(kt_winter < kt_summer);
    }

    #[test]
    fn the_low_sun_singularity_produces_no_light_from_nowhere() {
        // The failure every clearness-index correlation has at sunrise: the
        // denominator heads for zero and the ratio swings across its range. The
        // elevation floor is what makes it a non-event.
        let array = ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0);
        let start = metering::calendar::day_start_utc(time::macros::date!(2026 - 06 - 21));
        for i in 0..96 {
            let s = Slot::containing(start).offset(i);
            let sun = sun_position(BERLIN, s);
            if !sun.is_up() {
                continue;
            }
            let ghi = clear_sky_ghi(sun);
            let poa = array.plane_of_array(sun, ghi);
            assert!(
                poa.is_finite() && poa <= sun.dni_extra_w_per_m2,
                "slot {i}: {poa} W/m² on the plane at {} degrees",
                sun.elevation_deg
            );
        }
    }
}

#[cfg(test)]
mod inverter_tests {
    use super::*;
    use hems_core::prelude::Power;

    fn array() -> ArrayModel {
        ArrayModel::new(Power::from_kw(10.0), Power::from_kw(10.0), 35.0, 180.0)
    }

    /// The curve, against the values `pvlib.inverter.pvwatts` produces for the
    /// same defaults.
    ///
    /// Pinned rather than merely exercised: this is somebody else's correlation,
    /// and a transcription error in it is invisible in every aggregate figure
    /// this workspace reports — it would simply make the roof a few percent
    /// wrong for ever, which is the error the residual corrector was already
    /// hiding once (D194).
    #[test]
    fn the_part_load_curve_is_the_published_one() {
        // At rated power the curve returns the datasheet figure by construction:
        // `(η_nom/η_ref)·(−0,0162 − 0,0059 + 0,9858)` and the bracket *is* η_ref.
        assert!(
            (inverter_efficiency(1.0, 0.96) - 0.96).abs() < 1e-6,
            "at rated load the curve must return the nominal efficiency, got {}",
            inverter_efficiency(1.0, 0.96)
        );
        for (zeta, expected) in [
            (0.50, 0.9622),
            (0.20, 0.9494),
            (0.10, 0.9216),
            (0.05, 0.8636),
            (0.02, 0.6878),
        ] {
            let got = inverter_efficiency(zeta, 0.96);
            assert!(
                (got - expected).abs() < 5e-4,
                "ζ = {zeta}: expected {expected}, got {got}"
            );
        }
    }

    /// Below its own start-up threshold an inverter yields nothing, and the
    /// threshold is where the published curve crosses zero rather than a second
    /// constant somebody chose.
    #[test]
    fn a_trickle_of_direct_current_does_not_become_alternating_current() {
        assert_eq!(inverter_efficiency(0.004, 0.96), 0.0);
        assert_eq!(inverter_efficiency(0.0, 0.96), 0.0);
        assert_eq!(inverter_efficiency(-1.0, 0.96), 0.0);
        assert_eq!(inverter_efficiency(f64::NAN, 0.96), 0.0);
        // …and the array agrees: a few watts of plane irradiance on a ten
        // kilowatt roof is below the threshold.
        assert_eq!(array().ac_power(0.4, 10.0), Power::ZERO);
    }

    /// The whole point of the change: an alternating current is strictly less
    /// than the direct current behind it, and the gap widens as the light fails.
    #[test]
    fn the_inverter_costs_more_at_dawn_than_at_noon() {
        let a = array();
        let efficiency_at = |poa: f64| {
            let cell_c = 15.0 + poa / 800.0 * 25.0;
            let dc = a.kwp_dc.get()
                * (poa / 1000.0)
                * (1.0 + a.temperature_coefficient * (cell_c - 25.0))
                * (1.0 - a.system_loss);
            a.ac_power(poa, 15.0).outflow().get() / dc
        };
        let noon = efficiency_at(950.0);
        let dawn = efficiency_at(40.0);
        assert!(
            (0.95..=0.97).contains(&noon),
            "a well-lit inverter runs near its datasheet figure, got {noon}"
        );
        assert!(
            dawn < noon - 0.03,
            "the part-load droop must be visible: dawn {dawn}, noon {noon}"
        );
        assert!(dawn > 0.5, "…but a dim roof is not a dead one, got {dawn}");
    }

    /// An oversized array still clips at the inverter's own limit, and the
    /// clipping is on the **alternating-current** side of the efficiency.
    #[test]
    fn an_oversized_array_clips_at_the_inverter_and_not_above_it() {
        let a = ArrayModel::new(Power::from_kw(15.0), Power::from_kw(10.0), 35.0, 180.0);
        let clipped = a.ac_power(1000.0, 25.0).outflow().get();
        assert!(
            clipped <= a.ac_nominal.get() + 1e-9,
            "an inverter cannot put out more than it is rated for: {clipped} W"
        );
        assert!(clipped > 9_000.0, "…and it should be right at the rail");
    }
}
