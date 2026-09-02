//! Every CloudEvents `type` the workspace emits, the envelope one travels in,
//! and the signature that says which box sent it.
//!
//! An event type is an interface. Spelling one differently in the emitter and
//! the consumer produces a system that runs, logs nothing unusual, and quietly
//! does not work — the failure mode that costs the most to find.
//!
//! So the names live here as constants, [`ALL`] lists them, and
//! `cargo xtask check-events` fails the build if a string literal that looks
//! like an event type appears anywhere in the workspace without being in this
//! catalogue.
//!
//! # Naming
//!
//! `de.hems.<aggregate>.<thing>.<past-tense-verb>` — reverse-DNS as CloudEvents
//! 1.0 recommends, and past tense because an event is something that has already
//! happened. `de.hems.grid.lpc.limit.received`, not `de.hems.grid.set_limit`.
//!
//! # One of them is on a wire, and the rest are not yet
//!
//! [`SITE_DAY_REPORTED`] is what a box sends `obsd` at the end of a day — the
//! only link between the edge and the fleet that exists — and it travels as a
//! CloudEvent in [`envelope`], signed with [`webhook`]. The rest of the
//! catalogue is the agreed vocabulary, written down before its first emitter
//! rather than reverse-engineered from six of them; it arrives with the driver
//! loop and the local bus `hemsd` still has to grow.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod envelope;
pub mod webhook;

pub use envelope::{EnvelopeError, Event};
pub use webhook::{Signed, WebhookError};

// ── Grid ────────────────────────────────────────────────────────────────────

/// A network operator's § 14a limit arrived.
pub const GRID_LPC_LIMIT_RECEIVED: &str = "de.hems.grid.lpc.limit.received";
/// A § 14a limit stopped applying.
pub const GRID_LPC_LIMIT_RELEASED: &str = "de.hems.grid.lpc.limit.released";
/// The link to the Energy Guard failed and the failsafe took over.
pub const GRID_LPC_FAILSAFE_ENTERED: &str = "de.hems.grid.lpc.failsafe.entered";
/// The failsafe was released, either by contact returning or by its own timer.
pub const GRID_LPC_FAILSAFE_LEFT: &str = "de.hems.grid.lpc.failsafe.left";
/// A § 9 EEG feed-in limit arrived.
pub const GRID_LPP_LIMIT_RECEIVED: &str = "de.hems.grid.lpp.limit.received";
/// A control event was recorded for the two-year evidence log, `[A1 7.3]`.
pub const GRID_EVIDENCE_RECORDED: &str = "de.hems.grid.evidence.recorded";
/// The commanded ceiling was below the minimum power owed, `[A1 4.5]`.
pub const GRID_BELOW_MINIMUM_DETECTED: &str = "de.hems.grid.below-minimum.detected";

// ── Settlement ──────────────────────────────────────────────────────────────

/// A month's MiSpeL Abgrenzung was computed for a site, `[MiSpeL A1]`.
pub const SETTLEMENT_MISPEL_ABGRENZUNG_COMPUTED: &str =
    "de.hems.settlement.mispel.abgrenzung.computed";
/// A year's MiSpeL Pauschal figures were computed for a site, `[MiSpeL A2]`.
pub const SETTLEMENT_MISPEL_PAUSCHAL_COMPUTED: &str = "de.hems.settlement.mispel.pauschal.computed";
/// A quarter hour was allocated over an energy-sharing community, § 42c EnWG.
pub const SETTLEMENT_SHARING_ALLOCATED: &str = "de.hems.settlement.sharing.allocated";

// ── Site ────────────────────────────────────────────────────────────────────

/// A site was commissioned or re-configured.
pub const SITE_CONFIGURED: &str = "de.hems.site.configured";
/// A new plan was published by the optimiser.
pub const SITE_PLAN_PUBLISHED: &str = "de.hems.site.plan.published";
/// The optimiser could not produce a plan.
pub const SITE_PLAN_FAILED: &str = "de.hems.site.plan.failed";
/// A setpoint was issued to a device.
pub const SITE_SETPOINT_ISSUED: &str = "de.hems.site.setpoint.issued";
/// The measured grid power and the sum of the assets disagree.
pub const SITE_BALANCE_DIVERGED: &str = "de.hems.site.balance.diverged";
/// A box reported what one local day came to, `hems_core::report::DayKpis`.
///
/// The one event on a wire today: `hemsd --report-to` sends it to `obsd`. It is
/// keyed on the **local day** rather than on the moment it was sent, so a box
/// that comes back after an outage and re-sends yesterday is correcting itself
/// rather than adding a day.
pub const SITE_DAY_REPORTED: &str = "de.hems.site.day.reported";

// ── Devices ─────────────────────────────────────────────────────────────────

/// A device was discovered on the local network.
pub const DEVICE_DISCOVERED: &str = "de.hems.device.discovered";
/// A device was paired and trusted.
pub const DEVICE_PAIRED: &str = "de.hems.device.paired";
/// A device stopped reporting.
pub const DEVICE_LOST: &str = "de.hems.device.lost";
/// A device refused a command.
pub const DEVICE_COMMAND_REJECTED: &str = "de.hems.device.command.rejected";

// ── Tariffs and forecasts ───────────────────────────────────────────────────

/// New prices are available for a site.
pub const TARIFF_PUBLISHED: &str = "de.hems.tariff.published";
/// A quarter hour with a negative day-ahead price is coming (§ 51 EEG).
pub const TARIFF_NEGATIVE_PRICE_AHEAD: &str = "de.hems.tariff.negative-price.ahead";
/// A new forecast is available.
pub const FORECAST_PUBLISHED: &str = "de.hems.forecast.published";
/// A forecast failed its calibration check.
pub const FORECAST_MISCALIBRATED: &str = "de.hems.forecast.miscalibrated";

// ── Flexibility ─────────────────────────────────────────────────────────────

/// A site offered its flexibility to an aggregator.
pub const FLEX_ENVELOPE_OFFERED: &str = "de.hems.flex.envelope.offered";
/// An aggregator dispatched a site.
pub const FLEX_DISPATCH_CONFIRMED: &str = "de.hems.flex.dispatch.confirmed";

/// Every type in the catalogue.
pub const ALL: &[&str] = &[
    GRID_LPC_LIMIT_RECEIVED,
    GRID_LPC_LIMIT_RELEASED,
    GRID_LPC_FAILSAFE_ENTERED,
    GRID_LPC_FAILSAFE_LEFT,
    GRID_LPP_LIMIT_RECEIVED,
    GRID_EVIDENCE_RECORDED,
    GRID_BELOW_MINIMUM_DETECTED,
    SETTLEMENT_MISPEL_ABGRENZUNG_COMPUTED,
    SETTLEMENT_MISPEL_PAUSCHAL_COMPUTED,
    SETTLEMENT_SHARING_ALLOCATED,
    SITE_CONFIGURED,
    SITE_PLAN_PUBLISHED,
    SITE_PLAN_FAILED,
    SITE_SETPOINT_ISSUED,
    SITE_BALANCE_DIVERGED,
    SITE_DAY_REPORTED,
    DEVICE_DISCOVERED,
    DEVICE_PAIRED,
    DEVICE_LOST,
    DEVICE_COMMAND_REJECTED,
    TARIFF_PUBLISHED,
    TARIFF_NEGATIVE_PRICE_AHEAD,
    FORECAST_PUBLISHED,
    FORECAST_MISCALIBRATED,
    FLEX_ENVELOPE_OFFERED,
    FLEX_DISPATCH_CONFIRMED,
];

/// Whether `candidate` is a catalogued event type.
#[must_use]
pub fn is_known(candidate: &str) -> bool {
    ALL.contains(&candidate)
}

/// The prefix every hems event type starts with.
pub const PREFIX: &str = "de.hems.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_is_namespaced_and_lower_case() {
        for t in ALL {
            assert!(t.starts_with(PREFIX), "{t} is outside the namespace");
            assert_eq!(*t, t.to_lowercase(), "{t} is not lower case");
            assert!(
                !t.contains('_'),
                "{t} uses an underscore; the separator is '-'"
            );
        }
    }

    #[test]
    fn the_catalogue_has_no_duplicates() {
        for (i, t) in ALL.iter().enumerate() {
            assert!(!ALL[..i].contains(t), "{t} appears twice");
        }
    }

    #[test]
    fn lookup_works_both_ways() {
        assert!(is_known(GRID_LPC_LIMIT_RECEIVED));
        assert!(!is_known("de.hems.grid.lpc.limit.invented"));
    }
}

/// Whether `event_type` matches a subscription `pattern`.
///
/// A glob: `*` stands for any run of characters and `?` for one. So
/// `de.hems.grid.*` wakes on every § 14a event and `*` on all of them.
///
/// The one matcher every subscription mechanism uses, for the reason [`ALL`]
/// exists: a pattern written at the place that dispatches is one nobody can
/// list, and a typo in one silently matches nothing — which looks exactly like a
/// subscriber with nothing to say.
#[must_use]
pub fn matches(pattern: &str, event_type: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    // Iterative rather than recursive, with one backtrack point: a pattern is
    // short and a stack overflow on a configuration string would be a poor way
    // to learn that somebody wrote `**`.
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = event_type.chars().collect();
    let (mut pi, mut vi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, vi));
            pi += 1;
        } else if let Some((sp, sv)) = star {
            pi = sp + 1;
            vi = sv + 1;
            star = Some((sp, vi));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod subscription_patterns {
    use super::*;

    #[test]
    fn a_family_wakes_on_everything_under_it() {
        assert!(matches("de.hems.grid.*", GRID_LPC_LIMIT_RECEIVED));
        assert!(matches("de.hems.grid.*", GRID_BELOW_MINIMUM_DETECTED));
        assert!(!matches("de.hems.grid.*", SITE_DAY_REPORTED));
        assert!(matches("*", SITE_DAY_REPORTED));
    }

    #[test]
    fn an_exact_pattern_wakes_on_one_type() {
        assert!(matches(SITE_DAY_REPORTED, SITE_DAY_REPORTED));
        assert!(!matches(SITE_DAY_REPORTED, SITE_PLAN_PUBLISHED));
    }

    #[test]
    fn a_pattern_that_matches_nothing_in_the_catalogue_is_findable() {
        // The reason this lives beside `ALL`: a subscription nobody can list is
        // a subscriber that never runs, and that looks exactly like a
        // subscriber with nothing to say.
        assert!(
            !ALL.iter().any(|t| matches("de.hems.grid.lpc.limit", t)),
            "the family needs its wildcard, and a test says so rather than a \
             specialist silently never waking"
        );
        assert!(ALL.iter().any(|t| matches("de.hems.grid.lpc.*", t)));
    }

    #[test]
    fn a_star_in_the_middle_backtracks_correctly() {
        assert!(matches("de.*.day.reported", SITE_DAY_REPORTED));
        assert!(matches("de.hems.*.reported", SITE_DAY_REPORTED));
        assert!(!matches("de.hems.*.refused", SITE_DAY_REPORTED));
        assert!(matches("de.hems.site.day.reporte?", SITE_DAY_REPORTED));
    }
}
