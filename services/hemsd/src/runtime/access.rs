//! Who may ask this box anything.
//!
//! The box holds a household's own electricity: `/v1/status` says what every
//! device is doing right now, `/v1/series` is the Data Act's local API over the
//! one-second history, `/v1/overrides` decides what the arbiter wants, and
//! `/s2/{asset}` lets an external energy manager drive the house. Every one of
//! them authenticates its caller through `hems_service::auth`, the same model
//! the fleet daemons use (D108, D112).
//!
//! # A token the box issues itself
//!
//! Not a setting with a default, which is the shape that ships insecure: a
//! default token is a published token, and a box that *refused to start* without
//! one would be a box an installer works around by inventing a weak one. So the
//! box generates its own on first start, from the operating system's entropy,
//! and keeps it in its own store beside the EEBUS key — for the same reason the
//! SKI lives there (`runtime::ship`): an installer reads it off a screen once,
//! and a value that changed on every boot would make that a step they had to
//! repeat.
//!
//! It is printed at start-up next to the SKI, which is the other credential a
//! commissioning visit has to carry away.
//!
//! **A box with no store cannot keep one**, and then the token is fresh every
//! run and says so. That is honest rather than convenient: `store_path = None`
//! is the demonstration mode, and a demonstration whose credential survived a
//! restart would be keeping state it has been told not to keep.
//!
//! A deployment that provisions credentials of its own sets `[api] token`
//! instead, and then nothing is generated and nothing is stored.
//!
//! # What it is not
//!
//! It is **not** a second authorisation model. The token resolves to the same
//! `hems_service::auth::Authority` a fleet service would issue — `box_at(site)`,
//! the household's own — so the capability names, the attenuation rules and the
//! constant-time comparison are the workspace's one implementation rather than a
//! second opinion written for the edge.
//!
//! Nor does it protect the **health surface**. `/livez`, `/readyz` and
//! `/metrics` stay open, because an orchestrator that has to hold a household's
//! credential in order to restart a crashed box is an orchestrator that will be
//! given one too widely. They carry no household data — that is the property
//! that makes it safe, and it is `hems_service`'s to keep.

use std::sync::Arc;

use hems_service::auth::{Authority, Capabilities, Credentials, SiteScope, bearer};
use hems_service::config::Secret;
use tokio::sync::RwLock;

use crate::store::Store;

/// What a connected energy manager may do with the credential the box issued it.
///
/// The household's own set **less the Data Act export**: an aggregator drives
/// devices, and Article 4 of Regulation (EU) 2023/2854 is a right of the *user*
/// rather than of whoever the user has asked to optimise for them. A manager
/// that could take the one-second series could reconstruct when the household
/// showered, cooked and went away.
///
/// Narrowing it further — to the S2 routes alone — wants a capability of its own
/// (`hems.flex.drive`) rather than a special case here; `Capabilities` attenuate
/// by containment, so that is a new pattern rather than a new mechanism.
fn manager_capabilities() -> Capabilities {
    Capabilities::of(hems_service::auth::ALL_CAPABILITIES.iter().copied())
        .attenuate(&Capabilities::of([
            hems_service::auth::RECORD_READ,
            hems_service::auth::RECORD_WRITE,
        ]))
        .unwrap_or_else(|_| Capabilities::none())
}

/// How the box's own surfaces are reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApiSettings {
    /// The bearer token that opens `/v1/*` and `/s2/*`.
    ///
    /// Absent — the ordinary case — means the box issues one itself, keeps it in
    /// its own store and prints it at start-up. Set it where something else
    /// already provisions credentials for this fleet, and then nothing is
    /// generated and nothing is stored.
    ///
    /// It is a [`Secret`], so `env:HEMS_API_TOKEN` and `file:/run/secrets/…`
    /// work here as they do everywhere else in this workspace — a token written
    /// literally into a configuration file is a token in a backup.
    pub token: Option<Secret>,
}

/// The credentials this box answers to, and the household it answers about.
///
/// Two kinds. The household's own token, fixed for the life of the box, and one
/// per **energy manager** it has connected — issued, listed and withdrawn while
/// the box runs, which is why the set is behind a lock rather than built once.
#[derive(Clone)]
pub struct LocalAccess {
    credentials: Arc<RwLock<Credentials>>,
    site: String,
    /// The household's own token, kept so the set can be **rebuilt** when a
    /// manager is withdrawn. `Credentials` is deliberately append-only.
    own: String,
    store: Option<Arc<tokio::sync::Mutex<Store>>>,
}

/// An energy manager this household has connected.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Manager {
    /// What the household calls it.
    pub name: String,
}

impl LocalAccess {
    /// Resolve the box's own credential: configured, remembered, or issued.
    ///
    /// # Errors
    /// When a configured secret names an environment variable that is not set
    /// or a file that cannot be read, or when the store refuses the write.
    pub fn resolve(
        settings: &ApiSettings,
        site: &str,
        store: Option<&Store>,
    ) -> anyhow::Result<(Self, Issued)> {
        let (token, issued) = match (&settings.token, store) {
            // A deployment provisioned one, so nothing is generated and nothing
            // is stored.
            (Some(secret), _) => (secret.resolve_from_process()?, How::Configured),
            // The box issued one before and remembered it — the ordinary case
            // after the first start.
            (None, Some(store)) => {
                if let Some(token) = store.api_token()? {
                    (token, How::Remembered)
                } else {
                    let token = generate();
                    store.put_api_token(&token)?;
                    (token, How::Fresh)
                }
            }
            // Nothing to keep it in, so it lasts as long as the process. See the
            // module note: that is what `store_path = None` means.
            (None, None) => (generate(), How::Ephemeral),
        };
        // One token, used for both: the box has to answer to exactly what it
        // prints, and building the credential and the announcement from separate
        // values is a box nobody can log in to.
        Ok((Self::new(site, &token), Issued { token, how: issued }))
    }

    /// A credential set built in code, for a test.
    #[must_use]
    pub fn for_testing(site: &str, token: &str) -> Self {
        Self::new(site, token)
    }

    fn new(site: &str, token: &str) -> Self {
        Self {
            credentials: Arc::new(RwLock::new(Credentials::default().with_site(site, token))),
            site: site.to_owned(),
            own: token.to_owned(),
            store: None,
        }
    }

    /// Keep the store, so a manager connected on a running box survives a
    /// restart — the same reason the EEBUS trust store is kept (D102).
    #[must_use]
    pub fn remembering(mut self, store: Option<Arc<tokio::sync::Mutex<Store>>>) -> Self {
        self.store = store;
        self
    }

    /// Load the managers this household connected earlier.
    ///
    /// # Errors
    /// [`StoreError`](crate::store::StoreError) where the read fails.
    pub async fn restore(&self) -> Result<usize, crate::store::StoreError> {
        let Some(store) = &self.store else {
            return Ok(0);
        };
        let managers = store.lock().await.managers()?;
        let mut credentials = self.credentials.write().await;
        for (name, token) in &managers {
            *credentials = credentials.clone().with_authority(
                Authority::new(
                    format!("cem:{name}"),
                    manager_capabilities(),
                    SiteScope::One(self.site.clone()),
                ),
                token,
            );
        }
        Ok(managers.len())
    }

    /// Connect an energy manager, and return the credential it presents.
    ///
    /// Replaces the credential of a manager already connected under that name,
    /// which is how one is **rotated**: the household names the same manager
    /// again and the old token stops working on the next request.
    ///
    /// # Errors
    /// [`StoreError`](crate::store::StoreError) where the write fails.
    pub async fn connect_manager(&self, name: &str) -> Result<String, crate::store::StoreError> {
        let token = generate();
        if let Some(store) = &self.store {
            store.lock().await.put_manager(name, &token)?;
        }
        let mut credentials = self.credentials.write().await;
        *credentials = credentials.clone().with_authority(
            Authority::new(
                format!("cem:{name}"),
                manager_capabilities(),
                SiteScope::One(self.site.clone()),
            ),
            &token,
        );
        Ok(token)
    }

    /// Withdraw one. Returns whether it was connected.
    ///
    /// The credential stops working immediately rather than at the end of some
    /// session: a household that has decided a manager should stop driving its
    /// house has not decided it may finish the quarter hour first.
    ///
    /// # Errors
    /// [`StoreError`](crate::store::StoreError) where the write fails.
    pub async fn forget_manager(&self, name: &str) -> Result<bool, crate::store::StoreError> {
        let existed = match &self.store {
            Some(store) => store.lock().await.forget_manager(name)?,
            None => false,
        };
        // Rebuilt from the store rather than removed in place, because
        // `Credentials` is deliberately append-only: it is a set of things that
        // are allowed, and a type that could quietly drop one is a type where a
        // revocation can go missing.
        let rebuilt = Credentials::default().with_site(&self.site, &self.own);
        let mut credentials = self.credentials.write().await;
        *credentials = rebuilt;
        drop(credentials);
        self.restore().await?;
        Ok(existed)
    }

    /// Every manager currently connected.
    ///
    /// # Errors
    /// [`StoreError`](crate::store::StoreError) where the read fails.
    pub async fn managers(&self) -> Result<Vec<Manager>, crate::store::StoreError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        Ok(store
            .lock()
            .await
            .managers()?
            .into_iter()
            .map(|(name, _)| Manager { name })
            .collect())
    }

    /// Whether this request may act on this household.
    ///
    /// One answer for every failure — absent, malformed, unknown — because which
    /// it was is an operational fact for whoever runs the box and a probing aid
    /// for anybody else.
    pub async fn authority(&self, header: Option<&str>) -> Option<Authority> {
        let token = bearer(header)?;
        self.credentials.read().await.authority_of(token)
    }

    /// The household this box is.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }
}

/// Refuse any request that does not carry this box's own token.
///
/// **One layer over the whole router, not a check in each handler**, and that is
/// the shape rather than the convenience. `obsd` served its fleet summary — which
/// names every household that failed to respect a network operator's reduction —
/// to any valid credential, because four call sites each spelled the test
/// themselves and one spelled it wrong (D112). A route added to this box
/// tomorrow cannot forget a layer it never had to remember.
///
/// Every failure is one status. Which it was — no header, a malformed one, a
/// token nobody issued — is an operational fact for whoever runs the box, and a
/// probing aid for anybody else.
///
/// # Errors
/// [`axum::http::StatusCode::UNAUTHORIZED`] when the request carries no
/// credential this box issued.
pub async fn require_token(
    axum::extract::State(access): axum::extract::State<LocalAccess>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::http::StatusCode> {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let site = access.site().to_owned();
    let Some(authority) = access.authority(presented).await else {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    };
    if !authority.may_read(&site) {
        return Err(axum::http::StatusCode::UNAUTHORIZED);
    }
    // The Data Act export is the household's own, not its manager's: Article 4
    // of Regulation (EU) 2023/2854 is a right of the *user*, and the one-second
    // series says when they showered, cooked and went away. It is the one
    // capability a manager's credential does not carry, and this is where the
    // difference is enforced rather than merely granted.
    if request.uri().path().starts_with("/v1/series") && !authority.may_read_everything(&site) {
        return Err(axum::http::StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

/// Where the token came from, so start-up can say the right thing about it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issued {
    /// The token itself, for the line an installer reads.
    pub token: String,
    /// How the box came by it.
    pub how: How,
}

impl Issued {
    /// The line to print at start-up, or `None` where the operator already has
    /// the token because they configured it.
    #[must_use]
    pub fn announcement(&self) -> Option<String> {
        match self.how {
            How::Configured => None,
            How::Remembered | How::Fresh => Some(format!(
                "🔑 API  {}\n   the bearer token for this box's own surfaces, kept in its store",
                self.token
            )),
            How::Ephemeral => Some(format!(
                "🔑 API  {}\n   this box keeps no store, so this token lasts until it restarts",
                self.token
            )),
        }
    }
}

/// How the box came by its token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum How {
    /// A deployment provisioned it.
    Configured,
    /// The box issued it earlier and remembered it.
    #[default]
    Remembered,
    /// The box issued it just now and kept it.
    Fresh,
    /// The box issued it just now and has nowhere to keep it.
    Ephemeral,
}

/// A fresh token from the operating system's entropy, and nothing else.
///
/// Thirty-two bytes, hex. A credential derived from a site name, a serial number
/// or a clock is a credential somebody can compute, which is why `getrandom` is
/// the only source this workspace uses for one.
///
/// # Panics
/// When the operating system cannot produce entropy, which is a box that must
/// not go on to serve a surface it cannot protect.
fn generate() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system's entropy");
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_token_opens_its_own_household_and_nothing_else() {
        let access = LocalAccess::for_testing("reference-household", "s3cret");
        let authority = access
            .authority(Some("Bearer s3cret"))
            .await
            .expect("the right token is accepted");
        assert!(authority.may_read("reference-household"));
        assert!(authority.may_write("reference-household"));
        assert!(
            !authority.may_read("somebody-elses-household"),
            "a box's own token is scoped to the box's own household"
        );
        assert!(
            !authority.may_read_the_fleet(),
            "and it is not a fleet credential"
        );
    }

    #[tokio::test]
    async fn every_way_of_not_presenting_a_token_is_refused() {
        let access = LocalAccess::for_testing("reference-household", "s3cret");
        for header in [
            None,
            Some(""),
            Some("s3cret"),
            Some("Bearer"),
            Some("Bearer "),
            Some("Bearer wrong"),
            Some("Basic s3cret"),
            // A prefix of the real token, which a comparison that stopped at the
            // first difference would take longer to refuse than a wrong one.
            Some("Bearer s3cre"),
        ] {
            assert!(
                access.authority(header).await.is_none(),
                "{header:?} should not open this box"
            );
        }
    }

    /// The token the box **prints** is the token the box **accepts**.
    ///
    /// Building the credential and the announcement from two calls to
    /// `generate()` compiles, passes every other test here, and produces a box
    /// nobody can log in to — it answers `401` to the number on its own screen.
    /// This is the assertion that the two come from one value.
    #[tokio::test]
    async fn the_token_announced_is_the_token_accepted() {
        let temporary = std::env::temp_dir().join(format!(
            "hems-access-{}.redb",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock after 1970")
                .as_nanos()
        ));
        let store = Store::open(&temporary).expect("a fresh store");
        for expected in [How::Fresh, How::Remembered] {
            let (access, issued) =
                LocalAccess::resolve(&ApiSettings::default(), "reference-household", Some(&store))
                    .expect("a box can issue its own credential");
            assert_eq!(
                issued.how, expected,
                "the second resolve remembers the first"
            );
            assert!(
                access
                    .authority(Some(&format!("Bearer {}", issued.token)))
                    .await
                    .is_some(),
                "the box refused the token it printed"
            );
        }
        // …and the same for a box with nothing to remember it in.
        let (access, issued) =
            LocalAccess::resolve(&ApiSettings::default(), "reference-household", None)
                .expect("a box with no store still protects itself");
        assert_eq!(issued.how, How::Ephemeral);
        assert!(
            access
                .authority(Some(&format!("Bearer {}", issued.token)))
                .await
                .is_some(),
            "the box refused the token it printed"
        );
        drop(store);
        let _ = std::fs::remove_file(&temporary);
    }

    #[test]
    fn a_generated_token_is_not_guessable_and_not_the_same_twice() {
        let first = generate();
        let second = generate();
        assert_eq!(first.len(), 64, "thirty-two bytes, hex");
        assert_ne!(first, second);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_configured_token_is_not_announced_and_an_issued_one_is() {
        // The operator who set it already has it; printing it again would put a
        // provisioned secret into a log that a generated one has to be in.
        assert_eq!(
            Issued {
                token: "x".into(),
                how: How::Configured
            }
            .announcement(),
            None
        );
        for how in [How::Remembered, How::Fresh, How::Ephemeral] {
            let line = Issued {
                token: "abc".into(),
                how,
            }
            .announcement()
            .expect("an issued token has to be shown to somebody");
            assert!(line.contains("abc"));
        }
    }
}
