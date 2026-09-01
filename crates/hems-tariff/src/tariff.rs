//! The contract: what the household pays and is paid.

use std::collections::BTreeMap;

use hems_core::prelude::Slot;
use hems_grid::modul3::{Modul3Calendar, Preisstufe};
use rust_decimal::Decimal;
use time::Date;

use crate::levies::Levies;

/// How the energy component is priced.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum EnergyPrice {
    /// One price all year, ct/kWh net.
    Fixed {
        /// The price.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
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
        #[cfg_attr(feature = "serde", serde(with = "crate::wire::decimal_map"))]
        spot: BTreeMap<Slot, Decimal>,
        /// The supplier's markup, ct/kWh.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        markup_ct_per_kwh: Decimal,
        /// What to assume where no spot price is known yet — beyond tomorrow's
        /// auction, and after an outage. Refusing to plan at all would be worse.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
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
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        arbeitspreis: Decimal,
    },
    /// **Modul 1** — a flat annual reduction, the working price unchanged.
    Modul1 {
        /// The ordinary working price, ct/kWh.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        arbeitspreis: Decimal,
        /// The annual reduction, euros. Shown to the household as a lump sum
        /// because that is how it is billed; it never changes a marginal price,
        /// so it never changes what the optimiser does.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
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
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        arbeitspreis: Decimal,
        /// The share of the working price that remains, 0.4 by default.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        remaining_share: Decimal,
        /// The annual metering charge for the extra measuring point, euros.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        metering_eur_per_year: Decimal,
    },
    /// **Modul 3** — time-variable network charges, only together with Modul 1.
    Modul3 {
        /// The operator's calendar of windows and levels.
        calendar: Modul3Calendar,
        /// The high-tariff working price, ct/kWh.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        ht: Decimal,
        /// The standard working price, ct/kWh.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        st: Decimal,
        /// The low-tariff working price, ct/kWh.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        nt: Decimal,
        /// The Modul 1 reduction that comes with it, euros per year.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
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

/// Which way a household is paid for what it exports.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum Remuneration {
    /// Nothing is paid for exported energy.
    None,
    /// The EEG feed-in tariff (§ 19 Abs. 1 Nr. 2), ct/kWh.
    Eeg {
        /// The tariff.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        ct_per_kwh: Decimal,
    },
    /// Direktvermarktung (§ 19 Abs. 1 Nr. 1): the spot price the seller actually
    /// realises, plus the Marktprämie.
    Direktvermarktung {
        /// The Marktwert to fall back on where no spot price is known.
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        marktwert_ct_per_kwh: Decimal,
        /// The Marktprämie on top, `MP = AW − MW` (Anlage 1 zu § 23a).
        #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
        praemie_ct_per_kwh: Decimal,
    },
}

/// What exporting earns, and the one statutory fact that reaches both schemes.
///
/// # Why § 51 is a field of the struct and not of the variant
///
/// *"Für Zeiträume, in denen der Spotmarktpreis negativ ist, verringert sich der
/// **anzulegende Wert** auf null"* (§ 51 Abs. 1 EEG). The anzulegender Wert is
/// what the Einspeisevergütung is calculated from (§ 53 Abs. 1) **and** what the
/// Marktprämie is calculated from (Anlage 1 zu § 23a Nr. 1, *"AW der anzulegende
/// Wert unter Berücksichtigung der §§ 19 bis 54"*), so the rule reaches both and
/// zeroes both.
///
/// Carrying it inside one variant is how it reached only one. A household in
/// Direktvermarktung was priced at `spot + Prämie` in a negative quarter hour —
/// which is the household being told it still earns the premium in exactly the
/// hours the statute is paying it to stop, so the plan feeds in where it should
/// absorb or curtail. That is the whole behavioural point of § 51, inverted.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FeedIn {
    /// How the export is paid for.
    pub scheme: Remuneration,
    /// The day from which § 51 EEG reduces this plant's anzulegender Wert to
    /// zero in a negative quarter hour, if it ever does.
    ///
    /// `None` is "not yet, and possibly never", which is the ordinary state of a
    /// German household roof: § 51 Abs. 2 Nr. 1 exempts a plant below 100 kW for
    /// every period **before the end of the calendar year in which it is fitted
    /// with an intelligent metering system**, and Nr. 2 exempts one below 2 kW
    /// until the end of the year the Bundesnetzagentur makes its § 85 Abs. 2
    /// Nr. 12 Festlegung in — which it has not.
    ///
    /// So this is a date rather than a boolean, and the date is not the day the
    /// meter was fitted: it is the first of January after it.
    /// `hems_grid::para9::para51_applies_from` derives it, because the facts it
    /// needs are the plant's, not the tariff's.
    #[cfg_attr(
        feature = "serde",
        serde(default, with = "hems_core::wire::iso_date::option")
    )]
    pub para51_from: Option<Date>,
}

impl FeedIn {
    /// Nothing is paid for what leaves the house.
    pub const NONE: Self = Self {
        scheme: Remuneration::None,
        para51_from: None,
    };

    /// The EEG feed-in tariff, with § 51 not yet reaching this plant.
    #[must_use]
    pub const fn eeg(ct_per_kwh: Decimal) -> Self {
        Self {
            scheme: Remuneration::Eeg { ct_per_kwh },
            para51_from: None,
        }
    }

    /// Direktvermarktung, with § 51 not yet reaching this plant.
    #[must_use]
    pub const fn direktvermarktung(
        marktwert_ct_per_kwh: Decimal,
        praemie_ct_per_kwh: Decimal,
    ) -> Self {
        Self {
            scheme: Remuneration::Direktvermarktung {
                marktwert_ct_per_kwh,
                praemie_ct_per_kwh,
            },
            para51_from: None,
        }
    }

    /// Say from which day § 51 EEG reaches this plant.
    #[must_use]
    pub const fn under_para51_from(mut self, from: Option<Date>) -> Self {
        self.para51_from = from;
        self
    }

    /// Whether § 51 EEG zeroes the anzulegender Wert on `day`.
    #[must_use]
    pub fn para51_applies_on(&self, day: Date) -> bool {
        self.para51_from.is_some_and(|from| day >= from)
    }
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
    /// The membership of a § 42c energy-sharing community, if the household has
    /// one.
    #[cfg_attr(feature = "serde", serde(default))]
    pub sharing: Option<SharingTariff>,
    /// The supplier's annual standing charge, euros.
    #[cfg_attr(feature = "serde", serde(default))]
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
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
            feed_in: FeedIn::NONE,
            sharing: None,
            standing_charge_eur_per_year: Decimal::ZERO,
        }
    }

    /// The same tariff, inside an energy-sharing community.
    #[must_use]
    pub fn in_community(mut self, sharing: SharingTariff) -> Self {
        self.sharing = Some(sharing);
        self
    }
}

/// What a § 42c allocation costs this household, as a *price*.
///
/// § 42c Abs. 3 Nr. 3 EnWG makes the price of the shared electricity a term of
/// the written agreement between the members. What the household then pays for
/// an allocated kilowatt-hour is that price **instead of its supplier's energy
/// price** — and nothing else changes, because the electricity crosses the
/// public grid to reach it:
///
/// ```text
/// shared_import = VAT × (community_energy + network + levies)
/// ```
///
/// against the supplier's `VAT × (energy + network + levies)`. Network charges,
/// the Stromsteuer, the KWKG and § 19 levies, the Konzessionsabgabe and value
/// added tax are all untouched by § 42c, and a model that forgot that would tell
/// a household that joining a community makes its kilowatt-hours nearly free.
/// It does not. On the reference winter day the community sells at **12 ct/kWh
/// net** and the kilowatt-hour still arrives at the meter at **32,5 ct** —
/// against 47,9 ct from the supplier — because 10 ct of network charge, 5,3 ct
/// of levies and 19 % of value added tax do not care where the electron came
/// from. That is a third off the allocated kilowatt-hours, which is a great
/// deal and is not the ninety per cent somebody imagining "free solar from the
/// neighbours" would price.
///
/// A community whose contract *also* carries a reduced Netzentgelt says so
/// through [`SharingTariff::network_ct_per_kwh`], because that is a different
/// agreement with a different counterparty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SharingTariff {
    /// The community's own energy price, ct/kWh net, § 42c Abs. 3 Nr. 3.
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub energy_ct_per_kwh: Decimal,
    /// A network working price that applies only to the allocated kilowatt-hours,
    /// ct/kWh net.
    ///
    /// `None` — the ordinary case — leaves the household's own network charge in
    /// place, which is what § 42c on its own does.
    #[cfg_attr(feature = "serde", serde(default))]
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str_option"))]
    pub network_ct_per_kwh: Option<Decimal>,
}

impl SharingTariff {
    /// A community that sells its members electricity at `ct_per_kwh` net and
    /// changes nothing else.
    #[must_use]
    pub const fn at(energy_ct_per_kwh: Decimal) -> Self {
        Self {
            energy_ct_per_kwh,
            network_ct_per_kwh: None,
        }
    }

    /// …and carries a network charge of its own for the allocated kilowatt-hours.
    #[must_use]
    pub const fn with_network_charge(mut self, ct_per_kwh: Decimal) -> Self {
        self.network_ct_per_kwh = Some(ct_per_kwh);
        self
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
