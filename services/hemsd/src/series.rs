//! The box's own measurement series: days of seconds, years of quarter hours.
//!
//! The guard runs once a control period against what the meters say. `[A1 7.2]`
//! asks the box to keep the minute-resolution trace during a § 14a reduction and
//! nothing more, which leaves the commonest question a household or an installer
//! has — "what was the battery doing at half past two" — unanswerable on the box
//! that watched it. This is where those readings go.
//!
//! # Two tiers, and the reader is told which
//!
//! Seconds are a *diagnostic* window measured in days; a store's worth of them
//! is rolled into quarter-hour means kept for three years, which is what makes
//! "this January against last January" a question the box answers without a
//! fleet behind it. [`Series::window`] picks the tier and **says which one
//! answered**: a year at one second is 31 million readings nobody asked for, and
//! a caller handed quarter-hourly means without being told would label them
//! one-second data.
//!
//! # What goes here, and what must not
//!
//! **The series, never the settlement.** What lives here is measured power — an
//! `f64` before it arrived, and diagnostic rather than statutory. The
//! quarter-hour registers, the § 14a evidence and the box's identity stay in
//! `redb` ([`crate::store`]).
//!
//! The reason is a **transaction** rather than a type. A register and the outbox
//! marker saying the fleet still owes it are written in one `redb` write
//! ([`crate::store::Store::put_quarter_hours`]); two stores have no shared
//! transaction, so splitting them would make a settlement that was stored and
//! never forwarded — or forwarded and never stored — something a power cut can
//! produce (D168).
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

/// The measurement the quarter-hour tier is written to.
const QUARTER_HOURS: &str = "power_15m";

/// The name the rollup is registered under.
const ROLLUP: &str = "quarter-hours";

/// The field a quarter-hour bucket carries.
///
/// `chronix` names a rollup's output `<field>_<aggregation>`, so the mean of
/// `watts` arrives as this. Stated once, because a reader that guessed it would
/// find an empty column rather than an error.
const WATTS_MEAN: &str = "watts_avg";

/// The longest window the raw tier answers.
///
/// A day and an hour: a 25-hour Sunday in October is still one day, and asking
/// for "yesterday" in local time must not silently coarsen on the one day of the
/// year that is longer than the others.
const SECONDS_WINDOW: time::Duration = time::Duration::hours(25);

/// How long the quarter-hour tier is kept.
///
/// Three years. The raw second-by-second tier is a *diagnostic* window measured
/// in days; this is the household's own history, and three years is what makes
/// "this January against last January" a question the box can answer without a
/// fleet. It is deliberately longer than `[A1 7.3]`'s two years for the § 14a
/// record, which lives in the other store and is a different kind of thing.
const QUARTER_HOUR_YEARS: i64 = 3;

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

/// How closely spaced the readings in a window are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Every control tick, as the box measured it.
    Seconds,
    /// The mean over a quarter hour — the grain everything in this workspace
    /// settles on, and the only one that survives past the raw retention.
    QuarterHours,
}

/// A window of history, and the grain it is in.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Window {
    /// How closely spaced the readings are.
    pub resolution: Resolution,
    /// The readings themselves, in order.
    pub readings: Vec<Reading>,
}

/// The box's measurement series.
pub struct Series {
    db: Chronix,
    /// How long the raw tier is kept, so a read knows which tier can answer.
    raw_keeps: time::Duration,
}

impl Series {
    /// Open — or create — the series at `path`, keeping `keep_days` of it.
    ///
    /// [`ChronixConfig::small`] is the gateway profile: a memory budget of about
    /// 48 MB, a single compaction worker, and a WAL sized for flash. Retention
    /// is enforced by the store's own maintenance thread, so there is no sweep
    /// for this daemon to schedule and forget.
    ///
    /// What the preset does **not** bound is a thread pool. Reading a bucket of
    /// four segments or more takes `rayon`'s global pool, sized to the core
    /// count, inside the process that owes the sixty-second § 14a heartbeat —
    /// and it is the store's global rather than this daemon's to set (R33).
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
        let series = Self {
            db,
            raw_keeps: time::Duration::days(i64::from(keep_days.max(1))),
        };
        series.declare_quarter_hours()?;
        Ok(series)
    }

    /// Register the quarter-hour tier, so a household keeps three years of its
    /// own history on a box that keeps days of seconds.
    ///
    /// **Fifteen minutes, read against Europe/Berlin.** Everything this
    /// workspace settles is a `Slot` — a quarter hour of the Berlin day — and a
    /// bucket that did not line up with one would produce a history nobody could
    /// put beside a register. At this width the zone changes nothing, because
    /// Berlin's standard offset is a whole number of hours; it is stated so that
    /// the alignment is a decision rather than a coincidence, and so a coarser
    /// tier declared here later inherits the right calendar.
    ///
    /// The **mean**, because the field is a power: a quarter hour's average watt
    /// is the energy it moved divided by its length, and summing an instantaneous
    /// power over 900 samples would be a number in no unit at all.
    ///
    /// Materialising is the store's own — compaction runs it, and retention will
    /// not drop raw data a tier has not yet aggregated — so there is no sweep for
    /// this daemon to schedule and forget.
    fn declare_quarter_hours(&self) -> Result<(), SeriesError> {
        let wanted = chronix::RollupBuilder::new()
            .name(ROLLUP)
            .source(MEASUREMENT)
            .target(QUARTER_HOURS)
            .every("15m")
            .timezone("Europe/Berlin")
            .aggregation(chronix::RollupAggFn::Avg)
            .group_by(POINT)
            .retention_ns(QUARTER_HOUR_YEARS * 365 * 24 * 3_600 * 1_000_000_000)
            .build()
            .map_err(|e| SeriesError::Store(e.to_string()))?;

        // The declaration is persisted in the store, so every run after the
        // first finds its own. Re-declaring is refused, which makes this a read
        // rather than an error to swallow — and the comparison is what lets a
        // *changed* tier take effect: a box upgraded to a different bucket or a
        // longer retention would otherwise keep aggregating to the old one for
        // three years with nothing to say so.
        let existing = self
            .db
            .list_rollups()
            .map_err(|e| SeriesError::Store(e.to_string()))?
            .into_iter()
            .find(|declared| declared.name == ROLLUP);
        if let Some(declared) = existing {
            if same(&declared, &wanted) {
                return Ok(());
            }
            tracing::info!(
                rollup = ROLLUP,
                "the quarter-hour tier was declared differently by an earlier version; replacing it"
            );
            self.db
                .delete_rollup(ROLLUP)
                .map_err(|e| SeriesError::Store(e.to_string()))?;
        }
        self.db
            .create_rollup(wanted)
            .map_err(|e| SeriesError::Store(e.to_string()))
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

    /// Every reading for one point of measurement in `[from, to)`, in order,
    /// and **which tier answered**.
    ///
    /// **Half-open**, like every other window in this workspace — a `Slot`,
    /// `histd`'s register window, a Nachweis. `chronix`'s own `TimeRange` is
    /// inclusive at *both* ends, so the conversion happens here, once, rather
    /// than at each caller: two windows laid end to end must not both contain
    /// the instant they meet at, and a reader who used this beside
    /// `Store::quarter_hours_between` would otherwise get a different rule from
    /// each.
    ///
    /// Returning the resolution rather than silently changing it is the point. A
    /// caller that asked for a year and got 35 040 readings has not been given
    /// second-by-second data, and a reader who was not told would draw a chart
    /// claiming it was.
    ///
    /// # Errors
    /// [`SeriesError::Store`] where the read fails.
    pub fn window(
        &self,
        point: &str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Window, SeriesError> {
        // Once: the rule reads the clock, so asking twice could label a window
        // with a tier that did not answer it.
        let resolution = self.resolution_for(from, to);
        let readings = match resolution {
            Resolution::Seconds => self.raw(point, from, to)?,
            Resolution::QuarterHours => self.quarter_hours(point, from, to)?,
        };
        Ok(Window {
            resolution,
            readings,
        })
    }

    /// Which tier answers a window.
    ///
    /// Two reasons to coarsen, and either is enough.
    ///
    /// **The data is gone.** Past the raw horizon the quarter-hour tier is the
    /// only place the household's history still exists. The horizon is
    /// approached rather than exact — the store sweeps on its own schedule, so a
    /// read landing on it may find the last hours already dropped, and answering
    /// from a tier being swept underneath the reader is how a chart acquires a
    /// hole.
    ///
    /// **The answer would be unusable.** A year at one second is 31 million
    /// readings: a box with a 48 MB budget cannot assemble it, and nothing that
    /// asked for a year wanted it. [`SECONDS_WINDOW`] is where a question stops
    /// being "what was the battery doing at half past two" and starts being a
    /// history — a day, with enough slack for a 25-hour one.
    fn resolution_for(&self, from: OffsetDateTime, to: OffsetDateTime) -> Resolution {
        let age = OffsetDateTime::now_utc() - from;
        if to - from <= SECONDS_WINDOW && age < self.raw_keeps - time::Duration::hours(1) {
            Resolution::Seconds
        } else {
            Resolution::QuarterHours
        }
    }

    /// The raw tier, as it was measured.
    fn raw(
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
        Ok(readings(&batch, WATTS))
    }

    /// The quarter-hour tier, through the store's **rollup view** rather than a
    /// plain read of the target measurement.
    ///
    /// The materialised tier lags the newest write by the out-of-order window —
    /// a couple of hours on this profile — because a bucket is aggregated only
    /// once nothing can still land in it. Reading the target measurement
    /// directly would therefore end a thirty-day chart two hours early, which a
    /// household reads as an outage. The view splices the buckets above the
    /// watermark on, computed from the raw data in the same pass with the same
    /// accumulator, so the recent half and the stored half cannot disagree.
    ///
    /// The filter goes to the **store**, not to this loop.
    ///
    /// It used to answer for every point at once and the tag was matched here,
    /// which was bounded — a house has a handful of points of measurement — and
    /// was still the whole tag-group set materialised to read one series. 0.6's
    /// `rollup_where` takes the filter, and it is worth taking for a reason
    /// beyond the copying: it is refused by name if the key is not one of the
    /// rollup's `group_by_tags`. A filter on any other key would narrow the live
    /// half — read from the source, which still carries every tag — and match
    /// nothing in the materialised half, so the answer would be short on one
    /// side of the watermark and whole on the other. That is a defect this
    /// daemon could not have detected in its own filtering, because both halves
    /// arrive as one batch by the time it sees them. D198.
    fn quarter_hours(
        &self,
        point: &str,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<Reading>, SeriesError> {
        let batch = self
            .db
            .rollup_where(
                ROLLUP,
                nanos(from),
                nanos(to).saturating_sub(1),
                &[(POINT, point)],
            )
            .map_err(|e| SeriesError::Store(e.to_string()))?;
        Ok(readings(&batch, WATTS_MEAN))
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

/// Whether a rollup already declared is the one this version wants.
///
/// Field by field rather than a derived comparison, because `RollupConfig` has
/// none — and the name is deliberately not compared, since it is what identified
/// the two as the same tier.
fn same(declared: &chronix::RollupConfig, wanted: &chronix::RollupConfig) -> bool {
    declared.source_measurement == wanted.source_measurement
        && declared.target_measurement == wanted.target_measurement
        && declared.bucket == wanted.bucket
        && declared.aggregations == wanted.aggregations
        && declared.group_by_tags == wanted.group_by_tags
        && declared.retention_ns == wanted.retention_ns
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
fn readings(batch: &RecordBatch, field: &str) -> Vec<Reading> {
    use arrow::array::{Array as _, Int64Array};
    let Some(times) = batch
        .column_by_name("_time")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
    else {
        return Vec::new();
    };
    let Some(watts) = batch.column_by_name(field) else {
        return Vec::new();
    };
    // Everything in the batch belongs to the caller. **Both** paths now filter
    // in the store — the seconds tier through the query's own `.tag(…)` and the
    // quarter-hour tier through `rollup_where` (D198) — so this took an
    // `Option<&str>` that every call site passed `None`. A parameter nothing
    // supplies is a filter nothing applies, and leaving it in would have left a
    // reader believing the tag was still being checked here.
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

    /// The start of a quarter hour a few minutes ago.
    ///
    /// Relative to the clock rather than a date somebody typed: which tier
    /// answers a window depends on how old it is, so a fixed instant would have
    /// meant one thing the week it was written and another every week after.
    ///
    /// On a quarter, because a window that begins mid-bucket loses that bucket —
    /// the quarter hour it opens in started before it did, and is not the
    /// window's to report. That is right, and it is not what these tests are
    /// about.
    fn recently() -> OffsetDateTime {
        let now = OffsetDateTime::now_utc() - time::Duration::minutes(5);
        let quarter = i64::from(now.minute() % 15) * 60 + i64::from(now.second());
        (now - time::Duration::seconds(quarter))
            .replace_nanosecond(0)
            .expect("zero is a nanosecond")
    }

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
        let now = recently();
        let dir = Dir::new("roundtrip");
        let series = Series::open(&dir.0, 7).expect("a series");
        series
            .record(
                now,
                &[
                    (GRID, Power::from_kw(1.2)),
                    (NETZWIRKSAM, Power::from_kw(0.9)),
                    ("dach", Power::from_kw(-3.4)),
                ],
            )
            .expect("one tick");

        let grid = series
            .window(
                GRID,
                now - time::Duration::minutes(1),
                now + time::Duration::minutes(1),
            )
            .expect("the grid series")
            .readings;
        assert_eq!(grid.len(), 1, "one reading, not one per point: {grid:?}");
        assert!((grid[0].watts - 1_200.0).abs() < 1e-6, "{:?}", grid[0]);
        assert_eq!(grid[0].at, now, "to the instant it was taken");

        // The sign convention survives, which is the half a series that mixes
        // producers and consumers has to get right: a roof feeding back is
        // negative in `hems-core`'s load convention and must not come back
        // positive.
        let pv = series
            .window(
                "dach",
                now - time::Duration::minutes(1),
                now + time::Duration::minutes(1),
            )
            .expect("the roof")
            .readings;
        assert!((pv[0].watts + 3_400.0).abs() < 1e-6, "{:?}", pv[0]);

        series.close().expect("a clean close");
    }

    #[test]
    fn one_point_of_measurement_does_not_return_anothers() {
        let now = recently();
        // The tag is the whole of the separation, so this is what says the tag
        // is being applied at all — a query that ignored it would return three
        // readings and every figure drawn from the series would be a mixture.
        let dir = Dir::new("tags");
        let series = Series::open(&dir.0, 7).expect("a series");
        for minute in 0..3 {
            let at = now + time::Duration::minutes(minute);
            series
                .record(
                    at,
                    &[(GRID, Power::from_kw(1.0)), ("dach", Power::from_kw(-2.0))],
                )
                .expect("a tick");
        }
        let window = (
            now - time::Duration::hours(1),
            now + time::Duration::hours(1),
        );
        let count = |point: &str| {
            series
                .window(point, window.0, window.1)
                .expect("a window")
                .readings
                .len()
        };
        assert_eq!(count(GRID), 3);
        assert_eq!(count("dach"), 3);
        assert_eq!(
            count("nothing"),
            0,
            "a point nobody measured has no readings"
        );
        series.close().expect("a clean close");
    }

    #[test]
    fn a_window_returns_only_what_is_inside_it() {
        let now = recently();
        let dir = Dir::new("window");
        let series = Series::open(&dir.0, 7).expect("a series");
        for minute in 0..10 {
            series
                .record(
                    now + time::Duration::minutes(minute),
                    &[(GRID, Power::from_kw(minute as f64))],
                )
                .expect("a tick");
        }
        let inside = series
            .window(
                GRID,
                now + time::Duration::minutes(2),
                now + time::Duration::minutes(5),
            )
            .expect("a window")
            .readings;
        let watts: Vec<f64> = inside.iter().map(|r| r.watts).collect();
        assert_eq!(
            watts,
            vec![2_000.0, 3_000.0, 4_000.0],
            "half-open: the start is in and the end is not"
        );
        series.close().expect("a clean close");
    }

    #[test]
    fn a_short_recent_window_comes_back_as_it_was_measured() {
        let now = recently();
        let dir = Dir::new("tier-seconds");
        let series = Series::open(&dir.0, 7).expect("a series");
        series
            .record(now, &[(GRID, Power::from_kw(1.0))])
            .expect("a tick");
        let window = series
            .window(
                GRID,
                now - time::Duration::hours(1),
                now + time::Duration::minutes(1),
            )
            .expect("a window");
        assert_eq!(window.resolution, Resolution::Seconds);
        assert_eq!(window.readings.len(), 1);
        series.close().expect("a clean close");
    }

    #[test]
    fn a_long_window_is_answered_at_quarter_hours_and_says_so() {
        // Thirty hours of a household, written forwards — the store's live path
        // accepts a write only within a couple of shards of the newest one, so a
        // test that started at the end and worked backwards would be writing
        // into a window that had closed.
        //
        // What this pins is the whole tier: that the rollup was declared, that
        // the store aggregates it, that the view answers above the watermark as
        // well as below it, and that a quarter hour comes back as the *mean* of
        // what was in it rather than a sum or a sample.
        let now = recently();
        let dir = Dir::new("tier-quarters");
        let series = Series::open(&dir.0, 7).expect("a series");

        let start = now - time::Duration::hours(30);
        // Two readings in each quarter hour, 1 kW apart, so the mean is a figure
        // neither of them is — and the roof carries the **negative** of the
        // connection point's, so the two series are told apart by value and not
        // only by how many came back. Writing both the same made a filter that
        // returned the wrong point indistinguishable from one that worked.
        for quarter in 0..120_i64 {
            let at = start + time::Duration::minutes(quarter * 15);
            let low = Power::from_kw(quarter as f64);
            let high = Power::from_kw(quarter as f64 + 1.0);
            series
                .record(at, &[(GRID, low), ("dach", -low)])
                .expect("a tick");
            series
                .record(
                    at + time::Duration::minutes(7),
                    &[(GRID, high), ("dach", -high)],
                )
                .expect("a tick");
        }

        let window = series
            .window(GRID, start, now + time::Duration::minutes(1))
            .expect("a window");
        assert_eq!(
            window.resolution,
            Resolution::QuarterHours,
            "thirty hours is not a question anybody wants 108 000 readings to"
        );
        assert_eq!(
            window.readings.len(),
            120,
            "one reading per quarter hour of the window: {:?}",
            window.readings.first().zip(window.readings.last())
        );
        let first = window.readings[0];
        assert!(
            (first.watts - 500.0).abs() < 1e-6,
            "the mean of 0 kW and 1 kW, not either of them: {first:?}"
        );
        assert_eq!(first.at, start, "the bucket the readings fell in");
        // The last quarter hour is above the materialisation watermark — the
        // store aggregates only what can no longer change — so its presence is
        // what says the live half of the view is spliced on.
        let last = window.readings[119];
        assert!(
            (last.watts - 119_500.0).abs() < 1e-6,
            "the most recent bucket, still being written: {last:?}"
        );

        // And the tag survives the coarsening. The filter is the **store's**
        // since 0.6 (`rollup_where`), which is the reason to check it by value
        // here rather than trust it: a filter that did nothing would return both
        // points and double the count, and one that returned the wrong point
        // would keep the count and flip the sign.
        let roof = series
            .window("dach", start, now + time::Duration::minutes(1))
            .expect("a window");
        assert_eq!(roof.readings.len(), 120);
        assert!(
            (roof.readings[0].watts + 500.0).abs() < 1e-6,
            "the roof's own mean, not the connection point's: {:?}",
            roof.readings[0]
        );
        assert!(
            (roof.readings[119].watts + 119_500.0).abs() < 1e-6,
            "…above the watermark too: {:?}",
            roof.readings[119]
        );
        assert!(
            series
                .window("nothing", start, now + time::Duration::minutes(1))
                .expect("a window")
                .readings
                .is_empty(),
            "a point nobody measured has no quarter hours either"
        );

        series.close().expect("a clean close");
    }

    #[test]
    fn the_stored_half_of_the_tier_says_what_the_live_half_did() {
        // The two halves of the view answer from different places — the target
        // measurement below the materialisation watermark, the raw data above it
        // — and a household reading its own January is reading the first. In a
        // test nothing has been materialised yet, so every other case here is
        // exercising the live half only; this one runs the store's own
        // materialisation and asks the same question again.
        let now = recently();
        let dir = Dir::new("tier-materialised");
        let series = Series::open(&dir.0, 7).expect("a series");
        let start = now - time::Duration::hours(30);
        for quarter in 0..120_i64 {
            let at = start + time::Duration::minutes(quarter * 15);
            series
                .record(at, &[(GRID, Power::from_kw(quarter as f64))])
                .expect("a tick");
            series
                .record(
                    at + time::Duration::minutes(7),
                    &[(GRID, Power::from_kw(quarter as f64 + 1.0))],
                )
                .expect("a tick");
        }
        let before = series.window(GRID, start, now).expect("a window");

        let written = series
            .db
            .materialise_rollups()
            .expect("the store aggregates what can no longer change");
        assert!(
            written > 0,
            "nothing was materialised, so this proves nothing about the stored half"
        );

        let after = series.window(GRID, start, now).expect("a window");
        assert_eq!(
            after.readings, before.readings,
            "the stored half and the live half disagree about the same quarter hours"
        );
        series.close().expect("a clean close");
    }

    #[test]
    fn a_quarter_hour_bucket_starts_where_a_settlement_slot_does() {
        // Every quarter hour this workspace settles is a `Slot` of the
        // Europe/Berlin day. A tier whose buckets were offset from those would
        // produce a history nobody could put beside a register — and the two
        // agree only because Berlin's offset from UTC is a whole number of
        // hours, which is a fact about the zone rather than about the store.
        let now = recently();
        let dir = Dir::new("tier-alignment");
        let series = Series::open(&dir.0, 7).expect("a series");
        let start = now - time::Duration::hours(30);
        for minute in 0..(31 * 60_i64) {
            series
                .record(
                    start + time::Duration::minutes(minute),
                    &[(GRID, Power::from_kw(1.0))],
                )
                .expect("a tick");
        }
        let window = series.window(GRID, start, now).expect("a window");
        for reading in &window.readings {
            let berlin = reading
                .at
                .to_offset(time::UtcOffset::from_hms(1, 0, 0).expect("a whole hour"));
            assert_eq!(
                (berlin.minute() % 15, berlin.second()),
                (0, 0),
                "a bucket that is not on a quarter of the Berlin hour: {reading:?}"
            );
        }
        series.close().expect("a clean close");
    }

    #[test]
    fn the_second_run_of_the_box_finds_its_own_tier() {
        // The declaration is persisted, so every run after the first meets a
        // rollup that already exists. A box that treated that as an error would
        // fail to start on its second boot — and one that declared a *second*
        // tier would aggregate everything twice.
        let dir = Dir::new("tier-reopen");
        let series = Series::open(&dir.0, 7).expect("a first run");
        series.close().expect("a clean close");

        let again = Series::open(&dir.0, 7).expect("a second run");
        let declared = again.db.list_rollups().expect("the registry");
        assert_eq!(declared.len(), 1, "{declared:?}");
        assert_eq!(declared[0].target_measurement, QUARTER_HOURS);
        again.close().expect("a clean close");
    }

    #[test]
    fn a_tier_declared_differently_is_replaced() {
        // What an upgrade looks like: a box in the field carries the tier an
        // older version asked for, and the registry outlives the binary. Without
        // the comparison it would keep aggregating to the old bucket for three
        // years and nothing would say so.
        let dir = Dir::new("tier-upgrade");
        {
            let series = Series::open(&dir.0, 7).expect("a series");
            series.db.delete_rollup(ROLLUP).expect("the tier");
            let stale = chronix::RollupBuilder::new()
                .name(ROLLUP)
                .source(MEASUREMENT)
                .target(QUARTER_HOURS)
                .every("1h")
                .aggregation(chronix::RollupAggFn::Avg)
                .group_by(POINT)
                .build()
                .expect("a rollup an older version might have wanted");
            series.db.create_rollup(stale).expect("declared");
            series.close().expect("a clean close");
        }

        let series = Series::open(&dir.0, 7).expect("the new version starts");
        let declared = series.db.list_rollups().expect("the registry");
        assert_eq!(declared.len(), 1, "replaced, not added to: {declared:?}");
        assert_eq!(
            declared[0].bucket,
            chronix::prelude::TimeBucket::parse("15m", Some("Europe/Berlin"))
                .expect("a quarter hour"),
            "the tier this version asks for"
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
