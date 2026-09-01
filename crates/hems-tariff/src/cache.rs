//! What the box still knows when the WAN is gone.
//!
//! `tariffd` fetches a day-ahead curve from one of five sources
//! ([`crate::source`]). Between fetches, and for as long as an outage lasts, the
//! planner still has to be given prices — so what has arrived is kept, and this
//! module is the part of keeping it that is a *decision* rather than a socket.
//!
//! # A curve arrives twice, and the two disagree
//!
//! The day-ahead auction clears once, but a box asks for it more than once: at
//! 13:00 when it is published, again at 14:00 because the first request timed
//! out, again at midnight from a different source because the first one is down.
//! Three answers for the same quarter hour, and they will not all be equal —
//! ENTSO-E publishes to the cent, aWATTar rounds, SMARD restates.
//!
//! So the reconciliation rule is written down rather than left to whichever
//! write happened last:
//!
//! 1. a **more trusted** source replaces a less trusted one, always;
//! 2. at equal trust, the **later** observation wins, because a restatement is a
//!    correction;
//! 3. a **finer** publication wins over a coarser one at the same trust, because
//!    four quarter hours that were really one hour are not four observations.
//!
//! Rule 1 is the one that matters and it is the one "last write wins" gets
//! wrong: a Tibber curve that arrives after an ENTSO-E one is not a correction
//! of it, it is a different number computed for a different purpose, and letting
//! it overwrite makes the plan follow whichever source answered most recently.
//!
//! # Why it holds two days and not one
//!
//! `[§ 41a EnWG]` tariffs are day-ahead: tomorrow's curve is published in the
//! early afternoon of today. A box that kept one day would have nothing for
//! tomorrow morning until the afternoon fetch succeeded, and a planner with a
//! 24-hour horizon asks about tomorrow from the moment it is past midnight. Two
//! days is also what an outage needs: a WAN that comes back within 48 hours
//! never costs the household a plan.

use std::collections::BTreeMap;

use hems_core::prelude::{Horizon, Slot};
use rust_decimal::Decimal;
use time::OffsetDateTime;

use crate::source::{PriceBasis, PriceSeries, Source};

/// How long a cached price is worth keeping.
///
/// Two days back as well as forward: a settlement question asked this morning is
/// about yesterday, and a curve dropped at midnight is a curve that has to be
/// fetched again to answer it.
pub const RETENTION: time::Duration = time::Duration::days(2);

/// How far the sources are trusted, highest first.
///
/// Not a preference — a statement about what each one *is*. ENTSO-E is the
/// auction operator's own publication and is the record; SMARD is the
/// Bundesnetzagentur restating it, half an hour later and to the same cent;
/// aWATTar and Tibber are resellers publishing a curve they have already shaped
/// for their own product, and Energy-Charts is a research portal that publishes
/// carbon rather than price.
///
/// A household on a Tibber tariff should still take ENTSO-E's number for the
/// *auction*, because that is what its own contract is indexed to.
#[must_use]
pub const fn trust(source: Source) -> u8 {
    match source {
        Source::Entsoe => 4,
        Source::Smard => 3,
        Source::Awattar => 2,
        Source::Tibber => 1,
        Source::EnergyCharts => 0,
    }
}

/// One quarter hour's price, and where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Observed {
    /// The wholesale price, ct/kWh.
    #[cfg_attr(feature = "serde", serde(with = "rust_decimal::serde::str"))]
    pub price_ct: Decimal,
    /// Which source published it.
    pub source: Source,
    /// The resolution it was published at, minutes.
    pub published_minutes: u16,
    /// When the box observed it.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub observed_at: OffsetDateTime,
}

impl Observed {
    /// Whether `other` should replace this observation.
    ///
    /// The three rules of the module note, in the order they are written.
    #[must_use]
    pub fn superseded_by(&self, other: &Self) -> bool {
        match trust(other.source).cmp(&trust(self.source)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => {
                match other.published_minutes.cmp(&self.published_minutes) {
                    // Finer wins: four quarter hours that were really one hour
                    // are one observation, not four.
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                    // A restatement at the same resolution from the same source
                    // is a correction, and the later one is the correction.
                    std::cmp::Ordering::Equal => other.observed_at >= self.observed_at,
                }
            }
        }
    }
}

/// What the box knows about prices, and how old each part of it is.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PriceCache {
    #[cfg_attr(feature = "serde", serde(default))]
    points: BTreeMap<Slot, Observed>,
}

/// What one merge changed, so a daemon can tell a fetch that taught it something
/// from one that did not.
///
/// The distinction between [`Merged::replaced`] and [`Merged::confirmed`] is the
/// one that matters and it is easy to miss. A source polled every quarter of an
/// hour answers with the same day-ahead curve every time; if re-receiving a
/// number the cache already holds counted as a *correction*, every poll would
/// look like progress and the quiet failure this is built to surface — a source
/// answering `200` with a curve that stopped moving on Tuesday — would never be
/// visible. So a re-observation refreshes the provenance and reports itself as
/// having changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Merged {
    /// Quarter hours the cache had never seen.
    pub added: usize,
    /// Quarter hours whose price is now a **different** number.
    pub replaced: usize,
    /// Quarter hours re-observed at the same price.
    pub confirmed: usize,
    /// Quarter hours the cache preferred its own, more trusted, answer for.
    pub kept: usize,
}

impl Merged {
    /// Whether the cache learned anything it did not already know.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added == 0 && self.replaced == 0
    }

    /// How many quarter hours the merge looked at.
    #[must_use]
    pub const fn considered(&self) -> usize {
        self.added + self.replaced + self.confirmed + self.kept
    }
}

impl PriceCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take in what a source published, observed at `at`.
    ///
    /// A **gross consumer** series is refused rather than merged: adding a
    /// markup and the whole levy stack to a price that already contains them
    /// prices a kilowatt-hour at about twice what it costs, and the cache is
    /// where that mistake would become permanent.
    pub fn merge(&mut self, series: &PriceSeries, at: OffsetDateTime) -> Merged {
        let mut merged = Merged::default();
        if series.basis != PriceBasis::Wholesale {
            return merged;
        }
        for (slot, price_ct) in &series.points {
            let candidate = Observed {
                price_ct: *price_ct,
                source: series.source,
                published_minutes: series.published_minutes,
                observed_at: at,
            };
            match self.points.get(slot) {
                None => {
                    self.points.insert(*slot, candidate);
                    merged.added += 1;
                }
                Some(existing) if existing.superseded_by(&candidate) => {
                    // The same number arriving again is not a correction. It
                    // still refreshes the provenance — the box has seen it more
                    // recently, and from possibly a better source — but it is
                    // reported as having taught nothing, which is what lets a
                    // daemon notice a feed that has stopped moving.
                    let unchanged = existing.price_ct == candidate.price_ct;
                    self.points.insert(*slot, candidate);
                    if unchanged {
                        merged.confirmed += 1;
                    } else {
                        merged.replaced += 1;
                    }
                }
                Some(_) => merged.kept += 1,
            }
        }
        merged
    }

    /// Drop everything more than [`RETENTION`] away from `now`, in either
    /// direction.
    ///
    /// Both directions, because a cache that only ever grew forward would carry
    /// every quarter hour the box has ever seen into a gateway's flash.
    pub fn prune(&mut self, now: OffsetDateTime) {
        self.points
            .retain(|slot, _| (slot.start() - now).abs() <= RETENTION);
    }

    /// The spot map for [`crate::tariff::EnergyPrice::Dynamic`].
    #[must_use]
    pub fn spot(&self) -> BTreeMap<Slot, Decimal> {
        self.points
            .iter()
            .map(|(slot, o)| (*slot, o.price_ct))
            .collect()
    }

    /// What the cache holds for one quarter hour.
    #[must_use]
    pub fn at(&self, slot: Slot) -> Option<&Observed> {
        self.points.get(&slot)
    }

    /// How many quarter hours are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// How much of `horizon` the cache can price, in `[0, 1]`.
    ///
    /// The readiness question, and it is deliberately not "is the last fetch
    /// recent". A daemon that fetched five minutes ago and got nothing useful is
    /// not ready; one whose last fetch failed but which still holds tomorrow is.
    #[must_use]
    pub fn coverage(&self, horizon: Horizon) -> f64 {
        if horizon.len == 0 {
            return 1.0;
        }
        let held = horizon
            .slots()
            .filter(|s| self.points.contains_key(s))
            .count();
        held as f64 / horizon.len as f64
    }

    /// The last quarter hour the cache can price without a gap from `from`.
    ///
    /// A horizon is planned in order, so what matters is not how many slots are
    /// held but how far the *unbroken* run reaches: a cache holding tomorrow
    /// morning and tomorrow evening with the afternoon missing can plan neither.
    #[must_use]
    pub fn contiguous_until(&self, from: Slot) -> Option<Slot> {
        if !self.points.contains_key(&from) {
            return None;
        }
        let mut last = from;
        while self.points.contains_key(&last.next()) {
            last = last.next();
        }
        Some(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOON: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    fn series(source: Source, ct: i64, slots: usize, minutes: u16) -> PriceSeries {
        let first = Slot::containing(NOON);
        PriceSeries {
            points: (0..slots)
                .map(|i| (first.offset(i64::try_from(i).unwrap()), Decimal::new(ct, 0)))
                .collect(),
            source,
            basis: PriceBasis::Wholesale,
            published_minutes: minutes,
        }
    }

    #[test]
    fn a_first_curve_is_taken_whole() {
        let mut cache = PriceCache::new();
        let merged = cache.merge(&series(Source::Awattar, 8, 4, 60), NOON);
        assert_eq!(merged.added, 4);
        assert_eq!(cache.len(), 4);
    }

    #[test]
    fn a_more_trusted_source_replaces_a_less_trusted_one_whenever_it_arrives() {
        // The rule "last write wins" gets wrong. A Tibber curve arriving after
        // an ENTSO-E one is not a correction of it; it is a different number
        // computed for a different purpose.
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Entsoe, 8, 4, 15), NOON);
        let later = NOON + time::Duration::hours(1);
        let merged = cache.merge(&series(Source::Tibber, 30, 4, 15), later);
        assert_eq!(merged.replaced, 0);
        assert_eq!(merged.kept, 4);
        assert_eq!(
            cache.at(Slot::containing(NOON)).unwrap().source,
            Source::Entsoe
        );
    }

    #[test]
    fn a_less_trusted_source_fills_a_gap_the_trusted_one_left() {
        // Trust decides who *wins*, not who may speak: a box with no ENTSO-E
        // token still has to be able to plan.
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Tibber, 30, 4, 60), NOON);
        assert_eq!(cache.len(), 4);
        assert_eq!(
            cache.at(Slot::containing(NOON)).unwrap().source,
            Source::Tibber
        );
    }

    #[test]
    fn the_same_curve_arriving_again_is_confirmation_rather_than_a_correction() {
        // The quiet failure this exists to surface: a source answering 200 with
        // a curve that stopped moving on Tuesday. If a re-observation counted as
        // a correction, every poll would look like progress.
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Smard, 8, 4, 15), NOON);
        let later = NOON + time::Duration::minutes(15);
        let merged = cache.merge(&series(Source::Smard, 8, 4, 15), later);
        assert_eq!(merged.confirmed, 4);
        assert_eq!(merged.replaced, 0);
        assert!(merged.is_empty(), "nothing was learned");
        // …and the provenance is still refreshed, because the box has seen it.
        assert_eq!(cache.at(Slot::containing(NOON)).unwrap().observed_at, later);
    }

    #[test]
    fn a_restatement_from_the_same_source_is_a_correction() {
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Smard, 8, 4, 15), NOON);
        let later = NOON + time::Duration::hours(1);
        let merged = cache.merge(&series(Source::Smard, 9, 4, 15), later);
        assert_eq!(merged.replaced, 4);
        assert_eq!(
            cache.at(Slot::containing(NOON)).unwrap().price_ct,
            Decimal::new(9, 0)
        );
    }

    #[test]
    fn a_finer_publication_wins_over_a_coarser_one() {
        // Four quarter hours that were really one hourly number are one
        // observation, not four — so a source that later publishes the real
        // quarter hours is telling the box something it did not have.
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Smard, 8, 4, 60), NOON);
        let merged = cache.merge(&series(Source::Smard, 9, 4, 15), NOON);
        assert_eq!(merged.replaced, 4);
        assert_eq!(
            cache.at(Slot::containing(NOON)).unwrap().published_minutes,
            15
        );

        // …and not the other way round.
        let later = NOON + time::Duration::hours(2);
        let merged = cache.merge(&series(Source::Smard, 7, 4, 60), later);
        assert_eq!(merged.kept, 4);
    }

    #[test]
    fn a_gross_consumer_series_is_refused_rather_than_cached() {
        // It would be priced through the whole levy stack a second time.
        let mut cache = PriceCache::new();
        let mut gross = series(Source::Tibber, 30, 4, 60);
        gross.basis = PriceBasis::GrossConsumer;
        assert!(cache.merge(&gross, NOON).is_empty());
        assert!(cache.is_empty());
    }

    #[test]
    fn pruning_drops_both_ends() {
        let mut cache = PriceCache::new();
        let first = Slot::containing(NOON);
        cache.merge(&series(Source::Smard, 8, 4, 15), NOON);
        // A curve from a week ago and one from a week ahead.
        let old = PriceSeries {
            points: [(first.offset(-96 * 7), Decimal::ONE)]
                .into_iter()
                .collect(),
            ..series(Source::Smard, 8, 0, 15)
        };
        let future = PriceSeries {
            points: [(first.offset(96 * 7), Decimal::ONE)].into_iter().collect(),
            ..series(Source::Smard, 8, 0, 15)
        };
        cache.merge(&old, NOON);
        cache.merge(&future, NOON);
        assert_eq!(cache.len(), 6);
        cache.prune(NOON);
        assert_eq!(cache.len(), 4, "both ends go");
    }

    #[test]
    fn coverage_is_about_the_horizon_and_not_about_the_last_fetch() {
        // A daemon whose last fetch failed but which still holds tomorrow is
        // ready; one that fetched five minutes ago and learned nothing is not.
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Smard, 8, 48, 15), NOON);
        let horizon = Horizon::new(NOON, 96);
        assert!((cache.coverage(horizon) - 0.5).abs() < 1e-12);
        assert!((cache.coverage(Horizon::new(NOON, 0)) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_hole_in_the_middle_stops_the_contiguous_run() {
        // A horizon is planned in order: holding tomorrow morning and tomorrow
        // evening with the afternoon missing plans neither.
        let mut cache = PriceCache::new();
        cache.merge(&series(Source::Smard, 8, 8, 15), NOON);
        let first = Slot::containing(NOON);
        let gapped = PriceSeries {
            points: [(first.offset(10), Decimal::ONE)].into_iter().collect(),
            ..series(Source::Smard, 8, 0, 15)
        };
        cache.merge(&gapped, NOON);
        assert_eq!(cache.contiguous_until(first), Some(first.offset(7)));
        assert_eq!(cache.contiguous_until(first.offset(9)), None);
    }
}
