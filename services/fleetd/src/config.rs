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
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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
}

impl AsMut<hems_service::Settings> for Settings {
    fn as_mut(&mut self) -> &mut hems_service::Settings {
        &mut self.service
    }
}
