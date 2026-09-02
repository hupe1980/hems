//! Which § 14a breaches share a cause.
//!
//! # The count is `obsd`'s; the correlation is the agent's
//!
//! `obsd` already reports every breach as a named finding with a site and a
//! date, and it is right to: one household in ten thousand is an incident with a
//! name, and a percentage reads as success. This specialist does not have a
//! second implementation of that rule — it reads the same days.
//!
//! What it adds is the pattern across them, and there are two worth having.
//!
//! **A breach on a box that was on the fallback is a different fault.** A
//! household that spent time with no plan the arbiter would follow was being
//! held by the guard's conservative assumptions, not by a plan — so a breach
//! there points at the planner's inputs (no prices, no sky, no history) rather
//! than at the device that overshot. An operator sent to look at a contactor
//! when the cause is an expired `forecastd` URL loses a day.
//!
//! **A ceiling below the `[A1 4.5]` minimum is not the box's fault at all.**
//! hems applies such a command, because refusing a network operator is not a
//! decision a box takes — but the minimum is the customer's entitlement and
//! somebody has to see that it was not honoured. Grouped by date, because a
//! command below the minimum arriving at many households on one day is one
//! operator's mistake rather than many households' bad luck.

use agentplane::prelude::*;
use hems_core::report::DayKpis;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::advice::{Advice, AtRisk, Proposal};

/// The days one run reads.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Days {
    /// Every day the fleet has on record, from any number of sites.
    pub days: Vec<DayKpis>,
}

/// The specialist.
#[derive(Debug, Default)]
pub struct ComplianceTriage;

/// The name this specialist is invoked under.
pub const NAME: &str = "compliance-triage";

#[async_trait]
impl Skill for ComplianceTriage {
    fn descriptor(&self) -> SkillDescriptor {
        SkillDescriptor::new(NAME)
    }

    async fn invoke(
        &self,
        _cx: &mut StepCtx<'_>,
        input: Tainted<Value>,
    ) -> Result<Outcome, SkillError> {
        let days: Days = match serde_json::from_value(input.peek().clone()) {
            Ok(days) => days,
            Err(error) => return Ok(Outcome::fail(format!("unreadable input: {error}"))),
        };
        let value = serde_json::to_value(triage(&days))
            .map_err(|error| SkillError::Other(error.to_string()))?;
        // The answer inherits the input's taint rather than being minted
        // clean: what a specialist says about untrusted data is untrusted, and
        // `Tainted` is what carries that through the journal.
        Ok(Outcome::done(input.map(|_| value)))
    }
}

/// The findings, ranked.
#[must_use]
pub fn triage(input: &Days) -> Proposal {
    let mut advice = Vec::new();

    // ── Breaches that happened while the box had no plan ─────────────────────
    let breached: Vec<&DayKpis> = input
        .days
        .iter()
        .filter(|d| !d.respected_the_grid)
        .collect();
    let unplanned: Vec<String> = breached
        .iter()
        .filter(|d| d.minutes_without_a_plan > 0)
        .map(|d| d.site.clone())
        .collect();

    if !breached.is_empty() {
        let sites: Vec<String> = breached.iter().map(|d| d.site.clone()).collect();
        // Only worth saying when it is most of them. "Three of forty also had no
        // plan" is a coincidence dressed as a finding.
        let dominant = unplanned.len() * 2 > breached.len();
        let headline = if dominant {
            format!(
                "{} of {} § 14a breaches were on boxes that also spent time with no plan",
                unplanned.len(),
                breached.len()
            )
        } else {
            format!(
                "{} § 14a breaches, with no single cause visible",
                breached.len()
            )
        };
        let suggested = if dominant {
            "look at the planner's inputs first — prices, the sky, the site's own \
             history — rather than at the devices that overshot"
                .to_owned()
        } else {
            "read the days one at a time; nothing here groups them".to_owned()
        };
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline,
            at_risk: AtRisk::Households(distinct(&sites)),
            evidence: Proposal::some_of(&sites),
            covers: breached.len(),
            suggested,
        });
    }

    // ── Ceilings below the minimum the customer is owed ──────────────────────
    let mut by_date: BTreeMap<time::Date, Vec<String>> = BTreeMap::new();
    for day in input.days.iter().filter(|d| d.below_minimum_commanded) {
        by_date.entry(day.date).or_default().push(day.site.clone());
    }
    for (date, sites) in by_date {
        // A command below the minimum reaching several households on one day is
        // one operator's mistake, not several households' bad luck.
        let many = sites.len() > 1;
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "on {date}, {} {} commanded below the [A1 4.5] minimum",
                sites.len(),
                if many {
                    "households were"
                } else {
                    "household was"
                }
            ),
            at_risk: AtRisk::Households(distinct(&sites)),
            evidence: Proposal::some_of(&sites),
            covers: sites.len(),
            suggested: if many {
                "one command reached several households, so ask the network \
                 operator rather than the boxes — hems applied it, because \
                 refusing is not a box's decision, and the minimum is the \
                 customer's entitlement"
                    .to_owned()
            } else {
                "check what the network operator commanded against the minimum \
                 this household is owed"
                    .to_owned()
            },
        });
    }

    Proposal {
        advice,
        considered: input.days.len(),
    }
    .ranked()
}

/// How many distinct households, which is what "at risk" counts.
fn distinct(sites: &[String]) -> usize {
    sites
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn day(site: &str, on: time::Date) -> DayKpis {
        DayKpis {
            site: site.into(),
            date: on,
            respected_the_grid: true,
            ..DayKpis::default()
        }
    }

    #[test]
    fn a_breach_on_a_box_with_no_plan_points_at_the_planner() {
        // The correlation `obsd`'s counts cannot make. An operator sent to look
        // at a contactor when the cause is an expired `forecastd` URL loses a
        // day.
        let days = Days {
            days: (0..4)
                .map(|i| DayKpis {
                    respected_the_grid: false,
                    minutes_without_a_plan: 120,
                    ..day(&format!("haus-{i}"), date!(2026 - 01 - 15))
                })
                .chain(std::iter::once(DayKpis {
                    respected_the_grid: false,
                    ..day("haus-9", date!(2026 - 01 - 15))
                }))
                .collect(),
        };

        let proposal = triage(&days);
        let first = &proposal.advice[0];
        assert!(
            first.headline.contains("4 of 5"),
            "it says how many: {}",
            first.headline
        );
        assert!(first.suggested.contains("planner"), "{}", first.suggested);
        assert_eq!(first.at_risk, AtRisk::Households(5));
        assert_eq!(proposal.considered, 5);
    }

    #[test]
    fn a_scatter_of_breaches_is_not_dressed_up_as_a_cause() {
        // "Three of forty also had no plan" is a coincidence, and a queue that
        // reports coincidences as findings is one nobody reads.
        let days = Days {
            days: (0..10)
                .map(|i| DayKpis {
                    respected_the_grid: false,
                    minutes_without_a_plan: u32::from(i < 2) * 30,
                    ..day(&format!("haus-{i}"), date!(2026 - 01 - 15))
                })
                .collect(),
        };
        let proposal = triage(&days);
        assert!(
            proposal.advice[0].headline.contains("no single cause"),
            "{}",
            proposal.advice[0].headline
        );
    }

    #[test]
    fn one_command_below_the_minimum_reaching_many_households_is_one_mistake() {
        // Grouped by date, because that is what tells an operator to ask the
        // network operator rather than to check three boxes.
        let days = Days {
            days: (0..3)
                .map(|i| DayKpis {
                    below_minimum_commanded: true,
                    ..day(&format!("haus-{i}"), date!(2026 - 02 - 03))
                })
                .collect(),
        };
        let proposal = triage(&days);
        assert_eq!(proposal.advice.len(), 1, "one date, one finding");
        let a = &proposal.advice[0];
        assert!(a.headline.contains("2026-02-03"), "{}", a.headline);
        assert!(a.headline.contains("3 households were"), "{}", a.headline);
        assert!(a.suggested.contains("network operator"), "{}", a.suggested);
        assert!(
            a.suggested.contains("entitlement"),
            "and it says whose it is: {}",
            a.suggested
        );
    }

    #[test]
    fn a_compliant_fleet_produces_nothing_to_read() {
        // An advisory queue that always has something in it is one nobody reads.
        let days = Days {
            days: (0..20)
                .map(|i| day(&format!("haus-{i}"), date!(2026 - 01 - 15)))
                .collect(),
        };
        let proposal = triage(&days);
        assert!(proposal.advice.is_empty());
        assert_eq!(proposal.considered, 20);
    }

    #[test]
    fn the_evidence_is_bounded_and_says_how_many_it_stands_for() {
        let days = Days {
            days: (0..40)
                .map(|i| DayKpis {
                    respected_the_grid: false,
                    ..day(&format!("haus-{i}"), date!(2026 - 01 - 15))
                })
                .collect(),
        };
        let a = &triage(&days).advice[0];
        assert_eq!(a.evidence.len(), Proposal::EVIDENCE_SHOWN);
        assert_eq!(a.covers, 40, "and it says what it stands for");
    }
}
