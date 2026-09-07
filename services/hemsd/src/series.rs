//! The box's own one-second measurement series.
//!
//! The guard runs once a control period against what the meters say. `[A1 7.2]`
//! asks the box to keep the minute-resolution trace during a § 14a reduction and
//! nothing more, which leaves the commonest question a household or an installer
//! has — "what was the battery doing at half past two" — unanswerable on the box
//! that watched it. This is where those readings go.
//!
//! # What goes here, and what must not
//!
//! **The series, never the settlement.** `chronix`'s `FieldValue` is
//! `f64`/`i64`/`u64`/`bool`/`String`; a MiSpeL register is an exact decimal, and
//! P3 forbids one through an `f64` (D168). So the quarter-hour registers, the
//! § 14a evidence and the box's identity stay in `redb` ([`crate::store`]), and
//! what lives here is measured power — an `f64` before it arrived, and
//! diagnostic rather than statutory.
//!
//! That split is also what makes the durability trade acceptable.
//! `ChronixConfig::small` coalesces its WAL fsync to every five seconds, so a
//! power cut can cost five seconds of the trace. Five seconds of a diagnostic
//! series is a different loss from five seconds of a legal record, and the
//! record is not here.
//!
//! # One measurement, one tag
//!
//! `power{point=…}` with a single `watts` field. A tag per *point of
//! measurement* rather than a field per asset, because a household adds and
//! removes assets and a schema that grew a column for each would make the series
//! of a site that changed unreadable against the series of one that did not.
//! Cardinality is bounded by the number of assets a house has.
//!
//! The sign is `hems-core`'s throughout: **positive is drawn**, negative is fed
//! back. A series that flipped the convention per point would be a series
//! nobody can sum.

use std::collections::BTreeMap;
use std::path::Path;

use chronix::chronix_query;
use chronix::prelude::{
    Chronix, ChronixConfig, FieldValue, Point, RecordBatch, SeriesKey, Timestamp,
};
use hems_core::prelude::{AssetId, Power};
use time::OffsetDateTime;

/// A wall-clock instant as the store's own timestamp: nanoseconds since the
/// epoch, signed.
fn nanos(at: OffsetDateTime) -> Timestamp {
    // Saturating rather than `as`: an instant outside `i64` nanoseconds is the
    // year 2262, and a silent wrap would put a reading in 1823.
    i64::try_from(at.unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

/// The measurement every point is written under.
const MEASUREMENT: &str = "power";

/// The tag naming the point of measurement.
const POINT: &str = "point";

/// The one field. Watts, load convention.
const WATTS: &str = "watts";

/// The point of measurement for the connection point itself.
pub const GRID: &str = "grid";

/// The netzwirksamer Leistungsbezug, `[A1 2.3]` — what a § 14a ceiling is
/// measured against, which is not the same as the grid draw.
pub const NETZWIRKSAM: &str = "netzwirksam";

/// Why the series could not be opened or written.
#[derive(Debug, thiserror::Error)]
pub enum SeriesError {
    /// The store itself.
    #[error("the measurement series failed: {0}")]
    Store(String),
    /// A tag or field name the store will not accept.
    ///
    /// An asset identifier is already narrower than `chronix`'s own rule, so
    /// this is unreachable for anything `hems-core` will construct — and it is
    /// an error rather than a panic because the alternative to recording a
    /// measurement is never a crash.
    #[error("{what} is not a name the series accepts: {detail}")]
    NotAName {
        /// Which name.
        what: String,
        /// What the store said.
        detail: String,
    },
}

/// One reading, as it comes back out.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Reading {
    /// When it was taken.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// What was measured, watts, load convention.
    pub watts: f64,
}

/// The box's measurement series.
pub struct Series {
    db: Chronix,
}

impl Series {
    /// Open — or create — the series at `path`, keeping `keep_days` of it.
    ///
    /// [`ChronixConfig::small`] is the gateway profile: a memory budget of about
    /// 48 MB, a single compaction worker, and a WAL sized for flash. Retention
    /// is enforced by the store's own maintenance thread, so there is no sweep
    /// for this daemon to schedule and forget.
    ///
    /// # Errors
    /// [`SeriesError::Store`] where the directory cannot be opened or locked —
    /// which is what a second `hemsd` on the same box looks like.
    pub fn open(path: &Path, keep_days: u16) -> Result<Self, SeriesError> {
        let mut config = ChronixConfig::small(path);
        // Every field stays overridable after `small()`, which is what its own
        // documentation promises — so retention is set here rather than by
        // rebuilding the preset and risking a different one.
        config.retention = Some(std::time::Duration::from_secs(
            u64::from(keep_days.max(1)) * 24 * 3_600,
        ));
        let db = Chronix::open(config).map_err(|e| SeriesError::Store(e.to_string()))?;
        Ok(Self { db })
    }

    /// Record one tick's worth of measurements.
    ///
    /// A **batch**, because they share an instant: written one at a time they
    /// would be one WAL append each for readings that are one observation, and a
    /// reader asking what the house was doing at 14:32:05 could see half of it.
    ///
    /// # Errors
    /// [`SeriesError::Store`] where the write fails.
    pub fn record(&self, at: OffsetDateTime, points: &[(&str, Power)]) -> Result<(), SeriesError> {
        if points.is_empty() {
            return Ok(());
        }
        let timestamp = nanos(at);
        let mut batch = Vec::with_capacity(points.len());
        for (point, power) in points {
            batch.push(Self::point(point, *power, timestamp)?);
        }
        self.db
            .insert_batch(&batch)
            .map(|_| ())
            .map_err(|e| SeriesError::Store(e.to_string()))
    }

    fn point(point: &str, power: Power, timestamp: Timestamp) -> Result<Point, SeriesError> {
        let mut tags = BTreeMap::new();
        tags.insert(POINT.to_owned(), point.to_owned());
        let key = SeriesKey::new(MEASUREMENT, tags).map_err(|e| SeriesError::NotAName {
            what: point.to_owned(),
            detail: e.to_string(),
        })?;
        let mut fields = BTreeMap::new();
        fields.insert(WATTS.to_owned(), FieldValue::F64(power.get()));
        Point::new(key, fields, timestamp).map_err(|e| SeriesError::NotAName {
            what: point.to_owned(),
            detail: e.to_string(),
        })
    }

    /// Every reading for one point of measurement in `[from, to)`, in order.
    ///
    /// **Half-open**, like every other window in this workspace — a `Slot`,
    /// `histd`'s register window, a Nachweis. `chronix`'s own `TimeRange` is
    /// inclusive at *both* ends, so the conversion happens here, once, rather
    /// than at each caller: two windows laid end to end must not both contain
    /// the instant they meet at, and a reader who used this beside
    /// `Store::quarter_hours_between` would otherwise get a different rule from
    /// each.
    ///
    /// # Errors
    /// [`SeriesError::Store`] where the read fails.
    pub fn between(
        &self,
        point: &str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<Reading>, SeriesError> {
        let plan = self
            .db
            .query()
            .measurement(MEASUREMENT)
            .tag(POINT, point)
            // One nanosecond short of `to`, which is what makes the window
            // half-open against an inclusive range. The store's resolution *is*
            // the nanosecond, so this excludes exactly the end instant and
            // nothing else.
            .range(nanos(from), nanos(to).saturating_sub(1))
            .build()
            .map_err(|e| SeriesError::Store(e.to_string()))?;
        let batch = self
            .db
            .execute(&plan)
            .map_err(|e| SeriesError::Store(e.to_string()))?;
        Ok(readings(&batch))
    }

    /// Flush and release the directory lock.
    ///
    /// Called on the way out rather than left to `Drop`: what is in the memtable
    /// when a process ends is the most recent few seconds, which is the part of a
    /// diagnostic trace somebody is most likely to be looking for.
    ///
    /// # Errors
    /// [`SeriesError::Store`] where the flush fails.
    pub fn close(self) -> Result<(), SeriesError> {
        self.db
            .close()
            .map_err(|e| SeriesError::Store(e.to_string()))
    }
}

/// Turn one Arrow batch into readings.
///
/// The time column is `_time`, which is the store's own name for it everywhere —
/// a field called `timestamp` would be a second column with the same meaning.
///
/// It is read as the **`Int64`** it is stored as, and not through
/// `chronix_query::extract_f64`, which is the obvious call and is wrong here:
/// nanoseconds since the epoch is about 1,8 × 10¹⁸ in 2026, which needs 61 bits,
/// and an `f64` mantissa carries 53. Every instant would come back rounded to
/// the nearest 256 ns. Nothing in this workspace would notice at a one-second
/// cadence, which is exactly why it is worth saying: the *value* column is a
/// genuine `f64` and goes through the helper, and the time column never can.
fn readings(batch: &RecordBatch) -> Vec<Reading> {
    use arrow::array::{Array as _, Int64Array};
    let Some(times) = batch
        .column_by_name("_time")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
    else {
        return Vec::new();
    };
    let Some(watts) = batch.column_by_name(WATTS) else {
        return Vec::new();
    };
    (0..batch.num_rows())
        .filter_map(|row| {
            let value = chronix_query::extract_f64(watts.as_ref(), row)?;
            let at =
                OffsetDateTime::from_unix_timestamp_nanos(i128::from(times.value(row))).ok()?;
            Some(Reading { at, watts: value })
        })
        .collect()
}

/// What one tick of the control loop measured, ready for [`Series::record`].
///
/// Assembled here rather than in the control loop so that what is written and
/// what is read are decided in one place — the alternative is a caller that
/// records `grid` and a reader that asks for `mains`.
#[must_use]
pub fn tick_points(
    grid: Option<Power>,
    netzwirksam: Option<Power>,
    assets: &[(AssetId, Power)],
) -> Vec<(&str, Power)> {
    let mut points: Vec<(&str, Power)> = Vec::with_capacity(assets.len() + 2);
    if let Some(grid) = grid {
        points.push((GRID, grid));
    }
    if let Some(netzwirksam) = netzwirksam {
        points.push((NETZWIRKSAM, netzwirksam));
    }
    for (asset, power) in assets {
        points.push((asset.as_str(), *power));
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 14:32:00 UTC);

    /// A directory of its own, named with the process id: `cargo test` runs test
    /// binaries in parallel and `chronix` takes an exclusive lock on its own
    /// data directory, so a shared path would be one test failing on the other.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(what: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("hems-series-{what}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_tick_goes_in_and_comes_back_out() {
        let dir = Dir::new("roundtrip");
        let series = Series::open(&dir.0, 7).expect("a series");
        series
            .record(
                NOW,
                &[
                    (GRID, Power::from_kw(1.2)),
                    (NETZWIRKSAM, Power::from_kw(0.9)),
                    ("dach", Power::from_kw(-3.4)),
                ],
            )
            .expect("one tick");

        let grid = series
            .between(
                GRID,
                NOW - time::Duration::minutes(1),
                NOW + time::Duration::minutes(1),
            )
            .expect("the grid series");
        assert_eq!(grid.len(), 1, "one reading, not one per point: {grid:?}");
        assert!((grid[0].watts - 1_200.0).abs() < 1e-6, "{:?}", grid[0]);
        assert_eq!(grid[0].at, NOW, "to the instant it was taken");

        // The sign convention survives, which is the half a series that mixes
        // producers and consumers has to get right: a roof feeding back is
        // negative in `hems-core`'s load convention and must not come back
        // positive.
        let pv = series
            .between(
                "dach",
                NOW - time::Duration::minutes(1),
                NOW + time::Duration::minutes(1),
            )
            .expect("the roof");
        assert!((pv[0].watts + 3_400.0).abs() < 1e-6, "{:?}", pv[0]);

        series.close().expect("a clean close");
    }

    #[test]
    fn one_point_of_measurement_does_not_return_anothers() {
        // The tag is the whole of the separation, so this is what says the tag
        // is being applied at all — a query that ignored it would return three
        // readings and every figure drawn from the series would be a mixture.
        let dir = Dir::new("tags");
        let series = Series::open(&dir.0, 7).expect("a series");
        for minute in 0..3 {
            let at = NOW + time::Duration::minutes(minute);
            series
                .record(
                    at,
                    &[(GRID, Power::from_kw(1.0)), ("dach", Power::from_kw(-2.0))],
                )
                .expect("a tick");
        }
        let window = (
            NOW - time::Duration::hours(1),
            NOW + time::Duration::hours(1),
        );
        assert_eq!(series.between(GRID, window.0, window.1).unwrap().len(), 3);
        assert_eq!(series.between("dach", window.0, window.1).unwrap().len(), 3);
        assert!(
            series
                .between("nothing", window.0, window.1)
                .unwrap()
                .is_empty(),
            "a point nobody measured has no readings"
        );
        series.close().expect("a clean close");
    }

    #[test]
    fn a_window_returns_only_what_is_inside_it() {
        let dir = Dir::new("window");
        let series = Series::open(&dir.0, 7).expect("a series");
        for minute in 0..10 {
            series
                .record(
                    NOW + time::Duration::minutes(minute),
                    &[(GRID, Power::from_kw(minute as f64))],
                )
                .expect("a tick");
        }
        let inside = series
            .between(
                GRID,
                NOW + time::Duration::minutes(2),
                NOW + time::Duration::minutes(5),
            )
            .expect("a window");
        let watts: Vec<f64> = inside.iter().map(|r| r.watts).collect();
        assert_eq!(
            watts,
            vec![2_000.0, 3_000.0, 4_000.0],
            "half-open: the start is in and the end is not"
        );
        series.close().expect("a clean close");
    }

    #[test]
    fn the_points_of_one_tick_are_assembled_in_one_place() {
        // `tick_points` exists so that what the control loop writes and what the
        // API reads are decided together. A missing measurement is *absent*
        // rather than zero: a box with no roof has not measured a dark one.
        let asset = AssetId::new("hausspeicher").expect("a literal identifier");
        let assets = vec![(asset.clone(), Power::from_kw(-1.5))];
        let points = tick_points(Some(Power::from_kw(2.0)), None, &assets);
        let names: Vec<&str> = points.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![GRID, "hausspeicher"],
            "no point for a quiet meter"
        );
    }
}
