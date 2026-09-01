//! What the box remembers, and what it still owes the fleet.
//!
//! `[A1 7.3]` documents a § 14a control event for **two years**, and the house
//! is never worse off when the cloud is gone, so the record has to be
//! here. `histd` keeps the fleet's copy and answers across a portfolio; this one
//! answers for one household, with the WAN cut.
//!
//! The two do not share code, and they share the **types** instead —
//! `hems_grid::ControlEvent` and `QuarterHour` travel between them. That is the
//! same mechanism that keeps `DayKpis` honest between here and `obsd`: a renamed
//! field is a compile error rather than a column that quietly stops matching.
//!
//! # The outbox
//!
//! A box records **first** and forwards **second**. [`Store::pending_events`]
//! and [`Store::pending_quarter_hours`] are what the fleet has not acknowledged
//! and [`Store::mark_forwarded`] is the acknowledgement, so a box offline for a
//! week keeps its own two years and
//! reconciles when it comes back. The other order makes the WAN a dependency of
//! the record, and the day a network operator asks about is the day the link was
//! down.
//!
//! Forwarded is not deleted: the two years are the household's, so [`Store::prune`]
//! follows the retention window and never an acknowledgement.
//!
//! # One household, one process
//!
//! The edge is a single daemon, so there is no `site_id` here: a column
//! holding the same value in every row is a join key for a join nobody makes.
//! The site's name belongs to the report that leaves the box, not to its own
//! record.

use std::path::Path;

use hems_core::prelude::{GuardRule, Power, Slot};
use hems_grid::evidence::{ComplianceSample, ControlEvent};
use hems_grid::mispel::QuarterHour;
use rusqlite::{Connection, params};
use thiserror::Error;
use time::OffsetDateTime;

/// How long a § 14a control event is kept, `[A1 7.3]`.
pub const RETENTION: time::Duration = time::Duration::days(2 * 365);

/// Why the store could not answer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database itself.
    #[error("the box's store failed: {0}")]
    Sql(#[from] rusqlite::Error),
    /// An event could not be turned into a document to store.
    #[error("the event could not be serialised: {detail}")]
    NotSerialisable {
        /// What `serde` said.
        detail: String,
    },
    /// A stored document is not one this build can read.
    ///
    /// Named with the row rather than swallowed: one unreadable event in two
    /// years of them is a fact an operator has to be told, not a gap in a
    /// Nachweis nobody can account for.
    #[error("the stored event {id} cannot be read by this build: {detail}")]
    NotReadable {
        /// Which row.
        id: i64,
        /// What `serde` said.
        detail: String,
    },
    /// A stored quantity is not the exact decimal it was written as.
    #[error("the stored quantity {value:?} in {column} is not a decimal")]
    NotADecimal {
        /// Which column.
        column: &'static str,
        /// What was in it.
        value: String,
    },
    /// The database is at a revision this build does not know.
    ///
    /// A downgraded box. Two years of § 14a evidence is the last record in this
    /// workspace that should be repaired by guesswork.
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
/// file per change and never an edit to one already applied. `mako` applies
/// them with `sqlx::migrate!`; this is SQLite, so they are compiled in and the
/// applied revision lives in SQLite's own `user_version`.
const MIGRATIONS: &[(i32, &str)] = &[(1, include_str!("../migrations/0001_schema.sql"))];

/// What has not reached the fleet yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Backlog {
    /// Control events waiting.
    pub events: usize,
    /// Quarter hours waiting.
    pub quarter_hours: usize,
}

impl Backlog {
    /// Whether the box is up to date with the fleet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events == 0 && self.quarter_hours == 0
    }
}

/// One event as it is held.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    /// The row identifier, which is what an acknowledgement names.
    pub id: i64,
    /// The event, with its compliance trace re-attached.
    pub event: ControlEvent,
}

/// The upsert both write paths use, so a single row and a batch cannot come to
/// mean different things.
///
/// It clears `forwarded_at`: a restated register is a different number from the
/// one the fleet was given, so it is owed again.
const QUARTER_HOUR_UPSERT: &str = "INSERT INTO quarter_hour (
     slot_start, grid_draw_kwh, grid_feed_in_kwh, device_consumption_kwh,
     device_generation_kwh, anzulegender_wert_ct, spot_price_ct, recorded_at
 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
 ON CONFLICT(slot_start) DO UPDATE SET
     grid_draw_kwh          = excluded.grid_draw_kwh,
     grid_feed_in_kwh       = excluded.grid_feed_in_kwh,
     device_consumption_kwh = excluded.device_consumption_kwh,
     device_generation_kwh  = excluded.device_generation_kwh,
     anzulegender_wert_ct   = excluded.anzulegender_wert_ct,
     spot_price_ct          = excluded.spot_price_ct,
     recorded_at            = excluded.recorded_at,
     forwarded_at           = NULL";

/// Its parameters, in the order the statement names them.
fn quarter_hour_params(
    q: &QuarterHour,
    recorded_at: OffsetDateTime,
) -> [rusqlite::types::Value; 8] {
    use rusqlite::types::Value;
    [
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

/// The box's own record.
pub struct Store {
    connection: Connection,
}

impl Store {
    /// Open — or create — the database at `path`, and bring the schema up.
    ///
    /// # Errors
    /// [`StoreError::Sql`], or [`StoreError::FromTheFuture`] for a file written
    /// by a newer build.
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

    /// A store in memory, for a test.
    ///
    /// # Errors
    /// As [`Store::open`].
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::open(Path::new(":memory:"))
    }

    /// Bring the database up to the newest revision in [`MIGRATIONS`].
    fn migrate(&self) -> Result<(), StoreError> {
        // Every `PRAGMA` this store needs, on every open. `journal_mode` is
        // persistent and cannot be set inside the transaction a migration runs
        // in; `foreign_keys` and `busy_timeout` are per *connection*. WAL lets a
        // reader and a writer run at once and does nothing about two writers —
        // a retention sweep and an evidence write are two — so the busy timeout
        // is what turns `SQLITE_BUSY` into a short wait rather than a failed
        // write.
        self.connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;

        let at: i32 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let newest = MIGRATIONS.last().map_or(0, |(v, _)| *v);
        if at > newest {
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

    /// Write one quarter hour's registers.
    ///
    /// **Upsert, and it clears `forwarded_at`.** A restated register is a
    /// different number from the one the fleet was given, so it is owed again.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn put_quarter_hour(
        &self,
        quarter: &QuarterHour,
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            QUARTER_HOUR_UPSERT,
            rusqlite::params_from_iter(quarter_hour_params(quarter, recorded_at)),
        )?;
        Ok(())
    }

    /// Write a whole day's registers in **one** transaction.
    ///
    /// Ninety-six rows, and one statement per row is one commit — and one
    /// `fsync` — each. A day's registers are also one *fact*: a settlement that
    /// can observe half of them is a settlement that can be run on half a day.
    ///
    /// # Errors
    /// [`StoreError::Sql`]. Nothing is written if any row fails.
    pub fn put_quarter_hours(
        &mut self,
        quarters: &[QuarterHour],
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        {
            let mut statement = transaction.prepare(QUARTER_HOUR_UPSERT)?;
            for quarter in quarters {
                statement.execute(rusqlite::params_from_iter(quarter_hour_params(
                    quarter,
                    recorded_at,
                )))?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Write a closed control event and its compliance trace.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::NotSerialisable`].
    pub fn put_control_event(&mut self, event: &ControlEvent) -> Result<i64, StoreError> {
        // Two years from the day it *closed*, not from the day it arrived: an
        // event that ran for a week is documented for two years after it ended.
        let expires_at = event.released_at.unwrap_or(event.received_at) + RETENTION;
        let mut document = event.clone();
        document.samples.clear();
        let document =
            serde_json::to_string(&document).map_err(|e| StoreError::NotSerialisable {
                detail: e.to_string(),
            })?;

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO control_event (
                 document, rule, received_at, released_at, first_ceiling_w,
                 strictest_ceiling_w, minimum_power_w, below_minimum, expires_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
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

    /// Every control event on record, oldest first, with its trace re-attached.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::NotReadable`].
    pub fn control_events(&self) -> Result<Vec<StoredEvent>, StoreError> {
        self.read_events(
            "SELECT id, document FROM control_event ORDER BY received_at, id",
            [],
        )
    }

    /// The events the fleet has not acknowledged, oldest first, at most `limit`.
    ///
    /// # Errors
    /// As [`Store::control_events`].
    pub fn pending_events(&self, limit: usize) -> Result<Vec<StoredEvent>, StoreError> {
        self.read_events(
            "SELECT id, document FROM control_event
             WHERE forwarded_at IS NULL ORDER BY received_at, id LIMIT ?1",
            params![i64::try_from(limit).unwrap_or(i64::MAX)],
        )
    }

    /// The quarter hours the fleet has not acknowledged, oldest first.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::NotADecimal`].
    pub fn pending_quarter_hours(&self, limit: usize) -> Result<Vec<QuarterHour>, StoreError> {
        self.read_quarter_hours(
            "SELECT slot_start, grid_draw_kwh, grid_feed_in_kwh, device_consumption_kwh,
                    device_generation_kwh, anzulegender_wert_ct, spot_price_ct
             FROM quarter_hour WHERE forwarded_at IS NULL ORDER BY slot_start LIMIT ?1",
            params![i64::try_from(limit).unwrap_or(i64::MAX)],
        )
    }

    /// Every quarter hour on record, oldest first.
    ///
    /// # Errors
    /// As [`Store::pending_quarter_hours`].
    pub fn quarter_hours(&self) -> Result<Vec<QuarterHour>, StoreError> {
        self.read_quarter_hours(
            "SELECT slot_start, grid_draw_kwh, grid_feed_in_kwh, device_consumption_kwh,
                    device_generation_kwh, anzulegender_wert_ct, spot_price_ct
             FROM quarter_hour ORDER BY slot_start",
            [],
        )
    }

    /// Record that the fleet has these events and these quarter hours.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn mark_forwarded(
        &mut self,
        events: &[i64],
        slots: &[Slot],
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        if events.is_empty() && slots.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        {
            let mut by_id =
                transaction.prepare("UPDATE control_event SET forwarded_at = ?2 WHERE id = ?1")?;
            for id in events {
                by_id.execute(params![id, at.unix_timestamp()])?;
            }
            let mut by_slot = transaction
                .prepare("UPDATE quarter_hour SET forwarded_at = ?2 WHERE slot_start = ?1")?;
            for slot in slots {
                by_slot.execute(params![slot.start().unix_timestamp(), at.unix_timestamp()])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// How much the box is still holding for the fleet.
    ///
    /// A backlog that only grows is a fleet link that has been down for longer
    /// than anybody noticed, and it is invisible in every other KPI: the
    /// household was managed correctly throughout.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn backlog(&self) -> Result<Backlog, StoreError> {
        let count = |sql: &str| -> Result<usize, StoreError> {
            let n: i64 = self.connection.query_row(sql, [], |row| row.get(0))?;
            Ok(usize::try_from(n).unwrap_or(0))
        };
        Ok(Backlog {
            events: count("SELECT COUNT(*) FROM control_event WHERE forwarded_at IS NULL")?,
            quarter_hours: count("SELECT COUNT(*) FROM quarter_hour WHERE forwarded_at IS NULL")?,
        })
    }

    /// Delete every event whose two years are up, and the registers older than
    /// the same window.
    ///
    /// Returns how many events went; their traces go with them by
    /// `ON DELETE CASCADE`, because a trace whose event has been deleted is a
    /// set of numbers nobody can interpret.
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
            params![(now - RETENTION).unix_timestamp()],
        )?;
        Ok(events)
    }

    /// Run a query whose columns are `(id, document)`.
    fn read_events(
        &self,
        sql: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let mut statement = self.connection.prepare(sql)?;
        let rows: Vec<(i64, String)> = statement
            .query_map(args, |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, document) in rows {
            let mut event: ControlEvent =
                serde_json::from_str(&document).map_err(|e| StoreError::NotReadable {
                    id,
                    detail: e.to_string(),
                })?;
            event.samples = self.samples_of(id)?;
            out.push(StoredEvent { id, event });
        }
        Ok(out)
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

    /// Run a query whose columns are the seven a [`QuarterHour`] is built from.
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
}

fn decimal(column: &'static str, value: &str) -> Result<rust_decimal::Decimal, StoreError> {
    value.parse().map_err(|_| StoreError::NotADecimal {
        column,
        value: value.to_owned(),
    })
}

/// The name a `GuardRule` is stored under.
///
/// Written out rather than taken from `Debug`, which is not a wire format:
/// nothing promises it round trips, and renaming a variant would silently change
/// what two years of evidence say. Only the *projection* uses this; the event
/// itself is reconstructed from its `serde` document.
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

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::AssetId;
    use hems_grid::evidence::Action;
    use hems_grid::para14a::ControlMode;
    use rust_decimal::Decimal;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

    fn quarter(at: OffsetDateTime) -> QuarterHour {
        QuarterHour {
            grid_draw: Decimal::new(1_234_567, 6),
            ..QuarterHour::empty(Slot::containing(at))
        }
    }

    fn event(received: OffsetDateTime) -> ControlEvent {
        let mut e = ControlEvent::received(
            GuardRule::Lpc,
            ControlMode::Ems,
            Power::from_kw(4.2),
            Power::from_kw(10.5),
            received,
        );
        e.applied_at = Some(received);
        e.acted = Some(Action::Commanded);
        e.released_at = Some(received + time::Duration::minutes(90));
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
    fn an_event_is_read_back_exactly_as_it_was_written() {
        // The property the document column exists for: `Debug` is not a
        // serialisation, so an event stored field by field cannot be recovered.
        let mut store = Store::in_memory().unwrap();
        let written = event(NOW);
        store.put_control_event(&written).unwrap();
        let read = store.control_events().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].event, written, "including its whole trace");
    }

    #[test]
    fn a_settlement_quantity_survives_to_the_last_digit() {
        let store = Store::in_memory().unwrap();
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        assert_eq!(
            store.quarter_hours().unwrap()[0].grid_draw,
            Decimal::new(1_234_567, 6)
        );
    }

    #[test]
    fn a_whole_day_goes_in_one_call() {
        // Ninety-six rows, one statement each, one commit for the lot. The
        // *atomicity* is structural — `transaction()` and one `commit()` — and
        // is deliberately not asserted here: there is no way to make this batch
        // fail part-way from outside the store, and a test that cannot fail is
        // not a test. What this pins is that every row arrives.
        let mut store = Store::in_memory().unwrap();
        let day: Vec<QuarterHour> = (0..96)
            .map(|i| quarter(NOW + time::Duration::minutes(15 * i)))
            .collect();
        store.put_quarter_hours(&day, NOW).unwrap();
        assert_eq!(store.quarter_hours().unwrap().len(), 96);
        assert_eq!(store.backlog().unwrap().quarter_hours, 96, "all owed");
    }

    #[test]
    fn a_batch_and_a_single_row_write_the_same_thing() {
        // Two write paths, one statement: the single row and the batch share the
        // upsert, so they cannot come to disagree about what a register is.
        let mut batched = Store::in_memory().unwrap();
        batched.put_quarter_hours(&[quarter(NOW)], NOW).unwrap();
        let one = Store::in_memory().unwrap();
        one.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        assert_eq!(
            batched.quarter_hours().unwrap(),
            one.quarter_hours().unwrap()
        );
    }

    #[test]
    fn nothing_counts_as_forwarded_until_the_fleet_has_said_so() {
        let mut store = Store::in_memory().unwrap();
        store.put_control_event(&event(NOW)).unwrap();
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        assert_eq!(
            store.backlog().unwrap(),
            Backlog {
                events: 1,
                quarter_hours: 1
            }
        );
    }

    #[test]
    fn an_acknowledgement_empties_the_outbox_and_keeps_the_record() {
        let mut store = Store::in_memory().unwrap();
        store.put_control_event(&event(NOW)).unwrap();
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();

        let ids: Vec<i64> = store
            .pending_events(10)
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        let slots: Vec<Slot> = store
            .pending_quarter_hours(10)
            .unwrap()
            .iter()
            .map(|q| q.slot)
            .collect();
        store.mark_forwarded(&ids, &slots, NOW).unwrap();

        assert!(store.backlog().unwrap().is_empty());
        // Forwarded is not deleted: `[A1 7.3]`'s two years are the household's,
        // so a record that left as soon as the fleet had a copy would be a
        // record that depends on the fleet.
        assert_eq!(store.control_events().unwrap().len(), 1);
        assert_eq!(store.quarter_hours().unwrap().len(), 1);
    }

    #[test]
    fn a_week_offline_is_a_backlog_and_not_a_gap() {
        let store = Store::in_memory().unwrap();
        for day in 0..7 {
            store
                .put_quarter_hour(&quarter(NOW + time::Duration::days(day)), NOW)
                .unwrap();
        }
        assert_eq!(store.backlog().unwrap().quarter_hours, 7);
    }

    #[test]
    fn a_catch_up_that_is_interrupted_leaves_the_rest_owed() {
        let mut store = Store::in_memory().unwrap();
        for day in 0..7 {
            store
                .put_quarter_hour(&quarter(NOW + time::Duration::days(day)), NOW)
                .unwrap();
        }
        let slots: Vec<Slot> = store
            .pending_quarter_hours(3)
            .unwrap()
            .iter()
            .map(|q| q.slot)
            .collect();
        assert_eq!(slots.len(), 3, "the limit is a limit");
        store.mark_forwarded(&[], &slots, NOW).unwrap();
        assert_eq!(store.backlog().unwrap().quarter_hours, 4);
    }

    #[test]
    fn a_restated_register_is_owed_to_the_fleet_again() {
        // The fleet was given a number that has since been corrected, so the
        // correction is outstanding even though the slot is not new.
        let mut store = Store::in_memory().unwrap();
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        store
            .mark_forwarded(&[], &[Slot::containing(NOW)], NOW)
            .unwrap();
        assert!(store.backlog().unwrap().is_empty());

        let mut restated = quarter(NOW);
        restated.grid_draw = Decimal::new(9_999_999, 6);
        store.put_quarter_hour(&restated, NOW).unwrap();
        assert_eq!(store.backlog().unwrap().quarter_hours, 1);
        assert_eq!(
            store.quarter_hours().unwrap()[0].grid_draw,
            Decimal::new(9_999_999, 6)
        );
    }

    #[test]
    fn the_two_years_run_from_the_day_the_event_closed() {
        let mut store = Store::in_memory().unwrap();
        store.put_control_event(&event(NOW)).unwrap();
        // A day short of two years after it *ended*, it is still there.
        let almost = NOW + time::Duration::minutes(90) + RETENTION - time::Duration::days(1);
        assert_eq!(store.prune(almost).unwrap(), 0);
        assert_eq!(store.control_events().unwrap().len(), 1);
        // A day after, it is not — and its trace went with it.
        assert_eq!(store.prune(almost + time::Duration::days(2)).unwrap(), 1);
        assert!(store.control_events().unwrap().is_empty());
        let samples: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM compliance_sample", [], |r| r.get(0))
            .unwrap();
        assert_eq!(samples, 0, "ON DELETE CASCADE");
    }

    #[test]
    fn a_store_written_by_a_newer_build_is_refused_rather_than_used() {
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
    fn a_reopened_store_still_has_everything_and_re_runs_no_migration() {
        let path = std::env::temp_dir().join(format!("hems-box-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut store = Store::open(&path).unwrap();
            store.put_control_event(&event(NOW)).unwrap();
            store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.control_events().unwrap().len(), 1);
        assert_eq!(store.quarter_hours().unwrap().len(), 1);
        let at: i32 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(at, MIGRATIONS.last().unwrap().0);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
