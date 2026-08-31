//! Levies and taxes — the part of the bill that changes once a year.
//!
//! Everything here is in cents per kilowatt-hour and applies to imported
//! energy. The values are defaults for a household customer; an operator
//! overrides them from the supplier's price sheet, which is the only place they
//! are ever authoritative.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// The levies and taxes on a kilowatt-hour drawn from the grid, ct/kWh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Levies {
    /// Stromsteuer (§ 3 StromStG).
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub stromsteuer: Decimal,
    /// KWKG-Umlage.
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub kwkg: Decimal,
    /// § 19 StromNEV-Umlage.
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub para19: Decimal,
    /// Offshore-Netzumlage.
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub offshore: Decimal,
    /// Konzessionsabgabe — depends on the municipality and the tariff type.
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub konzessionsabgabe: Decimal,
    /// Value added tax, as a fraction (0.19 for the ordinary rate).
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub vat_rate: Decimal,
}

impl Levies {
    /// The sum of the levies before value added tax, ct/kWh.
    #[must_use]
    pub fn sum_net(&self) -> Decimal {
        self.stromsteuer + self.kwkg + self.para19 + self.offshore + self.konzessionsabgabe
    }

    /// Apply value added tax to a net price in ct/kWh.
    #[must_use]
    pub fn gross(&self, net: Decimal) -> Decimal {
        net * (Decimal::ONE + self.vat_rate)
    }

    /// A household default for 2026.
    ///
    /// Placeholders in the honest sense: they are the right order of magnitude
    /// and the wrong number for any particular customer, which is why every
    /// operator configuration overrides them from the supplier's price sheet.
    #[must_use]
    pub fn household_2026() -> Self {
        Self {
            stromsteuer: Decimal::new(205, 2),
            kwkg: Decimal::new(28, 2),
            para19: Decimal::new(64, 2),
            offshore: Decimal::new(65, 2),
            konzessionsabgabe: Decimal::new(166, 2),
            vat_rate: Decimal::new(19, 2),
        }
    }

    /// The levy total as a float, for the optimiser.
    #[must_use]
    pub fn sum_net_f64(&self) -> f64 {
        self.sum_net().to_f64().unwrap_or(0.0)
    }
}

impl Default for Levies {
    fn default() -> Self {
        Self::household_2026()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levies_add_up_and_take_vat() {
        let l = Levies::household_2026();
        assert_eq!(l.sum_net(), Decimal::new(528, 2));
        // 5,28 ct + 19 % = 6,2832 ct — exactly, not 6.283199999999999.
        assert_eq!(l.gross(l.sum_net()), Decimal::new(62832, 4));
    }

    #[test]
    fn a_customer_can_override_every_component() {
        let l = Levies {
            konzessionsabgabe: Decimal::ZERO,
            ..Levies::household_2026()
        };
        assert_eq!(l.sum_net(), Decimal::new(362, 2));
    }
}
