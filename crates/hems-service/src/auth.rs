//! Who is asking, what they may do, and which households they may do it to.
//!
//! Every fleet service answers questions about one household's electricity, and
//! two of the answers are the household itself: `histd`'s Data Act export is
//! everything the product generated — when the shower ran, when the car charged,
//! which fortnight nobody was in. So *who is asking* is not a deployment detail
//! to be added later behind a reverse proxy.
//!
//! # Three questions, and they are separate on purpose
//!
//! A credential answers **who** (cryptography), a set of [`Capabilities`]
//! answers **what** they may do, and a [`SiteScope`] answers **which
//! households**. Collapsing any two of them is how this workspace has produced
//! the same bug twice:
//!
//! * `fleetd`'s roster was served to anybody who could reach the port, because
//!   "read the fleet" had no credential of its own (D108);
//! * `obsd`'s fleet summary was served to **any** valid credential — including
//!   one household's own box token — because "read the aggregate" had no *verb*
//!   of its own, so four call sites each spelled it themselves and one spelled
//!   it wrong. That summary names every household that failed to respect a
//!   network operator's reduction (D112).
//!
//! So every question a route asks is a **named method** here — `may_read`,
//! `may_write`, `may_read_everything`, `may_read_the_fleet` — and a route that
//! invents its own test is the bug, not the exception.
//!
//! # Capabilities, not market roles — and they attenuate
//!
//! `mako`'s authorisation is built on **Marktrollen** (LF, NB, MSB) because
//! `mako` is a market participant. hems is not one: it is a box on a wall and
//! the services around it (D8).
//!
//! What replaces them is a set of dotted capability patterns, deliberately in
//! the shape `agentplane::core::identity::Scope` uses, because that is the
//! runtime `agentd` will be built on (M8) and its authority model is the one
//! hems has to compose with rather than duplicate. Two pattern forms and no
//! more — `hems.record.read`, or `hems.record.*` — because attenuation has to be
//! decidable by **containment**, and a richer grammar (regex, negation) makes
//! containment undecidable in the general case.
//!
//! Attenuation is the property a closed `enum` of roles cannot express, and it
//! is the one an agent needs: an agent acting for an operator must be able to
//! hold **less** than that operator — reading a Nachweis without reading the
//! roster, say. `Role::Operator` delegated to an agent is still `Role::Operator`.
//! A capability set delegated to an agent is a subset, and
//! [`Capabilities::contains`] is what makes "no wider than its delegator"
//! checkable rather than reviewable.
//!
//! # Tenancy is a field on the principal, not a layer above it
//!
//! A [`SiteScope`] rides on every credential, so "which households does this
//! token reach" has one answer whatever the holder may do. agentplane calls this
//! a principal's **audience** and means the same thing by it, so the word here is
//! the tenant's name and a `Delegation` maps onto this type without a
//! translation table.
//!
//! A deployment hosting one operator's boxes writes [`SiteScope::Every`] and has
//! *said* so; one hosting several writes a tenant, and a token from one cannot
//! read the other's breach list.
//!
//! # Comparison is constant-time, and an empty set accepts nothing
//!
//! A token compared with `==` leaks where two differ, one byte of timing at a
//! time. And a service configured with no tokens rejects everything rather than
//! everyone: the deployment where somebody forgot the credentials is exactly the
//! one nobody would notice.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Secret;

/// Read one household's § 14a record — the evidence a Nachweis is built from.
pub const RECORD_READ: &str = "hems.record.read";
/// Write one household's record. A box's own, and nothing else's.
pub const RECORD_WRITE: &str = "hems.record.write";
/// Take the household's Data Act Article 4 export.
pub const EXPORT_READ: &str = "hems.export.read";
/// Read an answer about every household in scope — a summary, a roster.
pub const FLEET_READ: &str = "hems.fleet.read";

/// Every capability this workspace defines.
///
/// A catalogue for the same reason `hems-events` has one: a capability spelled
/// differently in the grant and the check is a system that runs, logs nothing
/// unusual and quietly does not work.
pub const ALL_CAPABILITIES: &[&str] = &[RECORD_READ, RECORD_WRITE, EXPORT_READ, FLEET_READ];

/// What a principal may do, as dotted patterns.
///
/// Two forms and only two: an exact capability (`hems.record.read`) or a prefix
/// family (`hems.record.*`). See the module note on why the grammar stops there.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities(BTreeSet<String>);

impl Capabilities {
    /// Everything. The root of a delegation chain, held by an owner.
    #[must_use]
    pub fn root() -> Self {
        Self(BTreeSet::from(["*".to_owned()]))
    }

    /// Nothing at all.
    ///
    /// Not an absent grant — an over-attenuated chain ends here, and it permits
    /// no capability rather than every one.
    #[must_use]
    pub fn none() -> Self {
        Self(BTreeSet::new())
    }

    /// Build from patterns.
    pub fn of<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(patterns.into_iter().map(Into::into).collect())
    }

    /// Whether nothing is granted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The patterns, for a log line or a delegation.
    pub fn patterns(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Whether this set permits `capability`.
    #[must_use]
    pub fn permits(&self, capability: &str) -> bool {
        self.0.iter().any(|p| covers(p, capability))
    }

    /// Whether this set covers everything `other` covers.
    ///
    /// The attenuation test, and the reason the grammar is two forms wide: a
    /// delegate may hold no more than its delegator, and that has to be
    /// decidable rather than argued about.
    #[must_use]
    pub fn contains(&self, other: &Self) -> bool {
        other.0.iter().all(|o| self.0.iter().any(|s| covers(s, o)))
    }

    /// Narrow this set to `to`, refusing anything wider.
    ///
    /// What `agentd` calls when it acts for somebody: the result is inside both,
    /// or there is no result. Widening is not representable rather than
    /// forbidden by review.
    ///
    /// # Errors
    /// The first pattern in `to` that this set does not cover.
    pub fn attenuate(&self, to: &Self) -> Result<Self, String> {
        match to.0.iter().find(|o| !self.0.iter().any(|s| covers(s, o))) {
            Some(widened) => Err(widened.clone()),
            None => Ok(to.clone()),
        }
    }
}

/// Whether pattern `a` covers everything pattern `b` can match.
///
/// Split out because the wildcard-versus-wildcard case is where this is easy to
/// get wrong: `hems.*` covers `hems.record.*`, and `hems.record.*` covers
/// neither `hems.*` nor `hems.export.read`.
fn covers(a: &str, b: &str) -> bool {
    if a == "*" {
        return true;
    }
    if b == "*" {
        // Only `*` covers `*`, and that was handled above.
        return false;
    }
    match (a.strip_suffix(".*"), b.strip_suffix(".*")) {
        // Both families: a's prefix must be a segment-prefix of b's.
        (Some(pa), Some(pb)) => pb == pa || (pb.starts_with(pa) && pb.as_bytes()[pa.len()] == b'.'),
        // A family against an exact capability.
        (Some(pa), None) => b == pa || (b.starts_with(pa) && b.as_bytes()[pa.len()] == b'.'),
        // An exact capability can never cover a family, however similar they look.
        (None, Some(_)) => false,
        (None, None) => a == b,
    }
}

/// Which households a credential reaches.
///
/// agentplane calls this a principal's *audience* and means the same thing: the
/// tenant a credential is spendable at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteScope {
    /// Exactly one household. A box's own credential.
    One(String),
    /// A named set of households — one tenant of a shared deployment.
    ///
    /// Resolved from configuration at start-up rather than looked up per
    /// request, so what a token reaches is a property of the token that can be
    /// printed, logged and tested, rather than the result of a join that might
    /// be missing a row.
    Tenant {
        /// What the tenant is called, for logs and for whoever reads them.
        name: String,
        /// Every household it covers.
        sites: BTreeSet<String>,
    },
    /// Every household this daemon knows.
    ///
    /// Correct for a **single-tenant** deployment — a Stadtwerk running hems for
    /// its own customers — and a cross-tenant read in any other. It is a named
    /// variant rather than an absent field so that a deployment which has it has
    /// said so, in configuration, where somebody can see it.
    Every,
}

impl SiteScope {
    /// Whether `site` is one of the households this scope covers.
    #[must_use]
    pub fn covers(&self, site: &str) -> bool {
        match self {
            Self::One(own) => own == site,
            Self::Tenant { sites, .. } => sites.contains(site),
            Self::Every => true,
        }
    }

    /// What to call this scope in a log line.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::One(own) => own,
            Self::Tenant { name, .. } => name,
            Self::Every => EVERY_TENANT,
        }
    }
}

/// What a presented credential is allowed to do, and to whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    subject: String,
    capabilities: Capabilities,
    sites: SiteScope,
}

impl Authority {
    /// A principal with exactly these capabilities over these households.
    #[must_use]
    pub fn new(subject: impl Into<String>, capabilities: Capabilities, sites: SiteScope) -> Self {
        Self {
            subject: subject.into(),
            capabilities,
            sites,
        }
    }

    /// A box's credential for its own household.
    ///
    /// It is the only principal that **writes** a record, because `[A1 7.2]` is
    /// the household's record of what a network operator commanded and what the
    /// connection point drew — an operator that could write it would be marking
    /// its own homework. It is also the only one that may take the Data Act
    /// export, because Article 4 of Regulation (EU) 2023/2854 is a right of the
    /// *user*. It holds no fleet capability: an aggregate over every household
    /// is not this household's data.
    #[must_use]
    pub fn box_at(site: impl Into<String>) -> Self {
        let site = site.into();
        Self::new(
            format!("site:{site}"),
            Capabilities::of([RECORD_READ, RECORD_WRITE, EXPORT_READ]),
            SiteScope::One(site),
        )
    }

    /// A fleet operator's credential over `sites`.
    ///
    /// Reads the § 14a record — which is what a network operator's Nachweis is —
    /// and reads the aggregate. Writes nothing, and never takes the household's
    /// Data Act export.
    #[must_use]
    pub fn operator(sites: SiteScope) -> Self {
        Self::new(
            format!("operator:{}", sites.name()),
            Capabilities::of([RECORD_READ, FLEET_READ]),
            sites,
        )
    }

    /// Who this is, for a log line and for a journal entry.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// What it may do.
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Which households it reaches.
    #[must_use]
    pub const fn sites(&self) -> &SiteScope {
        &self.sites
    }

    /// The same principal, narrowed.
    ///
    /// What an agent is admitted under: never wider than the credential it acts
    /// for, over both axes. The site scope narrows to `sites` only if that is
    /// inside the scope already held.
    ///
    /// # Errors
    /// The capability pattern or the site that would have widened it.
    pub fn attenuate(
        &self,
        subject: impl Into<String>,
        capabilities: &Capabilities,
        sites: SiteScope,
    ) -> Result<Self, String> {
        let capabilities = self.capabilities.attenuate(capabilities)?;
        let narrower = match (&self.sites, &sites) {
            (SiteScope::Every, _) => true,
            (_, SiteScope::Every) => false,
            (held, SiteScope::One(one)) => held.covers(one),
            (held, SiteScope::Tenant { sites, .. }) => sites.iter().all(|s| held.covers(s)),
        };
        if !narrower {
            return Err(format!(
                "site scope {:?} is wider than {:?}",
                sites, self.sites
            ));
        }
        Ok(Self::new(subject, capabilities, sites))
    }

    /// Whether this credential may read `site`'s § 14a record.
    #[must_use]
    pub fn may_read(&self, site: &str) -> bool {
        self.capabilities.permits(RECORD_READ) && self.sites.covers(site)
    }

    /// Whether this credential may **write** `site`'s record.
    #[must_use]
    pub fn may_write(&self, site: &str) -> bool {
        self.capabilities.permits(RECORD_WRITE) && self.sites.covers(site)
    }

    /// Whether this credential may read the household's Data Act export.
    ///
    /// The export is everything the product generated, including when the shower
    /// ran and which fortnight nobody was in.
    #[must_use]
    pub fn may_read_everything(&self, site: &str) -> bool {
        self.capabilities.permits(EXPORT_READ) && self.sites.covers(site)
    }

    /// Whether this credential may read an answer **about the whole scope** —
    /// `obsd`'s summary, `fleetd`'s roster.
    ///
    /// Not a household's own read, however wide its site scope: an aggregate
    /// over every household is not any one household's data. This is the verb
    /// whose absence caused the defect the module note describes.
    #[must_use]
    pub fn may_read_the_fleet(&self) -> bool {
        self.capabilities.permits(FLEET_READ)
    }
}

/// One operator credential as it is configured.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCredential {
    /// The credential, or an `env:`/`file:` reference to it (D82).
    pub token: Secret,
    /// Which tenant's households it reaches.
    ///
    /// A name from the tenant table, or `"*"` for every household this daemon
    /// knows. `"*"` is right for a single-tenant deployment and is a
    /// cross-tenant read in any other, which is why it has to be written down
    /// rather than being what happens when a field is missing.
    pub tenant: String,
}

/// The wildcard tenant: every household this daemon knows.
///
/// Not a legal tenant name, so it cannot collide with one.
pub const EVERY_TENANT: &str = "*";

/// The credentials a service accepts.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    sites: Vec<(String, String)>,
    operators: Vec<(SiteScope, String)>,
}

impl Credentials {
    /// Build from configured secrets, resolving every `env:`/`file:` reference.
    ///
    /// `tenants` maps a tenant name to the households it covers; an operator
    /// credential names one of them, or [`EVERY_TENANT`].
    ///
    /// # Errors
    /// [`crate::ConfigError::UnresolvedSecret`] when a reference names something
    /// that is not there — which stops the daemon rather than starting one that
    /// accepts a token nobody issued. [`crate::ConfigError::UnknownTenant`] when
    /// a credential names a tenant the table does not define, because a
    /// credential scoped to nothing reads no household and looks, at the other
    /// end, like a permissions problem rather than like a typo.
    pub fn resolve(
        sites: &BTreeMap<String, Secret>,
        tenants: &BTreeMap<String, BTreeSet<String>>,
        operators: &[OperatorCredential],
    ) -> Result<Self, crate::ConfigError> {
        Ok(Self {
            sites: sites
                .iter()
                .map(|(site, secret)| Ok((site.clone(), secret.resolve_from_process()?)))
                .collect::<Result<_, crate::ConfigError>>()?,
            operators: operators
                .iter()
                .map(|credential| {
                    let scope = if credential.tenant == EVERY_TENANT {
                        SiteScope::Every
                    } else {
                        let sites = tenants.get(&credential.tenant).cloned().ok_or_else(|| {
                            crate::ConfigError::UnknownTenant {
                                tenant: credential.tenant.clone(),
                            }
                        })?;
                        SiteScope::Tenant {
                            name: credential.tenant.clone(),
                            sites,
                        }
                    };
                    Ok((scope, credential.token.resolve_from_process()?))
                })
                .collect::<Result<_, crate::ConfigError>>()?,
        })
    }

    /// A credential set built in code, for a test.
    ///
    /// `resolve` is the production path and reads the process environment; this
    /// is the one that does not, so a test never has to mutate global state to
    /// say who is allowed to call.
    #[must_use]
    pub fn with_site(mut self, site: impl Into<String>, token: impl Into<String>) -> Self {
        self.sites.push((site.into(), token.into()));
        self
    }

    /// The same, for a fleet operator over every household this daemon knows.
    #[must_use]
    pub fn with_operator(self, token: impl Into<String>) -> Self {
        self.with_operator_of(SiteScope::Every, token)
    }

    /// The same, for an operator scoped to one tenant.
    #[must_use]
    pub fn with_operator_of(mut self, sites: SiteScope, token: impl Into<String>) -> Self {
        self.operators.push((sites, token.into()));
        self
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
                found = Some(Authority::box_at(site.clone()));
            }
        }
        for (scope, secret) in &self.operators {
            if constant_time_eq(token.as_bytes(), secret.as_bytes()) {
                found = found.or_else(|| Some(Authority::operator(scope.clone())));
            }
        }
        found
    }

    /// The authority behind an `Authorization` header, if any.
    ///
    /// One place, so a route that forgets the `Bearer ` prefix or the
    /// constant-time comparison is a route that does not compile rather than one
    /// that works in testing.
    #[must_use]
    pub fn authority_in(&self, header: Option<&str>) -> Option<Authority> {
        bearer(header).and_then(|token| self.authority_of(token))
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
        Credentials::default()
            .with_site("haus-1", "tok-1")
            .with_site("haus-2", "tok-2")
            .with_operator("tok-netz")
    }

    fn tenant(name: &str, sites: &[&str]) -> SiteScope {
        SiteScope::Tenant {
            name: name.to_owned(),
            sites: sites.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Two tenants on one daemon, which is the deployment the scope exists for.
    fn shared() -> Credentials {
        Credentials::default()
            .with_site("haus-1", "tok-1")
            .with_site("haus-3", "tok-3")
            .with_operator_of(tenant("nord", &["haus-1", "haus-2"]), "tok-nord")
            .with_operator_of(tenant("sued", &["haus-3"]), "tok-sued")
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
    fn a_household_never_reads_an_answer_about_every_household() {
        // The defect this verb exists to prevent. `obsd`'s summary carries the
        // named list of households that did not respect a network operator's
        // reduction, and `fleetd`'s roster says which households exist and which
        // are unpatched. Neither is any one household's data, however the
        // question is phrased — and a box asking about "the fleet" is not asking
        // about itself.
        let own = credentials().authority_of("tok-1").unwrap();
        assert!(
            !own.may_read_the_fleet(),
            "a box's own credential is not a fleet credential"
        );
        assert!(
            credentials()
                .authority_of("tok-netz")
                .unwrap()
                .may_read_the_fleet()
        );
    }

    #[test]
    fn one_tenants_operator_cannot_read_another_tenants_household() {
        // The whole point of a site scope. Both hold an operator credential on
        // the same daemon; neither is the other's operator.
        let nord = shared().authority_of("tok-nord").unwrap();
        assert!(nord.may_read("haus-1") && nord.may_read("haus-2"));
        assert!(
            !nord.may_read("haus-3"),
            "another tenant's household is not in this operator's scope"
        );

        let sued = shared().authority_of("tok-sued").unwrap();
        assert!(sued.may_read("haus-3"));
        assert!(!sued.may_read("haus-1"));

        // Both may ask for an aggregate; what each gets back is its own scope's,
        // which is the caller's obligation and is why `sites()` is public.
        assert!(nord.may_read_the_fleet() && sued.may_read_the_fleet());
        assert_eq!(nord.sites().name(), "nord");
        assert_eq!(sued.sites().name(), "sued");
    }

    #[test]
    fn an_unscoped_operator_is_a_deployment_that_said_so() {
        let every = credentials().authority_of("tok-netz").unwrap();
        assert_eq!(every.sites(), &SiteScope::Every);
        assert_eq!(every.sites().name(), "*");
        assert!(every.may_read("a-household-nobody-configured"));
    }

    #[test]
    fn a_credential_naming_a_tenant_nobody_defined_stops_the_daemon() {
        // Resolving it to the empty set would start a daemon that accepts the
        // token and reads no household — which at the other end looks like a
        // permissions problem and is a typo in a configuration file.
        let refused = Credentials::resolve(
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[OperatorCredential {
                token: Secret::literal("tok-nord"),
                tenant: "nord".into(),
            }],
        );
        assert!(matches!(
            refused,
            Err(crate::ConfigError::UnknownTenant { .. })
        ));
    }

    // ── Capability patterns ──────────────────────────────────────────────────

    #[test]
    fn a_family_covers_what_is_under_it_and_nothing_above() {
        let family = Capabilities::of(["hems.record.*"]);
        assert!(family.permits(RECORD_READ));
        assert!(family.permits(RECORD_WRITE));
        assert!(
            !family.permits(EXPORT_READ),
            "the Data Act export is not under `record`, and that is the point \
             of putting it in its own family"
        );
        assert!(!family.permits(FLEET_READ));

        assert!(Capabilities::root().permits(FLEET_READ));
        assert!(!Capabilities::none().permits(RECORD_READ));
    }

    #[test]
    fn an_exact_capability_never_covers_a_family() {
        // The case that is easy to get wrong, and the one that would silently
        // widen a delegation if it were.
        let exact = Capabilities::of([RECORD_READ]);
        assert!(!exact.contains(&Capabilities::of(["hems.record.*"])));
        assert!(Capabilities::of(["hems.record.*"]).contains(&exact));
        assert!(Capabilities::of(["hems.*"]).contains(&Capabilities::of(["hems.record.*"])));
        assert!(!Capabilities::of(["hems.record.*"]).contains(&Capabilities::of(["hems.*"])));

        // A prefix that is not a whole segment is not a prefix.
        assert!(!Capabilities::of(["hems.rec.*"]).permits(RECORD_READ));
    }

    #[test]
    fn an_agent_holds_less_than_whoever_it_acts_for() {
        // The property a closed set of roles cannot express, and the reason this
        // is patterns rather than an enum. `agentd` reads a Nachweis on an
        // operator's behalf without also inheriting the roster.
        let operator = credentials().authority_of("tok-netz").unwrap();

        let agent = operator
            .attenuate(
                "agent:nachweis-reader",
                &Capabilities::of([RECORD_READ]),
                SiteScope::One("haus-1".into()),
            )
            .expect("narrower on both axes");
        assert!(agent.may_read("haus-1"));
        assert!(!agent.may_read("haus-2"), "narrowed to one household");
        assert!(
            !agent.may_read_the_fleet(),
            "and it did not inherit the roster"
        );

        // Widening is refused rather than reviewed.
        let widened = operator.attenuate(
            "agent:greedy",
            &Capabilities::of([RECORD_WRITE]),
            SiteScope::Every,
        );
        assert!(
            widened.is_err(),
            "an operator cannot delegate a write it does not hold"
        );

        let own = credentials().authority_of("tok-1").unwrap();
        assert!(
            own.attenuate(
                "agent:x",
                &Capabilities::of([RECORD_READ]),
                SiteScope::Every
            )
            .is_err(),
            "nor may one household's agent widen to every household"
        );
    }

    #[test]
    fn a_token_nobody_issued_is_nobody() {
        assert!(credentials().authority_of("tok-invented").is_none());
        assert!(credentials().authority_of("").is_none());
        assert!(
            credentials()
                .authority_in(Some("Bearer tok-invented"))
                .is_none()
        );
        assert!(credentials().authority_in(None).is_none());
        assert!(credentials().authority_in(Some("Bearer tok-1")).is_some());
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
