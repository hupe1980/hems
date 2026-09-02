//! What the household itself asks for.
//!
//! "Charge the car now, I do not care what it costs." "Leave the heat pump
//! alone this evening." "We are away until Sunday." A plan that cannot be
//! overruled by the person paying for the electricity is a plan they will
//! eventually pull the fuse on.
//!
//! # It goes through the arbiter, and therefore through the guard
//!
//! An override is a **desire**, not a setpoint. `hems_realtime::Arbiter` reads
//! it first and then narrows it into whatever the grid, the fuses and the
//! hardware leave open — so `[BK6-22-300 A1 4.6 S. 3]` still holds, and a
//! household in the middle of a § 14a reduction that presses *boost* gets as
//! much as the reduction allows and not a watt more.
//!
//! That is why this is the one write on the local API. An endpoint that set a
//! value on a driver would have gone round the guard; one that sets an override
//! cannot, because the only thing it changes is what the arbiter *wants*.
//!
//! # They expire
//!
//! An override with no end is a household that boosted its car in March and
//! wonders in July why its bill is what it is. Every one carries an expiry, the
//! default is generous rather than clever, and the arbiter simply stops seeing
//! it — which returns the asset to the plan without anything having to be
//! cancelled.

use std::collections::BTreeMap;
use std::sync::Arc;

use hems_core::prelude::AssetId;
use hems_core::setpoint::UserOverride;
use time::OffsetDateTime;
use tokio::sync::RwLock;

/// How long an override lasts when the caller does not say.
///
/// Four hours: long enough to charge a car or warm a house, short enough that a
/// household which forgot about it is not still paying for it tomorrow.
pub const DEFAULT_FOR: time::Duration = time::Duration::hours(4);

/// The longest one may last.
///
/// A day. Anything longer is a change of *configuration* — a household that
/// genuinely wants its battery left alone has said something about its house
/// rather than about this afternoon, and a setting is where that belongs.
pub const MAX_FOR: time::Duration = time::Duration::hours(24);

/// One override, and when it stops applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// What the household asked for.
    pub what: UserOverride,
    /// When it stops applying.
    pub until: OffsetDateTime,
}

/// The overrides in force, shared with the control loop.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    held: Arc<RwLock<BTreeMap<AssetId, Held>>>,
}

impl Overrides {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for one, clamped to [`MAX_FOR`].
    ///
    /// Replaces whatever was in force for that asset: a household pressing
    /// *boost* twice means one boost, not two, and a second press extends it.
    pub async fn set(
        &self,
        asset: AssetId,
        what: UserOverride,
        for_: Option<time::Duration>,
        now: OffsetDateTime,
    ) -> Held {
        let held = Held {
            what,
            until: now
                + for_
                    .unwrap_or(DEFAULT_FOR)
                    .clamp(time::Duration::ZERO, MAX_FOR),
        };
        self.held.write().await.insert(asset, held);
        held
    }

    /// Withdraw one. Returns whether anything was in force.
    pub async fn clear(&self, asset: &AssetId) -> bool {
        self.held.write().await.remove(asset).is_some()
    }

    /// Withdraw all of them.
    pub async fn clear_all(&self) -> usize {
        let mut held = self.held.write().await;
        let n = held.len();
        held.clear();
        n
    }

    /// What is in force now, as the arbiter wants it.
    ///
    /// Expired entries are dropped on the way past rather than swept on a timer
    /// of their own: the only thing that cares is this call, and a sweep is a
    /// second place for the two to disagree about what "now" is.
    pub async fn active(&self, now: OffsetDateTime) -> BTreeMap<AssetId, UserOverride> {
        let mut held = self.held.write().await;
        held.retain(|_, h| h.until > now);
        held.iter().map(|(id, h)| (id.clone(), h.what)).collect()
    }

    /// What is in force, with the expiry, for the status surface.
    pub async fn all(&self, now: OffsetDateTime) -> BTreeMap<AssetId, Held> {
        let mut held = self.held.write().await;
        held.retain(|_, h| h.until > now);
        held.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 12:00:00 UTC);

    fn car() -> AssetId {
        AssetId::new("wallbox").expect("a valid identifier")
    }

    #[tokio::test]
    async fn an_override_stops_applying_on_its_own() {
        // The whole reason they carry an expiry: a household that boosted its
        // car in March must not still be paying for it in July, and nothing
        // should have to remember to cancel it.
        let overrides = Overrides::new();
        overrides
            .set(
                car(),
                UserOverride::Boost,
                Some(time::Duration::hours(2)),
                NOW,
            )
            .await;
        assert_eq!(overrides.active(NOW).await.len(), 1);
        assert!(
            overrides
                .active(NOW + time::Duration::hours(3))
                .await
                .is_empty(),
            "and the asset goes back to the plan with nothing cancelled"
        );
    }

    #[tokio::test]
    async fn a_request_for_a_week_is_clamped_to_a_day() {
        // Longer than a day is a statement about the *house* rather than about
        // this afternoon, and a setting is where that belongs.
        let overrides = Overrides::new();
        let held = overrides
            .set(
                car(),
                UserOverride::Pause,
                Some(time::Duration::days(7)),
                NOW,
            )
            .await;
        assert_eq!(held.until, NOW + MAX_FOR);
    }

    #[tokio::test]
    async fn pressing_boost_twice_is_one_boost() {
        let overrides = Overrides::new();
        overrides.set(car(), UserOverride::Boost, None, NOW).await;
        let second = overrides
            .set(
                car(),
                UserOverride::Boost,
                None,
                NOW + time::Duration::hours(1),
            )
            .await;
        let all = overrides.all(NOW + time::Duration::hours(1)).await;
        assert_eq!(all.len(), 1, "one asset, one override");
        assert_eq!(
            all[&car()].until,
            second.until,
            "and the second press extends it rather than queueing behind it"
        );
    }

    #[tokio::test]
    async fn a_negative_duration_is_not_an_override_that_never_ends() {
        // A caller that asks for minus an hour has made a mistake, and the
        // clamp's floor is what stops it becoming an override in the past that
        // `active` keeps for ever by comparing the wrong way round.
        let overrides = Overrides::new();
        overrides
            .set(
                car(),
                UserOverride::Away,
                Some(-time::Duration::hours(1)),
                NOW,
            )
            .await;
        assert!(overrides.active(NOW).await.is_empty());
    }
}
