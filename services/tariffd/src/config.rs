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
    #[serde(flatten)]
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
