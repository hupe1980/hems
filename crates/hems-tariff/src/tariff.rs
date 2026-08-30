//! The contract: what the household pays and is paid.

use std::collections::BTreeMap;

use hems_core::prelude::Slot;
use hems_grid::modul3::{Modul3Calendar, Preisstufe};
use rust_decimal::Decimal;

use crate::levies::Levies;

/// How the energy component is priced.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum EnergyPrice {
    /// One price all year, ct/kWh net.
    Fixed {
        /// The price.
        ct_per_kwh: Decimal,
    },
    /// The day-ahead price plus the supplier's markup, ct/kWh net.
    ///
    /// Every supplier has had to offer one of these since 01.01.2025 (§ 41a
    /// EnWG), and since 01.10.2025 the underlying market time unit is a quarter
    /// hour — which is why the whole workspace plans in quarter hours.
    Dynamic {
        /// The spot price per slot, ct/kWh. Slots outside the map fall back to
        /// [`EnergyPrice::Dynamic::fallback_ct_per_kwh`].
        spot: BTreeMap<Slot, Decimal>,
        /// The supplier's markup, ct/kWh.
        markup_ct_per_kwh: Decimal,
        /// What to assume where no spot price is known yet — beyond tomorrow's
        /// auction, and after an outage. Refusing to plan at all would be worse.
        fallback_ct_per_kwh: Decimal,
    },
}

impl EnergyPrice {
    /// The net energy price in `slot`, ct/kWh, and whether it is known.
    #[must_use]
    pub fn at(&self, slot: Slot) -> (Decimal, bool) {
        match self {
            EnergyPrice::Fixed { ct_per_kwh } => (*ct_per_kwh, true),
            EnergyPrice::Dynamic {
                spot,
                markup_ct_per_kwh,
                fallback_ct_per_kwh,
            } => match spot.get(&slot) {
                Some(p) => (*p + *markup_ct_per_kwh, true),
                None => (*fallback_ct_per_kwh + *markup_ct_per_kwh, false),
            },
        }
    }

    /// The raw spot price in `slot`, where there is one. Needed for § 51 EEG,
    /// which turns on the sign of the *market* price, not of what the household
    /// pays.
    #[must_use]
    pub fn spot_at(&self, slot: Slot) -> Option<Decimal> {
        match self {
            EnergyPrice::Fixed { .. } => None,
            EnergyPrice::Dynamic { spot, .. } => spot.get(&slot).copied(),
        }
    }
}

/// The network charge, and which § 14a module the customer chose.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "modul"))]
pub enum NetworkCharge {
    /// No § 14a module: the ordinary Arbeitspreis, ct/kWh.
    None {
        /// The ordinary working price.
        arbeitspreis: Decimal,
    },
    /// **Modul 1** — a flat annual reduction, the working price unchanged.
    Modul1 {
        /// The ordinary working price, ct/kWh.
        arbeitspreis: Decimal,
        /// The annual reduction, euros. Shown to the household as a lump sum
        /// because that is how it is billed; it never changes a marginal price,
        /// so it never changes what the optimiser does.
        reduction_eur_per_year: Decimal,
    },
    /// **Modul 2** — 60 % off the working price, at a Marktlokation of its own.
    ///
    /// Worth having only above a few thousand kilowatt-hours a year, because the
    /// second metering point carries its own annual charge. [`crate::advisor`]
    /// computes the threshold from the household's own history rather than from
    /// a rule of thumb.
    Modul2 {
        /// The ordinary working price, ct/kWh.
        arbeitspreis: Decimal,
        /// The share of the working price that remains, 0.4 by default.
        remaining_share: Decimal,
        /// The annual metering charge for the extra measuring point, euros.
        metering_eur_per_year: Decimal,
    },
    /// **Modul 3** — time-variable network charges, only together with Modul 1.
    Modul3 {
        /// The operator's calendar of windows and levels.
        calendar: Modul3Calendar,
        /// The high-tariff working price, ct/kWh.
        ht: Decimal,
        /// The standard working price, ct/kWh.
        st: Decimal,
        /// The low-tariff working price, ct/kWh.
        nt: Decimal,
        /// The Modul 1 reduction that comes with it, euros per year.
        reduction_eur_per_year: Decimal,
    },
}

impl NetworkCharge {
    /// The working price in `slot`, ct/kWh, with the level where one applies.
    #[must_use]
    pub fn at(&self, slot: Slot) -> (Decimal, Option<Preisstufe>) {
        match self {
            NetworkCharge::None { arbeitspreis } | NetworkCharge::Modul1 { arbeitspreis, .. } => {
                (*arbeitspreis, None)
            }
            NetworkCharge::Modul2 {
                arbeitspreis,
                remaining_share,
                ..
            } => (*arbeitspreis * *remaining_share, None),
            NetworkCharge::Modul3 {
                calendar,
                ht,
                st,
                nt,
                ..
            } => {
                let stufe = calendar.stufe_at(slot);
                let price = match stufe {
                    Preisstufe::Ht => *ht,
                    Preisstufe::St => *st,
                    Preisstufe::Nt => *nt,
                };
                (price, Some(stufe))
            }
        }
    }

    /// The fixed annual effect in euros — negative when it is a reduction.
    #[must_use]
    pub fn annual_fixed_eur(&self) -> Decimal {
        match self {
            NetworkCharge::None { .. } => Decimal::ZERO,
            NetworkCharge::Modul1 {
                reduction_eur_per_year,
                ..
            }
            | NetworkCharge::Modul3 {
                reduction_eur_per_year,
                ..
            } => -*reduction_eur_per_year,
            NetworkCharge::Modul2 {
                metering_eur_per_year,
                ..
            } => *metering_eur_per_year,
        }
    }
}

/// How feed-in is remunerated.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum FeedIn {
    /// Nothing is paid for exported energy.
    None,
    /// The EEG feed-in tariff, ct/kWh.
    ///
    /// Since 25.02.2025 a supported system earns **nothing** in a quarter hour
    /// with a negative day-ahead price (§ 51 EEG) — 573 hours of 2025 were like
    /// that. Self-consumption or curtailment is then the rational choice, and
    /// [`crate::stack::SlotPrice`] carries the zero so the optimiser sees it.
    Eeg {
        /// The tariff.
        ct_per_kwh: Decimal,
        /// Whether the § 51 negative-price rule applies to this system.
        negative_price_rule: bool,
    },
    /// Direktvermarktung: the market value plus the market premium, ct/kWh.
    Direktvermarktung {
        /// The monthly Marktwert Solar.
        marktwert_ct_per_kwh: Decimal,
        /// The premium on top.
        praemie_ct_per_kwh: Decimal,
    },
}

/// A household's complete price situation.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tariff {
    /// The energy component.
    pub energy: EnergyPrice,
    /// The network charge and § 14a module.
    pub network: NetworkCharge,
    /// Levies and taxes.
    pub levies: Levies,
    /// What exporting earns.
    pub feed_in: FeedIn,
    /// The supplier's annual standing charge, euros.
    #[cfg_attr(feature = "serde", serde(default))]
    pub standing_charge_eur_per_year: Decimal,
}

impl Tariff {
    /// A fixed-price tariff without any § 14a module — the baseline everything
    /// else is compared against.
    #[must_use]
    pub fn fixed(ct_per_kwh: Decimal, arbeitspreis: Decimal) -> Self {
        Self {
            energy: EnergyPrice::Fixed { ct_per_kwh },
            network: NetworkCharge::None { arbeitspreis },
            levies: Levies::default(),
            feed_in: FeedIn::None,
            standing_charge_eur_per_year: Decimal::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_grid::modul3::Quarter;
    use time::macros::datetime;

    fn slot(utc: time::OffsetDateTime) -> Slot {
        Slot::containing(utc)
    }

    #[test]
    fn a_dynamic_price_falls_back_and_says_it_did() {
        let mut spot = BTreeMap::new();
        let known = slot(datetime!(2026-01-15 10:00:00 UTC));
        spot.insert(known, Decimal::new(8, 0));
        let p = EnergyPrice::Dynamic {
            spot,
            markup_ct_per_kwh: Decimal::new(3, 0),
            fallback_ct_per_kwh: Decimal::new(12, 0),
        };
        assert_eq!(p.at(known), (Decimal::new(11, 0), true));
        let unknown = slot(datetime!(2026-01-16 10:00:00 UTC));
        assert_eq!(p.at(unknown), (Decimal::new(15, 0), false));
    }

    #[test]
    fn modul_2_takes_sixty_percent_off_the_working_price() {
        let n = NetworkCharge::Modul2 {
            arbeitspreis: Decimal::new(1000, 2),
            remaining_share: Decimal::new(4, 1),
            metering_eur_per_year: Decimal::new(25, 0),
        };
        assert_eq!(
            n.at(slot(datetime!(2026-01-15 10:00:00 UTC))).0,
            Decimal::new(400, 2)
        );
        assert_eq!(n.annual_fixed_eur(), Decimal::new(25, 0));
    }

    #[test]
    fn modul_3_follows_the_operators_windows() {
        let calendar = Modul3Calendar::new(
            "9900000000001",
            2026,
            (17 * 60, 20 * 60),
            (22 * 60, 6 * 60),
            vec![Quarter::Q1, Quarter::Q4],
        );
        let n = NetworkCharge::Modul3 {
            calendar,
            ht: Decimal::new(1800, 2),
            st: Decimal::new(1000, 2),
            nt: Decimal::new(300, 2),
            reduction_eur_per_year: Decimal::new(150, 0),
        };
        // 18:00 local in January is 17:00 UTC — inside the high-tariff window.
        let (price, stufe) = n.at(slot(datetime!(2026-01-15 17:00:00 UTC)));
        assert_eq!(price, Decimal::new(1800, 2));
        assert_eq!(stufe, Some(Preisstufe::Ht));
        // 03:00 local is the cheap window.
        assert_eq!(
            n.at(slot(datetime!(2026-01-15 02:00:00 UTC))).0,
            Decimal::new(300, 2)
        );
        assert_eq!(n.annual_fixed_eur(), Decimal::new(-150, 0));
    }

    #[test]
    fn modul_1_never_changes_a_marginal_price() {
        // Which is exactly why it never changes what the optimiser does — the
        // whole benefit is a lump sum on the annual bill.
        let n = NetworkCharge::Modul1 {
            arbeitspreis: Decimal::new(1000, 2),
            reduction_eur_per_year: Decimal::new(120, 0),
        };
        let a = n.at(slot(datetime!(2026-01-15 03:00:00 UTC))).0;
        let b = n.at(slot(datetime!(2026-01-15 18:00:00 UTC))).0;
        assert_eq!(a, b);
        assert_eq!(n.annual_fixed_eur(), Decimal::new(-120, 0));
    }
}
