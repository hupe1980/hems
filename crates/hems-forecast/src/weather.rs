//! What somebody else's model says the sky will do.
//!
//! [`crate::solar`] computes what an array would produce under a **clear** sky
//! from geometry alone, and [`crate::residual`] corrects that with what this
//! roof has actually been delivering. Between them sits the one thing neither
//! can know: whether tomorrow is cloudy.
//!
//! Open-Meteo publishes ICON-D2 — the German weather service's own 2 km model —
//! free, without a key, at quarter-hour resolution over central Europe. This is
//! the parser for it: a `&str` in, a series of irradiance and temperature out,
//! no socket, no clock. The fetching is `forecastd`'s.
//!
//! # Ask for `timeformat=unixtime`, and refuse anything else
//!
//! Open-Meteo's default time format is `2026-06-21T13:00` — a local wall-clock
//! string with **no offset**, in the timezone the query asked for. Parsing that
//! correctly means knowing which timezone was asked for, and getting it wrong is
//! invisible for ten months of the year and an hour out for the other two. It is
//! the same trap `metering`'s DST calendar exists to close, arriving from
//! outside.
//!
//! So this parser reads **integers** and nothing else, and a body that carries
//! strings is refused rather than guessed at. The request is one query parameter
//! longer and a whole class of error stops existing.
//!
//! # Quarter hours where the model has them, held where it does not
//!
//! `minutely_15` is ICON-D2's own grid and matches the planner's exactly.
//! `hourly` is the fallback — for a longer horizon, or a model that has no
//! quarter hours — and one hourly value becomes four identical quarter hours
//! rather than four interpolated ones. Interpolating would invent a sunrise that
//! climbs smoothly through the hour it is published for; holding says what the
//! number actually is, which is an average over that hour, and it is the same
//! zero-order hold the planner already assumes about every other input it has.

use hems_core::prelude::Slot;
use thiserror::Error;
use time::OffsetDateTime;

/// Why a weather body could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WeatherError {
    /// The body is not JSON, or not the shape Open-Meteo publishes.
    #[error("the weather body is not the expected shape: {0}")]
    Malformed(String),
    /// A time array carries strings rather than Unix seconds.
    #[error(
        "the times are formatted rather than Unix seconds — request \
         `timeformat=unixtime`, because a local wall-clock string with no offset \
         is an hour wrong twice a year and right the rest of the time"
    )]
    NotUnixTime,
    /// The arrays do not line up.
    #[error("the {field} array has {found} entries against {expected} times")]
    Ragged {
        /// Which array.
        field: &'static str,
        /// How long it is.
        found: usize,
        /// How long the time array is.
        expected: usize,
    },
    /// The body has no usable block at all.
    #[error("the body carries neither `minutely_15` nor `hourly` irradiance")]
    Empty,
}

/// One quarter hour of weather.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WeatherPoint {
    /// Global horizontal irradiance, W/m² — the input
    /// [`crate::ArrayModel::plane_of_array`] wants.
    pub ghi_w_per_m2: f64,
    /// Air temperature at two metres, °C. Drives the heat loss, the coefficient
    /// of performance and the cell temperature.
    pub temperature_c: f64,
    /// Total cloud cover in `[0, 1]`, where the model publishes one.
    ///
    /// Not used to compute anything — the irradiance already carries the cloud —
    /// but worth keeping: a forecast whose irradiance is high and whose cloud
    /// cover is total is a body worth logging rather than trusting.
    pub cloud_cover: Option<f64>,
}

/// A weather forecast over a run of quarter hours.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct WeatherSeries {
    /// One entry per quarter hour, in order.
    pub slots: Vec<(Slot, WeatherPoint)>,
    /// The resolution the model published at, minutes — 15 or 60.
    ///
    /// Kept for the same reason [`crate::quantile::Band`] keeps its width: a
    /// household told the afternoon is bright should not be shown four
    /// independent quarter hours when the model produced one hour.
    pub published_minutes: u16,
}

impl WeatherSeries {
    /// The weather in one quarter hour.
    #[must_use]
    pub fn at(&self, slot: Slot) -> Option<WeatherPoint> {
        self.slots
            .binary_search_by(|(s, _)| s.cmp(&slot))
            .ok()
            .map(|i| self.slots[i].1)
    }

    /// How many quarter hours it covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether it covers none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// What `array` would produce in each slot, as a **positive** magnitude in
    /// watts.
    ///
    /// The geometry is [`crate::solar`]'s and the sky is the model's; what comes
    /// out is a *modelled* production, which is the input
    /// [`crate::residual::ResidualModel`] corrects into a forecast. It is
    /// deliberately not a forecast on its own: a number straight out of a
    /// weather model knows nothing about the tree that shades the east string.
    #[must_use]
    pub fn modelled_production(
        &self,
        array: &crate::ArrayModel,
        location: hems_core::prelude::GeoPoint,
    ) -> Vec<(Slot, f64)> {
        self.slots
            .iter()
            .map(|(slot, point)| {
                let sun = crate::solar::sun_position(location, *slot);
                let poa = array.plane_of_array(sun, point.ghi_w_per_m2);
                let power = array.ac_power(poa, point.temperature_c);
                (*slot, power.outflow().get())
            })
            .collect()
    }

    /// The outdoor temperature in each slot, for the planner's thermal model.
    #[must_use]
    pub fn outdoor_c(&self) -> Vec<f64> {
        self.slots.iter().map(|(_, p)| p.temperature_c).collect()
    }
}

/// Parse an Open-Meteo forecast body.
///
/// Prefers `minutely_15` and falls back to `hourly`. The request must have been
/// made with `timeformat=unixtime`; see the module note for why that is a
/// refusal rather than a guess.
///
/// # Errors
/// [`WeatherError`] when the body is not the expected shape, when the times are
/// formatted strings, when the arrays do not line up, or when neither block
/// carries irradiance.
pub fn open_meteo(json: &str) -> Result<WeatherSeries, WeatherError> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| WeatherError::Malformed(e.to_string()))?;

    for (block, minutes) in [("minutely_15", 15_u16), ("hourly", 60)] {
        let Some(section) = root.get(block) else {
            continue;
        };
        match block_to_series(section, minutes) {
            Ok(series) if !series.is_empty() => return Ok(series),
            // A block that is present and unusable is an error worth reporting
            // rather than a reason to try the coarser one: a `minutely_15` whose
            // arrays do not line up is a bug in the request, and silently
            // falling back to `hourly` would hide it.
            Err(e @ (WeatherError::NotUnixTime | WeatherError::Ragged { .. })) => return Err(e),
            _ => {}
        }
    }
    Err(WeatherError::Empty)
}

/// One `minutely_15` or `hourly` block.
fn block_to_series(
    section: &serde_json::Value,
    minutes: u16,
) -> Result<WeatherSeries, WeatherError> {
    let times = section
        .get("time")
        .and_then(|v| v.as_array())
        .ok_or_else(|| WeatherError::Malformed("no `time` array".into()))?;
    if times.iter().any(serde_json::Value::is_string) {
        return Err(WeatherError::NotUnixTime);
    }
    let seconds: Vec<i64> = times.iter().filter_map(serde_json::Value::as_i64).collect();
    if seconds.len() != times.len() {
        return Err(WeatherError::Malformed(
            "a time is neither an integer nor a string".into(),
        ));
    }

    let column = |name: &'static str| -> Result<Option<Vec<Option<f64>>>, WeatherError> {
        let Some(array) = section.get(name).and_then(|v| v.as_array()) else {
            return Ok(None);
        };
        if array.len() != times.len() {
            return Err(WeatherError::Ragged {
                field: name,
                found: array.len(),
                expected: times.len(),
            });
        }
        Ok(Some(array.iter().map(serde_json::Value::as_f64).collect()))
    };

    let Some(ghi) = column("shortwave_radiation")? else {
        return Ok(WeatherSeries::default());
    };
    let temperature = column("temperature_2m")?;
    let cloud = column("cloud_cover")?;

    let repeats = usize::from(minutes / 15).max(1);
    let mut slots = Vec::with_capacity(seconds.len() * repeats);
    for (i, unix) in seconds.iter().enumerate() {
        let Ok(instant) = OffsetDateTime::from_unix_timestamp(*unix) else {
            return Err(WeatherError::Malformed(format!("{unix} is not an instant")));
        };
        // A model value that is `null` — beyond the run's horizon, or a variable
        // the query did not ask for — is a slot with no weather rather than a
        // slot with zero weather. Zero irradiance is a real forecast and a
        // missing one is not.
        let Some(ghi) = ghi[i] else { continue };
        let point = WeatherPoint {
            ghi_w_per_m2: ghi.max(0.0),
            temperature_c: temperature
                .as_ref()
                .and_then(|t| t[i])
                .unwrap_or(DEFAULT_TEMPERATURE_C),
            cloud_cover: cloud
                .as_ref()
                .and_then(|c| c[i])
                .map(|percent| (percent / 100.0).clamp(0.0, 1.0)),
        };
        let first = Slot::containing(instant);
        for step in 0..repeats {
            slots.push((first.offset(i64::try_from(step).unwrap_or(0)), point));
        }
    }

    Ok(WeatherSeries {
        slots,
        published_minutes: minutes,
    })
}

/// What to assume where a model publishes irradiance and no temperature.
///
/// Mild enough that a plan made without it is neither optimistic about the
/// coefficient of performance nor panicked about the heat loss — the same
/// number and the same argument as `Problem::outdoor_at`.
const DEFAULT_TEMPERATURE_C: f64 = 10.0;

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOON: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    fn minutely(values: &[(f64, f64)]) -> String {
        let times: Vec<String> = (0..values.len())
            .map(|i| (NOON.unix_timestamp() + i64::try_from(i).unwrap() * 900).to_string())
            .collect();
        let ghi: Vec<String> = values.iter().map(|(g, _)| g.to_string()).collect();
        let temp: Vec<String> = values.iter().map(|(_, t)| t.to_string()).collect();
        format!(
            r#"{{"minutely_15":{{"time":[{}],"shortwave_radiation":[{}],"temperature_2m":[{}]}}}}"#,
            times.join(","),
            ghi.join(","),
            temp.join(",")
        )
    }

    #[test]
    fn a_quarter_hourly_body_becomes_one_point_per_slot() {
        let series = open_meteo(&minutely(&[(800.0, 24.0), (750.0, 24.5)])).unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series.published_minutes, 15);
        let point = series.at(Slot::containing(NOON)).unwrap();
        assert!((point.ghi_w_per_m2 - 800.0).abs() < 1e-9);
        assert!((point.temperature_c - 24.0).abs() < 1e-9);
    }

    #[test]
    fn an_hourly_body_is_held_across_its_four_quarter_hours() {
        // Held, not interpolated: the published number is an average over the
        // hour, and interpolating would invent a sunrise that climbs smoothly
        // through it.
        let body = format!(
            r#"{{"hourly":{{"time":[{},{}],"shortwave_radiation":[400,500],"temperature_2m":[18,19]}}}}"#,
            NOON.unix_timestamp(),
            NOON.unix_timestamp() + 3600
        );
        let series = open_meteo(&body).unwrap();
        assert_eq!(series.len(), 8);
        assert_eq!(series.published_minutes, 60);
        for step in 0..4 {
            let point = series.at(Slot::containing(NOON).offset(step)).unwrap();
            assert!((point.ghi_w_per_m2 - 400.0).abs() < 1e-9, "slot {step}");
        }
        assert!(
            (series
                .at(Slot::containing(NOON).offset(4))
                .unwrap()
                .ghi_w_per_m2
                - 500.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn quarter_hours_win_over_hours_where_both_are_published() {
        let body = format!(
            r#"{{"minutely_15":{{"time":[{}],"shortwave_radiation":[800],"temperature_2m":[24]}},
                 "hourly":{{"time":[{}],"shortwave_radiation":[400],"temperature_2m":[18]}}}}"#,
            NOON.unix_timestamp(),
            NOON.unix_timestamp()
        );
        let series = open_meteo(&body).unwrap();
        assert_eq!(series.published_minutes, 15);
        assert_eq!(series.len(), 1);
    }

    #[test]
    fn a_formatted_time_is_refused_rather_than_guessed_at() {
        // `2026-06-21T13:00` with no offset is an hour wrong twice a year and
        // right the rest of the time, which is the worst kind of wrong.
        let body = r#"{"hourly":{"time":["2026-06-21T13:00"],"shortwave_radiation":[400]}}"#;
        assert_eq!(open_meteo(body), Err(WeatherError::NotUnixTime));
    }

    #[test]
    fn a_ragged_body_is_an_error_and_not_a_short_series() {
        let body = format!(
            r#"{{"hourly":{{"time":[{},{}],"shortwave_radiation":[400]}}}}"#,
            NOON.unix_timestamp(),
            NOON.unix_timestamp() + 3600
        );
        assert!(matches!(
            open_meteo(&body),
            Err(WeatherError::Ragged {
                field: "shortwave_radiation",
                ..
            })
        ));
    }

    #[test]
    fn a_broken_fine_block_is_not_papered_over_by_the_coarse_one() {
        // A `minutely_15` whose arrays do not line up is a bug in the request.
        // Falling back to `hourly` would hide it and quietly halve the
        // resolution of every plan.
        let body = format!(
            r#"{{"minutely_15":{{"time":[{},{}],"shortwave_radiation":[800]}},
                 "hourly":{{"time":[{}],"shortwave_radiation":[400]}}}}"#,
            NOON.unix_timestamp(),
            NOON.unix_timestamp() + 900,
            NOON.unix_timestamp()
        );
        assert!(matches!(
            open_meteo(&body),
            Err(WeatherError::Ragged { .. })
        ));
    }

    #[test]
    fn a_null_value_is_a_slot_with_no_weather_rather_than_a_dark_one() {
        // Zero irradiance is a real forecast; a missing one is not, and reading
        // the second as the first is how a plan defers everything into a
        // midnight it believes is free.
        let body = format!(
            r#"{{"minutely_15":{{"time":[{},{}],"shortwave_radiation":[800,null],"temperature_2m":[24,24]}}}}"#,
            NOON.unix_timestamp(),
            NOON.unix_timestamp() + 900
        );
        let series = open_meteo(&body).unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series.at(Slot::containing(NOON).offset(1)), None);
    }

    #[test]
    fn a_body_with_no_irradiance_at_all_says_so() {
        let body = r#"{"latitude":52.5,"longitude":13.4}"#;
        assert_eq!(open_meteo(body), Err(WeatherError::Empty));
        assert!(matches!(
            open_meteo("not json"),
            Err(WeatherError::Malformed(_))
        ));
    }

    #[test]
    fn cloud_cover_arrives_as_a_fraction() {
        let body = format!(
            r#"{{"minutely_15":{{"time":[{}],"shortwave_radiation":[300],"cloud_cover":[85]}}}}"#,
            NOON.unix_timestamp()
        );
        let point = open_meteo(&body)
            .unwrap()
            .at(Slot::containing(NOON))
            .unwrap();
        assert!((point.cloud_cover.unwrap() - 0.85).abs() < 1e-9);
        assert!(
            (point.temperature_c - DEFAULT_TEMPERATURE_C).abs() < 1e-9,
            "and a missing temperature is mild rather than freezing"
        );
    }

    #[test]
    fn a_modelled_production_comes_out_of_the_geometry_and_the_sky() {
        // The seam this module exists to close: somebody else's irradiance
        // through this workspace's own array model.
        use hems_core::prelude::{GeoPoint, Power};
        let series = open_meteo(&minutely(&[(850.0, 25.0), (0.0, 12.0)])).unwrap();
        let array = crate::ArrayModel::new(Power::from_kw(9.8), Power::from_kw(8.0), 35.0, 180.0);
        let berlin = GeoPoint {
            latitude: 52.5,
            longitude: 13.4,
            altitude_m: 34.0,
        };
        let production = series.modelled_production(&array, berlin);
        assert_eq!(production.len(), 2);
        assert!(production[0].1 > 3000.0, "midsummer noon: {production:?}");
        assert_eq!(production[1].1, 0.0, "no irradiance, no production");
        assert_eq!(series.outdoor_c(), vec![25.0, 12.0]);
    }
}
