//! What `fleetd` is told.

use std::collections::BTreeMap;

/// One site the fleet expects to see.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteEntry {
    /// The single-use secret an installer put on the box.
    ///
    /// Not in the code and never logged: it is a credential, and it stops being
    /// one the moment the box has enrolled. A [`hems_service::Secret`], so a
    /// fleet can write `"env:SITE_HAUS1_SECRET"` or
    /// `"file:/run/secrets/haus-1"` and keep it out of the file entirely.
    pub enrolment_secret: hems_service::Secret,
    /// The configuration document this site should be running, as TOML.
    ///
    /// Opaque here on purpose. `fleetd` distributes configuration and does not
    /// interpret it: a fleet service that validated a `hemsd` site model would
    /// be a fleet service that has to be upgraded before a box can be.
    #[serde(default)]
    pub config: String,
    /// The version of that document. Any string; the box reports it back.
    #[serde(default)]
    pub config_version: String,
    /// The Ed25519 signature over `(site, version, config)`, hexadecimal.
    ///
    /// Produced **elsewhere**, like a release's: `fleetd` holds signatures and
    /// never a signing key, so a `fleetd` an attacker owns can serve no
    /// configuration any box will accept. That is the whole of what "the fleet
    /// is not the trust anchor" means, and without this it was a claim about the
    /// update channel alone — while a configuration decides which assets a site
    /// has, what its comfort band is, and where it reports.
    ///
    /// Empty is allowed and means **this site's configuration is unsigned**, so
    /// `/v1/config` refuses to serve it. An operator who has not signed a
    /// document has not published it.
    #[serde(default)]
    pub config_signature: String,
}

/// Everything `fleetd` is configured with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shared daemon settings.
    #[serde(flatten)]
    pub service: hems_service::Settings,
    /// The sites, by identifier.
    pub sites: BTreeMap<String, SiteEntry>,
    /// The releases on offer, by component.
    ///
    /// Signed elsewhere. `fleetd` holds signatures and never a signing key —
    /// see the crate note.
    pub releases: BTreeMap<String, hems_service::Release>,
    /// The Model Context Protocol surface, off by default.
    ///
    /// It holds a household's data, so a token is **required** when it is
    /// switched on and the surface answers as whatever authority that token
    /// already carries here — an operator, or one site.
    #[serde(default)]
    pub mcp: hems_service::McpSettings,
    /// Who may read the fleet roster.
    ///
    /// `/v1/fleet` lists every enrolled household, the version each is on and
    /// when it was last heard from. That is not a box's own data and it is not
    /// public: it says which households exist, which are on an old build and
    /// which are currently unreachable, which is a target list. A box's own
    /// routes authenticate with its enrolment credential; this one needs an
    /// operator's.
    ///
    /// Empty means the roster is not served at all, for the same reason an
    /// empty credential set on `histd` serves nothing.
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
    /// Where the enrolments and the running reports are kept.
    ///
    /// Not optional. The credential a box was issued exists in exactly two
    /// places — the box and this file — and the enrolment secret that could
    /// mint a replacement is single-use and spent. A `fleetd` with nowhere to
    /// write is one restart away from a fleet of households it cannot
    /// recognise, so there is no in-memory mode to fall into by accident.
    pub store_path: std::path::PathBuf,
    /// How long a box may be quiet before the roster calls it silent, seconds.
    ///
    /// A silent box may be perfectly compliant and unreachable — a household
    /// router, a power cut — because the § 14a guard runs on the box and needs
    /// nothing from here.
    pub silent_after_s: u64,
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
            sites: BTreeMap::new(),
            releases: BTreeMap::new(),
            tenants: std::collections::BTreeMap::new(),
            operators: Vec::new(),
            store_path: std::path::PathBuf::from("fleetd.sqlite"),
            // Two days. A household that has not reported for two days is worth
            // looking at; one that missed an hour is a router rebooting.
            silent_after_s: 2 * 24 * 3600,
            mcp: hems_service::McpSettings::default(),
        }
    }
}
