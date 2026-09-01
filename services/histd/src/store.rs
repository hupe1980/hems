//! The database, and the only place SQL is written.

use std::path::Path;

use hems_core::prelude::{GuardRule, Power, Slot};
use hems_grid::evidence::{ComplianceSample, ControlEvent};
use hems_grid::mispel::QuarterHour;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use time::OffsetDateTime;

/// One event as it is held: the event itself, plus what the store knows about it
/// that the event does not.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    /// The row identifier, which is what an acknowledgement names.
    pub id: i64,
    /// The event, with its compliance trace re-attached.
    pub event: ControlEvent,
    /// When its two years are up, `[A1 7.3]`.
    pub expires_at: OffsetDateTime,
}

/// How long a § 14a control event is kept, `[A1 7.3]`.
pub const EVIDENCE_RETENTION: time::Duration = time::Duration::days(2 * 365);

/// Why the store could not answer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database itself.
    #[error("the history store failed: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A stored quantity is not the exact decimal it was written as.
    ///
    /// It should be impossible — nothing else writes these columns — and it is
    /// an error rather than a `0` because a settlement quantity that silently
    /// became zero is worse than one that could not be read.
    #[error("the stored quantity {value:?} in {column} is not a decimal")]
    NotADecimal {
        /// Which column.
        column: &'static str,
        /// What was in it.
        value: String,
    },
    /// An event could not be turned into a document to store.
    ///
    /// Impossible for the types in this workspace, and an error rather than a
    /// panic because the alternative to storing evidence is never a crash.
    #[error("the event could not be serialised: {detail}")]
    NotSerialisable {
        /// What `serde` said.
        detail: String,
    },
    /// A stored document is not one this build can read.
    ///
    /// A downgraded box, or a file another program has written to. Named with
    /// the row rather than swallowed, because one unreadable event in two years
    /// of them is a fact an operator has to be told rather than a gap in a
    /// Nachweis nobody can account for.
    #[error("the stored event {id} cannot be read by this build: {detail}")]
    NotReadable {
        /// Which row.
        id: i64,
        /// What `serde` said.
        detail: String,
    },
    /// The database is at a revision this build does not know.
    ///
    /// A downgraded box. Opening it read-write anyway would write rows shaped
    /// for the older schema into a newer one, and § 14a evidence is the last
    /// record in this workspace that should be repaired by guesswork.
    #[error("the store is at schema revision {found}, and this build understands {understood}")]
    FromTheFuture {
        /// What the file says.
        found: i32,
        /// The newest revision this build carries.
        understood: i32,
    },
}

/// The schema, one numbered file per revision, applied in order.
///
/// The layout is `mako`'s — `services/<daemon>/migrations/NNNN_*.sql`, a new
/// file per change and never an edit to one already applied. What differs is
/// the runner:
/// `mako` is PostgreSQL and calls `sqlx::migrate!`, and this is SQLite, so the
/// files are compiled in and the applied revision is SQLite's own
/// `user_version` — a 32-bit integer in the database header that exists for
/// exactly this and costs neither a table nor a query.
///
/// A migration is applied inside a transaction with `user_version` set in the
/// same one, so a process killed halfway leaves a database that is either at the
/// old revision or at the new one.
const MIGRATIONS: &[(i32, &str)] = &[(1, include_str!("../migrations/0001_schema.sql"))];

/// A handle onto the database file, from which connections are opened.
///
/// # Why reads get their own connection
///
/// One connection behind one mutex serialises every query, and the queries here
/// are not the same size: a household's Data Act export is the two years of
/// `[A1 7.3]` and takes about 370 ms, while a box posting the evidence of a
/// reduction that is happening **now** takes a fraction of a millisecond.
/// Behind one lock the second waits for the first — measured at 2,7 s with eight
/// exports in flight — and `[A1 7.2]` is a record of something with a clock on
/// it.
///
/// SQLite in WAL mode allows **many readers and one writer**, which is exactly
/// the shape of this workload, so a read opens its own connection and a write
/// goes through the single one that owns the write lock. Opening is tens of
/// microseconds against a query of tens of milliseconds, so a pool would be
/// bookkeeping for a cost that is already in the noise.
#[derive(Debug, Clone)]
pub struct Db {
    path: std::path::PathBuf,
}

impl Db {
    /// The database at `path`.
    ///
    /// A **file**, deliberately: `:memory:` cannot be shared between
    /// connections, so a handle onto one would hand out a fresh empty database
    /// each time. Tests that want concurrency use a temporary file, which is
    /// what a deployment has anyway.
    #[must_use]
    pub fn at(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Open one connection.
    ///
    /// # Errors
    /// As [`Store::open`].
    pub fn connect(&self) -> Result<Store, StoreError> {
        Store::open(&self.path)
    }
}

/// The upsert both write paths use, so a single row and a batch cannot come to
/// mean different things.
///
/// **Upsert with the transaction time refreshed.** A register is restated — a
/// substitute value replaced by a real one, a correction from the metering point
/// operator — and the settlement wants the current answer while still being able
/// to say when it learned it.
const QUARTER_HOUR_UPSERT: &str = "INSERT INTO quarter_hour (
     site_id, slot_start, grid_draw_kwh, grid_feed_in_kwh,
     device_consumption_kwh, device_generation_kwh,
     anzulegender_wert_ct, spot_price_ct, recorded_at
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
 ON CONFLICT(site_id, slot_start) DO UPDATE SET
     grid_draw_kwh          = excluded.grid_draw_kwh,
     grid_feed_in_kwh       = excluded.grid_feed_in_kwh,
     device_consumption_kwh = excluded.device_consumption_kwh,
     device_generation_kwh  = excluded.device_generation_kwh,
     anzulegender_wert_ct   = excluded.anzulegender_wert_ct,
     spot_price_ct          = excluded.spot_price_ct,
     recorded_at            = excluded.recorded_at";

/// Its parameters, in the order the statement names them.
///
/// Owned values rather than a `params!` slice, because that borrows temporaries
/// and cannot leave the function that built it.
fn quarter_hour_params(
    site: &str,
    q: &QuarterHour,
    recorded_at: OffsetDateTime,
) -> [rusqlite::types::Value; 9] {
    use rusqlite::types::Value;
    [
        Value::Text(site.to_owned()),
        Value::Integer(q.slot.start().unix_timestamp()),
        Value::Text(q.grid_draw.to_string()),
        Value::Text(q.grid_feed_in.to_string()),
        Value::Text(q.device_consumption.to_string()),
        Value::Text(q.device_generation.to_string()),
        Value::Text(q.anzulegender_wert.to_string()),
        Value::Text(q.spot_price.to_string()),
        Value::Integer(recorded_at.unix_timestamp()),
    ]
}

/// The fleet's record, on disk.
pub struct Store {
    connection: Connection,
}

impl Store {
    /// Open — or create — the database at `path`, and bring the schema up.
    ///
    /// # Errors
    /// [`StoreError::Sql`] when the file cannot be opened or the schema applied.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = if path.as_os_str() == ":memory:" {
            Connection::open_in_memory()?
        } else {
            Connection::open(path)?
        };
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    /// Bring the database up to the newest revision in [`MIGRATIONS`].
    ///
    /// # Errors
    /// [`StoreError::Sql`] if a revision cannot be applied, or
    /// [`StoreError::FromTheFuture`] if the file was written by a newer build.
    fn migrate(&self) -> Result<(), StoreError> {
        // Every `PRAGMA` this store needs, on every open, and none of them in a
        // migration file. `journal_mode` is persistent and cannot be set inside
        // a transaction, which a migration is; `foreign_keys` and `busy_timeout`
        // are per *connection*, so a migration would set them for the one that
        // created the schema and for nothing afterwards.
        //
        // WAL lets a reader and a writer run at once and does nothing about two
        // *writers* — the retention sweep and an evidence write are two — so the
        // busy timeout is what turns `SQLITE_BUSY` into a wait of a few
        // milliseconds instead of a failed write.
        self.connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;

        let at: i32 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let newest = MIGRATIONS.last().map_or(0, |(v, _)| *v);
        if at > newest {
            // A box that has been downgraded. Refusing is the only safe answer:
            // the columns a newer revision added are still there, and this build
            // would write rows the newer one cannot read back.
            return Err(StoreError::FromTheFuture {
                found: at,
                understood: newest,
            });
        }
        for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > at) {
            self.connection.execute_batch(&format!(
                "BEGIN; {sql}\nPRAGMA user_version = {version}; COMMIT;"
            ))?;
        }
        Ok(())
    }

    /// A store in memory, for a test.
    ///
    /// # Errors
    /// As [`Store::open`].
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::open(Path::new(":memory:"))
    }

    /// Write one quarter hour's registers.
    ///
    /// **Upsert, with the transaction time refreshed.** A register is restated —
    /// a substitute value replaced by a real one, a correction from the metering
    /// point operator — and the settlement wants the current answer while still
    /// being able to say when it learned it. That is `metering`'s valid time and
    /// `meterstore`'s `recorded_at`, and a local store needs both for the same
    /// reason a fleet one does.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn put_quarter_hour(
        &self,
        site: &str,
        quarter: &QuarterHour,
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            QUARTER_HOUR_UPSERT,
            rusqlite::params_from_iter(quarter_hour_params(site, quarter, recorded_at)),
        )?;
        Ok(())
    }

    /// Write many quarter hours in **one** transaction.
    ///
    /// A box posts ninety-six of them at the end of a day. One statement per row
    /// is one implicit transaction per row, and in WAL mode that is one commit —
    /// and one `fsync` — each: measured at tens of seconds for a couple of
    /// years' worth, against under a second batched. The registers of one day
    /// are also one *fact*, and a settlement that can observe half of them is a
    /// settlement that can be run on half a day.
    ///
    /// # Errors
    /// [`StoreError::Sql`]. Nothing is written if any row fails.
    pub fn put_quarter_hours(
        &mut self,
        site: &str,
        quarters: &[QuarterHour],
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        {
            let mut statement = transaction.prepare(QUARTER_HOUR_UPSERT)?;
            for quarter in quarters {
                statement.execute(rusqlite::params_from_iter(quarter_hour_params(
                    site,
                    quarter,
                    recorded_at,
                )))?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every quarter hour of `site` in `[from, to)`, in order.
    ///
    /// # Errors
    /// [`StoreError::Sql`], or [`StoreError::NotADecimal`] for a column that has
    /// been written by something other than this store.
    /// `None` on either bound means "no bound", which is what a Data Act export
    /// asks for. Taking an instant instead and expecting a caller to invent a
    /// far-future one is how an export comes back empty on a leap of arithmetic
    /// nobody notices.
    pub fn quarter_hours(
        &self,
        site: &str,
        from: Option<OffsetDateTime>,
        to: Option<OffsetDateTime>,
    ) -> Result<Vec<QuarterHour>, StoreError> {
        self.read_quarter_hours(
            "SELECT slot_start, grid_draw_kwh, grid_feed_in_kwh,
                    device_consumption_kwh, device_generation_kwh,
                    anzulegender_wert_ct, spot_price_ct
             FROM quarter_hour
             WHERE site_id = ?1 AND slot_start >= ?2 AND slot_start < ?3
             ORDER BY slot_start",
            params![
                site,
                from.map_or(i64::MIN, OffsetDateTime::unix_timestamp),
                to.map_or(i64::MAX, OffsetDateTime::unix_timestamp)
            ],
        )
    }

    /// Run a query whose columns are the seven a [`QuarterHour`] is built from.
    ///
    /// One place the register columns are turned back into exact decimals, so a
    /// window query and an outbox query cannot come to read the same row
    /// differently.
    fn read_quarter_hours(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<QuarterHour>, StoreError> {
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map(args, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (unix, draw, feed_in, consumption, generation, aw, spot) = row?;
            let quarter =
                Slot::containing(OffsetDateTime::from_unix_timestamp(unix).map_err(|_| {
                    StoreError::NotADecimal {
                        column: "slot_start",
                        value: unix.to_string(),
                    }
                })?);
            out.push(QuarterHour {
                grid_draw: decimal("grid_draw_kwh", &draw)?,
                grid_feed_in: decimal("grid_feed_in_kwh", &feed_in)?,
                device_consumption: decimal("device_consumption_kwh", &consumption)?,
                device_generation: decimal("device_generation_kwh", &generation)?,
                anzulegender_wert: decimal("anzulegender_wert_ct", &aw)?,
                spot_price: decimal("spot_price_ct", &spot)?,
                ..QuarterHour::empty(quarter)
            });
        }
        Ok(out)
    }

    /// Write a closed control event and its compliance trace.
    ///
    /// Returns the row identifier, so a caller can attach more to it.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn put_control_event(
        &mut self,
        site: &str,
        event: &ControlEvent,
    ) -> Result<i64, StoreError> {
        let transaction = self.connection.transaction()?;
        // Two years from the day it *closed*, not from the day it arrived: an
        // event that ran for a week is documented for two years after it ended,
        // which is the reading that never keeps less than `[A1 7.3]` asks for.
        let expires_at = event.released_at.unwrap_or(event.received_at) + EVIDENCE_RETENTION;
        // The document carries everything except the trace, which is the table
        // below. Written from the same value as the projections beside it, in
        // one statement, so the two cannot disagree about one event.
        let mut document = event.clone();
        document.samples.clear();
        let document =
            serde_json::to_string(&document).map_err(|e| StoreError::NotSerialisable {
                detail: e.to_string(),
            })?;
        transaction.execute(
            "INSERT INTO control_event (
                 site_id, document, rule, received_at, released_at, first_ceiling_w,
                 strictest_ceiling_w, minimum_power_w, below_minimum, expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                site,
                document,
                rule_name(event.rule),
                event.received_at.unix_timestamp(),
                event.released_at.map(OffsetDateTime::unix_timestamp),
                event.first_ceiling().get(),
                event.strictest_ceiling().get(),
                event
                    .ceilings
                    .first()
                    .map_or(0.0, |c| c.minimum_power.get()),
                i32::from(event.below_minimum()),
                expires_at.unix_timestamp(),
            ],
        )?;
        let id = transaction.last_insert_rowid();
        {
            let mut sample = transaction.prepare(
                "INSERT OR REPLACE INTO compliance_sample (event_id, at, netzwirksam_w, ceiling_w)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for s in &event.samples {
                sample.execute(params![
                    id,
                    s.at.unix_timestamp(),
                    s.netzwirksam.get(),
                    s.ceiling.get(),
                ])?;
            }
        }
        transaction.commit()?;
        Ok(id)
    }

    /// Every control event of `site` in `[from, to)` by the instant it was
    /// received, oldest first, with its compliance trace re-attached.
    ///
    /// This is what a Nachweis `[A1 7.2]` and a Data Act export are built from,
    /// and it is the only way an event leaves the store — so the record a
    /// network operator is shown is the record that was written, reconstructed
    /// through `serde` rather than reassembled field by field.
    ///
    /// # Errors
    /// [`StoreError::Sql`], or [`StoreError::NotReadable`] for a document this
    /// build cannot parse.
    pub fn control_events(
        &self,
        site: &str,
        from: Option<OffsetDateTime>,
        to: Option<OffsetDateTime>,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let lower = from.map_or(i64::MIN, OffsetDateTime::unix_timestamp);
        let upper = to.map_or(i64::MAX, OffsetDateTime::unix_timestamp);
        let mut statement = self.connection.prepare(
            "SELECT id, document, expires_at FROM control_event
             WHERE site_id = ?1 AND received_at >= ?2 AND received_at < ?3
             ORDER BY received_at, id",
        )?;
        let rows: Vec<(i64, String, i64)> = statement
            .query_map(params![site, lower, upper], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, document, expires_at) in rows {
            let mut event: ControlEvent =
                serde_json::from_str(&document).map_err(|e| StoreError::NotReadable {
                    id,
                    detail: e.to_string(),
                })?;
            event.samples = self.samples_of(id)?;
            out.push(StoredEvent {
                id,
                event,
                expires_at: OffsetDateTime::from_unix_timestamp(expires_at)
                    .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            });
        }
        Ok(out)
    }

    /// How many control events `site` has on record.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn control_event_count(&self, site: &str) -> Result<usize, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM control_event WHERE site_id = ?1",
            params![site],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// How many compliance samples one event carries.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn sample_count(&self, event_id: i64) -> Result<usize, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM compliance_sample WHERE event_id = ?1",
            params![event_id],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// The earliest event still on record for `site`, if any.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn earliest_event(&self, site: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        let unix: Option<i64> = self
            .connection
            .query_row(
                "SELECT MIN(received_at) FROM control_event WHERE site_id = ?1",
                params![site],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(unix.and_then(|u| OffsetDateTime::from_unix_timestamp(u).ok()))
    }

    /// Delete every event whose two years are up, and the registers older than
    /// the same window.
    ///
    /// Returns how many events went. Their traces go with them by
    /// `ON DELETE CASCADE`: a trace whose event has been deleted is a set of
    /// numbers nobody can interpret.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn prune(&self, now: OffsetDateTime) -> Result<usize, StoreError> {
        let events = self.connection.execute(
            "DELETE FROM control_event WHERE expires_at <= ?1",
            params![now.unix_timestamp()],
        )?;
        self.connection.execute(
            "DELETE FROM quarter_hour WHERE slot_start <= ?1",
            params![(now - EVIDENCE_RETENTION).unix_timestamp()],
        )?;
        Ok(events)
    }

    /// One event's compliance trace, oldest first.
    fn samples_of(&self, event_id: i64) -> Result<Vec<ComplianceSample>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT at, netzwirksam_w, ceiling_w FROM compliance_sample
             WHERE event_id = ?1 ORDER BY at",
        )?;
        let samples = statement
            .query_map(params![event_id], |row| {
                Ok(ComplianceSample {
                    at: OffsetDateTime::from_unix_timestamp(row.get(0)?)
                        .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                    netzwirksam: Power::new(row.get(1)?),
                    ceiling: Power::new(row.get(2)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(samples)
    }
}

/// The name a `GuardRule` is stored under.
///
/// Written out rather than taken from `Debug`, because a stored value is a wire
/// format and `Debug` is not one: nothing promises it round trips, and a rename
/// of a variant would silently change what two years of evidence say. Only the
/// *projection* uses this; the event itself is reconstructed from its `serde`
/// document.
fn rule_name(rule: GuardRule) -> &'static str {
    match rule {
        GuardRule::Lpc => "lpc",
        GuardRule::Lpp => "lpp",
        GuardRule::Para9Cap => "para9_cap",
        GuardRule::Failsafe => "failsafe",
        GuardRule::CircuitLimit => "circuit_limit",
        GuardRule::ContractLimit => "contract_limit",
        GuardRule::Unbalance => "unbalance",
        GuardRule::DeviceLimit => "device_limit",
        GuardRule::BackupReserve => "backup_reserve",
    }
}

fn decimal(column: &'static str, value: &str) -> Result<rust_decimal::Decimal, StoreError> {
    value.parse().map_err(|_| StoreError::NotADecimal {
        column,
        value: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::GuardRule;
    use hems_core::prelude::{AssetId, Power, Slot};
    use hems_grid::evidence::{Action, ComplianceSample};
    use hems_grid::para14a::ControlMode;
    use rust_decimal::Decimal;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

    fn quarter(at: OffsetDateTime, draw: i64) -> QuarterHour {
        QuarterHour {
            grid_draw: Decimal::new(draw, 3),
            grid_feed_in: Decimal::new(125, 3),
            ..QuarterHour::empty(Slot::containing(at))
        }
    }

    fn event(received: OffsetDateTime, released: Option<OffsetDateTime>) -> ControlEvent {
        let mut e = ControlEvent::received(
            GuardRule::Lpc,
            ControlMode::Ems,
            Power::from_kw(4.2),
            Power::from_kw(10.5),
            received,
        );
        e.applied_at = Some(received);
        e.acted = Some(Action::Commanded);
        e.released_at = released;
        e.assets = vec![AssetId::new("wallbox").unwrap()];
        e.samples = (0..3)
            .map(|i| ComplianceSample {
                at: received + time::Duration::minutes(i),
                netzwirksam: Power::from_kw(3.0),
                ceiling: Power::from_kw(4.2),
            })
            .collect();
        e
    }

    #[test]
    fn a_quarter_hour_survives_the_round_trip_to_the_last_digit() {
        // The property the whole column type turns on: a settlement quantity
        // that went through an `f64` is a settlement nobody can reproduce.
        let store = Store::in_memory().unwrap();
        let mut written = quarter(NOW, 1_234_567);
        written.grid_draw = Decimal::new(1_234_567, 6);
        store.put_quarter_hour("site-1", &written, NOW).unwrap();

        let read = store
            .quarter_hours("site-1", Some(NOW), Some(NOW + time::Duration::hours(1)))
            .unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].grid_draw, Decimal::new(1_234_567, 6));
        assert_eq!(read[0].grid_draw.to_string(), "1.234567");
    }

    #[test]
    fn a_restated_register_replaces_the_one_before_it() {
        let store = Store::in_memory().unwrap();
        store
            .put_quarter_hour("site-1", &quarter(NOW, 1000), NOW)
            .unwrap();
        store
            .put_quarter_hour("site-1", &quarter(NOW, 2000), NOW + time::Duration::days(1))
            .unwrap();
        let read = store
            .quarter_hours("site-1", Some(NOW), Some(NOW + time::Duration::hours(1)))
            .unwrap();
        assert_eq!(read.len(), 1, "one slot, not two");
        assert_eq!(read[0].grid_draw, Decimal::new(2000, 3));
    }

    #[test]
    fn two_sites_do_not_see_each_others_registers() {
        let store = Store::in_memory().unwrap();
        store
            .put_quarter_hour("site-1", &quarter(NOW, 1000), NOW)
            .unwrap();
        store
            .put_quarter_hour("site-2", &quarter(NOW, 9000), NOW)
            .unwrap();
        let read = store
            .quarter_hours("site-1", Some(NOW), Some(NOW + time::Duration::hours(1)))
            .unwrap();
        assert_eq!(read[0].grid_draw, Decimal::new(1000, 3));
    }

    #[test]
    fn an_event_and_its_whole_trace_are_stored_together() {
        let mut store = Store::in_memory().unwrap();
        let id = store
            .put_control_event("site-1", &event(NOW, Some(NOW + time::Duration::hours(1))))
            .unwrap();
        assert_eq!(store.control_event_count("site-1").unwrap(), 1);
        assert_eq!(store.sample_count(id).unwrap(), 3);
    }

    #[test]
    fn the_two_years_run_from_the_day_the_event_closed() {
        // An event that ran for a week is documented for two years after it
        // *ended*, which is the reading that never keeps less than [A1 7.3]
        // asks for.
        let mut store = Store::in_memory().unwrap();
        let long = event(NOW, Some(NOW + time::Duration::days(7)));
        store.put_control_event("site-1", &long).unwrap();

        // Two years after it arrived it is still there, because it did not end
        // then.
        let two_years_after_arrival = NOW + EVIDENCE_RETENTION + time::Duration::hours(1);
        assert_eq!(store.prune(two_years_after_arrival).unwrap(), 0);
        assert_eq!(store.control_event_count("site-1").unwrap(), 1);

        let two_years_after_release =
            NOW + time::Duration::days(7) + EVIDENCE_RETENTION + time::Duration::hours(1);
        assert_eq!(store.prune(two_years_after_release).unwrap(), 1);
        assert_eq!(store.control_event_count("site-1").unwrap(), 0);
    }

    #[test]
    fn pruning_an_event_takes_its_trace_with_it() {
        // A compliance trace whose event has been deleted is a column of numbers
        // nobody can interpret.
        let mut store = Store::in_memory().unwrap();
        let id = store
            .put_control_event("site-1", &event(NOW, Some(NOW)))
            .unwrap();
        assert_eq!(store.sample_count(id).unwrap(), 3);
        store
            .prune(NOW + EVIDENCE_RETENTION + time::Duration::days(1))
            .unwrap();
        assert_eq!(store.sample_count(id).unwrap(), 0);
    }

    #[test]
    fn nothing_inside_the_two_years_is_ever_pruned() {
        // The direction that matters: a box that deleted evidence early has
        // destroyed the household's own proof that it obeyed the operator.
        let mut store = Store::in_memory().unwrap();
        store
            .put_control_event("site-1", &event(NOW, Some(NOW)))
            .unwrap();
        let nearly = NOW + EVIDENCE_RETENTION - time::Duration::days(1);
        assert_eq!(store.prune(nearly).unwrap(), 0);
        assert_eq!(store.control_event_count("site-1").unwrap(), 1);
        assert_eq!(store.earliest_event("site-1").unwrap(), Some(NOW));
    }

    #[test]
    fn the_schema_records_which_revision_it_is_at() {
        let store = Store::in_memory().unwrap();
        let at: i32 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(at, MIGRATIONS.last().unwrap().0);
    }

    #[test]
    fn opening_an_up_to_date_store_applies_nothing() {
        // The property that makes a numbered migration different from a
        // `CREATE TABLE IF NOT EXISTS`: the second open must not re-run the
        // first revision, because a revision that is not idempotent — an
        // `ALTER TABLE`, an `INSERT` of a seed row — would fail or duplicate.
        let file =
            std::env::temp_dir().join(format!("hems-histd-migrate-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&file);
        Store::open(&file).unwrap();
        // `0001_schema.sql` is `IF NOT EXISTS` throughout, so a re-run would
        // pass; what this pins is that the runner does not attempt it.
        let again = Store::open(&file).unwrap();
        let at: i32 = again
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(at, MIGRATIONS.last().unwrap().0);
        drop(again);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_store_written_by_a_newer_build_is_refused_rather_than_used() {
        // A downgraded box. Two years of § 14a evidence is the last record in
        // this workspace that should be repaired by guessing what a column an
        // older build has never heard of was for.
        let store = Store::in_memory().unwrap();
        store
            .connection
            .execute_batch("PRAGMA user_version = 9999")
            .unwrap();
        assert!(matches!(
            store.migrate(),
            Err(StoreError::FromTheFuture {
                found: 9999,
                understood: 1
            })
        ));
    }

    #[test]
    fn an_event_survives_being_written_and_read_back_unchanged() {
        // The property the document column exists for. The old schema wrote the
        // enums through `format!("{:?}")`, which is not a serialisation and
        // could not be read back at all — so a Nachweis was reassembled field by
        // field, and a renamed variant would have silently changed what two
        // years of evidence said.
        let mut store = Store::in_memory().unwrap();
        let written = event(NOW, Some(NOW + time::Duration::minutes(90)));
        store.put_control_event("site-1", &written).unwrap();
        let read = store.control_events("site-1", None, None).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].event, written, "including its whole trace");
    }

    #[test]
    fn a_store_that_is_reopened_still_has_everything() {
        // The whole point. `EvidenceRecorder` built this record for four
        // versions and it died with the process.
        let dir = std::env::temp_dir().join("hems-histd-reopen");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(format!("{}.sqlite", NOW.unix_timestamp()));
        std::fs::remove_file(&path).ok();
        {
            let mut store = Store::open(&path).unwrap();
            store
                .put_control_event("site-1", &event(NOW, Some(NOW)))
                .unwrap();
            store
                .put_quarter_hour("site-1", &quarter(NOW, 1000), NOW)
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.control_event_count("site-1").unwrap(), 1);
        assert_eq!(
            store
                .quarter_hours("site-1", Some(NOW), Some(NOW + time::Duration::hours(1)))
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_file(&path).ok();
    }
}
