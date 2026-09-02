//! Which event reaches which specialist.
//!
//! # The table is data, and it is checked against the catalogue
//!
//! A subscription written as a string literal where it dispatches is one nobody
//! can list. Worse, a typo in it silently matches nothing — and a specialist
//! that never runs looks exactly like a specialist with nothing to say — a
//! mechanism built, documented, tested by its own tests and reached by nothing
//! (D91, D99).
//!
//! So the table is one `const`, its patterns go through
//! [`hems_events::matches`], and a test asserts that **every pattern matches at
//! least one type in [`hems_events::ALL`]**. A rename in the catalogue that
//! orphans a subscription here fails the build rather than quietly stopping a
//! specialist.

use hems_events::matches;

/// One specialist, and what wakes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subscription {
    /// The specialist's name, as its [`agentplane::prelude::SkillDescriptor`]
    /// states it.
    pub specialist: &'static str,
    /// The event types that wake it.
    pub on: &'static [&'static str],
}

/// The table.
///
/// Deliberately small. A specialist that subscribes to everything runs on every
/// event and has an opinion about most of them, which is how an advisory queue
/// stops being read.
pub const TABLE: &[Subscription] = &[
    Subscription {
        specialist: crate::skills::compliance::NAME,
        // A day carries the compliance record, and a ceiling below the
        // `[A1 4.5]` minimum arrives separately because the answer to it is a
        // question for the network operator rather than for the box.
        on: &[
            hems_events::SITE_DAY_REPORTED,
            hems_events::GRID_BELOW_MINIMUM_DETECTED,
        ],
    },
    Subscription {
        specialist: crate::skills::provenance::NAME,
        // Only the day. What this specialist reads is the ratio between kinds of
        // day, and no single grid event changes it.
        on: &[hems_events::SITE_DAY_REPORTED],
    },
];

/// The specialists an event wakes, in table order.
#[must_use]
pub fn specialists_for(event_type: &str) -> Vec<&'static str> {
    TABLE
        .iter()
        .filter(|s| s.on.iter().any(|pattern| matches(pattern, event_type)))
        .map(|s| s.specialist)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_matches_something_in_the_catalogue() {
        // A subscription that matches nothing is a specialist that never runs,
        // and from outside that is indistinguishable from one with nothing to
        // say. A rename in `hems-events` fails here rather than in production.
        for subscription in TABLE {
            for pattern in subscription.on {
                assert!(
                    hems_events::ALL.iter().any(|t| matches(pattern, t)),
                    "{} subscribes to {pattern:?}, which matches no event type \
                     this workspace emits",
                    subscription.specialist
                );
            }
        }
    }

    #[test]
    fn every_subscribed_specialist_is_one_the_daemon_registers() {
        // The other direction: a table row naming a specialist nothing wires is
        // a dispatch into nothing.
        let registered = crate::registered_specialists();
        for subscription in TABLE {
            assert!(
                registered.contains(&subscription.specialist),
                "{} is subscribed and not registered",
                subscription.specialist
            );
        }
    }

    #[test]
    fn a_reported_day_wakes_both_specialists_and_a_price_wakes_neither() {
        assert_eq!(
            specialists_for(hems_events::SITE_DAY_REPORTED),
            vec![
                crate::skills::compliance::NAME,
                crate::skills::provenance::NAME
            ]
        );
        assert_eq!(
            specialists_for(hems_events::GRID_BELOW_MINIMUM_DETECTED),
            vec![crate::skills::compliance::NAME]
        );
        assert!(
            specialists_for(hems_events::SITE_PLAN_PUBLISHED).is_empty(),
            "a plan is not an advisory question"
        );
    }
}
