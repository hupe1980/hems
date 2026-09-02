//! What `agentd` is configured with.

use std::path::PathBuf;

/// Everything `agentd` is configured with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The shared daemon settings.
    #[serde(flatten)]
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
        }
    }
}
