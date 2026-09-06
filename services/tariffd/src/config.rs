//! What `tariffd` is told.

use std::collections::BTreeMap;

use hems_tariff::source::Source;

/// One source's endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// The URL to fetch.
    pub url: String,
    /// Headers to send — an ENTSO-E `securityToken`, a Tibber bearer.
    ///
    /// Out of the code and out of the logs: a token is a credential and a URL
    /// with one in the query string ends up in somebody's access log. Each value
    /// is a [`hems_service::Secret`], so the usual deployment writes
    /// `"env:ENTSOE_TOKEN"` and the token never enters the configuration file
    /// either.
    #[serde(default)]
    pub headers: BTreeMap<String, hems_service::Secret>,
}

/// Everything `tariffd` is configured with.
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
    /// Which sources to ask, and where.
    ///
    /// Empty by default, and that is the honest default: a `tariffd` nobody has
    /// given an endpoint to has nothing to fetch, reports itself **not ready**,
    /// and says which source it is missing — rather than coming up green and
    /// serving an empty cache.
    pub sources: BTreeMap<Source, Endpoint>,
    /// How often to ask each source, seconds.
    ///
    /// The day-ahead auction clears once a day, so this is not about resolution:
    /// it is about how quickly the box has tomorrow's curve after it is
    /// published, and how quickly it recovers from a failed fetch. A quarter of
    /// an hour costs a free API ninety-six requests a day, which is polite.
    pub poll_interval_s: u64,
    /// How long one request may take.
    pub request_timeout_s: u64,
    /// The longest the backoff may grow to after repeated failures, seconds.
    ///
    /// A fleet of boxes retrying a failed public API every fifteen seconds is a
    /// denial of service against somebody who is giving the data away.
    pub max_backoff_s: u64,
    /// The curated Modul 3 calendars, one per network operator and year.
    ///
    /// There is no machine-readable national format for a Zählzeitdefinition —
    /// a PDF or an Excel sheet per network operator — so somebody transcribes
    /// each one, once per Netzgebiet rather than once per household, and this
    /// is where the fleet keeps them. The shape of each calendar is
    /// [`hems_grid::modul3::Transcription`], the same one a box takes as
    /// `[tariff.modul3]`, so a transcription is portable between the two.
    ///
    /// Every entry is checked against the BDEW Anwendungshilfe at start-up and
    /// a violation **refuses to start** the daemon: a fleet serving windows
    /// nobody may sell is a whole Netzgebiet of households priced against a
    /// tariff nobody may be billed on, which is worse than one box (D126). A
    /// `source` is required for the same reason it is under `run --check` —
    /// when a household queries a bill, the first question is which document
    /// said so.
    #[serde(default)]
    pub modul3: Vec<Modul3Entry>,
    /// The Model Context Protocol surface, off by default.
    ///
    /// Open like the REST routes when it is switched on: a day-ahead auction
    /// result is a published figure, not a household's data. What it costs is
    /// the operator's own upstream quota, which is a rate-limiting question.
    #[serde(default)]
    pub mcp: hems_service::McpSettings,
    /// How many slots the readiness probe requires the cache to cover.
    ///
    /// Ninety-six is one day: a box asking for a 24-hour horizon can be answered
    /// entirely from cache, which is what "ready" should mean for a price
    /// service.
    pub ready_slots: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            service: hems_service::Settings::default(),
            sources: BTreeMap::new(),
            poll_interval_s: 900,
            request_timeout_s: 20,
            max_backoff_s: 3600,
            modul3: Vec::new(),
            mcp: hems_service::McpSettings::default(),
            ready_slots: 96,
        }
    }
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}

/// One network operator's calendar in the curated catalogue.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Modul3Entry {
    /// The network operator this calendar belongs to — its BDEW-Codenummer,
    /// which is what a box's site configuration already names.
    pub netzbetreiber: String,
    /// The calendar, as transcribed from the operator's price sheet.
    pub calendar: hems_grid::modul3::Transcription,
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
    const EXAMPLE: &str = include_str!("../tariffd.example.toml");

    #[test]
    fn the_example_configuration_parses_and_describes_a_service() {
        let settings: Settings = toml::from_str(EXAMPLE).expect("the shipped example parses");
        assert!(
            !settings.sources.is_empty(),
            "a `tariffd` with no source never becomes ready, so an example with \
             none would document a service that cannot start usefully"
        );
    }
}
