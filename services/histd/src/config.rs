//! What `histd` is told.

use std::path::PathBuf;

/// Everything `histd` is configured with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shared daemon settings.
    #[serde(flatten)]
    pub service: hems_service::Settings,
    /// One bearer token per site — the credential its box presents.
    ///
    /// A box may read and write **its own** record and no other. Each is a
    /// [`hems_service::Secret`], so the usual deployment writes
    /// `haus-1 = "env:HEMS_HISTD_TOKEN_HAUS1"` and no credential enters the
    /// configuration file.
    ///
    /// Empty means this service accepts **nothing**, which is the safe reading
    /// of "nobody configured it": what these routes serve is a household's whole
    /// consumption record and the evidence a network operator settles on.
    #[serde(default)]
    pub site_tokens: std::collections::BTreeMap<String, hems_service::Secret>,
    /// Tokens that may read **any** site's § 14a evidence.
    ///
    /// A network operator checking a reduction it commanded, or an internal
    /// service building a portfolio view. They may not write — an operator that
    /// could write the record of its own control actions is marking its own
    /// homework — and they may not read the Data Act export, which is the
    /// household's under Article 4 and not theirs.
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
    /// Where the database lives.
    ///
    /// `:memory:` is honoured and is what the tests use. A box uses a path on
    /// its own flash, and a fleet deployment points this at a volume — because a
    /// two-year record on an ephemeral filesystem is a two-hour record.
    pub database: PathBuf,
    /// How often to delete what has aged out, seconds.
    ///
    /// Daily. `[A1 7.3]`'s two years are not a number anybody is racing, and a
    /// gateway box has better things to do than sweep a table every minute.
    pub retention_sweep_s: u64,
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
            site_tokens: std::collections::BTreeMap::new(),
            tenants: std::collections::BTreeMap::new(),
            operators: Vec::new(),
            database: PathBuf::from("hems-history.sqlite"),
            retention_sweep_s: 24 * 3600,
            mcp: hems_service::McpSettings::default(),
        }
    }
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}
