//! What `agentd` is configured with.

use std::path::PathBuf;

/// Everything `agentd` is configured with.
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
    /// Where the journal lives.
    ///
    /// An embedded file for one instance. It is the **plan of record**: a run,
    /// its input, its answer and every effect, append-only and hash-chained, so
    /// "why did the queue say that in March" is a replay rather than an
    /// argument. On an ephemeral filesystem it is a log that answers nothing.
    pub journal: PathBuf,
    /// Which tenant's households the specialists may read.
    ///
    /// The operator this daemon acts for; every specialist runs under an
    /// authority **attenuated** from it, which cannot widen (D118). `"*"` is
    /// right for a single-tenant deployment and is a cross-tenant read in any
    /// other, which is why it is written down rather than being what happens
    /// when a field is missing (D112).
    pub tenant: String,
    /// Which households each tenant covers.
    #[serde(default)]
    pub tenants: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// Where the fleet's days are read from.
    ///
    /// `obsd`'s base URL. Its **API** rather than its database: the scope
    /// predicate that keeps one tenant's rows out of another tenant's answer
    /// lives in `obsd`'s store (D112), and a second reader with its own `SELECT`
    /// is a second place for that rule to be got wrong.
    pub obsd: String,
    /// The credential this daemon presents to `obsd`, or an `env:`/`file:`
    /// reference to it (D82).
    ///
    /// It has to hold `hems.fleet.read` at that end: a specialist's finding is
    /// about a population, and a box's own token reaches one household.
    ///
    /// `None` stops the daemon rather than starting one that reads nothing —
    /// a plane whose every review is refused looks, from the queue, exactly like
    /// a fleet in good order.
    #[serde(default)]
    pub obsd_token: Option<hems_service::Secret>,
    /// How long to wait for `obsd`, seconds.
    ///
    /// A whole window of a fleet is a large document, and this is a background
    /// review rather than a request somebody is waiting on — so it is generous
    /// where a user-facing timeout would not be.
    pub obsd_timeout_s: u64,
    /// How often a review runs, seconds.
    ///
    /// Six hours. The specialists answer questions about a **window**, so a
    /// cadence far shorter than the window produces the same answer repeatedly
    /// at the cost of a fleet-sized read each time; one far longer means a
    /// § 14a pattern waits a day to be seen. Boxes report once a day, so
    /// anything under an hour is re-reading the same days.
    pub review_every_s: u64,
    /// The tokens that may **read** the advisory queue.
    ///
    /// The queue names households that did not respect a network operator's
    /// reduction, so it needs `hems.fleet.read` — the same credential model
    /// `obsd` and `histd` use, and empty means nothing is served.
    #[serde(default)]
    pub operators: Vec<hems_service::OperatorCredential>,
    /// The Model Context Protocol surface, off by default.
    ///
    /// It speaks about households, so a token is **required** when it is
    /// switched on and every call is authorised as whatever that token already
    /// carries here.
    #[serde(default)]
    pub mcp: hems_service::McpSettings,
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            service: hems_service::Settings::default(),
            journal: PathBuf::from("agentd-journal.redb"),
            tenant: hems_service::auth::EVERY_TENANT.to_owned(),
            tenants: std::collections::BTreeMap::new(),
            obsd: "http://127.0.0.1:7780".to_owned(),
            obsd_token: None,
            obsd_timeout_s: 30,
            review_every_s: 6 * 3_600,
            operators: Vec::new(),
            mcp: hems_service::McpSettings::default(),
        }
    }
}
