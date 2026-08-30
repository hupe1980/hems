//! Identifiers.
//!
//! A site is identified by a UUID because it is created by software and has to
//! be unique across a fleet. Everything inside a site — assets, circuits, driver
//! templates — is identified by a short human-written string, because those
//! names appear in a TOML file an installer edits, in MQTT topics, in the local
//! UI and in support tickets.

use core::fmt;
use core::str::FromStr;

use uuid::Uuid;

use crate::error::IdError;

/// The identity of one installation, stable across reconfiguration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct SiteId(Uuid);

impl SiteId {
    /// A fresh, time-ordered identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wrap an existing UUID.
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for SiteId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SiteId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

/// The identity of a plan produced by the optimiser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct PlanId(Uuid);

impl PlanId {
    /// A fresh, time-ordered identity.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// The underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

macro_rules! slug_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        #[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap a name.
            ///
            /// Accepts 1–64 characters from `a-z`, `0-9`, `.`, `_` and `-`.
            /// Upper case is folded to lower case, so `Wallbox` and `wallbox`
            /// are the same identifier and cannot both appear in one site.
            ///
            /// # Errors
            /// [`IdError`] when the name is empty, too long, or contains an
            /// unsupported character.
            pub fn new(name: impl AsRef<str>) -> Result<Self, IdError> {
                let name = name.as_ref().trim().to_ascii_lowercase();
                if name.is_empty() {
                    return Err(IdError::Empty);
                }
                if name.chars().count() > 64 {
                    return Err(IdError::TooLong(name.chars().count()));
                }
                if let Some(bad) = name
                    .chars()
                    .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
                {
                    return Err(IdError::BadCharacter(bad));
                }
                Ok(Self(name))
            }

            /// The identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

slug_id!(
    /// The name of one asset within a site — `wallbox-garage`, `battery`, `wp`.
    AssetId
);
slug_id!(
    /// The name of one circuit within a site — `main`, `sub-garage`.
    CircuitId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_ids_are_case_folded_and_validated() {
        assert_eq!(
            AssetId::new("Wallbox-Garage").unwrap().as_str(),
            "wallbox-garage"
        );
        assert_eq!(AssetId::new("  battery  ").unwrap().as_str(), "battery");
        assert_eq!(AssetId::new(""), Err(IdError::Empty));
        assert_eq!(AssetId::new("wall box"), Err(IdError::BadCharacter(' ')));
        assert_eq!(AssetId::new("a".repeat(65)), Err(IdError::TooLong(65)));
    }

    #[test]
    fn site_ids_are_time_ordered() {
        let a = SiteId::new();
        let b = SiteId::new();
        assert!(a < b, "v7 UUIDs sort by creation time");
    }
}
