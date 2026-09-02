//! What a specialist may say, and the reason it cannot say anything else.
//!
//! # Advisory only, as a property rather than a promise
//!
//! Every agent in this daemon **proposes**. The control planes decide: the guard
//! bounds what a household may draw, the arbiter chooses a setpoint inside that
//! bound, and neither of them reads anything in this module. Nothing an agent
//! says moves a watt.
//!
//! Written down, that is a promise somebody has to keep. Two things make it a
//! property instead.
//!
//! **The output type is a leaf.** [`Advice`] carries observations and a
//! suggestion for a human. It has no method that returns a `Setpoint`, a `Plan`,
//! a `GuardVerdict` or an override, and nothing in this workspace consumes one —
//! so there is no path from an agent's answer into a device. A reviewer checks
//! that by looking at what `Advice` can be turned into, which is nothing.
//!
//! **The principal cannot hold a capability that writes.** A specialist runs
//! under an authority derived from an operator's by
//! [`hems_service::Authority::attenuate`], which refuses to widen either axis,
//! and [`advisory`] is the only constructor here. A test asserts what is not
//! reachable from it.
//!
//! # And it may not read the household's Data Act export
//!
//! The sharper of the two exclusions, and the one worth stating on its own.
//! `hems_service::auth::EXPORT_READ` opens everything the product generated:
//! when the shower ran, when the car charged, which fortnight nobody was in.
//! Article 4 of Regulation (EU) 2023/2854 is a right of the **user**, and an
//! advisory agent is not one — so it is absent from [`ADVISORY`] deliberately
//! rather than by omission, and the test says so.
//!
//! # Why the advice is ranked by a quantity and not by a score
//!
//! A triage that returns "high, medium, low" has invented a scale. The
//! quantities here are the workspace's own — households, minutes on the
//! fallback, days of record — so an operator comparing two findings is comparing
//! the same units they will be asked about.
//!
//! And **households are counted, never averaged**, which is the same rule
//! `obsd` follows for the same reason: one household in ten thousand that failed
//! to respect a network operator's reduction is an incident with a name, and
//! "99,99 %" is the same fact and reads as success.

use hems_service::auth::{FLEET_READ, RECORD_READ};
use hems_service::{Authority, Capabilities};
use serde::{Deserialize, Serialize};

/// What is at stake in a finding, in the workspace's own units.
///
/// Not a severity. A severity is a judgement this daemon is not entitled to
/// make; a quantity is a fact, and two of the same kind can be compared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtRisk {
    /// Households affected.
    ///
    /// The § 14a unit. Counted and never turned into a rate.
    Households(usize),
    /// Minutes households spent with no plan the arbiter would follow.
    Minutes(u64),
    /// Days of record a conclusion rests on — or does not.
    Days(usize),
}

impl core::fmt::Display for AtRisk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Households(1) => write!(f, "1 household"),
            Self::Households(n) => write!(f, "{n} households"),
            Self::Minutes(n) => write!(f, "{n} min"),
            Self::Days(1) => write!(f, "1 day"),
            Self::Days(n) => write!(f, "{n} days"),
        }
    }
}

/// One thing a specialist noticed, and what it suggests a human do about it.
///
/// A leaf type. See the module note: nothing consumes one, and that is the
/// guarantee rather than an omission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advice {
    /// Which specialist said it.
    pub specialist: String,
    /// One line an operator reads first.
    pub headline: String,
    /// What is at stake.
    pub at_risk: AtRisk,
    /// The sites this is about, named so somebody can go and look.
    ///
    /// Bounded: a finding covering four hundred households is not made more
    /// useful by listing all four hundred. [`Proposal::EVIDENCE_SHOWN`] is the
    /// cut and [`Advice::covers`] says how many there really were.
    pub evidence: Vec<String>,
    /// How many the finding covers, of which [`Self::evidence`] names some.
    pub covers: usize,
    /// What a human might do. **A suggestion, never an instruction to a
    /// machine**: nothing in this workspace reads this field.
    pub suggested: String,
}

/// Everything a specialist produced in one run.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Proposal {
    /// The advice, most at stake first within a kind.
    pub advice: Vec<Advice>,
    /// How many days the specialist read.
    pub considered: usize,
}

impl Proposal {
    /// How many sites an [`Advice`] names before it says "and n more".
    ///
    /// An operator queue that scrolls is one nobody reads.
    pub const EVIDENCE_SHOWN: usize = 5;

    /// Sort so the largest quantity is first, within a kind.
    ///
    /// Households are compared with households and minutes with minutes. Two
    /// different kinds are **not** ranked against each other, because there is
    /// no exchange rate between a household and a minute that this daemon is
    /// entitled to invent — so they are grouped, and each group is ordered.
    #[must_use]
    pub fn ranked(mut self) -> Self {
        self.advice.sort_by(|a, b| {
            kind_order(&a.at_risk)
                .cmp(&kind_order(&b.at_risk))
                .then_with(|| within_kind(&b.at_risk, &a.at_risk))
                .then_with(|| a.headline.cmp(&b.headline))
        });
        self
    }

    /// One line per piece of advice, for an operator queue.
    pub fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.advice.iter().map(|advice| {
            format!(
                "[{}] {} ({} at stake across {}) — {}",
                advice.specialist, advice.headline, advice.at_risk, advice.covers, advice.suggested
            )
        })
    }

    /// Name at most [`Self::EVIDENCE_SHOWN`] of `sites`.
    #[must_use]
    pub fn some_of(sites: &[String]) -> Vec<String> {
        sites.iter().take(Self::EVIDENCE_SHOWN).cloned().collect()
    }
}

/// § 14a first, because it is the one with a regulator behind it.
const fn kind_order(at_risk: &AtRisk) -> u8 {
    match at_risk {
        AtRisk::Households(_) => 0,
        AtRisk::Minutes(_) => 1,
        AtRisk::Days(_) => 2,
    }
}

fn within_kind(a: &AtRisk, b: &AtRisk) -> core::cmp::Ordering {
    match (a, b) {
        (AtRisk::Households(a), AtRisk::Households(b)) | (AtRisk::Days(a), AtRisk::Days(b)) => {
            a.cmp(b)
        }
        (AtRisk::Minutes(a), AtRisk::Minutes(b)) => a.cmp(b),
        _ => core::cmp::Ordering::Equal,
    }
}

/// The capabilities a specialist may hold. Every one of them reads.
///
/// A `const` list rather than a set built at each call site, because the
/// guarantee this daemon makes is about *this* list and a test asserts what is
/// not in it — including `EXPORT_READ`, for the reason in the module note.
pub const ADVISORY: &[&str] = &[RECORD_READ, FLEET_READ];

/// An authority a specialist may run under, derived from an operator's own.
///
/// `None` when the operator does not itself hold every advisory capability,
/// because [`Authority::attenuate`] refuses to widen — and a delegation that
/// quietly granted less than it was asked for is a permission error that
/// surfaces somewhere else entirely.
///
/// This is where "advisory only" stops being a promise. An agent that wanted to
/// write a household's § 14a record would need an authority holding
/// `hems_service::auth::RECORD_WRITE`, and the only constructor in this daemon
/// is this one.
#[must_use]
pub fn advisory(operator: &Authority) -> Option<Authority> {
    operator
        .attenuate(
            format!("agent:{}", operator.subject()),
            &Capabilities::of(ADVISORY.iter().copied()),
            operator.sites().clone(),
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_service::SiteScope;
    use hems_service::auth::{EXPORT_READ, RECORD_WRITE};

    fn operator() -> Authority {
        Authority::operator(SiteScope::Every)
    }

    #[test]
    fn no_specialist_can_hold_a_capability_that_writes() {
        // The advisory-only rule, as a test rather than a paragraph.
        let agent = advisory(&operator()).expect("an operator holds the advisory set");
        assert!(
            !agent.capabilities().permits(RECORD_WRITE),
            "an advisory authority must not be able to write a household's record"
        );
        for capability in ADVISORY {
            assert!(agent.capabilities().permits(capability), "{capability}");
        }
    }

    #[test]
    fn no_specialist_can_take_a_households_data_act_export() {
        // The sharper exclusion. The export is everything the product
        // generated — when the shower ran, which fortnight nobody was in — and
        // Article 4 is a right of the *user*. An advisory agent is not one.
        let agent = advisory(&operator()).expect("derivable");
        assert!(!agent.capabilities().permits(EXPORT_READ));
        assert!(!agent.may_read_everything("haus-1"));
    }

    #[test]
    fn an_agent_reaches_no_further_than_the_operator_it_acts_for() {
        let nord = Authority::operator(SiteScope::Tenant {
            name: "nord".into(),
            sites: ["haus-1".to_owned()].into_iter().collect(),
        });
        let agent = advisory(&nord).expect("derivable");
        assert!(agent.may_read("haus-1"));
        assert!(
            !agent.may_read("haus-2"),
            "another tenant's household is not in the operator's scope, so it \
             is not in its agent's either"
        );
    }

    #[test]
    fn an_agent_cannot_be_derived_from_a_principal_that_holds_less() {
        // A box's own credential does not hold the fleet capability, so no
        // advisory agent can be derived from one. A delegation that quietly
        // granted less would be a permission error surfacing somewhere else.
        assert!(advisory(&Authority::box_at("haus-1")).is_none());
    }

    #[test]
    fn advice_is_ranked_by_a_quantity_and_never_across_kinds() {
        let advice = |at_risk: AtRisk, headline: &str| Advice {
            specialist: "s".into(),
            headline: headline.into(),
            at_risk,
            evidence: Vec::new(),
            covers: 1,
            suggested: String::new(),
        };
        let ranked = Proposal {
            advice: vec![
                advice(AtRisk::Minutes(10), "few minutes"),
                advice(AtRisk::Days(99), "many days"),
                advice(AtRisk::Minutes(400), "many minutes"),
                advice(AtRisk::Households(2), "two households"),
            ],
            considered: 4,
        }
        .ranked();

        let order: Vec<&str> = ranked.advice.iter().map(|a| a.headline.as_str()).collect();
        assert_eq!(
            order,
            vec!["two households", "many minutes", "few minutes", "many days"],
            "§ 14a first, then minutes largest-first, then days"
        );
    }

    #[test]
    fn one_household_is_not_pluralised() {
        assert_eq!(AtRisk::Households(1).to_string(), "1 household");
        assert_eq!(AtRisk::Households(3).to_string(), "3 households");
        assert_eq!(AtRisk::Minutes(90).to_string(), "90 min");
    }
}
