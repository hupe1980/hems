//! Who is asking, and whether they may.
//!
//! Every fleet service answers questions about one household's electricity, and
//! two of the answers are the household itself: `histd`'s Data Act export is
//! everything the product generated — when the shower ran, when the car charged,
//! which fortnight nobody was in. So *who is asking* is not a deployment detail
//! to be added later behind a reverse proxy.
//!
//! # A bearer token, and a scope
//!
//! `fleetd` mints a 256-bit credential per box at enrolment. Presenting it
//! answers **who**; it does not answer **which site**, and a service that checks
//! only the first will happily hand box A the record of box B when box A asks
//! for it. [`Authority::may_read`] is the second half, and it is separate on
//! purpose: they are different questions and only one of them is about
//! cryptography.
//!
//! # Comparison is constant-time, and an empty set accepts nothing
//!
//! A token compared with `==` leaks where two differ, one byte of timing at a
//! time. And a service configured with no tokens rejects everything rather than
//! everyone: the deployment where somebody forgot the credentials is exactly the
//! one nobody would notice.

use std::collections::BTreeMap;

use crate::config::Secret;

/// What a presented credential is allowed to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Authority {
    /// One site's own box. It may read and write that site and no other.
    Site(String),
    /// An operator of the fleet. It may read any site's § 14a evidence — which
    /// is what a network operator's Nachweis is — and may write nothing.
    ///
    /// Reading is deliberately not *everything*: a Nachweis is the record of
    /// what the operator itself commanded and what the connection point drew,
    /// and it is theirs to check. The Data Act export is the **household's**
    /// data under Article 4 and an operator has no claim on it, so
    /// [`Authority::may_read_everything`] is false here.
    Operator,
}

impl Authority {
    /// Whether this credential may act for `site`.
    #[must_use]
    pub fn may_read(&self, site: &str) -> bool {
        match self {
            Self::Site(own) => own == site,
            Self::Operator => true,
        }
    }

    /// Whether this credential may **write** `site`'s record.
    ///
    /// Only the box whose record it is. An operator that could write the
    /// evidence of its own control actions is an operator marking its own
    /// homework, and the whole point of `[A1 7.2]` is that the household keeps
    /// the record.
    #[must_use]
    pub fn may_write(&self, site: &str) -> bool {
        matches!(self, Self::Site(own) if own == site)
    }

    /// Whether this credential may read the household's Data Act export.
    ///
    /// The site's own credential only. Article 4 of Regulation (EU) 2023/2854 is
    /// a right of the *user*, and an operator holding a fleet token is not one.
    #[must_use]
    pub fn may_read_everything(&self, site: &str) -> bool {
        matches!(self, Self::Site(own) if own == site)
    }
}

/// The credentials a service accepts.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    sites: Vec<(String, String)>,
    operators: Vec<String>,
}

impl Credentials {
    /// Build from configured secrets, resolving every `env:`/`file:` reference.
    ///
    /// # Errors
    /// [`crate::ConfigError::UnresolvedSecret`] when a reference names something
    /// that is not there — which stops the daemon rather than starting one that
    /// accepts a token nobody issued.
    pub fn resolve(
        sites: &BTreeMap<String, Secret>,
        operators: &[Secret],
    ) -> Result<Self, crate::ConfigError> {
        Ok(Self {
            sites: sites
                .iter()
                .map(|(site, secret)| Ok((site.clone(), secret.resolve_from_process()?)))
                .collect::<Result<_, crate::ConfigError>>()?,
            operators: operators
                .iter()
                .map(Secret::resolve_from_process)
                .collect::<Result<_, _>>()?,
        })
    }

    /// Whether anything at all is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty() && self.operators.is_empty()
    }

    /// What `token` is allowed to do, if anything.
    ///
    /// Every candidate is compared even after a match, so how long this takes
    /// says nothing about which credential it was.
    #[must_use]
    pub fn authority_of(&self, token: &str) -> Option<Authority> {
        let mut found = None;
        for (site, secret) in &self.sites {
            if constant_time_eq(token.as_bytes(), secret.as_bytes()) {
                found = Some(Authority::Site(site.clone()));
            }
        }
        for secret in &self.operators {
            if constant_time_eq(token.as_bytes(), secret.as_bytes()) {
                found = found.or(Some(Authority::Operator));
            }
        }
        found
    }
}

/// The bearer token of an `Authorization` header, if there is one.
#[must_use]
pub fn bearer(header: Option<&str>) -> Option<&str> {
    header
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// A comparison whose duration does not depend on where two values differ.
///
/// The length is not secret — it is visible from the request anyway — but the
/// contents are, so the fold runs over the whole of the longer one.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        difference |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            sites: vec![
                ("haus-1".into(), "tok-1".into()),
                ("haus-2".into(), "tok-2".into()),
            ],
            operators: vec!["tok-netz".into()],
        }
    }

    #[test]
    fn a_box_may_read_and_write_its_own_site_and_no_other() {
        let a = credentials().authority_of("tok-1").unwrap();
        assert!(a.may_read("haus-1") && a.may_write("haus-1"));
        assert!(
            !a.may_read("haus-2"),
            "one household's record is not another's"
        );
        assert!(!a.may_write("haus-2"));
    }

    #[test]
    fn an_operator_may_read_the_evidence_and_write_nothing() {
        // `[A1 7.2]` is the household's record of what the operator commanded.
        // An operator that could write it would be marking its own homework.
        let a = credentials().authority_of("tok-netz").unwrap();
        assert!(a.may_read("haus-1") && a.may_read("haus-2"));
        assert!(!a.may_write("haus-1"));
    }

    #[test]
    fn the_data_act_export_is_the_households_and_not_the_operators() {
        // Article 4 of Regulation (EU) 2023/2854 is a right of the *user*. The
        // export is everything the product generated — when the shower ran, when
        // nobody was in — and a fleet token is not a household.
        let operator = credentials().authority_of("tok-netz").unwrap();
        assert!(operator.may_read("haus-1"));
        assert!(!operator.may_read_everything("haus-1"));

        let own = credentials().authority_of("tok-1").unwrap();
        assert!(own.may_read_everything("haus-1"));
        assert!(!own.may_read_everything("haus-2"));
    }

    #[test]
    fn a_token_nobody_issued_is_nobody() {
        assert!(credentials().authority_of("tok-invented").is_none());
        assert!(credentials().authority_of("").is_none());
    }

    #[test]
    fn a_service_with_no_credentials_accepts_none() {
        // The deployment where somebody forgot them is exactly the one nobody
        // would notice, so it rejects everything rather than everyone.
        let none = Credentials::default();
        assert!(none.is_empty());
        assert!(none.authority_of("tok-1").is_none());
    }

    #[test]
    fn a_bearer_token_is_read_only_from_a_bearer_header() {
        assert_eq!(bearer(Some("Bearer tok-1")), Some("tok-1"));
        assert_eq!(bearer(Some("Bearer  tok-1  ")), Some("tok-1"));
        assert_eq!(bearer(Some("Basic tok-1")), None);
        assert_eq!(bearer(Some("tok-1")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(None), None);
    }

    #[test]
    fn the_comparison_does_not_stop_at_the_first_difference() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"), "nor at a length");
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }
}
