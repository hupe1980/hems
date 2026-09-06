//! What `forecastd` is told.

use hems_core::prelude::GeoPoint;

/// One place to forecast for.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    /// The name a box asks for it by.
    pub id: String,
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
    /// Metres above sea level.
    #[serde(default)]
    pub altitude_m: f64,
}

impl Location {
    /// The geometry `hems-forecast` wants.
    #[must_use]
    pub const fn point(&self) -> GeoPoint {
        GeoPoint {
            latitude: self.latitude,
            longitude: self.longitude,
            altitude_m: self.altitude_m,
        }
    }
}

/// Everything `forecastd` is configured with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shared daemon settings.
    /// A **table** rather than a flattened set of top-level keys, so every
    /// daemon in this workspace is configured the same way (D160). The
    /// environment override is unaffected — it goes through `AsMut<Settings>`
    /// rather than through the file's shape.
    #[serde(default)]
    pub service: hems_service::Settings,
    /// Where to fetch from.
    ///
    /// The default is Open-Meteo's public endpoint, already carrying
    /// `timeformat=unixtime` — which is not a convenience. Open-Meteo's default
    /// format is a local wall-clock string with no offset, and reading one of
    /// those without knowing the timezone that was asked for is an hour wrong
    /// twice a year. `hems_forecast::open_meteo` refuses it rather than
    /// guessing, so the query has to ask for the other one.
    pub endpoint: String,
    /// The places to forecast for.
    ///
    /// Empty by default, and a `forecastd` with no locations never becomes
    /// ready — the same honesty as a `tariffd` with no sources.
    pub locations: Vec<Location>,
    /// How often to fetch, seconds. ICON-D2 runs eight times a day; an hour is
    /// comfortably inside that and is polite to a free endpoint.
    pub poll_interval_s: u64,
    /// How long one request may take.
    pub request_timeout_s: u64,
    /// The longest the backoff may grow to, seconds.
    pub max_backoff_s: u64,
    /// How many quarter hours ahead must be covered before the service is ready.
    pub ready_slots: usize,
    /// The Model Context Protocol surface, off by default.
    ///
    /// Open like the REST routes when it is switched on: a location's sky is public weather, not a household's data.
    #[serde(default)]
    pub mcp: hems_service::McpSettings,
}

/// Open-Meteo's forecast endpoint, with the variables and the time format this
/// workspace needs.
pub const DEFAULT_ENDPOINT: &str = "https://api.open-meteo.com/v1/forecast\
?timeformat=unixtime\
&minutely_15=shortwave_radiation,temperature_2m,cloud_cover\
&forecast_days=3\
&models=icon_d2";

impl Default for Settings {
    fn default() -> Self {
        Self {
            service: hems_service::Settings::default(),
            endpoint: DEFAULT_ENDPOINT.into(),
            locations: Vec::new(),
            poll_interval_s: 3600,
            request_timeout_s: 20,
            max_backoff_s: 3600 * 4,
            ready_slots: 96,
            mcp: hems_service::McpSettings::default(),
        }
    }
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}

#[cfg(test)]
mod example_tests {
    use super::*;

    /// The example file that ships with the daemon.
    ///
    /// Parsed by a test rather than trusted: a commented example that has
    /// drifted from the struct it documents is worse than none, because it is
    /// read by whoever is deploying this and every line of it looks
    /// authoritative. `include_str!` makes it a build input, and
    /// `cargo xtask check-examples` fails the build on a daemon that ships
    /// neither.
    const EXAMPLE: &str = include_str!("../forecastd.example.toml");

    #[test]
    fn the_example_configuration_parses_and_describes_a_service() {
        let settings: Settings = toml::from_str(EXAMPLE).expect("the shipped example parses");
        assert!(
            !settings.locations.is_empty(),
            "a `forecastd` with no location never becomes ready, so an example \
             with none would document a service that cannot start usefully"
        );
        assert!(
            settings.endpoint.contains("timeformat=unixtime"),
            "the parser refuses Open-Meteo's default wall-clock format rather \
             than guessing a timezone, so the query has to ask for the other one"
        );
    }
}
