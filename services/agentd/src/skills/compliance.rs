//! Which statutory breaches share a cause.
//!
//! # The lists are `obsd`'s; the correlation is the agent's
//!
//! `obsd` reports every breach as a named finding with a site and a date, and it
//! is right to: one household in ten thousand is an incident with a name, and a
//! percentage reads as success. This specialist does not re-implement that rule;
//! it reads the same lists and adds the pattern across them.
//!
//! **A breach on a box that was on the fallback is a different fault.** A
//! household with no plan the arbiter would follow was held by the guard's
//! conservative assumptions rather than by a plan, so the breach points at the
//! planner's inputs — no prices, no sky, no history — and not at the device that
//! overshot. An operator sent to a contactor when the cause is an expired
//! `forecastd` URL loses a day. It is a set intersection on `(site, date)`,
//! which two independent lists cannot state and a reader of both can.
//!
//! **A ceiling below the `[A1 4.5]` minimum is not the box's fault.** hems
//! applies such a command, because refusing a network operator is not a decision
//! a box takes — but the minimum is the customer's entitlement. Grouped by
//! **date**: one command reaching many households on one day is one operator's
//! mistake.
//!
//! **A roof over its § 9 EEG ceiling is grouped the other way.** Same connection
//! point, different rule and a different party to ask: § 9 Abs. 2 applies by
//! force of law to one plant's installed capacity and does not move, so the
//! grouping is by **site** — a roof that crosses it repeatedly met the same
//! misconfiguration each time, and that is an installer's visit.

use agentplane::prelude::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use crate::advice::{Advice, AtRisk, Proposal};
use crate::upstream::Window;
use hems_core::report::Summary;

/// The specialist.
#[derive(Debug, Default)]
pub struct ComplianceTriage;

/// The name this specialist is invoked under.
pub const NAME: &str = "compliance-triage";

/// How many days a single roof has to be over its § 9 EEG ceiling before that is
/// a configuration rather than a run of bad afternoons.
///
/// Three. A § 9 Abs. 2 ceiling is a fixed fraction of installed capacity and does
/// not move, so a plant that respects it on Monday and crosses it on Tuesday met
/// two different skies; a plant that crosses it three times met the same
/// misconfiguration three times.
pub const REPEATS_BEFORE_ITS_THE_PLANT: usize = 3;

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
        let window: Window = match serde_json::from_value(input.peek().clone()) {
            Ok(window) => window,
            Err(error) => return Ok(Outcome::fail(format!("unreadable input: {error}"))),
        };
        let value = serde_json::to_value(triage(&window))
            .map_err(|error| SkillError::Other(error.to_string()))?;
        // The answer inherits the input's taint rather than being minted
        // clean: what a specialist says about untrusted data is untrusted, and
        // `Tainted` is what carries that through the journal.
        Ok(Outcome::done(input.map(|_| value)))
    }
}

/// The findings, ranked.
#[must_use]
pub fn triage(input: &Window) -> Proposal {
    let fleet = &input.fleet;
    let advice: Vec<Advice> = breaches_that_had_no_plan(fleet)
        .into_iter()
        .chain(ceilings_below_the_minimum(fleet))
        .chain(roofs_over_the_feed_in_ceiling(fleet))
        .collect();

    Proposal {
        advice,
        considered: fleet.days,
    }
    .ranked()
}

/// Two of `obsd`'s lists, intersected on the day.
///
/// Each list on its own is an exact answer; whether they name the same days is
/// the question neither contains. The intersection is on `(site, date)` and not
/// on the site alone — a household that breached in January and was on the
/// fallback in March is two facts, and calling them one is the failure this
/// exists to avoid.
fn breaches_that_had_no_plan(fleet: &Summary) -> Option<Advice> {
    if fleet.breached.is_empty() {
        return None;
    }
    let unplanned: BTreeSet<(&str, time::Date)> = fleet
        .without_a_plan
        .iter()
        .map(|f| (f.site.as_str(), f.date))
        .collect();
    let both = fleet
        .breached
        .iter()
        .filter(|f| unplanned.contains(&(f.site.as_str(), f.date)))
        .count();

    let sites: Vec<String> = fleet.breached.iter().map(|f| f.site.clone()).collect();
    let (evidence, covers) = Proposal::evidence_for(&sites);
    // Only worth saying when it is most of them. "Three of forty also had no
    // plan" is a coincidence dressed as a finding.
    let dominant = both * 2 > fleet.breached.len();
    Some(Advice {
        specialist: NAME.to_owned(),
        headline: if dominant {
            format!(
                "{both} of {} § 14a breaches were on boxes that also spent time with no plan",
                fleet.breached.len()
            )
        } else {
            format!(
                "{} § 14a breaches, with no single cause visible",
                fleet.breached.len()
            )
        },
        at_risk: AtRisk::Households(covers),
        evidence,
        covers,
        suggested: if dominant {
            "look at the planner's inputs first — prices, the sky, the site's own \
             history — rather than at the devices that overshot"
                .to_owned()
        } else {
            "read the days one at a time; nothing here groups them".to_owned()
        },
    })
}

/// Grouped by **date**, because a network operator sends one command to many
/// households: several on one day is one mistake rather than several households'
/// bad luck.
fn ceilings_below_the_minimum(fleet: &Summary) -> Vec<Advice> {
    let mut by_date: BTreeMap<time::Date, Vec<String>> = BTreeMap::new();
    for finding in &fleet.below_minimum {
        by_date
            .entry(finding.date)
            .or_default()
            .push(finding.site.clone());
    }
    by_date
        .into_iter()
        .map(|(date, sites)| {
            let (evidence, covers) = Proposal::evidence_for(&sites);
            let many = covers > 1;
            Advice {
                specialist: NAME.to_owned(),
                headline: format!(
                    "on {date}, {covers} {} commanded below the [A1 4.5] minimum",
                    if many {
                        "households were"
                    } else {
                        "household was"
                    }
                ),
                at_risk: AtRisk::Households(covers),
                evidence,
                covers,
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
            }
        })
        .collect()
}

/// The other axis. § 14a is grouped by *date* because one command reaches many
/// households; § 9 is grouped by **site**, because a ceiling is a property of one
/// plant and does not move — so the same roof crossing it repeatedly is a
/// configuration, and one roof crossing it once is an afternoon.
fn roofs_over_the_feed_in_ceiling(fleet: &Summary) -> Option<Advice> {
    let mut by_site: BTreeMap<&str, usize> = BTreeMap::new();
    for finding in &fleet.over_feed_in_ceiling {
        *by_site.entry(finding.site.as_str()).or_default() += 1;
    }
    let repeat: Vec<String> = by_site
        .iter()
        .filter(|(_, days)| **days >= REPEATS_BEFORE_ITS_THE_PLANT)
        .map(|(site, _)| (*site).to_owned())
        .collect();
    if repeat.is_empty() {
        return None;
    }
    let (evidence, covers) = Proposal::evidence_for(&repeat);
    let worst = by_site.values().copied().max().unwrap_or_default();
    Some(Advice {
        specialist: NAME.to_owned(),
        headline: format!(
            "{covers} {} fed in above the § 9 EEG ceiling on {REPEATS_BEFORE_ITS_THE_PLANT} \
             days or more (worst: {worst})",
            if covers == 1 { "roof" } else { "roofs" }
        ),
        at_risk: AtRisk::Households(covers),
        evidence,
        covers,
        suggested: "a § 9 Abs. 2 ceiling is a fixed fraction of installed \
                    capacity and does not move, so a plant that crosses it \
                    repeatedly is configured wrongly rather than unlucky — \
                    check the installed capacity the box was commissioned \
                    with against the plant that is actually there"
            .to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::report::{Finding, Summary};
    use time::macros::date;

    /// A finding as `obsd` names one.
    fn finding(site: &str, on: time::Date) -> Finding {
        Finding {
            site: site.into(),
            date: on,
            detail: "as obsd described it".into(),
        }
    }

    /// A window over a summary a test wrote, so the specialist is exercised
    /// through the same envelope the review loop hands it.
    fn window(fleet: Summary) -> Window {
        Window {
            source: "https://obsd.example/v1/fleet".into(),
            fetched_at: time::macros::datetime!(2026-01-20 06:00 UTC),
            fleet,
        }
    }

    #[test]
    fn a_breach_on_a_box_with_no_plan_points_at_the_planner() {
        // The correlation `obsd`'s two lists cannot make on their own. An
        // operator sent to look at a contactor when the cause is an expired
        // `forecastd` URL loses a day.
        let breached: Vec<Finding> = (0..5)
            .map(|i| finding(&format!("haus-{i}"), date!(2026 - 01 - 15)))
            .collect();
        let without_a_plan: Vec<Finding> = (0..4)
            .map(|i| finding(&format!("haus-{i}"), date!(2026 - 01 - 15)))
            .collect();
        let proposal = triage(&window(Summary {
            days: 200,
            breached,
            without_a_plan,
            ..Summary::default()
        }));

        let first = &proposal.advice[0];
        assert!(
            first.headline.contains("4 of 5"),
            "it says how many: {}",
            first.headline
        );
        assert!(first.suggested.contains("planner"), "{}", first.suggested);
        assert_eq!(first.at_risk, AtRisk::Households(5));
        assert_eq!(proposal.considered, 200, "the fleet's own denominator");
    }

    #[test]
    fn a_breach_and_a_missing_plan_on_different_days_are_not_the_same_finding() {
        // The intersection is on `(site, date)` and not on the site alone. A
        // household that breached in January and was on the fallback in March is
        // two facts, and calling them one is the whole failure this correlation
        // is meant to avoid.
        let proposal = triage(&window(Summary {
            days: 200,
            breached: (0..4)
                .map(|i| finding(&format!("haus-{i}"), date!(2026 - 01 - 15)))
                .collect(),
            without_a_plan: (0..4)
                .map(|i| finding(&format!("haus-{i}"), date!(2026 - 03 - 02)))
                .collect(),
            ..Summary::default()
        }));
        assert!(
            proposal.advice[0].headline.contains("no single cause"),
            "{}",
            proposal.advice[0].headline
        );
    }

    #[test]
    fn a_scatter_of_breaches_is_not_dressed_up_as_a_cause() {
        // "Three of forty also had no plan" is a coincidence, and a queue that
        // reports coincidences as findings is one nobody reads.
        let proposal = triage(&window(Summary {
            days: 200,
            breached: (0..10)
                .map(|i| finding(&format!("haus-{i}"), date!(2026 - 01 - 15)))
                .collect(),
            without_a_plan: (0..2)
                .map(|i| finding(&format!("haus-{i}"), date!(2026 - 01 - 15)))
                .collect(),
            ..Summary::default()
        }));
        assert!(
            proposal.advice[0].headline.contains("no single cause"),
            "{}",
            proposal.advice[0].headline
        );
    }

    #[test]
    fn one_household_that_breached_on_several_days_is_named_once_and_counted_once() {
        // The two numbers on one finding have to be about one set. `obsd` lists
        // a breach per *day*, so a household that breached three times appears
        // three times — and `at_risk` counts households.
        let proposal = triage(&window(Summary {
            days: 200,
            breached: vec![
                finding("haus-1", date!(2026 - 01 - 15)),
                finding("haus-1", date!(2026 - 01 - 16)),
                finding("haus-1", date!(2026 - 01 - 17)),
                finding("haus-2", date!(2026 - 01 - 17)),
            ],
            ..Summary::default()
        }));
        let a = &proposal.advice[0];
        assert_eq!(a.evidence, vec!["haus-1", "haus-2"], "each named once");
        assert_eq!(a.covers, 2, "and counted once");
        assert_eq!(a.at_risk, AtRisk::Households(2));
        assert!(
            a.headline.contains("4 § 14a breaches"),
            "the headline still counts the days, which is what a breach is: {}",
            a.headline
        );
    }

    #[test]
    fn one_command_below_the_minimum_reaching_many_households_is_one_mistake() {
        // Grouped by date, because that is what tells an operator to ask the
        // network operator rather than to check three boxes.
        let proposal = triage(&window(Summary {
            days: 200,
            below_minimum: (0..3)
                .map(|i| finding(&format!("haus-{i}"), date!(2026 - 02 - 03)))
                .collect(),
            ..Summary::default()
        }));
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
    fn a_roof_repeatedly_over_the_eeg_ceiling_is_a_plant_and_not_an_afternoon() {
        // The other statutory limit on the same connection point, grouped the
        // other way. A ceiling is a fixed fraction of installed capacity and does
        // not move, so the same roof crossing it three times met the same
        // misconfiguration three times.
        let mut over: Vec<Finding> = (0..4)
            .map(|i| {
                finding(
                    "haus-1",
                    date!(2026 - 06 - 01) + time::Duration::days(i64::from(i)),
                )
            })
            .collect();
        // …and one roof that crossed it once, which is an afternoon.
        over.push(finding("haus-9", date!(2026 - 06 - 03)));

        let proposal = triage(&window(Summary {
            days: 200,
            over_feed_in_ceiling: over,
            ..Summary::default()
        }));
        assert_eq!(proposal.advice.len(), 1, "one plant, one finding");
        let a = &proposal.advice[0];
        assert_eq!(a.evidence, vec!["haus-1"], "and not the unlucky one");
        assert!(a.headline.contains("worst: 4"), "{}", a.headline);
        assert!(
            a.suggested
                .contains("installed \n                        capacity")
                || a.suggested.contains("installed capacity"),
            "it says what to check: {}",
            a.suggested
        );
    }

    #[test]
    fn one_afternoon_over_the_eeg_ceiling_is_not_a_finding() {
        // The threshold has to be able to say nothing, or it is not a threshold.
        let proposal = triage(&window(Summary {
            days: 200,
            over_feed_in_ceiling: vec![
                finding("haus-1", date!(2026 - 06 - 01)),
                finding("haus-1", date!(2026 - 06 - 02)),
            ],
            ..Summary::default()
        }));
        assert!(
            proposal.advice.is_empty(),
            "two days is weather; three is a configuration: {:?}",
            proposal.advice
        );
    }

    #[test]
    fn a_compliant_fleet_produces_nothing_to_read() {
        // An advisory queue that always has something in it is one nobody reads.
        let proposal = triage(&window(Summary {
            days: 200,
            sites: 20,
            ..Summary::default()
        }));
        assert!(proposal.advice.is_empty());
        assert_eq!(proposal.considered, 200);
    }

    #[test]
    fn the_evidence_is_bounded_and_says_how_many_it_stands_for() {
        let proposal = triage(&window(Summary {
            days: 200,
            breached: (0..40)
                .map(|i| finding(&format!("haus-{i}"), date!(2026 - 01 - 15)))
                .collect(),
            ..Summary::default()
        }));
        let a = &proposal.advice[0];
        assert_eq!(a.evidence.len(), Proposal::EVIDENCE_SHOWN);
        assert_eq!(a.covers, 40, "and it says what it stands for");
    }
}
