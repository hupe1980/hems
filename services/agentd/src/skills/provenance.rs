//! What the fleet's headline saving figure actually rests on.
//!
//! # The number is `obsd`'s; what stands behind it is the agent's
//!
//! `obsd` computes `saving_eur` over `measured_days` and reports the two
//! exclusions beside it. Everything it says is true. What it cannot do is notice
//! that the ratio between them has become absurd — that a fleet of four thousand
//! households is quoting a saving computed from three simulated days — because
//! that is a judgement about a population and `obsd` deals in exact counts.
//!
//! Two things make a saving figure not worth quoting, and they are different:
//!
//! * **it rests on simulations.** A box on a wall reports no baseline, because a
//!   baseline is what the day would have cost with no energy manager and only a
//!   simulator can re-run a day (D116). So every day from real hardware is
//!   excluded, and a fleet whose excluded days outnumber its measured ones is
//!   publishing a figure about `hemsd simulate`;
//! * **it rests on days the planner was shown the answer to.** A
//!   perfect-foresight day is an upper bound no household reaches — worth 60 %
//!   of the reference winter day's saving — and `obsd` already excludes them.
//!   What is worth saying is when they are most of what was *reported*, because
//!   that is a back-test being mistaken for a fleet.
//!
//! # And the forecast scores need days, not slots
//!
//! Forecast error is correlated across a day, so ninety-six quarter hours of one
//! Tuesday are close to one draw. Below twenty independent episodes a coverage
//! figure is a coin toss quoted to three significant figures (R22), and
//! `forecast_is_calibrated` already says so — but an operator reading a
//! dashboard sees the coverage first. This says how far off twenty it is.

use agentplane::prelude::*;
use hems_core::report::DayKpis;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::advice::{Advice, AtRisk, Proposal};

/// How many independent days a coverage figure needs before it is one.
///
/// Twenty, which is `hems_forecast`'s own threshold and not a second opinion
/// about it.
pub const EPISODES_FOR_A_CALIBRATION: usize = 20;

/// The days one run reads.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Days {
    /// Every day the fleet has on record.
    pub days: Vec<DayKpis>,
}

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
        let days: Days = match serde_json::from_value(input.peek().clone()) {
            Ok(days) => days,
            Err(error) => return Ok(Outcome::fail(format!("unreadable input: {error}"))),
        };
        let value = serde_json::to_value(review(&days))
            .map_err(|error| SkillError::Other(error.to_string()))?;
        // The answer inherits the input's taint rather than being minted
        // clean: what a specialist says about untrusted data is untrusted, and
        // `Tainted` is what carries that through the journal.
        Ok(Outcome::done(input.map(|_| value)))
    }
}

/// The findings, ranked.
#[must_use]
pub fn review(input: &Days) -> Proposal {
    let mut advice = Vec::new();

    let measured: Vec<&DayKpis> = input.days.iter().filter(|d| d.is_measurable()).collect();
    let foresight = input
        .days
        .iter()
        .filter(|d| d.foresight_was_perfect)
        .count();
    let no_baseline = input.days.len() - measured.len() - foresight;

    // A saving quoted from fewer days than it excludes is a saving about the
    // simulator. Said only when there is a saving being quoted at all.
    if !measured.is_empty() && no_baseline > measured.len() {
        let sites: Vec<String> = measured.iter().map(|d| d.site.clone()).collect();
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "the saving rests on {} modelled {} while {} from real boxes are excluded",
                measured.len(),
                if measured.len() == 1 { "day" } else { "days" },
                no_baseline
            ),
            at_risk: AtRisk::Days(measured.len()),
            evidence: Proposal::some_of(&sites),
            covers: measured.len(),
            suggested: "quote it as a simulation result, or say what share of the \
                        fleet it stands for — a box reports no baseline because a \
                        baseline is a counterfactual it cannot re-run"
                .to_owned(),
        });
    }

    // A back-test being mistaken for a fleet.
    if foresight > 0 && foresight * 2 > input.days.len() {
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "{foresight} of {} days on record were run with the weather known in advance",
                input.days.len()
            ),
            at_risk: AtRisk::Days(foresight),
            evidence: Vec::new(),
            covers: foresight,
            suggested: "this is a back-test rather than a fleet; a saving that \
                        included these days is an upper bound no household reaches"
                .to_owned(),
        });
    }

    // Whether the coverage figure beside the saving is a calibration at all.
    let scored = input.days.iter().filter(|d| d.forecast.is_some()).count();
    if scored > 0 && scored < EPISODES_FOR_A_CALIBRATION {
        advice.push(Advice {
            specialist: NAME.to_owned(),
            headline: format!(
                "the forecast coverage rests on {scored} scored {} of the {EPISODES_FOR_A_CALIBRATION} it needs",
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
        considered: input.days.len(),
    }
    .ranked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::report::{Economics, ForecastScores};
    use time::macros::date;

    fn from_a_box(site: &str) -> DayKpis {
        DayKpis {
            site: site.into(),
            date: date!(2026 - 01 - 15),
            ..DayKpis::default()
        }
    }

    fn modelled(site: &str) -> DayKpis {
        DayKpis {
            economics: Some(Economics::default()),
            ..from_a_box(site)
        }
    }

    #[test]
    fn a_saving_quoted_from_fewer_days_than_it_excludes_is_named_as_one() {
        // The judgement `obsd`'s exact counts cannot make: everything it reports
        // is true, and the ratio between the numbers is the finding.
        let days = Days {
            days: (0..3)
                .map(|i| modelled(&format!("sim-{i}")))
                .chain((0..40).map(|i| from_a_box(&format!("haus-{i}"))))
                .collect(),
        };
        let proposal = review(&days);
        let first = &proposal.advice[0];
        assert!(
            first.headline.contains("3 modelled days") && first.headline.contains("40"),
            "{}",
            first.headline
        );
        assert!(
            first.suggested.contains("counterfactual"),
            "{}",
            first.suggested
        );
    }

    #[test]
    fn a_fleet_whose_days_are_mostly_real_is_left_alone() {
        // The finding is the ratio, so it must not fire when the ratio is fine.
        let days = Days {
            days: (0..40)
                .map(|i| modelled(&format!("sim-{i}")))
                .chain((0..3).map(|i| from_a_box(&format!("haus-{i}"))))
                .collect(),
        };
        assert!(review(&days).advice.is_empty());
    }

    #[test]
    fn a_back_test_is_not_mistaken_for_a_fleet() {
        let days = Days {
            days: (0..10)
                .map(|i| DayKpis {
                    foresight_was_perfect: true,
                    ..modelled(&format!("sim-{i}"))
                })
                .chain((0..2).map(|i| modelled(&format!("real-{i}"))))
                .collect(),
        };
        let proposal = review(&days);
        let headlines: Vec<&str> = proposal
            .advice
            .iter()
            .map(|a| a.headline.as_str())
            .collect();
        assert!(
            headlines
                .iter()
                .any(|h| h.contains("weather known in advance")),
            "{headlines:?}"
        );
    }

    #[test]
    fn a_coverage_figure_says_how_far_off_a_calibration_it_is() {
        let days = Days {
            days: (0..4)
                .map(|i| DayKpis {
                    forecast: Some(ForecastScores {
                        pv_coverage: 0.8,
                        pv_crps: 100.0,
                        load_coverage: 0.8,
                        load_crps: 20.0,
                    }),
                    ..modelled(&format!("sim-{i}"))
                })
                .collect(),
        };
        let advice = review(&days);
        let finding = advice
            .advice
            .iter()
            .find(|a| a.headline.contains("coverage"))
            .expect("it says so");
        assert!(
            finding.headline.contains("4 scored days"),
            "{}",
            finding.headline
        );
        assert_eq!(finding.at_risk, AtRisk::Days(16), "sixteen short of twenty");
    }

    #[test]
    fn twenty_scored_days_is_no_longer_worth_saying() {
        let days = Days {
            days: (0..20)
                .map(|i| DayKpis {
                    forecast: Some(ForecastScores::default()),
                    ..modelled(&format!("sim-{i}"))
                })
                .collect(),
        };
        assert!(
            !review(&days)
                .advice
                .iter()
                .any(|a| a.headline.contains("coverage"))
        );
    }
}
