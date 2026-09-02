//! What `obsd` is told.

/// Everything `obsd` is configured with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shared daemon settings.
    #[serde(flatten)]
    pub service: hems_service::Settings,
    /// How many days of reports to keep per site.
    ///
    /// Sixty: two months is long enough for a calibration figure to be
    /// answerable ([`hems_forecast::CALIBRATION_DAYS`] is twenty) and short
    /// enough that a fleet of ten thousand sites is a few tens of megabytes.
    /// The *record* is `histd`'s; this is a window.
    pub keep_days: usize,
    /// How long after a site's last report it counts as silent, seconds.
    ///
    /// A box reports once a day, so two days is a box that has missed one and
    /// then missed another — which is a fault rather than a late night.
    pub silent_after_s: u64,
    /// The secret each household's box signs its day report with, by site.
    ///
    /// **One key per box, not one for the fleet.** A signature proves the bytes
    /// were not edited; it proves *who sent them* only if the sender holds a key
    /// nobody else does. With a shared secret any box could report a day
    /// attributed to any site — and what that writes to is `breached`, the named
    /// list of households that did not respect a network operator's reduction
    /// (D114).
    ///
    /// The list per site is rotation, which is the reason a signature scheme has
    /// a version prefix: an operator publishes a new secret, both are accepted
    /// for a window, the old one is withdrawn. Order does not matter —
    /// [`hems_events::webhook::verify`] tries all of them without
    /// short-circuiting, so how long a check takes says nothing about which one
    /// is live.
    ///
    /// Empty means the service accepts **nothing**, and that is deliberate. An
    /// unconfigured receiver that accepted unsigned reports would be an open
    /// write endpoint onto that same list, and the deployment where somebody
    /// forgot the environment variable is exactly the one nobody would notice.
    ///
    /// Each is a [`hems_service::Secret`], so the usual deployment writes
    /// `haus-1 = ["env:HEMS_OBSD_SECRET_HAUS_1"]` and the credential never
    /// enters the configuration file at all.
    pub webhook_secrets: std::collections::BTreeMap<String, Vec<hems_service::Secret>>,
    /// Tokens that may **read** the fleet view.
    ///
    /// `/v1/fleet` and `/v1/sites/{site}` carry what every household spent,
    /// saved and drew, and the named list of those that did not respect a
    /// network operator's reduction. Writing is authenticated by a signature;
    /// reading is a different question with a different caller — a
    /// person or an internal service — and needs its own credential.
    ///
    /// Empty means nothing is served, for the same reason an empty
    /// [`Settings::webhook_secrets`] accepts nothing.
    ///
    /// Which households each tenant covers.
    ///
    /// A shared deployment hosting several operators names each one's
    /// households here, and an operator credential names the tenant it belongs
    /// to. A single-tenant deployment leaves this empty and writes
    /// `tenant = "*"` on its credential, which is the same reach stated out
    /// loud rather than arrived at by omission (D112).
    #[serde(default)]
    pub tenants: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The operator credentials, each scoped to a tenant.
    #[serde(default)]
    pub operators: Vec<hems_service::OperatorCredential>,
    /// How far a report's signing timestamp may be from this service's clock,
    /// seconds.
    ///
    /// The replay bound rather than a clock-skew allowance: a captured request
    /// stops working once it is outside this window.
    pub webhook_tolerance_s: u64,
    /// The Model Context Protocol surface, off by default.
    ///
    /// It holds a household's data, so a token is **required** when it is
    /// switched on and the surface answers as whatever authority that token
    /// already carries here — an operator, or one site.
    #[serde(default)]
    pub mcp: hems_service::McpSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            service: hems_service::Settings::default(),
            keep_days: 60,
            silent_after_s: 2 * 24 * 3600,
            webhook_secrets: std::collections::BTreeMap::new(),
            tenants: std::collections::BTreeMap::new(),
            operators: Vec::new(),
            webhook_tolerance_s: hems_events::webhook::DEFAULT_TOLERANCE
                .whole_seconds()
                .unsigned_abs(),
            mcp: hems_service::McpSettings::default(),
        }
    }
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}
