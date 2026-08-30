//! One resolved price per slot — what the optimiser and the household see.

use hems_core::prelude::{Energy, Horizon, Slot};
use hems_grid::modul3::Preisstufe;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::tariff::{FeedIn, Tariff};

/// Everything a kilowatt-hour costs, or earns, in one quarter hour.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SlotPrice {
    /// The quarter hour.
    pub slot: Slot,
    /// The energy component, net, ct/kWh.
    pub energy_ct: Decimal,
    /// The network working price, net, ct/kWh.
    pub network_ct: Decimal,
    /// Levies and taxes before value added tax, ct/kWh.
    pub levies_ct: Decimal,
    /// What the household pays for a kilowatt-hour drawn here, gross, ct/kWh.
    pub import_ct: Decimal,
    /// What it earns for a kilowatt-hour fed in here, ct/kWh.
    pub export_ct: Decimal,
    /// The § 14a Modul 3 level, where the customer is on Modul 3.
    #[cfg_attr(feature = "serde", serde(default))]
    pub stufe: Option<Preisstufe>,
    /// `true` when the export price is zero because the day-ahead price was
    /// negative in this quarter hour (§ 51 EEG).
    pub negative_price_hour: bool,
    /// `false` when the energy price is a fallback rather than a known one.
    pub price_known: bool,
    /// Grams of CO₂ per kilowatt-hour drawn, where a source provides it.
    #[cfg_attr(feature = "serde", serde(default))]
    pub co2_g_per_kwh: Option<f64>,
}

impl SlotPrice {
    /// The import price as a float, €/kWh — the form the optimiser wants.
    #[must_use]
    pub fn import_f64(&self) -> f64 {
        self.import_ct.to_f64().unwrap_or(0.0) / 100.0
    }

    /// The export price as a float, €/kWh.
    #[must_use]
    pub fn export_f64(&self) -> f64 {
        self.export_ct.to_f64().unwrap_or(0.0) / 100.0
    }

    /// What drawing `energy` in this slot costs, in euros.
    #[must_use]
    pub fn cost_of(&self, energy: Energy) -> Decimal {
        let kwh = Decimal::try_from(energy.kwh()).unwrap_or(Decimal::ZERO);
        kwh * self.import_ct / Decimal::ONE_HUNDRED
    }

    /// What feeding in `energy` in this slot earns, in euros.
    #[must_use]
    pub fn revenue_of(&self, energy: Energy) -> Decimal {
        let kwh = Decimal::try_from(energy.kwh().abs()).unwrap_or(Decimal::ZERO);
        kwh * self.export_ct / Decimal::ONE_HUNDRED
    }
}

/// The resolved prices over a horizon.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PriceStack {
    /// One entry per slot, in order.
    pub slots: Vec<SlotPrice>,
}

impl PriceStack {
    /// Resolve `tariff` over `horizon`.
    #[must_use]
    pub fn build(tariff: &Tariff, horizon: Horizon) -> Self {
        Self {
            slots: horizon.slots().map(|slot| price_at(tariff, slot)).collect(),
        }
    }

    /// The price in one slot.
    #[must_use]
    pub fn at(&self, slot: Slot) -> Option<&SlotPrice> {
        self.slots.iter().find(|p| p.slot == slot)
    }

    /// The cheapest slots to draw in, cheapest first.
    ///
    /// The one query a household actually asks — "when should the car charge?" —
    /// and the one a rule-based system can answer without a solver.
    #[must_use]
    pub fn cheapest(&self, count: usize) -> Vec<&SlotPrice> {
        let mut sorted: Vec<&SlotPrice> = self.slots.iter().collect();
        sorted.sort_by(|a, b| a.import_ct.cmp(&b.import_ct).then(a.slot.cmp(&b.slot)));
        sorted.into_iter().take(count).collect()
    }

    /// The quarter hours where feeding in earns nothing (§ 51 EEG).
    #[must_use]
    pub fn negative_price_slots(&self) -> Vec<Slot> {
        self.slots
            .iter()
            .filter(|p| p.negative_price_hour)
            .map(|p| p.slot)
            .collect()
    }

    /// The spread between the dearest and cheapest slot, ct/kWh.
    ///
    /// The number that decides whether charging a battery from the grid pays at
    /// all: it has to cover the round-trip losses and the wear before anything
    /// is left over.
    #[must_use]
    pub fn spread_ct(&self) -> Decimal {
        let max = self
            .slots
            .iter()
            .map(|p| p.import_ct)
            .max()
            .unwrap_or(Decimal::ZERO);
        let min = self
            .slots
            .iter()
            .map(|p| p.import_ct)
            .min()
            .unwrap_or(Decimal::ZERO);
        max - min
    }
}

/// Resolve one slot of a tariff.
#[must_use]
pub fn price_at(tariff: &Tariff, slot: Slot) -> SlotPrice {
    let (energy_ct, price_known) = tariff.energy.at(slot);
    let (network_ct, stufe) = tariff.network.at(slot);
    let levies_ct = tariff.levies.sum_net();
    let import_ct = tariff.levies.gross(energy_ct + network_ct + levies_ct);

    let market_price = tariff.energy.spot_at(slot);
    let negative = market_price.is_some_and(|p| p < Decimal::ZERO);

    let export_ct = match &tariff.feed_in {
        FeedIn::None => Decimal::ZERO,
        FeedIn::Eeg {
            ct_per_kwh,
            negative_price_rule,
        } => {
            if *negative_price_rule && negative {
                Decimal::ZERO
            } else {
                *ct_per_kwh
            }
        }
        FeedIn::Direktvermarktung {
            marktwert_ct_per_kwh,
            praemie_ct_per_kwh,
        } => {
            // Under Direktvermarktung the seller is exposed to the price
            // itself, so a negative hour is a cost, not merely a missing
            // payment. The spot price is the honest signal to plan against.
            market_price.unwrap_or(*marktwert_ct_per_kwh) + *praemie_ct_per_kwh
        }
    };

    SlotPrice {
        slot,
        energy_ct,
        network_ct,
        levies_ct,
        import_ct,
        export_ct,
        stufe,
        negative_price_hour: negative,
        price_known,
        co2_g_per_kwh: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::levies::Levies;
    use crate::tariff::{EnergyPrice, NetworkCharge};
    use std::collections::BTreeMap;
    use time::macros::datetime;

    fn dynamic_tariff(prices: &[(time::OffsetDateTime, i64)]) -> Tariff {
        let spot: BTreeMap<Slot, Decimal> = prices
            .iter()
            .map(|(t, ct)| (Slot::containing(*t), Decimal::new(*ct, 0)))
            .collect();
        Tariff {
            energy: EnergyPrice::Dynamic {
                spot,
                markup_ct_per_kwh: Decimal::new(3, 0),
                fallback_ct_per_kwh: Decimal::new(12, 0),
            },
            network: NetworkCharge::None {
                arbeitspreis: Decimal::new(10, 0),
            },
            levies: Levies::household_2026(),
            feed_in: FeedIn::Eeg {
                ct_per_kwh: Decimal::new(786, 2),
                negative_price_rule: true,
            },
            standing_charge_eur_per_year: Decimal::ZERO,
        }
    }

    #[test]
    fn the_import_price_is_the_whole_stack_with_vat() {
        let t = dynamic_tariff(&[(datetime!(2026-01-15 10:00:00 UTC), 8)]);
        let p = price_at(&t, Slot::containing(datetime!(2026-01-15 10:00:00 UTC)));
        // energy 8 + 3 markup = 11, network 10, levies 5,28 → 26,28 net.
        assert_eq!(p.energy_ct, Decimal::new(11, 0));
        assert_eq!(p.import_ct, Decimal::new(3_127_320, 5), "26,28 × 1,19");
        assert!(p.price_known);
    }

    #[test]
    fn a_negative_hour_zeroes_the_eeg_payment() {
        // § 51 EEG since 25.02.2025 — 573 hours of 2025 looked like this.
        let t = dynamic_tariff(&[(datetime!(2026-06-15 11:00:00 UTC), -3)]);
        let p = price_at(&t, Slot::containing(datetime!(2026-06-15 11:00:00 UTC)));
        assert!(p.negative_price_hour);
        assert_eq!(p.export_ct, Decimal::ZERO);
    }

    #[test]
    fn a_system_outside_the_rule_keeps_being_paid() {
        let mut t = dynamic_tariff(&[(datetime!(2026-06-15 11:00:00 UTC), -3)]);
        t.feed_in = FeedIn::Eeg {
            ct_per_kwh: Decimal::new(786, 2),
            negative_price_rule: false,
        };
        let p = price_at(&t, Slot::containing(datetime!(2026-06-15 11:00:00 UTC)));
        assert!(p.negative_price_hour, "the hour is still negative");
        assert_eq!(
            p.export_ct,
            Decimal::new(786, 2),
            "but this plant is not covered"
        );
    }

    #[test]
    fn direktvermarktung_follows_the_price_into_negative_territory() {
        let mut t = dynamic_tariff(&[(datetime!(2026-06-15 11:00:00 UTC), -3)]);
        t.feed_in = FeedIn::Direktvermarktung {
            marktwert_ct_per_kwh: Decimal::new(5, 0),
            praemie_ct_per_kwh: Decimal::new(2, 0),
        };
        let p = price_at(&t, Slot::containing(datetime!(2026-06-15 11:00:00 UTC)));
        assert_eq!(p.export_ct, Decimal::new(-1, 0), "−3 spot + 2 premium");
    }

    #[test]
    fn an_unknown_slot_is_priced_but_flagged() {
        let t = dynamic_tariff(&[(datetime!(2026-01-15 10:00:00 UTC), 8)]);
        let p = price_at(&t, Slot::containing(datetime!(2026-01-20 10:00:00 UTC)));
        assert!(!p.price_known);
        assert_eq!(p.energy_ct, Decimal::new(15, 0), "12 fallback + 3 markup");
    }

    #[test]
    fn the_stack_finds_the_cheapest_hours_and_the_spread() {
        let base = datetime!(2026-01-15 00:00:00 UTC);
        let prices: Vec<_> = (0..8)
            .map(|i: i64| (base + time::Duration::minutes(15 * i), 20 - i * 2))
            .collect();
        let t = dynamic_tariff(&prices);
        let stack = PriceStack::build(&t, Horizon::new(base, 8));
        let cheapest = stack.cheapest(2);
        assert_eq!(cheapest.len(), 2);
        assert_eq!(
            cheapest[0].slot,
            Slot::containing(base + time::Duration::minutes(105))
        );
        // 20 ct down to 6 ct is 14 ct of spread, grossed up by VAT.
        assert_eq!(stack.spread_ct(), Decimal::new(1666, 2));
    }

    #[test]
    fn costs_and_revenues_come_back_exact() {
        let t = dynamic_tariff(&[(datetime!(2026-01-15 10:00:00 UTC), 8)]);
        let p = price_at(&t, Slot::containing(datetime!(2026-01-15 10:00:00 UTC)));
        // 4 kW for a quarter hour is 1 kWh.
        let cost = p.cost_of(Energy::from_kwh(1.0));
        assert_eq!(cost, Decimal::new(3_127_320, 7));
    }
}
