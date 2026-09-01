//! What the fleet says, and whether to believe it: a release, and a configuration.
//!
//! # Why this is in the shell and not in a service
//!
//! The Cyber Resilience Act (Regulation (EU) 2024/2847, Annex I Part I § 2(c)
//! and Part II) requires that a product with digital elements ships security
//! updates and that their **integrity is protected**. An unsigned image on a
//! device that holds a grid connection open is the whole of what that
//! requirement is about, and "the fleet server told us to" is not integrity — it
//! is a trust anchor that moves whenever DNS does.
//!
//! So the thing a box trusts is a **public key it was built with**, not a
//! server. `fleetd` publishes a manifest and a signature over it; every daemon
//! verifies that signature before it fetches a single byte of the artefact, and
//! verifies the artefact's digest before it runs it. A `fleetd` that is
//! compromised can then serve a manifest nobody will accept, which is the
//! property that makes the fleet server ordinary infrastructure rather than the
//! root of trust.
//!
//! # Sans-I/O, like everything else that has a decision in it
//!
//! Nothing here opens a socket or reads a clock. [`Release::verify`] is a pure
//! function of a manifest, a signature and a key, so "a tampered manifest is
//! refused" is a unit test rather than a thing somebody tries once with a real
//! server.
//!
//! # What it deliberately does not do
//!
//! It does not download, unpack or install. Those are I/O and they are the
//! caller's; what this owns is the two questions that have a right answer —
//! *is this manifest from us*, and *is this the artefact it describes*.
//!
//! # A configuration is the same question
//!
//! [`SignedConfig`] is the release argument applied to the other thing a box
//! pulls. A configuration document decides which assets the site has, what the
//! comfort band is and where the box reports — so a `fleetd` that could serve
//! an arbitrary one would be a trust anchor after all, and every sentence above
//! about the CRA would be a sentence about the update channel only.
//!
//! It is signed by the **same built-in key**, and `fleetd` holds the signature
//! and never a signing key, exactly as it does for a release. That is what makes
//! "the fleet is not the trust anchor" a property of the design rather than a
//! claim about one endpoint.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Why a release was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpdateError {
    /// The manifest could not be canonicalised — it is not the shape a
    /// signature was ever computed over.
    #[error("the manifest is not serialisable: {0}")]
    Unserialisable(String),
    /// The signature is not 64 bytes of hexadecimal.
    #[error("the signature is not a 64-byte Ed25519 signature")]
    MalformedSignature,
    /// The trusted key is not 32 bytes of hexadecimal, or is not a valid point.
    #[error("the trusted key is not a valid Ed25519 public key")]
    MalformedKey,
    /// The signature does not verify against the trusted key.
    ///
    /// Either the manifest was changed after it was signed, or it was signed by
    /// somebody this box does not trust. The box cannot tell those apart and
    /// must not try: both mean *do not install this*.
    #[error("the manifest is not signed by a key this box trusts")]
    NotOurs,
    /// The artefact does not hash to what the manifest said it would.
    #[error("the artefact hashes to {found}, and the manifest says {expected}")]
    WrongDigest {
        /// What was downloaded.
        found: String,
        /// What was promised.
        expected: String,
    },
    /// The manifest describes a version this box already has or has passed.
    #[error("{offered} is not newer than the running {running}")]
    NotNewer {
        /// What was offered.
        offered: String,
        /// What is running.
        running: String,
    },
    /// The document is genuine and is for a different component — or, for a
    /// [`SignedConfig`], for a different site.
    ///
    /// A signature says *we published this*; it does not say *this is the thing
    /// you asked for*. Both manifests are signed by the same key, so a fleet
    /// server, a proxy or a path confusion that hands `hemsd` the release of
    /// `tariffd` produces a document that verifies perfectly and installs the
    /// wrong binary onto a box that holds a grid connection open.
    #[error("the manifest is for {found}, and this is {expected}")]
    WrongComponent {
        /// What the manifest describes.
        found: String,
        /// What asked.
        expected: String,
    },
}

/// What `fleetd` says is available.
///
/// The fields are exactly what has to be signed, and nothing else: a manifest
/// carrying a field that is not covered by the signature is a manifest with a
/// mutable half, and it is always the mutable half an attacker reaches for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Which artefact — `hemsd`, `tariffd`.
    pub component: String,
    /// The version being offered, as `major.minor.patch`.
    pub version: String,
    /// Where to fetch it.
    pub url: String,
    /// The artefact's SHA-256, lower-case hexadecimal.
    pub sha256: String,
    /// Its size in bytes, so a download can be bounded before it starts.
    pub size_bytes: u64,
    /// When the release was published, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub published_at: time::OffsetDateTime,
}

impl Manifest {
    /// The exact bytes a signature is computed over.
    ///
    /// Canonical JSON of the manifest's own fields, in declaration order.
    /// `serde_json` writes a struct's fields in the order they are declared and
    /// this type has no maps, so the encoding is deterministic — which is the
    /// only property a signing payload needs and the one that is easiest to lose
    /// by signing a re-serialised document.
    ///
    /// # Errors
    /// [`UpdateError::Unserialisable`], which cannot happen for this type and is
    /// an error rather than a panic because a signing payload is not a place to
    /// unwrap.
    pub fn signing_payload(&self) -> Result<Vec<u8>, UpdateError> {
        serde_json::to_vec(self).map_err(|e| UpdateError::Unserialisable(e.to_string()))
    }
}

/// A manifest and the signature over it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// What is on offer.
    pub manifest: Manifest,
    /// The Ed25519 signature over [`Manifest::signing_payload`], hexadecimal.
    pub signature: String,
}

impl Release {
    /// Whether this release is a genuine one **for `component`**.
    ///
    /// The call an updater should make. [`Release::verify`] answers "did we
    /// publish this", which is only half of a decision: every component's
    /// manifest is signed by the same key, so a genuine `tariffd` release
    /// verifies just as well against a box that asked for `hemsd`.
    ///
    /// # Errors
    /// As [`Release::verify`], plus [`UpdateError::WrongComponent`].
    pub fn verify_for(
        &self,
        component: &str,
        trusted_key_hex: &str,
    ) -> Result<&Manifest, UpdateError> {
        // The signature first: a manifest nobody signed says nothing about what
        // it is for, and answering "wrong component" to a forgery would be
        // telling the forger which field to change.
        let manifest = self.verify(trusted_key_hex)?;
        if manifest.component == component {
            Ok(manifest)
        } else {
            Err(UpdateError::WrongComponent {
                found: manifest.component.clone(),
                expected: component.to_owned(),
            })
        }
    }

    /// Whether this release is signed by `trusted_key_hex`.
    ///
    /// **Call this before fetching anything.** The point of a signature is to
    /// decide whether to spend the bandwidth, not to regret having spent it.
    ///
    /// Prefer [`Release::verify_for`] wherever the caller knows which component
    /// it is updating, which is everywhere an updater runs.
    ///
    /// # Errors
    /// [`UpdateError::MalformedKey`], [`UpdateError::MalformedSignature`] or
    /// [`UpdateError::NotOurs`].
    pub fn verify(&self, trusted_key_hex: &str) -> Result<&Manifest, UpdateError> {
        verify_ed25519(
            trusted_key_hex,
            &self.signature,
            &self.manifest.signing_payload()?,
        )?;
        Ok(&self.manifest)
    }

    /// Whether `artefact` is the thing the manifest describes.
    ///
    /// # Errors
    /// [`UpdateError::WrongDigest`].
    pub fn check_artefact(&self, artefact: &[u8]) -> Result<(), UpdateError> {
        let found = hex::encode(Sha256::digest(artefact));
        if found.eq_ignore_ascii_case(self.manifest.sha256.trim()) {
            Ok(())
        } else {
            Err(UpdateError::WrongDigest {
                found,
                expected: self.manifest.sha256.clone(),
            })
        }
    }

    /// Whether the offered version is newer than `running`.
    ///
    /// A plain `major.minor.patch` comparison on numbers, because that is what
    /// this workspace's versions are. Anything that does not parse is treated as
    /// **not newer**, which is the safe direction: a box that cannot understand
    /// a version string should not install what carries it.
    ///
    /// # Errors
    /// [`UpdateError::NotNewer`].
    pub fn check_newer(&self, running: &str) -> Result<(), UpdateError> {
        let offered = parse_version(&self.manifest.version);
        let have = parse_version(running);
        match (offered, have) {
            (Some(o), Some(h)) if o > h => Ok(()),
            _ => Err(UpdateError::NotNewer {
                offered: self.manifest.version.clone(),
                running: running.to_owned(),
            }),
        }
    }
}

/// The configuration document a site should be running, and the signature over it.
///
/// # What is signed, and why all three fields
///
/// The **site**, so a genuine configuration for one household cannot be served
/// to another; the **version**, so an old one cannot be replayed over a newer
/// one; and the **document**. Signing the document alone is the version of this
/// that looks sufficient, and it lets a compromised fleet server hand every box
/// in the fleet the one site whose configuration it likes best.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedConfig {
    /// Which site the document is for.
    pub site: String,
    /// Its version. Any string; the box reports it back.
    pub version: String,
    /// The document itself, as TOML. Opaque here: `fleetd` distributes
    /// configuration and does not interpret it.
    pub config: String,
    /// The Ed25519 signature over [`SignedConfig::signing_payload`], hexadecimal.
    pub signature: String,
}

impl SignedConfig {
    /// The exact bytes a signature is computed over.
    ///
    /// Canonical JSON of the three signed fields in declaration order — the
    /// signature itself is not one of them. Same construction and same argument
    /// as [`Manifest::signing_payload`].
    ///
    /// # Errors
    /// [`UpdateError::Unserialisable`], which cannot happen for three strings.
    pub fn signing_payload(&self) -> Result<Vec<u8>, UpdateError> {
        #[derive(serde::Serialize)]
        struct Payload<'a> {
            site: &'a str,
            version: &'a str,
            config: &'a str,
        }
        serde_json::to_vec(&Payload {
            site: &self.site,
            version: &self.version,
            config: &self.config,
        })
        .map_err(|e| UpdateError::Unserialisable(e.to_string()))
    }

    /// Whether this document is the fleet's own, and is for `site`.
    ///
    /// # Errors
    /// [`UpdateError::MalformedKey`], [`UpdateError::MalformedSignature`],
    /// [`UpdateError::NotOurs`] or [`UpdateError::WrongComponent`] — the last
    /// naming the site rather than a component, because it is the same fault.
    pub fn verify_for(&self, site: &str, trusted_key_hex: &str) -> Result<&str, UpdateError> {
        verify_ed25519(trusted_key_hex, &self.signature, &self.signing_payload()?)?;
        if self.site == site {
            Ok(&self.config)
        } else {
            Err(UpdateError::WrongComponent {
                found: self.site.clone(),
                expected: site.to_owned(),
            })
        }
    }
}

/// Check one Ed25519 signature over `payload`.
///
/// Shared by [`Release`] and [`SignedConfig`] so the two cannot come to disagree
/// about what a malformed key or a bad signature is.
fn verify_ed25519(
    trusted_key_hex: &str,
    signature_hex: &str,
    payload: &[u8],
) -> Result<(), UpdateError> {
    let key_bytes: [u8; 32] = hex::decode(trusted_key_hex.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(UpdateError::MalformedKey)?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| UpdateError::MalformedKey)?;

    let signature_bytes: [u8; 64] = hex::decode(signature_hex.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(UpdateError::MalformedSignature)?;

    key.verify(payload, &Signature::from_bytes(&signature_bytes))
        .map_err(|_| UpdateError::NotOurs)
}

/// `major.minor.patch` as three numbers, or `None`.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // A trailing `-rc1` is not a version this comparison understands, and
    // "not understood" has to mean "not newer".
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use time::macros::datetime;

    /// A deterministic key pair, so the test is a test and not a coin toss.
    fn keys() -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public = hex::encode(signing.verifying_key().to_bytes());
        (signing, public)
    }

    fn manifest() -> Manifest {
        Manifest {
            component: "hemsd".into(),
            version: "0.2.0".into(),
            url: "https://updates.example/hemsd-0.2.0".into(),
            sha256: hex::encode(Sha256::digest(b"the artefact")),
            size_bytes: 12,
            published_at: datetime!(2026-06-21 12:00:00 UTC),
        }
    }

    fn signed(manifest: Manifest) -> Release {
        let (signing, _) = keys();
        let signature = signing.sign(&manifest.signing_payload().unwrap());
        Release {
            manifest,
            signature: hex::encode(signature.to_bytes()),
        }
    }

    #[test]
    fn a_release_we_signed_verifies_and_its_artefact_matches() {
        let (_, public) = keys();
        let release = signed(manifest());
        assert_eq!(release.verify(&public).unwrap().version, "0.2.0");
        release.check_artefact(b"the artefact").unwrap();
        release.check_newer("0.1.0").unwrap();
    }

    fn config(site: &str, version: &str) -> SignedConfig {
        let (signing, _) = keys();
        let mut c = SignedConfig {
            site: site.into(),
            version: version.into(),
            config: "[site]\nname = \"Haus 1\"\n".into(),
            signature: String::new(),
        };
        c.signature = hex::encode(signing.sign(&c.signing_payload().unwrap()).to_bytes());
        c
    }

    #[test]
    fn a_configuration_the_fleet_signed_is_accepted() {
        let (_, public) = keys();
        let c = config("haus-1", "7");
        assert_eq!(c.verify_for("haus-1", &public).unwrap(), c.config);
    }

    #[test]
    fn a_configuration_edited_after_signing_is_refused() {
        // The attack: a compromised `fleetd`, a proxy or a DNS answer that
        // rewrites the comfort band, the asset list or where the box reports.
        let (_, public) = keys();
        let mut c = config("haus-1", "7");
        c.config = "[site]\nname = \"somebody else's\"\n".into();
        assert_eq!(c.verify_for("haus-1", &public), Err(UpdateError::NotOurs));
    }

    #[test]
    fn another_households_configuration_is_refused_though_it_is_genuine() {
        // Signed by us, and for somebody else. Without the site inside the
        // signed payload a compromised fleet server could hand every box in the
        // fleet the one configuration it liked best.
        let (_, public) = keys();
        let c = config("haus-2", "7");
        assert!(matches!(
            c.verify_for("haus-1", &public),
            Err(UpdateError::WrongComponent { .. })
        ));
    }

    #[test]
    fn a_version_swapped_after_signing_is_refused() {
        // Rollback: an old configuration is genuine, and re-labelling it as the
        // current one must not verify.
        let (_, public) = keys();
        let mut c = config("haus-1", "7");
        c.version = "9".into();
        assert_eq!(c.verify_for("haus-1", &public), Err(UpdateError::NotOurs));
    }

    #[test]
    fn a_genuine_release_for_another_component_is_refused() {
        // Both are signed by the same key, so the signature cannot tell them
        // apart and the caller must not be the only thing that does.
        let (_, public) = keys();
        let mut other = manifest();
        other.component = "tariffd".into();
        let release = signed(other);
        assert!(release.verify(&public).is_ok(), "it really is ours");
        assert!(matches!(
            release.verify_for("hemsd", &public),
            Err(UpdateError::WrongComponent { .. })
        ));
        assert!(release.verify_for("tariffd", &public).is_ok());
    }

    #[test]
    fn a_forgery_is_a_forgery_rather_than_a_wrong_component() {
        // The order of the two checks: answering "wrong component" to something
        // nobody signed would tell a forger which field to change.
        let mut forged = signed(manifest());
        forged.manifest.component = "tariffd".into();
        let (_, public) = keys();
        assert_eq!(
            forged.verify_for("hemsd", &public),
            Err(UpdateError::NotOurs)
        );
    }

    #[test]
    fn a_manifest_changed_after_signing_is_refused() {
        // The attack the signature exists for: a fleet server, a proxy or a DNS
        // answer that swaps the URL for one of its own.
        let (_, public) = keys();
        let mut release = signed(manifest());
        release.manifest.url = "https://evil.example/hemsd".into();
        assert_eq!(release.verify(&public), Err(UpdateError::NotOurs));
    }

    #[test]
    fn a_release_signed_by_somebody_else_is_refused() {
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let m = manifest();
        let release = Release {
            signature: hex::encode(other.sign(&m.signing_payload().unwrap()).to_bytes()),
            manifest: m,
        };
        let (_, public) = keys();
        assert_eq!(release.verify(&public), Err(UpdateError::NotOurs));
    }

    #[test]
    fn the_artefact_is_checked_as_well_as_the_manifest() {
        // A signature over a manifest says the *manifest* is ours. It says
        // nothing about what a URL served, and a mirror that serves a different
        // file is not an exotic attack, it is a stale cache.
        let release = signed(manifest());
        let err = release.check_artefact(b"something else").unwrap_err();
        assert!(matches!(err, UpdateError::WrongDigest { .. }), "{err}");
    }

    #[test]
    fn a_downgrade_is_not_an_update() {
        // The other half of integrity: an attacker who cannot forge a signature
        // can still replay a *genuine* older release with a known hole in it.
        let mut m = manifest();
        m.version = "0.1.0".into();
        let release = signed(m);
        assert!(matches!(
            release.check_newer("0.2.0"),
            Err(UpdateError::NotNewer { .. })
        ));
        assert!(
            release.check_newer("0.1.0").is_err(),
            "and the same version is not an update either"
        );
    }

    #[test]
    fn a_version_this_build_cannot_parse_is_not_newer() {
        let mut m = manifest();
        m.version = "0.2.0-rc1".into();
        let release = signed(m);
        assert!(release.check_newer("0.1.0").is_err());
    }

    #[test]
    fn nonsense_keys_and_signatures_are_named_rather_than_panicking() {
        let release = signed(manifest());
        assert_eq!(release.verify("not hex"), Err(UpdateError::MalformedKey));
        assert_eq!(release.verify("aabb"), Err(UpdateError::MalformedKey));
        let mut broken = signed(manifest());
        broken.signature = "00".into();
        let (_, public) = keys();
        assert_eq!(broken.verify(&public), Err(UpdateError::MalformedSignature));
    }

    #[test]
    fn a_manifest_with_a_field_we_do_not_know_is_refused_on_the_way_in() {
        // A manifest with a half the signature does not cover is a manifest with
        // a mutable half, and it is always the mutable half an attacker reaches
        // for. `deny_unknown_fields` is what stops one existing.
        let mut value = serde_json::to_value(manifest()).unwrap();
        value["install_command"] = serde_json::json!("rm -rf /");
        assert!(serde_json::from_value::<Manifest>(value).is_err());
    }
}
