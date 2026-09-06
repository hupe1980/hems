//! How a price travels, when the `serde` feature is on.
//!
//! Every [`rust_decimal::Decimal`] in this workspace is written as the exact
//! decimal string its `Display` produces — `"12.345"` — and read back as a
//! string. Two things follow, and both matter for a quantity that ends up on an
//! invoice or in a Nachweis.
//!
//! A JSON **number** is a type error rather than a silent trip through `f64`,
//! so a producer that writes `0.30000000000000004` is refused instead of
//! rounded. And a format with no self-describing wire — postcard, bincode,
//! MessagePack, which is what an embedded store speaks — can read it at all:
//! the inherited `Decimal` impl asks `deserialize_any`, and that is the one
//! question those formats cannot answer.
//!
//! Every field states this for itself with
//! `serde(with = "rust_decimal::serde::str")`, rather than relying on
//! `rust_decimal`'s `serde-str` feature. Cargo features are global to a build
//! graph: switching the default impl here would change how every `Decimal`
//! deserialises in a crate that never named `hems-tariff`, and a feature any
//! *other* crate sets would decide how these quantities travel.
//! `cargo xtask check-wire` fails the build when a field forgets.
//!
//! [`rust_decimal::serde::str`] and `str_option` cover a field. A map *value*
//! is not a field, so it gets [`decimal_map`] below.

#![cfg(feature = "serde")]

/// A `BTreeMap<Slot, Decimal>` whose values travel as exact decimal strings.
pub(crate) mod decimal_map {
    use std::collections::BTreeMap;

    use hems_core::prelude::Slot;
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// One value, carrying the string form through the map's own impls.
    #[derive(Serialize, Deserialize)]
    struct Quantity(#[serde(with = "rust_decimal::serde::str")] Decimal);

    pub(crate) fn serialize<S: Serializer>(
        value: &BTreeMap<Slot, Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(value.iter().map(|(slot, d)| (slot, Quantity(*d))))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<Slot, Decimal>, D::Error> {
        Ok(BTreeMap::<Slot, Quantity>::deserialize(deserializer)?
            .into_iter()
            .map(|(slot, q)| (slot, q.0))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use hems_core::prelude::{Horizon, Slot};
    use rust_decimal::Decimal;
    use time::macros::datetime;

    use crate::{Levies, PriceStack, Tariff};

    const T0: time::OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

    #[test]
    fn a_quantity_travels_as_an_exact_decimal_string() {
        let levies = Levies::household_2026();
        let json = serde_json::to_string(&levies).unwrap();
        assert!(
            json.contains(r#""stromsteuer":"2.05""#),
            "a levy is a string, not a number: {json}"
        );
        assert_eq!(serde_json::from_str::<Levies>(&json).unwrap(), levies);
    }

    #[test]
    fn a_json_number_is_a_type_error_rather_than_a_trip_through_f64() {
        // The property the whole `serde(with)` discipline buys. A producer that
        // writes a float has lost digits before the value ever arrives, and a
        // quantity that becomes a Nachweis may not be rounded on the way in.
        let err = serde_json::from_str::<Levies>(
            r#"{"stromsteuer":2.05,"kwkg":"0.28","para19":"0.64","offshore":"0.65",
                "konzessionsabgabe":"1.66","vat_rate":"0.19"}"#,
        )
        .expect_err("a JSON number must not deserialise into a Decimal");
        assert!(
            err.to_string().contains("invalid type: floating point"),
            "and the error names the input rather than the field: {err}"
        );
    }

    #[test]
    fn a_price_map_travels_the_same_way() {
        // A map *value* is not a field, so it needs `decimal_map` — and the two
        // spot maps in this crate are the only quantities in the workspace that
        // are neither.
        let base = Tariff::fixed(Decimal::new(2500, 2), Decimal::new(950, 2));
        let stack = PriceStack::build(&base, Horizon::new(T0, 2));
        let json = serde_json::to_string(&stack).unwrap();
        assert!(!json.contains("e-"), "no float exponent survives: {json}");
        assert_eq!(serde_json::from_str::<PriceStack>(&json).unwrap(), stack);

        let spot: std::collections::BTreeMap<Slot, Decimal> =
            [(Slot::containing(T0), Decimal::new(12345, 3))].into();
        let tariff = Tariff {
            energy: crate::EnergyPrice::Dynamic {
                spot,
                markup_ct_per_kwh: Decimal::ZERO,
                fallback_ct_per_kwh: Decimal::ZERO,
            },
            ..base
        };
        let json = serde_json::to_string(&tariff).unwrap();
        assert!(json.contains(r#""12.345""#), "map values too: {json}");
        assert_eq!(serde_json::from_str::<Tariff>(&json).unwrap(), tariff);
    }
}

#[cfg(test)]
mod carbon_wire {
    /// The carbon curve is a `BTreeMap<Slot, f64>`, and a map **key** is the one
    /// shape this workspace has already been caught by.
    ///
    /// D98's defect was exactly this: a `BTreeMap<(DayType, u32), _>` is
    /// something JSON cannot express as a key, so the derive compiled, every
    /// other format accepted it, and the one a box actually stores in failed at
    /// run time with no symptom but a model that never improved.
    ///
    /// `Slot` serialises as a single scalar, so it *is* a valid key — and the
    /// value is an `f64` rather than a `Decimal`, deliberately: grams per
    /// kilowatt-hour is a physical estimate nobody is billed on, and P3 puts the
    /// exact arithmetic where money and a Nachweis are. This asserts both halves
    /// rather than trusting that the derive compiling means anything.
    #[test]
    fn a_tariff_with_a_carbon_curve_survives_json() {
        use hems_core::prelude::Slot;
        use rust_decimal::Decimal;

        let now = time::macros::datetime!(2026-06-21 12:00:00 UTC);
        let mut tariff = crate::tariff::Tariff::fixed(Decimal::new(30, 0), Decimal::new(10, 0));
        tariff.carbon_g_per_kwh = (0..4)
            .map(|i| {
                (
                    Slot::containing(now + time::Duration::minutes(15 * i)),
                    300.0 + i as f64,
                )
            })
            .collect();

        let json = serde_json::to_string(&tariff).expect("a tariff serialises");
        let back: crate::tariff::Tariff =
            serde_json::from_str(&json).expect("and reads back as itself");
        assert_eq!(
            back.carbon_g_per_kwh.len(),
            4,
            "a Slot has to be expressible as a JSON key: {json}"
        );
        assert_eq!(back, tariff);
    }
}
