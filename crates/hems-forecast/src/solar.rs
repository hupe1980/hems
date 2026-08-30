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
//! the equation of time, clear-sky global irradiance after Haurwitz, an
//! isotropic transposition onto the module plane, the usual cell-temperature
//! correction, and the inverter's clipping limit.

use hems_core::prelude::{GeoPoint, Power, Slot};

/// Where the sun is, seen from one place at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunPosition {
    /// Degrees above the horizon. Negative when the sun is down.
    pub elevation_deg: f64,
    /// Degrees clockwise from north; 180 is due south.
    pub azimuth_deg: f64,
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
    pub system_loss: f64,
    /// Relative power change per kelvin of cell temperature above 25 °C.
    /// Negative; −0,004 is typical for silicon.
    pub temperature_coefficient: f64,
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
        }
    }

    /// Irradiance on the module plane, W/m², from the global horizontal value.
    ///
    /// An isotropic sky: the direct component is projected onto the plane, the
    /// diffuse component is taken as a view factor of the sky, and a modest
    /// ground reflection is added. Good enough that the error is dominated by
    /// the cloud forecast rather than by this.
    #[must_use]
    pub fn plane_of_array(&self, sun: SunPosition, ghi: f64) -> f64 {
        if ghi <= 0.0 || !sun.is_up() {
            return 0.0;
        }
        // Erbs' correlation splits global into direct and diffuse via the
        // clearness index; at the clear-sky reference the ratio is stable
        // enough to take a fixed split.
        let diffuse_fraction = 0.25;
        let dhi = ghi * diffuse_fraction;
        let bhi = ghi - dhi;

        let tilt = self.tilt_deg.to_radians();
        let sun_el = sun.elevation_deg.to_radians();
        let delta_azimuth = (sun.azimuth_deg - self.azimuth_deg).to_radians();

        // Angle of incidence on the tilted plane.
        let cos_incidence =
            sun_el.sin() * tilt.cos() + sun_el.cos() * tilt.sin() * delta_azimuth.cos();
        let beam = if cos_incidence > 0.0 && sun_el.sin() > 1e-6 {
            bhi * cos_incidence / sun_el.sin()
        } else {
            0.0
        };

        let sky_view = f64::midpoint(1.0, tilt.cos());
        let ground_view = f64::midpoint(1.0, -tilt.cos());
        let albedo = 0.2;

        beam + dhi * sky_view + ghi * albedo * ground_view
    }

    /// Alternating-current power for a given plane irradiance and air
    /// temperature, as a **negative** value in the load convention.
    #[must_use]
    pub fn ac_power(&self, poa: f64, ambient_c: f64) -> Power {
        if poa <= 0.0 {
            return Power::ZERO;
        }
        // Nominal operating cell temperature model: the cell runs about 25 K
        // above ambient at full sun.
        let cell_c = ambient_c + poa / 800.0 * 25.0;
        let temperature_factor = 1.0 + self.temperature_coefficient * (cell_c - 25.0);
        let dc = self.kwp_dc.get() * (poa / 1000.0) * temperature_factor * (1.0 - self.system_loss);
        // The inverter clips, which is why an oversized array is not wasted.
        -Power::new(dc.max(0.0).min(self.ac_nominal.get()))
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
