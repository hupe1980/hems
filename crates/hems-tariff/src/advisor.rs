//! Which § 14a module would this household be better off on?
//!
//! Modul 1 is a lump sum, Modul 2 takes 60 % off the working price but adds a
//! second metering point, Modul 3 makes the working price depend on the time of
//! day. Which is best depends on how much the controllable devices consume and
//! *when* — so the answer is a property of the household's own history, not of a
//! rule of thumb, and the rules of thumb in circulation ("worth it above about
//! 3 000 kWh") are wrong by a factor of two between a cheap and an expensive
//! network area.
//!
//! Nobody selling a tariff can be relied on to run this comparison in the
//! customer's favour. A household's own energy manager can, and it is holding
//! the only data that answers it.

use std::collections::BTreeMap;

use hems_core::prelude::{Energy, Slot};
use rust_decimal::Decimal;

use crate::stack::price_at;
use crate::tariff::Tariff;

/// One candidate the household could switch to.
#[derive(Debug, Clone, PartialEq)]
pub struct ModulChoice {
    /// What to call it in the user interface.
    pub label: String,
    /// The tariff it implies.
    pub tariff: Tariff,
}

/// What one candidate would have cost.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Comparison {
    /// The candidate's label.
    pub label: String,
    /// What the measured consumption would have cost under it, euros.
    pub energy_cost_eur: Decimal,
    /// Fixed annual effects — a Modul 1 reduction, a Modul 2 metering charge.
    pub fixed_eur: Decimal,
    /// The two together.
    pub total_eur: Decimal,
    /// How much better or worse than the first candidate, euros. Negative is a
    /// saving.
    pub delta_eur: Decimal,
}

/// Price a measured consumption series under several candidates.
///
/// `consumption` is what the household actually drew, quarter hour by quarter
/// hour. The first candidate is the reference the others are compared against —
/// normally what the household is on today.
///
/// The comparison prices the *same* consumption under every candidate. That is
/// the honest comparison for Modul 1 and 2, which do not change behaviour. For
/// Modul 3 it is deliberately conservative: it shows what the household would
/// pay *without* shifting anything, so the number is a floor and any load
/// shifting only improves it.
#[must_use]
pub fn compare_moduls(
    consumption: &BTreeMap<Slot, Energy>,
    candidates: &[ModulChoice],
) -> Vec<Comparison> {
    let mut results: Vec<Comparison> = candidates
        .iter()
        .map(|candidate| {
            let energy_cost_eur = consumption
                .iter()
                .map(|(slot, energy)| price_at(&candidate.tariff, *slot).cost_of(*energy))
                .sum::<Decimal>();
            let fixed_eur = candidate.tariff.network.annual_fixed_eur()
                + candidate.tariff.standing_charge_eur_per_year;
            Comparison {
                label: candidate.label.clone(),
                energy_cost_eur,
                fixed_eur,
                total_eur: energy_cost_eur + fixed_eur,
                delta_eur: Decimal::ZERO,
            }
        })
        .collect();

    if let Some(reference) = results.first().map(|r| r.total_eur) {
        for r in &mut results {
            r.delta_eur = r.total_eur - reference;
        }
    }
    results
}

/// The consumption a Modul 2 metering point would need to carry before it pays
/// for itself, in kilowatt-hours per year.
///
/// Above this, the 60 % reduction on the working price outweighs the annual
/// charge for the extra measuring point. Below it, Modul 1's lump sum wins.
#[must_use]
pub fn modul2_break_even_kwh(
    arbeitspreis_ct: Decimal,
    remaining_share: Decimal,
    metering_eur_per_year: Decimal,
    modul1_reduction_eur_per_year: Decimal,
) -> Option<Decimal> {
    let saved_per_kwh_ct = arbeitspreis_ct * (Decimal::ONE - remaining_share);
    if saved_per_kwh_ct <= Decimal::ZERO {
        return None;
    }
    let must_beat_eur = metering_eur_per_year + modul1_reduction_eur_per_year;
    Some(must_beat_eur * Decimal::ONE_HUNDRED / saved_per_kwh_ct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::levies::Levies;
    use crate::tariff::{EnergyPrice, FeedIn, NetworkCharge};
    use time::macros::datetime;

    fn consumption(kwh_per_slot: f64, slots: usize) -> BTreeMap<Slot, Energy> {
        let base = datetime!(2026-01-15 00:00:00 UTC);
        (0..slots)
            .map(|i: usize| {
                (
                    Slot::containing(
                        base + time::Duration::minutes(15 * i64::try_from(i).unwrap()),
                    ),
                    Energy::from_kwh(kwh_per_slot),
                )
            })
            .collect()
    }

    fn tariff(network: NetworkCharge) -> Tariff {
        Tariff {
            energy: EnergyPrice::Fixed {
                ct_per_kwh: Decimal::new(15, 0),
            },
            network,
            levies: Levies::household_2026(),
            feed_in: FeedIn::None,
            standing_charge_eur_per_year: Decimal::ZERO,
        }
    }

    #[test]
    fn a_heavy_consumer_is_better_off_on_modul_2() {
        // 6 000 kWh a year through the heat pump: 96 slots of 62,5 kWh stands in
        // for the year's energy, which is all the comparison needs.
        let series = consumption(62.5, 96);
        let candidates = [
            ModulChoice {
                label: "Modul 1".into(),
                tariff: tariff(NetworkCharge::Modul1 {
                    arbeitspreis: Decimal::new(1000, 2),
                    reduction_eur_per_year: Decimal::new(120, 0),
                }),
            },
            ModulChoice {
                label: "Modul 2".into(),
                tariff: tariff(NetworkCharge::Modul2 {
                    arbeitspreis: Decimal::new(1000, 2),
                    remaining_share: Decimal::new(4, 1),
                    metering_eur_per_year: Decimal::new(25, 0),
                }),
            },
        ];
        let result = compare_moduls(&series, &candidates);
        assert!(
            result[1].delta_eur < Decimal::ZERO,
            "Modul 2 should win: {result:#?}"
        );
    }

    #[test]
    fn a_light_consumer_is_better_off_on_modul_1() {
        // 1 000 kWh a year.
        let series = consumption(10.4, 96);
        let candidates = [
            ModulChoice {
                label: "Modul 1".into(),
                tariff: tariff(NetworkCharge::Modul1 {
                    arbeitspreis: Decimal::new(1000, 2),
                    reduction_eur_per_year: Decimal::new(120, 0),
                }),
            },
            ModulChoice {
                label: "Modul 2".into(),
                tariff: tariff(NetworkCharge::Modul2 {
                    arbeitspreis: Decimal::new(1000, 2),
                    remaining_share: Decimal::new(4, 1),
                    metering_eur_per_year: Decimal::new(25, 0),
                }),
            },
        ];
        let result = compare_moduls(&series, &candidates);
        assert!(
            result[1].delta_eur > Decimal::ZERO,
            "Modul 1 should win: {result:#?}"
        );
    }

    #[test]
    fn the_break_even_depends_on_the_network_area_not_on_a_rule_of_thumb() {
        // A cheap area, 5 ct/kWh working price.
        let cheap = modul2_break_even_kwh(
            Decimal::new(500, 2),
            Decimal::new(4, 1),
            Decimal::new(25, 0),
            Decimal::new(120, 0),
        )
        .unwrap();
        // An expensive one, 11 ct/kWh.
        let dear = modul2_break_even_kwh(
            Decimal::new(1100, 2),
            Decimal::new(4, 1),
            Decimal::new(25, 0),
            Decimal::new(120, 0),
        )
        .unwrap();
        assert!(
            cheap > dear,
            "a cheap network area needs more consumption to justify Modul 2"
        );
        assert!(
            cheap > Decimal::new(4000, 0) && cheap < Decimal::new(5500, 0),
            "{cheap}"
        );
        assert!(
            dear > Decimal::new(2000, 0) && dear < Decimal::new(2500, 0),
            "{dear}"
        );
    }

    #[test]
    fn the_reference_candidate_has_no_delta() {
        let series = consumption(1.0, 4);
        let result = compare_moduls(
            &series,
            &[ModulChoice {
                label: "today".into(),
                tariff: tariff(NetworkCharge::None {
                    arbeitspreis: Decimal::new(1000, 2),
                }),
            }],
        );
        assert_eq!(result[0].delta_eur, Decimal::ZERO);
    }
}
