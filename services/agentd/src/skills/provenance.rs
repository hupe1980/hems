//! What the fleet's headline saving figure rests on.
//!
//! # The numbers are `obsd`'s; what stands behind them is the agent's
//!
//! `obsd` computes `saving_eur` over `measured_days` and reports the exclusions
//! beside it. Everything it says is true. What it cannot do is notice that the
//! **ratio** between them has become absurd — that a fleet of four thousand
//! households is quoting a saving computed from three simulated days — because
//! that is a judgement about a population and `obsd` deals in exact counts.
//!
//! So this reads counts and compares them, and computes nothing `obsd` computes.
//! Two things make a saving figure not worth quoting, and they are different:
//!
//! * **it rests on simulations.** A box on a wall reports no baseline, because a
//!   baseline is a counterfactual only a simulator can re-run (D116). A fleet
//!   whose excluded days outnumber its measured ones is publishing a figure
//!   about `hemsd simulate`;
//! * **it rests on days the planner was shown the answer to.** A
//!   perfect-foresight day is an upper bound no household reaches, worth 60 % of
//!   the reference winter day's saving, and `obsd` already excludes them. Worth
//!   saying when they are most of what was *reported*: that is a back-test being
//!   mistaken for a fleet.
//!
//! And the forecast scores need days, not slots. Error is correlated across a
//! day, so ninety-six quarter hours of one Tuesday are close to one draw; below
//! twenty episodes a coverage figure is a coin toss quoted to three significant
//! figures (R22). `forecast_is_calibrated` says so — but a dashboard shows the
//! coverage first, so this says how far off twenty it is.
//!
//! No finding here names a site: each is a statement about a ratio of counts,
//! and evidence that cannot be acted on trains a reader to skip the field where
//! it can.

use agentplane::prelude::*;
use serde_json::Value;

use crate::advice::{Advice, AtRisk, Proposal};
use crate::upstream::Window;

/// How many independent days a coverage figure needs before it is one.
///
/// Twenty, which is `hems_forecast`'s own threshold and not a second opinion
/// about it.
pub const EPISODES_FOR_A_CALIBRATION: usize = 20;

/// The specialist.
#[derive(Debug, Default)]
pub struct SavingProvenance;

/// The name this specialist is invoked under.
pub const NAME: &str = "saving-provenance";

#[async_trait]
impl Skill for SavingProvenance {
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
        let value = serde_json::to_value(review(&window))
            .map_err(|error| SkillError::Other(error.to_string()))?;
        // The answer inherits the input's taint rather than being minted
        // clean: what a specialist says about untrusted data is untrusted, and
        // `Tainted` is what carries that through the journal.
        Ok(Outcome::done(input.map(|_| value)))
    }
}

/// The findings, ranked.
#[must_use]
pub fn review(input: &Window) -> Proposal {
    let fleet = &input.fleet;
    let mut advice = Vec::new();

    // A saving quoted from fewer days than it excludes is a saving about the
    // simulator. Said only when there is a saving being quoted at all.
    if fleet.measured_days > 0 && fleet.unmeasurable_days > fleet.measured_days {
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "the saving rests on {} modelled {} while {} from real boxes are excluded",
                fleet.measured_days,
                if fleet.measured_days == 1 {
                    "day"
                } else {
                    "days"
                },
                fleet.unmeasurable_days
            ),
            at_risk: AtRisk::Days(fleet.measured_days),
            evidence: Vec::new(),
            covers: fleet.measured_days,
            suggested: "quote it as a simulation result, or say what share of the \
                        fleet it stands for — a box reports no baseline because a \
                        baseline is a counterfactual it cannot re-run"
                .to_owned(),
        });
    }

    // A back-test being mistaken for a fleet.
    if fleet.foresight_days > 0 && fleet.foresight_days * 2 > fleet.days {
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "{} of {} days on record were run with the weather known in advance",
                fleet.foresight_days, fleet.days
            ),
            at_risk: AtRisk::Days(fleet.foresight_days),
            evidence: Vec::new(),
            covers: fleet.foresight_days,
            suggested: "this is a back-test rather than a fleet; a saving that \
                        included these days is an upper bound no household reaches"
                .to_owned(),
        });
    }

    // Whether the coverage figure beside the saving is a calibration at all.
    let scored = fleet.forecast_episodes;
    if scored > 0 && scored < EPISODES_FOR_A_CALIBRATION {
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "the forecast coverage rests on {scored} scored {} of the \
                 {EPISODES_FOR_A_CALIBRATION} it needs",
                if scored == 1 { "day" } else { "days" }
            ),
            at_risk: AtRisk::Days(EPISODES_FOR_A_CALIBRATION - scored),
            evidence: Vec::new(),
            covers: scored,
            suggested: "do not quote the coverage yet — forecast error is \
                        correlated across a day, so ninety-six quarter hours of \
                        one Tuesday are close to one draw"
                .to_owned(),
        });
    }

    Proposal {
        advice,
        considered: fleet.days,
    }
    .ranked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::report::Summary;

    /// A window over a summary a test wrote.
    fn window(fleet: Summary) -> Window {
        Window {
            source: "https://obsd.example/v1/fleet".into(),
            fetched_at: time::macros::datetime!(2026-01-20 06:00 UTC),
            fleet,
        }
    }

    #[test]
    fn a_saving_quoted_from_fewer_days_than_it_excludes_is_named_as_one() {
        // The judgement `obsd`'s exact counts cannot make: everything it reports
        // is true, and the ratio between the numbers is the finding.
        let proposal = review(&window(Summary {
            days: 43,
            measured_days: 3,
            unmeasurable_days: 40,
            ..Summary::default()
        }));
        let first = &proposal.advice[0];
        assert!(
            first.headline.contains("3 modelled days") && first.headline.contains("40"),
            "{}",
            first.headline
        );
        assert!(
            first.suggested.contains("counterfactual"),
            "and it says why a box cannot report one: {}",
            first.suggested
        );
        assert!(
            first.evidence.is_empty(),
            "a statement about a ratio of counts names no household"
        );
        assert_eq!(proposal.considered, 43);
    }

    #[test]
    fn a_fleet_whose_days_are_mostly_real_is_left_alone() {
        let proposal = review(&window(Summary {
            days: 60,
            measured_days: 40,
            unmeasurable_days: 20,
            forecast_episodes: 40,
            ..Summary::default()
        }));
        assert!(proposal.advice.is_empty(), "{:?}", proposal.advice);
    }

    #[test]
    fn a_back_test_is_not_mistaken_for_a_fleet() {
        // More than half the record run with the weather known in advance is an
        // upper bound no household reaches, quoted as a result.
        let proposal = review(&window(Summary {
            days: 10,
            measured_days: 3,
            foresight_days: 7,
            forecast_episodes: 20,
            ..Summary::default()
        }));
        let headlines: Vec<&str> = proposal
            .advice
            .iter()
            .map(|a| a.headline.as_str())
            .collect();
        assert!(
            headlines
                .iter()
                .any(|h| h.contains("7 of 10") && h.contains("known in advance")),
            "{headlines:?}"
        );
    }

    #[test]
    fn a_coverage_figure_says_how_far_off_a_calibration_it_is() {
        // Below twenty independent days a coverage figure is a coin toss quoted
        // to three significant figures (R22).
        let proposal = review(&window(Summary {
            days: 60,
            measured_days: 40,
            unmeasurable_days: 20,
            forecast_episodes: 4,
            ..Summary::default()
        }));
        let a = proposal
            .advice
            .iter()
            .find(|a| a.headline.contains("coverage"))
            .expect("a calibration finding");
        assert!(a.headline.contains("4 scored days"), "{}", a.headline);
        assert_eq!(
            a.at_risk,
            AtRisk::Days(EPISODES_FOR_A_CALIBRATION - 4),
            "what is at stake is the days it still needs"
        );
    }

    #[test]
    fn twenty_scored_days_is_no_longer_worth_saying() {
        // The threshold has to be able to say nothing.
        let proposal = review(&window(Summary {
            days: 60,
            measured_days: 40,
            unmeasurable_days: 20,
            forecast_episodes: EPISODES_FOR_A_CALIBRATION,
            ..Summary::default()
        }));
        assert!(proposal.advice.is_empty(), "{:?}", proposal.advice);
    }

    #[test]
    fn a_fleet_with_no_saving_at_all_is_not_told_its_saving_is_wrong() {
        // `measured_days == 0` is a fleet of real boxes, which is the ordinary
        // state and not a finding: there is no saving figure being quoted, so
        // there is nothing to say about what it rests on (D116).
        let proposal = review(&window(Summary {
            days: 4_000,
            sites: 4_000,
            unmeasurable_days: 4_000,
            forecast_episodes: 4_000,
            ..Summary::default()
        }));
        assert!(proposal.advice.is_empty(), "{:?}", proposal.advice);
    }
}
