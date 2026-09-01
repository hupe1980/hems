//! The envelope every hems event travels in: CloudEvents 1.0, structured mode.
//!
//! # Why an envelope at all, when the body would do
//!
//! A body works while there is one message on one link. With two, a receiver has
//! to answer three questions the body cannot: *what is this*, *who sent it*, and
//! *have I already seen it*. Answering them per endpoint is how a fleet ends up
//! with four conventions and no vocabulary, which is why the envelope is CloudEvents
//! to avoid.
//!
//! **Structured** mode rather than binary: the whole event is one JSON document,
//! so the thing that is signed is the thing that is stored, and a body that has
//! been through a proxy that rewrote a header is still verifiable. Binary mode
//! spreads the metadata across HTTP headers, and headers are the part of a
//! request nobody can promise survives the trip.
//!
//! # `type` is checked against the catalogue
//!
//! [`Event::new`] takes a `&'static str` from [`crate::ALL`] rather than a
//! `String`, so an event type that is not in the catalogue does not compile; and
//! [`Event::parse`] refuses one on the way in, because a receiver that accepts
//! `de.hems.site.day.reportd` is a receiver that will silently drop the day it
//! was actually sent.
//!
//! # Sans-I/O, like the rest of the domain
//!
//! Nothing here reads a clock or mints an identifier: an emitter passes both in.
//! That is what makes a signed event a unit test rather than a thing somebody
//! tries once against a real server, and it is the same rule the guard and the
//! arbiter are built on.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The CloudEvents specification version every hems event carries.
pub const SPEC_VERSION: &str = "1.0";

/// The media type of a hems event's `data`.
pub const DATA_CONTENT_TYPE: &str = "application/json";

/// Why an event could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvelopeError {
    /// The bytes are not a JSON document at all.
    #[error("the event is not JSON: {0}")]
    NotJson(String),
    /// `specversion` is not one this build speaks.
    #[error("the event declares CloudEvents {found}, and this build speaks {SPEC_VERSION}")]
    WrongSpecVersion {
        /// What the event declared.
        found: String,
    },
    /// `type` is not in [`crate::ALL`].
    ///
    /// Refused rather than passed through: an unknown type is either a newer
    /// emitter or a typo, and a receiver cannot tell those apart. Accepting it
    /// means storing something nothing will ever look for.
    #[error("{found:?} is not an event type this workspace publishes")]
    UnknownType {
        /// What the event declared.
        found: String,
    },
    /// `type` is a catalogued type, but not the one the reader asked for.
    #[error("the event is a {found:?} and this endpoint takes {expected:?}")]
    WrongType {
        /// What arrived.
        found: String,
        /// What the endpoint reads.
        expected: &'static str,
    },
    /// `data` is not the shape the reader expects.
    #[error("the event's data is not what {expected:?} carries: {detail}")]
    WrongData {
        /// Which type was being read.
        expected: &'static str,
        /// What `serde` said.
        detail: String,
    },
}

/// One event, and what it carries.
///
/// The field names are CloudEvents' own, which is why `r#type` is spelled the
/// way it is: renaming it to something more comfortable in Rust would put a
/// serde attribute between this type and the specification, and the
/// specification is the point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event<T> {
    /// Always [`SPEC_VERSION`].
    pub specversion: String,
    /// Unique for this event from this source. Also the Standard Webhooks
    /// message id, so a receiver that de-duplicates and a receiver that verifies
    /// are looking at the same identifier.
    pub id: String,
    /// Who sent it — a URI reference. `hems://sites/<site>` for a box.
    pub source: String,
    /// One of [`crate::ALL`].
    #[serde(rename = "type")]
    pub r#type: String,
    /// When it happened, RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub time: OffsetDateTime,
    /// Always [`DATA_CONTENT_TYPE`].
    pub datacontenttype: String,
    /// What the event is about, where the source has a natural subdivision — a
    /// site's day is subjected by its date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// The payload.
    pub data: T,
}

impl<T> Event<T> {
    /// An event of a catalogued type.
    ///
    /// `event_type` is a `&'static str` on purpose: the only ones in the binary
    /// are the catalogue's own constants, so an emitter cannot invent a type by
    /// formatting a string.
    pub fn new(
        event_type: &'static str,
        source: impl Into<String>,
        id: impl Into<String>,
        at: OffsetDateTime,
        data: T,
    ) -> Self {
        debug_assert!(
            crate::is_known(event_type),
            "{event_type} is not in the catalogue"
        );
        Self {
            specversion: SPEC_VERSION.to_owned(),
            id: id.into(),
            source: source.into(),
            r#type: event_type.to_owned(),
            time: at,
            datacontenttype: DATA_CONTENT_TYPE.to_owned(),
            subject: None,
            data,
        }
    }

    /// What the event is about within its source.
    #[must_use]
    pub fn about(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
}

impl<T: Serialize> Event<T> {
    /// The bytes that travel, and that a signature is computed over.
    ///
    /// # Errors
    /// [`EnvelopeError::NotJson`] if `T` cannot be serialised, which for the
    /// payloads in this workspace it always can.
    pub fn to_bytes(&self) -> Result<Vec<u8>, EnvelopeError> {
        serde_json::to_vec(self).map_err(|e| EnvelopeError::NotJson(e.to_string()))
    }
}

impl<T: serde::de::DeserializeOwned> Event<T> {
    /// Read an event that must be of `expected` type.
    ///
    /// The three checks are separate errors on purpose. "Not a hems event",
    /// "a hems event for a different endpoint" and "the right event with the
    /// wrong body" are three different faults with three different fixes, and a
    /// single `400` tells an operator none of them.
    ///
    /// # Errors
    /// [`EnvelopeError`], one variant per check.
    pub fn parse(bytes: &[u8], expected: &'static str) -> Result<Self, EnvelopeError> {
        // Read the metadata first and the payload second: a body that is the
        // wrong *kind* of event should say so rather than failing on a field of
        // a type it was never going to be.
        let head: Head =
            serde_json::from_slice(bytes).map_err(|e| EnvelopeError::NotJson(e.to_string()))?;
        if head.specversion != SPEC_VERSION {
            return Err(EnvelopeError::WrongSpecVersion {
                found: head.specversion,
            });
        }
        if !crate::is_known(&head.r#type) {
            return Err(EnvelopeError::UnknownType { found: head.r#type });
        }
        if head.r#type != expected {
            return Err(EnvelopeError::WrongType {
                found: head.r#type,
                expected,
            });
        }
        serde_json::from_slice(bytes).map_err(|e| EnvelopeError::WrongData {
            expected,
            detail: e.to_string(),
        })
    }
}

/// Just enough of an event to decide whether to read the rest of it.
#[derive(Deserialize)]
struct Head {
    specversion: String,
    #[serde(rename = "type")]
    r#type: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Day {
        saved_eur: f64,
    }

    fn event() -> Event<Day> {
        Event::new(
            crate::SITE_DAY_REPORTED,
            "hems://sites/haus-1",
            "01924f1e-0000-7000-8000-000000000001",
            datetime!(2026-01-16 00:05:00 UTC),
            Day { saved_eur: 2.09 },
        )
        .about("2026-01-15")
    }

    #[test]
    fn an_event_round_trips_through_the_bytes_that_are_signed() {
        let sent = event();
        let bytes = sent.to_bytes().unwrap();
        let read = Event::<Day>::parse(&bytes, crate::SITE_DAY_REPORTED).unwrap();
        assert_eq!(read, sent);
    }

    #[test]
    fn the_wire_form_is_the_specification_s_own_field_names() {
        let json: serde_json::Value = serde_json::from_slice(&event().to_bytes().unwrap()).unwrap();
        assert_eq!(json["specversion"], "1.0");
        assert_eq!(json["type"], crate::SITE_DAY_REPORTED);
        assert_eq!(json["datacontenttype"], "application/json");
        assert_eq!(json["source"], "hems://sites/haus-1");
        assert_eq!(json["subject"], "2026-01-15");
        // RFC 3339 rather than a Unix second: an event a human has to read in a
        // log is one whose timestamp should not need a converter.
        assert_eq!(json["time"], "2026-01-16T00:05:00Z");
    }

    #[test]
    fn a_subject_that_was_never_set_is_absent_rather_than_null() {
        let bare = Event::new(
            crate::SITE_DAY_REPORTED,
            "hems://sites/haus-1",
            "id",
            datetime!(2026-01-16 00:05:00 UTC),
            Day { saved_eur: 0.0 },
        );
        let json: serde_json::Value = serde_json::from_slice(&bare.to_bytes().unwrap()).unwrap();
        assert!(json.get("subject").is_none());
    }

    #[test]
    fn an_event_type_outside_the_catalogue_is_refused() {
        // The typo case, and the reason `parse` checks the catalogue rather than
        // only the expected type: `reportd` would otherwise be indistinguishable
        // from a newer emitter and would be dropped without a name.
        let raw = br#"{"specversion":"1.0","id":"i","source":"s","type":"de.hems.site.day.reportd",
                      "time":"2026-01-16T00:05:00Z","datacontenttype":"application/json",
                      "data":{"saved_eur":1.0}}"#;
        assert!(matches!(
            Event::<Day>::parse(raw, crate::SITE_DAY_REPORTED),
            Err(EnvelopeError::UnknownType { .. })
        ));
    }

    #[test]
    fn a_catalogued_event_for_another_endpoint_says_so() {
        let other = Event::new(
            crate::SITE_PLAN_PUBLISHED,
            "hems://sites/haus-1",
            "id",
            datetime!(2026-01-16 00:05:00 UTC),
            Day { saved_eur: 0.0 },
        );
        let bytes = other.to_bytes().unwrap();
        assert!(matches!(
            Event::<Day>::parse(&bytes, crate::SITE_DAY_REPORTED),
            Err(EnvelopeError::WrongType {
                expected: crate::SITE_DAY_REPORTED,
                ..
            })
        ));
    }

    #[test]
    fn the_right_event_with_the_wrong_body_is_a_different_fault() {
        let raw =
            br#"{"specversion":"1.0","id":"i","source":"s","type":"de.hems.site.day.reported",
                      "time":"2026-01-16T00:05:00Z","datacontenttype":"application/json",
                      "data":{"saved_eur":"two euros"}}"#;
        assert!(matches!(
            Event::<Day>::parse(raw, crate::SITE_DAY_REPORTED),
            Err(EnvelopeError::WrongData { .. })
        ));
    }

    #[test]
    fn a_future_cloudevents_version_is_refused_rather_than_guessed_at() {
        let raw =
            br#"{"specversion":"2.0","id":"i","source":"s","type":"de.hems.site.day.reported",
                      "time":"2026-01-16T00:05:00Z","datacontenttype":"application/json",
                      "data":{"saved_eur":1.0}}"#;
        assert!(matches!(
            Event::<Day>::parse(raw, crate::SITE_DAY_REPORTED),
            Err(EnvelopeError::WrongSpecVersion { .. })
        ));
    }
}
