//! How a date travels, when the `serde` feature is on.
//!
//! `time` ships well-known modules for an instant — `time::serde::rfc3339` and
//! its `option` — and none for a [`time::Date`]. Its inherited impl writes the
//! compact `(year, ordinal)` tuple unless the `serde-human-readable` feature is
//! on, so a commissioning date lands in a configuration file as `[2024, 1]`
//! rather than `"2024-01-01"` — and the feature that would change that is global
//! to a build graph, so a library may not reach for it.
//!
//! A date therefore states its own form, the way a quantity and an instant do:
//! `serde(with = "hems_core::wire::iso_date")`, or `iso_date::option` for an
//! optional one. `cargo xtask check-wire` fails the build when a field forgets.

#![cfg(feature = "serde")]

/// A [`time::Date`] as the ISO 8601 calendar date `"2024-01-01"`.
pub mod iso_date {
    use serde::{Deserialize, Deserializer, Serializer, de};
    use time::Date;
    use time::format_description::BorrowedFormatItem;
    use time::macros::format_description;

    const ISO: &[BorrowedFormatItem<'_>] = format_description!("[year]-[month]-[day]");

    /// Write the date as `"2024-01-01"`.
    ///
    /// # Errors
    /// Only if the date cannot be formatted, which the descriptor above makes
    /// impossible for any date `time` can hold.
    pub fn serialize<S: Serializer>(value: &Date, serializer: S) -> Result<S::Ok, S::Error> {
        let text = value.format(ISO).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&text)
    }

    /// Read a date from `"2024-01-01"`.
    ///
    /// # Errors
    /// When the input is not a string, or not an ISO 8601 calendar date.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Date, D::Error> {
        let text = <std::borrow::Cow<'_, str>>::deserialize(deserializer)?;
        Date::parse(&text, ISO).map_err(de::Error::custom)
    }

    /// [`iso_date`](super::iso_date) for an optional date.
    ///
    /// A separate module because `serde(with)` is applied to the field's own
    /// type, and `Option<Date>` is a different type from `Date`.
    pub mod option {
        use serde::{Deserialize, Deserializer, Serializer};
        use time::Date;

        /// Write the date, or `null`.
        ///
        /// # Errors
        /// As [`super::serialize`].
        pub fn serialize<S: Serializer>(
            value: &Option<Date>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(date) => serializer.serialize_some(&Wrapper(*date)),
                None => serializer.serialize_none(),
            }
        }

        /// Read a date, or `null`.
        ///
        /// # Errors
        /// As [`super::deserialize`].
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Date>, D::Error> {
            Ok(Option::<Wrapper>::deserialize(deserializer)?.map(|w| w.0))
        }

        /// Carries the one-field `serde(with)` through `Option`'s own impls.
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrapper(#[serde(with = "super")] Date);
    }
}

#[cfg(test)]
mod tests {
    use time::Date;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Commissioned {
        #[serde(with = "super::iso_date::option")]
        at: Option<Date>,
    }

    #[test]
    fn a_date_travels_as_iso_8601_rather_than_a_tuple() {
        let value = Commissioned {
            at: Some(time::macros::date!(2024 - 01 - 01)),
        };
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, r#"{"at":"2024-01-01"}"#);
        assert_eq!(serde_json::from_str::<Commissioned>(&json).unwrap(), value);

        let none = Commissioned { at: None };
        assert_eq!(serde_json::to_string(&none).unwrap(), r#"{"at":null}"#);
        assert_eq!(
            serde_json::from_str::<Commissioned>(r#"{"at":null}"#).unwrap(),
            none
        );
    }

    #[test]
    fn a_tuple_is_a_type_error_rather_than_a_date() {
        // The inherited impl would read `[2024, 1]` happily, which is how a
        // configuration file ends up asking a household to write an ordinal.
        assert!(serde_json::from_str::<Commissioned>(r#"{"at":[2024,1]}"#).is_err());
    }
}
