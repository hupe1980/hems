//! The aggregation, as a pure function.
//!
//! No socket, no clock of its own — `now` is a parameter, exactly as it is in
//! every domain crate of this workspace. What that buys is that "one household
//! in ten thousand breached a limit and the summary says so" is a unit test
//! rather than a thing somebody hopes.

use std::collections::BTreeMap;

use hems_core::report::DayKpis;
use time::{Date, OffsetDateTime};

/// One site's recent days, newest last.
#[derive(Debug, Clone, Default)]
pub struct SiteHistory {
    days: BTreeMap<Date, DayKpis>,
    last_report: Option<OffsetDateTime>,
}

impl SiteHistory {
    /// The days on record, oldest first.
    pub fn days(&self) -> impl Iterator<Item = &DayKpis> {
        self.days.values()
    }

    /// When this site last said anything.
    #[must_use]
    pub const fn last_report(&self) -> Option<OffsetDateTime> {
        self.last_report
    }
}

/// Every site's recent days.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    sites: BTreeMap<String, SiteHistory>,
    keep_days: usize,
}

impl Fleet {
    /// A fleet keeping `keep_days` per site.
    #[must_use]
    pub fn new(keep_days: usize) -> Self {
        Self {
            sites: BTreeMap::new(),
            keep_days: keep_days.max(1),
        }
    }

    /// Take in one day.
    ///
    /// A day that is already on record is **replaced**, not appended: a box that
    /// re-sends yesterday after a reconnect is correcting itself, and a fleet
    /// that counted it twice would double one household's saving inside an
    /// average.
    pub fn record(&mut self, day: DayKpis, at: OffsetDateTime) {
        let history = self.sites.entry(day.site.clone()).or_default();
        history.last_report = Some(at);
        history.days.insert(day.date, day);
        // The window is bounded, oldest first. `keep_days` is at least one and a
        // day was just inserted, so the map is never empty inside this loop.
        while history.days.len() > self.keep_days {
            let Some(oldest) = history.days.keys().next().copied() else {
                break;
            };
            history.days.remove(&oldest);
        }
    }

    /// How many sites have ever reported.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// Whether none has.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// One site's days.
    #[must_use]
    pub fn site(&self, site: &str) -> Option<&SiteHistory> {
        self.sites.get(site)
    }

    /// The whole fleet, summarised.
    #[must_use]
    pub fn summarise(&self, now: OffsetDateTime, silent_after: time::Duration) -> Summary {
        self.summarise_within(&hems_service::SiteScope::Every, now, silent_after)
    }

    /// The same, over the households one caller may see.
    ///
    /// A shared deployment hosts several operators, and a summary is the one
    /// answer where "which households" cannot be a parameter the caller
    /// chooses: it carries the **named list** of those that did not respect a
    /// network operator's reduction, so a scope that leaked would hand one
    /// tenant another's compliance incidents (D112).
    ///
    /// `sites` counts the households in scope rather than the households on
    /// record, because a denominator that included somebody else's would make
    /// every rate quietly wrong as well.
    #[must_use]
    pub fn summarise_within(
        &self,
        scope: &hems_service::SiteScope,
        now: OffsetDateTime,
        silent_after: time::Duration,
    ) -> Summary {
        let visible = || self.sites.iter().filter(|(name, _)| scope.covers(name));
        let mut summary = Summary {
            sites: visible().count(),
            ..Summary::default()
        };
        let mut pv = hems_forecast::Calibration::default();
        let mut load = hems_forecast::Calibration::default();

        for (name, history) in visible() {
            if history
                .last_report
                .is_none_or(|last| now - last > silent_after)
            {
                summary.silent.push(name.clone());
            }
            for day in history.days.values() {
                summary.days += 1;
                // A day the planner was shown the answer to is an upper bound
                // rather than a result, and averaging it into a saving figure
                // produces a number no household can reach. It is counted and
                // then left out.
                // Self-sufficiency is **metered**, so it is averaged over every
                // day on record rather than over the ones a saving can be
                // computed from. The two exclusions below exist because a saving
                // is a *counterfactual* — an upper bound, or a baseline nobody
                // re-ran — and neither says anything about how much of its own
                // electricity a household made. Tying it to `measured_days`
                // meant a fleet of real boxes, which never carry economics
                // (D116), reported a self-sufficiency of **nought** while every
                // one of them was reporting a perfectly good figure from its own
                // three meters (D127).
                summary.self_sufficiency += day.self_sufficiency;
                match (day.saving_eur(), day.bill_saving_eur()) {
                    (Some(saving), Some(bill)) if day.is_measurable() => {
                        summary.measured_days += 1;
                        summary.saving_eur += saving;
                        summary.bill_saving_eur += bill;
                    }
                    // Counted apart, because they are excluded for different
                    // reasons and an operator needs to know which. A foresight
                    // day is an upper bound; a day with no baseline is a real
                    // household, and a fleet of them means the saving figure
                    // above rests on simulations.
                    _ if day.foresight_was_perfect => summary.foresight_days += 1,
                    _ => summary.unmeasurable_days += 1,
                }
                // The forecast scores merge as **episodes**: one day is one
                // draw, whatever its slot count. A day nobody scored contributes
                // nothing rather than an episode that scored zero.
                if let Some(f) = day.forecast {
                    pv = pv.merge(one_episode(f.pv_coverage, f.pv_crps));
                    load = load.merge(one_episode(f.load_coverage, f.load_crps));
                }
                if !day.respected_the_grid {
                    summary.breached.push(Finding {
                        site: name.clone(),
                        date: day.date,
                        detail: format!(
                            "the netzwirksamer Leistungsbezug went {:.0} W over a commanded ceiling",
                            day.worst_overshoot_w
                        ),
                    });
                }
                if day.below_minimum_commanded {
                    summary.below_minimum.push(Finding {
                        site: name.clone(),
                        date: day.date,
                        detail: "a commanded ceiling was below the minimum of [A1 4.5]".into(),
                    });
                }
                if day.minutes_without_a_plan > 0 {
                    summary.unplanned_minutes += u64::from(day.minutes_without_a_plan);
                    summary.sites_without_a_plan.push(Finding {
                        site: name.clone(),
                        date: day.date,
                        detail: format!("{} minutes on the fallback", day.minutes_without_a_plan),
                    });
                }
                // The other seam between a decision and the world. A **named**
                // day rather than a rate, like every other finding here: a
                // wallbox that refuses a third of its commands is one household
                // with one installation problem, and a fleet average would put
                // it at half a per cent and read as fine.
                summary.clipped_kwh += day.clipped_kwh;
                if day.clipped_ticks > 0 {
                    summary.sites_clipping.push(Finding {
                        site: name.clone(),
                        date: day.date,
                        detail: format!(
                            "{} commands the hardware could not hold ({:.2} kWh)",
                            day.clipped_ticks, day.clipped_kwh
                        ),
                    });
                }
            }
        }

        if summary.measured_days > 0 {
            let n = summary.measured_days as f64;
            summary.saving_eur /= n;
            summary.bill_saving_eur /= n;
        }
        if summary.days > 0 {
            summary.self_sufficiency /= summary.days as f64;
        }
        summary.pv_coverage = pv.coverage;
        summary.pv_crps = pv.crps;
        summary.load_coverage = load.coverage;
        summary.load_crps = load.crps;
        summary.forecast_episodes = pv.episodes;
        summary.forecast_is_calibrated = pv.is_well_calibrated() && load.is_well_calibrated();
        summary
    }
}

/// One day's forecast score, as an episode a fleet can merge.
///
/// `Calibration::merge` counts **episodes** as well as samples, because one day
/// is one draw whatever its slot count; a fleet summary that merged slot counts
/// would call twenty days of one site the same evidence as one day of twenty.
fn one_episode(coverage: f64, crps: f64) -> hems_forecast::Calibration {
    hems_forecast::Calibration {
        coverage,
        crps,
        samples: 1,
        episodes: 1,
        ..hems_forecast::Calibration::default()
    }
}

/// One thing worth a human looking at.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    /// Which site.
    pub site: String,
    /// Which day.
    #[serde(with = "hems_core::wire::iso_date")]
    pub date: Date,
    /// What happened.
    pub detail: String,
}

/// The whole fleet in one answer.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Summary {
    /// How many sites have ever reported.
    pub sites: usize,
    /// How many days are on record.
    pub days: usize,
    /// How many of them a saving may be computed from.
    pub measured_days: usize,
    /// How many were run with the weather known in advance and left out.
    pub foresight_days: usize,
    /// How many carried **no baseline**, and were therefore left out too.
    ///
    /// A baseline is what the day would have cost with no energy manager, and it
    /// is a counterfactual: only a simulator can re-run a day as an unmanaged
    /// house. So a box on a wall reports none, and every day from real hardware
    /// lands here (D116).
    ///
    /// Reported rather than hidden, because `measured_days` silently excluding
    /// most of the fleet is exactly the shape of a number that reads as a
    /// measurement and is not one. A summary where this is large and
    /// `measured_days` is small is a saving figure computed from simulations.
    pub unmeasurable_days: usize,

    /// The mean saving per measured day, euros.
    pub saving_eur: f64,
    /// The same on the electricity bill alone.
    pub bill_saving_eur: f64,
    /// The mean self-sufficiency, over **[`Summary::days`]** — every day on
    /// record, not the ones a saving can be computed from.
    ///
    /// Its denominator is deliberately not [`Summary::measured_days`]. A saving
    /// is a counterfactual and needs a baseline; how much of its own electricity
    /// a household made is three meter readings, and every box reports them. The
    /// two were shared, and the consequence was that a fleet of real boxes —
    /// which never carry economics (D116) — reported nought (D127).
    pub self_sufficiency: f64,

    /// **Every** day a network operator's instruction was not respected.
    ///
    /// A list and never a rate: one household in ten thousand is a compliance
    /// incident with a name and a date, and "99,99 %" reads as success.
    pub breached: Vec<Finding>,
    /// Every day a commanded ceiling went below the minimum of `[A1 4.5]`.
    ///
    /// Not a fault of the box — hems applies such a command, because refusing a
    /// network operator is not a decision a box takes on its own — but the
    /// entitlement is the customer's and somebody has to be able to see it.
    pub below_minimum: Vec<Finding>,
    /// Every day a box spent time on the fallback.
    pub sites_without_a_plan: Vec<Finding>,
    /// How many minutes that came to across the fleet.
    pub unplanned_minutes: u64,
    /// Every day a device could not hold a command the arbiter gave it.
    ///
    /// The seam between the control loop and the wiring: a charge point is off
    /// or above the 6 A of IEC 61851 with nothing in between, so the arbiter
    /// routinely decides a value a device cannot hold and
    /// `hems_device::realisable` resolves it — correctly, and silently, while
    /// every layer above goes on believing the decided value. A **named** day
    /// rather than a rate, for the same reason `breached` is: a wallbox that
    /// refuses a third of its commands is one household with one installation
    /// problem, and a fleet average puts it at half a per cent.
    pub sites_clipping: Vec<Finding>,
    /// How much energy that came to across the fleet, kWh.
    pub clipped_kwh: f64,
    /// Sites that have not reported recently.
    pub silent: Vec<String>,

    /// The production band's coverage across every measured day.
    pub pv_coverage: f64,
    /// Its CRPS, watts.
    pub pv_crps: f64,
    /// The load band's coverage.
    pub load_coverage: f64,
    /// Its CRPS, watts.
    pub load_crps: f64,
    /// How many independent days the two rest on.
    pub forecast_episodes: usize,
    /// Whether that is enough days, and the bands the width they claim.
    pub forecast_is_calibrated: bool,
}

impl Summary {
    /// Whether anything here needs a human.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.breached.is_empty() && self.sites_without_a_plan.is_empty() && self.silent.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::CostBreakdown;
    use time::macros::{date, datetime};

    const NOW: OffsetDateTime = datetime!(2026-03-01 06:00:00 UTC);
    const SILENT_AFTER: time::Duration = time::Duration::days(2);

    fn day(site: &str, on: Date) -> DayKpis {
        DayKpis {
            site: site.into(),
            date: on,
            self_sufficiency: 0.5,
            economics: Some(hems_core::report::Economics {
                cost: CostBreakdown {
                    energy_eur: 20.0,
                    ..CostBreakdown::default()
                },
                baseline: CostBreakdown {
                    energy_eur: 22.0,
                    ..CostBreakdown::default()
                },
            }),
            forecast: Some(hems_core::report::ForecastScores {
                pv_coverage: 0.8,
                pv_crps: 100.0,
                load_coverage: 0.8,
                load_crps: 20.0,
            }),
            respected_the_grid: true,
            ..DayKpis::default()
        }
    }

    #[test]
    fn one_breach_in_a_thousand_is_a_finding_with_a_name_and_not_a_rate() {
        // The whole reason `breached` is a list. "99,9 % compliance" reads as
        // success and is how a compliance incident disappears.
        let mut fleet = Fleet::new(60);
        for i in 0..999 {
            fleet.record(day(&format!("site-{i}"), date!(2026 - 02 - 27)), NOW);
        }
        let mut bad = day("site-999", date!(2026 - 02 - 27));
        bad.respected_the_grid = false;
        bad.worst_overshoot_w = 850.0;
        fleet.record(bad, NOW);

        let summary = fleet.summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.sites, 1000);
        assert_eq!(summary.breached.len(), 1);
        assert_eq!(summary.breached[0].site, "site-999");
        assert!(summary.breached[0].detail.contains("850"));
        assert!(!summary.is_clean());
    }

    #[test]
    fn a_day_shown_the_weather_is_counted_and_then_left_out_of_the_saving() {
        let mut fleet = Fleet::new(60);
        fleet.record(day("a", date!(2026 - 02 - 27)), NOW);
        let mut cheat = day("a", date!(2026 - 02 - 28));
        cheat.foresight_was_perfect = true;
        cheat.economics = Some(hems_core::report::Economics {
            cost: CostBreakdown {
                energy_eur: 10.0,
                ..CostBreakdown::default()
            },
            baseline: CostBreakdown {
                energy_eur: 22.0,
                ..CostBreakdown::default()
            },
        });
        fleet.record(cheat, NOW);

        let summary = fleet.summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.days, 2);
        assert_eq!(summary.measured_days, 1);
        assert_eq!(summary.foresight_days, 1);
        assert!(
            (summary.saving_eur - 2.0).abs() < 1e-9,
            "the upper bound must not raise the average: {}",
            summary.saving_eur
        );
    }

    #[test]
    fn a_day_that_arrives_twice_is_a_correction_and_not_a_second_day() {
        // A box reconnecting and re-sending yesterday must not double one
        // household's saving inside an average.
        let mut fleet = Fleet::new(60);
        fleet.record(day("a", date!(2026 - 02 - 27)), NOW);
        let mut restated = day("a", date!(2026 - 02 - 27));
        restated.economics = Some(hems_core::report::Economics {
            cost: CostBreakdown {
                energy_eur: 21.0,
                ..CostBreakdown::default()
            },
            baseline: CostBreakdown {
                energy_eur: 22.0,
                ..CostBreakdown::default()
            },
        });
        fleet.record(restated, NOW);

        let summary = fleet.summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.days, 1);
        assert!(
            (summary.saving_eur - 1.0).abs() < 1e-9,
            "the later one stands"
        );
    }

    #[test]
    fn a_site_that_has_stopped_reporting_is_named() {
        let mut fleet = Fleet::new(60);
        fleet.record(
            day("quiet", date!(2026 - 02 - 20)),
            NOW - time::Duration::days(5),
        );
        fleet.record(day("busy", date!(2026 - 02 - 27)), NOW);
        let summary = fleet.summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.silent, vec!["quiet".to_owned()]);
        assert!(!summary.is_clean());
    }

    #[test]
    fn the_window_is_bounded_and_the_oldest_day_goes_first() {
        let mut fleet = Fleet::new(3);
        for d in 20..26 {
            fleet.record(
                day(
                    "a",
                    Date::from_calendar_date(2026, time::Month::February, d).unwrap(),
                ),
                NOW,
            );
        }
        let history = fleet.site("a").unwrap();
        assert_eq!(history.days().count(), 3);
        assert_eq!(
            history.days().next().unwrap().date,
            date!(2026 - 02 - 23),
            "the three newest"
        );
    }

    #[test]
    fn the_forecast_merges_as_episodes_so_twenty_days_can_answer_the_question() {
        // One day is one draw. Twenty of them is what makes
        // `is_well_calibrated` answerable rather than structurally false.
        let mut fleet = Fleet::new(60);
        for d in 1..=20 {
            fleet.record(
                day(
                    "a",
                    Date::from_calendar_date(2026, time::Month::February, d).unwrap(),
                ),
                NOW,
            );
        }
        let summary = fleet.summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.forecast_episodes, 20);
        assert!((summary.pv_coverage - 0.8).abs() < 1e-9);
        assert!(summary.forecast_is_calibrated);
    }

    #[test]
    fn too_few_days_cannot_call_the_forecast_either_way() {
        let mut fleet = Fleet::new(60);
        fleet.record(day("a", date!(2026 - 02 - 27)), NOW);
        assert!(!fleet.summarise(NOW, SILENT_AFTER).forecast_is_calibrated);
    }

    #[test]
    fn an_empty_fleet_summarises_to_nothing_rather_than_to_a_division_by_zero() {
        let summary = Fleet::new(60).summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.sites, 0);
        assert!((summary.saving_eur - 0.0).abs() < f64::EPSILON);
        assert!(summary.is_clean());
    }
}

#[cfg(test)]
mod what_a_real_box_reports {
    use super::*;
    use hems_core::report::ForecastScores;

    const NOW: OffsetDateTime = time::macros::datetime!(2026-01-16 08:00:00 UTC);
    const SILENT_AFTER: time::Duration = time::Duration::days(2);

    /// A day from hardware: what happened, and no counterfactual.
    fn from_a_box(site: &str, day: u8) -> DayKpis {
        DayKpis {
            site: site.into(),
            date: Date::from_calendar_date(2026, time::Month::January, day).unwrap(),
            imported_kwh: 14.2,
            self_sufficiency: 0.31,
            // No `economics`: five of the six cost terms are modelled and the
            // baseline is a counterfactual. A box meters, and reports what it
            // metered.
            ..DayKpis::default()
        }
    }

    /// A day from the simulator: it modelled the money and scored its bands.
    fn from_the_simulator(site: &str, day: u8) -> DayKpis {
        DayKpis {
            economics: Some(hems_core::report::Economics {
                cost: hems_core::plan::CostBreakdown {
                    energy_eur: 21.08,
                    ..Default::default()
                },
                baseline: hems_core::plan::CostBreakdown {
                    energy_eur: 24.12,
                    ..Default::default()
                },
            }),
            forecast: Some(ForecastScores {
                pv_coverage: 0.8,
                pv_crps: 100.0,
                load_coverage: 0.8,
                load_crps: 20.0,
            }),
            ..from_a_box(site, day)
        }
    }

    #[test]
    fn a_fleet_of_real_boxes_reports_no_saving_rather_than_a_saving_of_zero() {
        // The failure this exists to prevent. Every day from hardware carries no
        // baseline, so a fleet that treated a missing one as zero would divide a
        // real saving by a denominator full of households that never had a
        // counterfactual — and publish a number that gets smaller the more
        // customers there are.
        let mut fleet = Fleet::new(60);
        for i in 0..5 {
            fleet.record(from_a_box(&format!("haus-{i}"), 15), NOW);
        }
        let summary = fleet.summarise(NOW, SILENT_AFTER);

        assert_eq!(summary.days, 5);
        assert_eq!(summary.measured_days, 0, "none of them may be averaged");
        assert_eq!(summary.unmeasurable_days, 5, "and the summary says so");
        assert_eq!(
            summary.foresight_days, 0,
            "for a different reason than this"
        );
        assert!(
            (summary.saving_eur - 0.0).abs() < f64::EPSILON,
            "a mean over nothing is zero, and `measured_days` is what says it is \
             a mean over nothing"
        );
        // …and the **metered** half is still reported. Self-sufficiency shared
        // `measured_days` as its denominator, so a fleet of real households — the
        // only kind there will ever be — published nought while every box was
        // reporting 31 % off its own three meters (D127). A saving is a
        // counterfactual; how much of its own electricity a house made is not.
        assert!(
            (summary.self_sufficiency - 0.31).abs() < 1e-9,
            "the meters answered even though the money could not: {}",
            summary.self_sufficiency
        );
    }

    #[test]
    fn the_metered_and_the_modelled_halves_have_different_denominators() {
        // A mixed fleet is the case that separates them: two households with a
        // simulator behind them and three real ones. The saving is a mean over
        // the two that have a baseline; the self-sufficiency is a mean over all
        // five, because all five metered it.
        let mut fleet = Fleet::new(60);
        for i in 0..2 {
            fleet.record(from_the_simulator(&format!("sim-{i}"), 15), NOW);
        }
        for i in 0..3 {
            fleet.record(
                DayKpis {
                    self_sufficiency: 0.61,
                    ..from_a_box(&format!("haus-{i}"), 15)
                },
                NOW,
            );
        }
        let summary = fleet.summarise(NOW, SILENT_AFTER);

        assert_eq!(summary.days, 5);
        assert_eq!(summary.measured_days, 2);
        assert!(
            (summary.saving_eur - 3.04).abs() < 1e-9,
            "over the two with a counterfactual: {}",
            summary.saving_eur
        );
        assert!(
            (summary.self_sufficiency - (0.31 * 2.0 + 0.61 * 3.0) / 5.0).abs() < 1e-9,
            "over all five: {}",
            summary.self_sufficiency
        );
    }

    #[test]
    fn an_unscored_day_does_not_drag_the_calibration_down() {
        // Boxes report no forecast score; the simulator does. Merging the boxes
        // as episodes that scored zero would put the fleet's coverage at 0,17
        // and call it a measurement.
        let mut fleet = Fleet::new(60);
        fleet.record(from_the_simulator("sim", 15), NOW);
        for i in 0..5 {
            fleet.record(from_a_box(&format!("haus-{i}"), 15), NOW);
        }
        let summary = fleet.summarise(NOW, SILENT_AFTER);

        assert_eq!(
            summary.forecast_episodes, 1,
            "one day was scored, so there is one draw"
        );
        assert!(
            (summary.pv_coverage - 0.8).abs() < 1e-9,
            "and the coverage is that day's, not it divided by six: {}",
            summary.pv_coverage
        );
    }

    #[test]
    fn the_two_exclusions_are_counted_apart() {
        // They mean different things. A foresight day is an upper bound nobody
        // can reach; a day with no baseline is a real household. A summary that
        // added them would hide which of the two a small `measured_days` meant.
        let mut fleet = Fleet::new(60);
        fleet.record(from_the_simulator("sim-1", 15), NOW);
        fleet.record(
            DayKpis {
                foresight_was_perfect: true,
                ..from_the_simulator("sim-2", 15)
            },
            NOW,
        );
        fleet.record(from_a_box("haus-1", 15), NOW);

        let summary = fleet.summarise(NOW, SILENT_AFTER);
        assert_eq!(summary.days, 3);
        assert_eq!(summary.measured_days, 1);
        assert_eq!(summary.foresight_days, 1);
        assert_eq!(summary.unmeasurable_days, 1);
    }
}
