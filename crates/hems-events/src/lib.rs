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
