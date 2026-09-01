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
    /// The secrets a box's report may be signed with, D11.
    ///
    /// A **list** rather than one, because rotation is the reason a signature
    /// scheme has a version prefix: an operator publishes a new secret, both are
    /// accepted for a window, the old one is withdrawn. Order does not matter —
    /// [`hems_events::webhook::verify`] tries all of them.
    ///
    /// Empty means the service accepts **nothing**, and that is deliberate. An
    /// unconfigured receiver that accepted unsigned reports would be an open
    /// write endpoint onto the list of households that did not respect a network
    /// operator's reduction, and the deployment where somebody forgot the
    /// environment variable is exactly the one nobody would notice.
    ///
    /// Each is a [`hems_service::Secret`], so the usual deployment writes
    /// `["env:HEMS_OBSD_WEBHOOK_SECRET"]` or `["file:/run/secrets/webhook"]`
    /// and the credential never enters the configuration file at all.
    pub webhook_secrets: Vec<hems_service::Secret>,
    /// Tokens that may **read** the fleet view.
    ///
    /// `/v1/fleet` and `/v1/sites/{site}` carry what every household spent,
    /// saved and drew, and the named list of those that did not respect a
    /// network operator's reduction. Writing is authenticated by a signature
    /// (D80); reading is a different question with a different caller — a
    /// person or an internal service — and needs its own credential.
    ///
    /// Empty means nothing is served, for the same reason an empty
    /// [`Settings::webhook_secrets`] accepts nothing.
    #[serde(default)]
    pub operator_tokens: Vec<hems_service::Secret>,
    /// How far a report's signing timestamp may be from this service's clock,
    /// seconds.
    ///
    /// The replay bound rather than a clock-skew allowance: a captured request
    /// stops working once it is outside this window.
    pub webhook_tolerance_s: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            service: hems_service::Settings::default(),
            keep_days: 60,
            silent_after_s: 2 * 24 * 3600,
            webhook_secrets: Vec::new(),
            operator_tokens: Vec::new(),
            webhook_tolerance_s: hems_events::webhook::DEFAULT_TOLERANCE
                .whole_seconds()
                .unsigned_abs(),
        }
    }
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}
