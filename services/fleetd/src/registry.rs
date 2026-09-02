//! Who is enrolled, what they should be running, and what they say they are.
//!
//! Pure: `now` is a parameter and the token generator is one too, so "the same
//! secret cannot enrol twice" and "a stolen token is not another site's token"
//! are unit tests rather than things somebody tries against a running server.
//!
//! The [`SiteEntry`] values it holds carry **resolved** secrets: `main` reads
//! every `env:` and `file:` reference once, at startup, so this module never
//! touches the environment or the filesystem and the comparison below is between
//! two credentials rather than between two references to one.

use std::collections::BTreeMap;

use thiserror::Error;
use time::OffsetDateTime;

use crate::config::SiteEntry;
use crate::store::{Enrolment, Report};

/// Why an enrolment or a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EnrolmentError {
    /// No site by that name.
    ///
    /// Deliberately the *same* error a wrong secret produces where it is
    /// returned to a caller — see [`EnrolmentError::is_credential_failure`].
    #[error("no such site")]
    UnknownSite,
    /// The secret does not match.
    #[error("that is not this site's enrolment secret")]
    WrongSecret,
    /// The secret has already been used.
    ///
    /// A single-use secret that still works after the box is in the field is a
    /// credential sitting in an installer's notes, and the second use is
    /// far more likely to be somebody else's than a retry.
    #[error("this site has already enrolled")]
    AlreadyEnrolled,
    /// The bearer token is not one this fleet issued.
    #[error("that token is not one this fleet issued")]
    UnknownToken,
}

impl EnrolmentError {
    /// Whether this is a failure a caller must not be able to tell apart from
    /// the others.
    ///
    /// An enrolment endpoint that answers "no such site" for one input and
    /// "wrong secret" for another is an endpoint that will happily enumerate
    /// every site identifier a fleet has. The API answers all three with the
    /// same status and the same body; the distinction is kept for the *log*,
    /// which is where an operator needs it and an attacker is not.
    #[must_use]
    pub const fn is_credential_failure(&self) -> bool {
        matches!(
            self,
            Self::UnknownSite | Self::WrongSecret | Self::AlreadyEnrolled
        )
    }
}

/// What a box gets back when it enrols.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Enrolled {
    /// Which site it is.
    pub site: String,
    /// The credential it presents from now on.
    pub token: String,
    /// When it was adopted.
    #[serde(with = "time::serde::rfc3339")]
    pub enrolled_at: OffsetDateTime,
}

/// What one enrolled box is doing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BoxState {
    /// Which site.
    pub site: String,
    /// The configuration version the fleet wants it on.
    pub wanted_version: String,
    /// The version it last said it was running, if it has said.
    pub running_version: Option<String>,
    /// When it last said anything.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
    /// Whether it is on the configuration the fleet wants.
    ///
    /// The question a rollout is actually about. A fleet that can only *push*
    /// cannot answer it, which is why the box reports back rather than being
    /// assumed to have complied.
    pub converged: bool,
}

/// The enrolled fleet.
#[derive(Debug, Default)]
pub struct Registry {
    sites: BTreeMap<String, SiteEntry>,
    /// Site → token.
    tokens: BTreeMap<String, String>,
    /// Token → site, so a lookup is not a scan.
    by_token: BTreeMap<String, String>,
    running: BTreeMap<String, String>,
    last_seen: BTreeMap<String, OffsetDateTime>,
}

impl Registry {
    /// A registry over the configured sites, and what the store remembers.
    ///
    /// The two halves come from different places on purpose. The **sites** are
    /// the operator's intent, declared in configuration; the **enrolments** and
    /// **reports** are facts about the running world, which only the store has.
    /// A site the store knows and the configuration no longer names keeps its
    /// row — deleting a household's credential because somebody edited a TOML
    /// file is not a decision this constructor gets to take — but it is not
    /// served a configuration, because there is none to serve.
    #[must_use]
    pub fn restore(
        sites: BTreeMap<String, SiteEntry>,
        enrolments: BTreeMap<String, Enrolment>,
        reports: BTreeMap<String, Report>,
    ) -> Self {
        let mut registry = Self::new(sites);
        for (site, enrolment) in enrolments {
            registry
                .by_token
                .insert(enrolment.token.clone(), site.clone());
            registry.tokens.insert(site.clone(), enrolment.token);
            registry.last_seen.insert(site, enrolment.enrolled_at);
        }
        for (site, report) in reports {
            registry
                .running
                .insert(site.clone(), report.running_version);
            registry.last_seen.insert(site, report.last_seen);
        }
        registry
    }

    /// A registry over the configured sites, with nothing enrolled.
    #[must_use]
    pub fn new(sites: BTreeMap<String, SiteEntry>) -> Self {
        Self {
            sites,
            ..Self::default()
        }
    }

    /// Whether this site may enrol with this secret, changing nothing.
    ///
    /// Split from [`Registry::adopt`] so the credential can be **written down
    /// before it is handed out**. A fleet that commits the enrolment in memory
    /// and then persists it has a failure mode where the box leaves holding a
    /// token the fleet forgets on its next restart, and no route by which it
    /// could ever enrol again — its secret is spent.
    ///
    /// # Errors
    /// [`EnrolmentError`].
    pub fn may_enrol(&self, site: &str, secret: &str) -> Result<(), EnrolmentError> {
        let entry = self.sites.get(site).ok_or(EnrolmentError::UnknownSite)?;
        if self.tokens.contains_key(site) {
            return Err(EnrolmentError::AlreadyEnrolled);
        }
        // A constant-time comparison, because the alternative leaks the secret
        // one byte at a time to anybody willing to measure.
        if !hems_service::auth::constant_time_eq(entry.enrolment_secret.expose(), secret.as_bytes())
        {
            return Err(EnrolmentError::WrongSecret);
        }
        Ok(())
    }

    /// Take a minted credential into the registry.
    ///
    /// Call only after [`Registry::may_enrol`] has said yes and the store has
    /// taken the row.
    pub fn adopt(&mut self, site: &str, token: String, at: OffsetDateTime) -> Enrolled {
        self.tokens.insert(site.to_owned(), token.clone());
        self.by_token.insert(token.clone(), site.to_owned());
        self.last_seen.insert(site.to_owned(), at);
        Enrolled {
            site: site.to_owned(),
            token,
            enrolled_at: at,
        }
    }

    /// Adopt a box: the two halves above, for a caller with nothing to persist.
    ///
    /// `mint` produces the credential; it is a parameter so a test can be
    /// deterministic and a deployment can use whatever entropy it trusts. The
    /// token is never derived from the site name or the secret — a credential
    /// that can be computed from something an installer wrote down is not one.
    ///
    /// # Errors
    /// [`EnrolmentError`].
    pub fn enrol(
        &mut self,
        site: &str,
        secret: &str,
        at: OffsetDateTime,
        mint: impl FnOnce() -> String,
    ) -> Result<Enrolled, EnrolmentError> {
        self.may_enrol(site, secret)?;
        Ok(self.adopt(site, mint(), at))
    }

    /// Which site a token belongs to.
    ///
    /// # Errors
    /// [`EnrolmentError::UnknownToken`].
    pub fn site_for(&self, token: &str) -> Result<&str, EnrolmentError> {
        self.by_token
            .get(token)
            .map(String::as_str)
            .ok_or(EnrolmentError::UnknownToken)
    }

    /// The configuration a site should be running.
    ///
    /// # Errors
    /// [`EnrolmentError::UnknownSite`].
    pub fn config_for(&self, site: &str) -> Result<&SiteEntry, EnrolmentError> {
        self.sites.get(site).ok_or(EnrolmentError::UnknownSite)
    }

    /// Record what a box says it is running.
    pub fn report(&mut self, site: &str, running_version: &str, at: OffsetDateTime) {
        self.running
            .insert(site.to_owned(), running_version.to_owned());
        self.last_seen.insert(site.to_owned(), at);
    }

    /// What every enrolled box is doing.
    #[must_use]
    pub fn states(&self) -> Vec<BoxState> {
        self.tokens
            .keys()
            .map(|site| {
                let wanted = self
                    .sites
                    .get(site)
                    .map(|e| e.config_version.clone())
                    .unwrap_or_default();
                let running = self.running.get(site).cloned();
                BoxState {
                    converged: running.as_ref() == Some(&wanted),
                    site: site.clone(),
                    wanted_version: wanted,
                    running_version: running,
                    last_seen: self.last_seen.get(site).copied(),
                }
            })
            .collect()
    }

    /// How many boxes have enrolled.
    #[must_use]
    pub fn enrolled(&self) -> usize {
        self.tokens.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    fn registry() -> Registry {
        Registry::new(
            [
                (
                    "site-1".to_owned(),
                    SiteEntry {
                        enrolment_secret: hems_service::Secret::literal("s3cret"),
                        config: "listen = \"0.0.0.0:8080\"\n".into(),
                        config_version: "7".into(),
                        config_signature: "00".repeat(64),
                    },
                ),
                (
                    "site-2".to_owned(),
                    SiteEntry {
                        enrolment_secret: hems_service::Secret::literal("other"),
                        config: String::new(),
                        config_version: "1".into(),
                        config_signature: "00".repeat(64),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        )
    }

    #[test]
    fn a_box_with_the_right_secret_is_adopted() {
        let mut r = registry();
        let enrolled = r
            .enrol("site-1", "s3cret", NOW, || "token-1".into())
            .unwrap();
        assert_eq!(enrolled.site, "site-1");
        assert_eq!(r.site_for("token-1").unwrap(), "site-1");
        assert_eq!(r.enrolled(), 1);
    }

    #[test]
    fn the_secret_is_single_use() {
        // A secret that still works after the box is in the field is a
        // credential sitting in an installer's notes, and the second use is far
        // more likely to be somebody else's than a retry.
        let mut r = registry();
        r.enrol("site-1", "s3cret", NOW, || "token-1".into())
            .unwrap();
        assert_eq!(
            r.enrol("site-1", "s3cret", NOW, || "token-2".into()),
            Err(EnrolmentError::AlreadyEnrolled)
        );
    }

    #[test]
    fn every_credential_failure_looks_the_same_to_a_caller() {
        // An endpoint that answers "no such site" for one input and "wrong
        // secret" for another will enumerate every site a fleet has.
        let mut r = registry();
        for e in [
            r.enrol("nobody", "s3cret", NOW, || "t".into()).unwrap_err(),
            r.enrol("site-1", "wrong", NOW, || "t".into()).unwrap_err(),
        ] {
            assert!(e.is_credential_failure(), "{e}");
        }
        r.enrol("site-1", "s3cret", NOW, || "t".into()).unwrap();
        assert!(
            r.enrol("site-1", "s3cret", NOW, || "t2".into())
                .unwrap_err()
                .is_credential_failure()
        );
    }

    #[test]
    fn one_sites_token_is_not_another_sites() {
        let mut r = registry();
        r.enrol("site-1", "s3cret", NOW, || "token-1".into())
            .unwrap();
        r.enrol("site-2", "other", NOW, || "token-2".into())
            .unwrap();
        assert_eq!(r.site_for("token-1").unwrap(), "site-1");
        assert_eq!(r.site_for("token-2").unwrap(), "site-2");
        assert_eq!(r.site_for("token-3"), Err(EnrolmentError::UnknownToken));
    }

    #[test]
    fn convergence_is_what_a_box_says_and_not_what_was_pushed() {
        // The question asked the morning after a rollout, and the one a
        // push-only fleet cannot answer.
        let mut r = registry();
        r.enrol("site-1", "s3cret", NOW, || "token-1".into())
            .unwrap();
        let before = &r.states()[0];
        assert_eq!(before.wanted_version, "7");
        assert_eq!(before.running_version, None);
        assert!(!before.converged);

        r.report("site-1", "6", NOW);
        assert!(!r.states()[0].converged, "it took an older one");
        r.report("site-1", "7", NOW);
        assert!(r.states()[0].converged);
    }

    #[test]
    fn a_box_that_has_not_enrolled_is_not_in_the_fleet_view() {
        let mut r = registry();
        r.enrol("site-1", "s3cret", NOW, || "token-1".into())
            .unwrap();
        assert_eq!(r.states().len(), 1, "site-2 has not arrived");
    }
}
