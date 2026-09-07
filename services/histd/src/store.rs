//! The database, and the only place SQL is written.
//!
//! PostgreSQL, through the pool [`hems_service::db`] builds: a fleet's writes do
//! not queue behind one lock, the service can run more than one replica, and a
//! settlement quantity is a `NUMERIC` the database adds up itself (D156). The
//! schema is `migrations/0001_schema.sql`.

use hems_core::prelude::{GuardRule, Power, Slot};
use hems_grid::evidence::{ComplianceSample, ControlEvent};
use hems_grid::mispel::QuarterHour;
use hems_service::db::{Db, Migration};
use rust_decimal::Decimal;
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

/// The bounds an "unbounded" window is asked for as.
///
/// `None` on either side of a window means *no bound*, which is what a Data Act
/// export asks for — and the way that reaches SQL matters. The obvious spelling,
/// `$2::timestamptz IS NULL OR slot_start >= $2`, plans as an `Index Cond` only
/// while the planner can see the value; under a **generic** plan, which is what
/// a prepared statement gets after five executions, the range collapses into a
/// `Filter` and a one-day question reads a household's whole two years.
///
/// Sentinels keep the comparison a comparison. They are the store's to invent
/// and never a caller's: expecting a caller to pass a far-future instant is how
/// an export comes back empty on a leap of arithmetic nobody notices. Both sit
/// comfortably inside PostgreSQL's `timestamptz` range (4713 BC to 294276 AD),
/// so neither is an overflow waiting for a birthday.
const BEGINNING: OffsetDateTime = time::macros::datetime!(0001-01-01 00:00:00 UTC);
/// The upper one. See [`BEGINNING`].
const FOREVER: OffsetDateTime = time::macros::datetime!(9999-12-31 23:59:59 UTC);

/// The schema this binary carries.
///
/// `include_str!` rather than a path read at run time, so the container needs no
/// files beside the binary and a revision cannot be edited between the build and
/// the deployment. `hems_service::db::migrate` checksums each one, so an edit to
/// a file already applied is refused rather than skipped.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "the evidence and settlement records",
    sql: include_str!("../migrations/0001_schema.sql"),
}];

/// Why the store could not answer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database itself.
    #[error("the history store failed: {0}")]
    Sql(#[from] tokio_postgres::Error),
    /// No connection could be taken from the pool.
    ///
    /// Separate from [`StoreError::Sql`] because it is a different fault with a
    /// different response: the database is unreachable or the pool is saturated,
    /// and a caller should back off rather than retry immediately.
    #[error("no database connection was available: {0}")]
    Pool(String),
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
    /// A rolled-back deployment, or a row another program has written. Named
    /// with the row rather than swallowed, because one unreadable event in two
    /// years of them is a fact an operator has to be told rather than a gap in a
    /// Nachweis nobody can account for.
    #[error("the stored event {id} cannot be read by this build: {detail}")]
    NotReadable {
        /// Which row.
        id: i64,
        /// What `serde` said.
        detail: String,
    },
}

impl From<deadpool_postgres::PoolError> for StoreError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        Self::Pool(e.to_string())
    }
}

/// The fleet's record.
///
/// A handle onto the pool, cheap to clone, with no writer to serialise behind —
/// so a fleet's forwarded evidence does not queue on the path a Nachweis is
/// built from (D156).
#[derive(Debug, Clone)]
pub struct Store {
    db: Db,
}

impl Store {
    /// A store over an already-open pool.
    #[must_use]
    pub const fn new(db: Db) -> Self {
        Self { db }
    }

    /// The pool underneath, for a readiness probe.
    #[must_use]
    pub const fn db(&self) -> &Db {
        &self.db
    }

    /// Write one quarter hour's registers, as known at `recorded_at`.
    ///
    /// **Append-only.** A register is restated — a substitute value replaced by
    /// a real one, a correction from the metering point operator — and a
    /// restatement writes a *new version* rather than replacing the one the
    /// month was settled on. That is `metering`'s valid time and `meterstore`'s
    /// `recorded_at`, and it is what lets a disputed Nachweis be reproduced from
    /// the registers it was actually computed from
    /// ([`Store::quarter_hours_as_of`]).
    ///
    /// Re-sending the *same* version is idempotent: a box that retries a batch
    /// it already placed writes nothing new.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn put_quarter_hour(
        &self,
        site: &str,
        quarter: &QuarterHour,
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.put_quarter_hours(site, std::slice::from_ref(quarter), recorded_at)
            .await
    }

    /// Write many quarter hours in **one** statement.
    ///
    /// A box posts ninety-six of them at the end of a day. One statement per row
    /// is ninety-six round trips to a database that is now across a network, so
    /// the rows go as nine arrays through `UNNEST` — one parse, one plan, one
    /// round trip, one transaction. The registers of one day are also one
    /// *fact*, and a settlement that can observe half of them is a settlement
    /// that can be run on half a day.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`]. Nothing is written if any
    /// row fails.
    pub async fn put_quarter_hours(
        &self,
        site: &str,
        quarters: &[QuarterHour],
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        if quarters.is_empty() {
            return Ok(());
        }
        let slots: Vec<OffsetDateTime> = quarters.iter().map(|q| q.slot.start()).collect();
        let draw: Vec<Decimal> = quarters.iter().map(|q| q.grid_draw).collect();
        let feed_in: Vec<Decimal> = quarters.iter().map(|q| q.grid_feed_in).collect();
        let consumption: Vec<Decimal> = quarters.iter().map(|q| q.device_consumption).collect();
        let generation: Vec<Decimal> = quarters.iter().map(|q| q.device_generation).collect();
        let aw: Vec<Decimal> = quarters.iter().map(|q| q.anzulegender_wert).collect();
        let spot: Vec<Decimal> = quarters.iter().map(|q| q.spot_price).collect();
        // `Z3V¼`/`Z3E¼`, which most sites do not have. `Option<Decimal>` all the
        // way into the array, so a `NULL` stays a null: the box's "not
        // separately metered" and a store that genuinely moved nothing are
        // different facts, and Basisfall A4 is refused on the first and settled
        // on the second.
        let storage_draw: Vec<Option<Decimal>> =
            quarters.iter().map(|q| q.storage_consumption).collect();
        let storage_feed: Vec<Option<Decimal>> =
            quarters.iter().map(|q| q.storage_generation).collect();

        let client = self.db.get().await?;
        client
            .execute(
                "INSERT INTO quarter_hour (
                     site_id, slot_start, grid_draw_kwh, grid_feed_in_kwh,
                     device_consumption_kwh, device_generation_kwh,
                     storage_consumption_kwh, storage_generation_kwh,
                     anzulegender_wert_ct, spot_price_ct, recorded_at
                 )
                 SELECT $1, s, d, f, c, g, sc, sg, a, p, $11
                 FROM UNNEST($2::timestamptz[], $3::numeric[], $4::numeric[],
                             $5::numeric[], $6::numeric[], $7::numeric[],
                             $8::numeric[], $9::numeric[], $10::numeric[])
                      AS t(s, d, f, c, g, sc, sg, a, p)
                 -- A *restatement* is a new row, because `recorded_at` is in the
                 -- key. This arm is therefore only the box re-sending a batch it
                 -- already placed — same slot, same instant — which has to be
                 -- idempotent rather than an error, because that is exactly what
                 -- an outbox does when an acknowledgement is lost.
                 ON CONFLICT (site_id, slot_start, recorded_at) DO UPDATE SET
                     grid_draw_kwh           = EXCLUDED.grid_draw_kwh,
                     grid_feed_in_kwh        = EXCLUDED.grid_feed_in_kwh,
                     device_consumption_kwh  = EXCLUDED.device_consumption_kwh,
                     device_generation_kwh   = EXCLUDED.device_generation_kwh,
                     storage_consumption_kwh = EXCLUDED.storage_consumption_kwh,
                     storage_generation_kwh  = EXCLUDED.storage_generation_kwh,
                     anzulegender_wert_ct    = EXCLUDED.anzulegender_wert_ct,
                     spot_price_ct           = EXCLUDED.spot_price_ct",
                &[
                    &site,
                    &slots,
                    &draw,
                    &feed_in,
                    &consumption,
                    &generation,
                    &storage_draw,
                    &storage_feed,
                    &aw,
                    &spot,
                    &recorded_at,
                ],
            )
            .await?;
        Ok(())
    }

    /// Every quarter hour of `site` in `[from, to)`, as it stands **now**.
    ///
    /// `None` on either bound means "no bound", which is what a Data Act export
    /// asks for. Taking an instant instead and expecting a caller to invent a
    /// far-future one is how an export comes back empty on a leap of arithmetic
    /// nobody notices.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn quarter_hours(
        &self,
        site: &str,
        from: Option<OffsetDateTime>,
        to: Option<OffsetDateTime>,
    ) -> Result<Vec<QuarterHour>, StoreError> {
        self.quarter_hours_as_of(site, from, to, None).await
    }

    /// The same, as the registers stood at `as_of`.
    ///
    /// The read half of the bitemporality the table is keyed for. A register is
    /// restated after a month has already been settled — a substitute value
    /// replaced by a real one, a correction from the metering point operator —
    /// and the two questions that then get asked are *what did we hand over*
    /// and *what does the correction change*. Neither is answerable from a
    /// record that overwrites.
    ///
    /// `as_of: None` is the current value, which is what every ordinary read
    /// wants; a settlement being reproduced passes the instant its Nachweis was
    /// produced at.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn quarter_hours_as_of(
        &self,
        site: &str,
        from: Option<OffsetDateTime>,
        to: Option<OffsetDateTime>,
        as_of: Option<OffsetDateTime>,
    ) -> Result<Vec<QuarterHour>, StoreError> {
        let client = self.db.get().await?;
        // Plain comparisons against sentinels rather than `$2 IS NULL OR …` —
        // see [`BEGINNING`]. This is an index scan on the primary key for both a
        // one-day window and a two-year export.
        //
        // `DISTINCT ON (slot_start)` with `recorded_at DESC` is the newest
        // version at or before `as_of`: the primary key already orders by
        // `(site_id, slot_start, recorded_at)`, so PostgreSQL takes the groups
        // from the index and only has to reverse within each — one row deep on
        // every household that has never been corrected.
        let rows = client
            .query(
                "SELECT DISTINCT ON (slot_start)
                        slot_start, grid_draw_kwh, grid_feed_in_kwh,
                        device_consumption_kwh, device_generation_kwh,
                        storage_consumption_kwh, storage_generation_kwh,
                        anzulegender_wert_ct, spot_price_ct
                 FROM quarter_hour
                 WHERE site_id = $1 AND slot_start >= $2 AND slot_start < $3
                   AND recorded_at <= $4
                 ORDER BY slot_start, recorded_at DESC",
                &[
                    &site,
                    &from.unwrap_or(BEGINNING),
                    &to.unwrap_or(FOREVER),
                    &as_of.unwrap_or(FOREVER),
                ],
            )
            .await?;

        Ok(rows
            .iter()
            .map(|row| {
                let slot = Slot::containing(row.get::<_, OffsetDateTime>(0));
                QuarterHour {
                    grid_draw: row.get(1),
                    grid_feed_in: row.get(2),
                    device_consumption: row.get(3),
                    device_generation: row.get(4),
                    storage_consumption: row.get(5),
                    storage_generation: row.get(6),
                    anzulegender_wert: row.get(7),
                    spot_price: row.get(8),
                    ..QuarterHour::empty(slot)
                }
            })
            .collect())
    }

    /// Write a closed control event and its compliance trace.
    ///
    /// Returns the row identifier, so a caller can attach more to it.
    ///
    /// # Errors
    /// [`StoreError::Sql`], [`StoreError::Pool`] or
    /// [`StoreError::NotSerialisable`].
    pub async fn put_control_event(
        &self,
        site: &str,
        event: &ControlEvent,
    ) -> Result<i64, StoreError> {
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
            serde_json::to_value(&document).map_err(|e| StoreError::NotSerialisable {
                detail: e.to_string(),
            })?;

        let mut client = self.db.get().await?;
        let transaction = client.transaction().await?;
        let id: i64 = transaction
            .query_one(
                "INSERT INTO control_event (
                     site_id, document, rule, received_at, released_at, first_ceiling_w,
                     strictest_ceiling_w, minimum_power_w, below_minimum, expires_at
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 RETURNING id",
                &[
                    &site,
                    &document,
                    &rule_name(event.rule),
                    &event.received_at,
                    &event.released_at,
                    &event.first_ceiling().get(),
                    &event.strictest_ceiling().get(),
                    &event
                        .ceilings
                        .first()
                        .map_or(0.0, |c| c.minimum_power.get()),
                    &event.below_minimum(),
                    &expires_at,
                ],
            )
            .await?
            .get(0);

        if !event.samples.is_empty() {
            let at: Vec<OffsetDateTime> = event.samples.iter().map(|s| s.at).collect();
            let netzwirksam: Vec<f64> = event.samples.iter().map(|s| s.netzwirksam.get()).collect();
            let ceiling: Vec<f64> = event.samples.iter().map(|s| s.ceiling.get()).collect();
            // A ninety-minute reduction sampled every minute is ninety rows, and
            // a long one is thousands. One `UNNEST` rather than a prepared
            // statement executed in a loop, for the same reason the registers
            // are: the database is across a network now.
            transaction
                .execute(
                    "INSERT INTO compliance_sample (event_id, at, netzwirksam_w, ceiling_w)
                     SELECT $1, a, n, c
                     FROM UNNEST($2::timestamptz[], $3::float8[], $4::float8[]) AS t(a, n, c)
                     ON CONFLICT (event_id, at) DO UPDATE SET
                         netzwirksam_w = EXCLUDED.netzwirksam_w,
                         ceiling_w     = EXCLUDED.ceiling_w",
                    &[&id, &at, &netzwirksam, &ceiling],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(id)
    }

    /// Every control event of `site` in `[from, to)` by the instant it was
    /// received, oldest first, with its compliance trace re-attached.
    ///
    /// This is what a Nachweis `[A1 7.2]` and a Data Act export are built from,
    /// and it is the only way an event leaves the store — so the record a network
    /// operator is shown is the record that was written, reconstructed through
    /// `serde` rather than reassembled field by field.
    ///
    /// # Errors
    /// [`StoreError::Sql`], [`StoreError::Pool`], or [`StoreError::NotReadable`]
    /// for a document this build cannot parse.
    pub async fn control_events(
        &self,
        site: &str,
        from: Option<OffsetDateTime>,
        to: Option<OffsetDateTime>,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let client = self.db.get().await?;
        // The traces come back in the **same** round trip, as an aggregate per
        // event. The SQLite store issued one query per event; over a network
        // that is a two-year export paying a round trip for every reduction the
        // household ever saw.
        //
        // Three parallel `array_agg`s rather than one `json_agg` of triples, and
        // that is a correctness choice rather than a stylistic one. A JSON
        // aggregate renders each `timestamptz` as **text**, using the session's
        // `DateStyle` — ISO by default, and something this parser would refuse on
        // a server configured otherwise. An array comes back in PostgreSQL's own
        // binary form and is decoded as a `Vec<OffsetDateTime>`: no rendering, no
        // parsing, no session setting between the row and the instant.
        //
        // `ORDER BY s.at` on each, so the three stay aligned.
        let rows = client
            .query(
                "SELECT e.id,
                        e.document,
                        e.expires_at,
                        COALESCE((SELECT array_agg(s.at ORDER BY s.at)
                                  FROM compliance_sample s WHERE s.event_id = e.id),
                                 '{}'::timestamptz[]),
                        COALESCE((SELECT array_agg(s.netzwirksam_w ORDER BY s.at)
                                  FROM compliance_sample s WHERE s.event_id = e.id),
                                 '{}'::float8[]),
                        COALESCE((SELECT array_agg(s.ceiling_w ORDER BY s.at)
                                  FROM compliance_sample s WHERE s.event_id = e.id),
                                 '{}'::float8[])
                 FROM control_event e
                 WHERE e.site_id = $1 AND e.received_at >= $2 AND e.received_at < $3
                 ORDER BY e.received_at, e.id",
                &[&site, &from.unwrap_or(BEGINNING), &to.unwrap_or(FOREVER)],
            )
            .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = row.get(0);
            let document: serde_json::Value = row.get(1);
            let mut event: ControlEvent =
                serde_json::from_value(document).map_err(|e| StoreError::NotReadable {
                    id,
                    detail: e.to_string(),
                })?;
            let at: Vec<OffsetDateTime> = row.get(3);
            let netzwirksam: Vec<f64> = row.get(4);
            let ceiling: Vec<f64> = row.get(5);
            event.samples = samples_from(id, &at, &netzwirksam, &ceiling)?;
            out.push(StoredEvent {
                id,
                event,
                expires_at: row.get(2),
            });
        }
        Ok(out)
    }

    /// How many control events `site` has on record.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn control_event_count(&self, site: &str) -> Result<usize, StoreError> {
        let client = self.db.get().await?;
        let row = client
            .query_one(
                "SELECT COUNT(*) FROM control_event WHERE site_id = $1",
                &[&site],
            )
            .await?;
        Ok(usize::try_from(row.get::<_, i64>(0)).unwrap_or(0))
    }

    /// How many compliance samples one event carries.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn sample_count(&self, event_id: i64) -> Result<usize, StoreError> {
        let client = self.db.get().await?;
        let row = client
            .query_one(
                "SELECT COUNT(*) FROM compliance_sample WHERE event_id = $1",
                &[&event_id],
            )
            .await?;
        Ok(usize::try_from(row.get::<_, i64>(0)).unwrap_or(0))
    }

    /// The earliest event still on record for `site`, if any.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn earliest_event(&self, site: &str) -> Result<Option<OffsetDateTime>, StoreError> {
        let client = self.db.get().await?;
        let row = client
            .query_one(
                "SELECT MIN(received_at) FROM control_event WHERE site_id = $1",
                &[&site],
            )
            .await?;
        Ok(row.get(0))
    }

    /// Delete every event whose two years are up, and the registers older than
    /// the same window.
    ///
    /// Returns how many events went. Their traces go with them by
    /// `ON DELETE CASCADE`: a trace whose event has been deleted is a set of
    /// numbers nobody can interpret.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn prune(&self, now: OffsetDateTime) -> Result<usize, StoreError> {
        let client = self.db.get().await?;
        let events = client
            .execute("DELETE FROM control_event WHERE expires_at <= $1", &[&now])
            .await?;
        client
            .execute(
                "DELETE FROM quarter_hour WHERE slot_start <= $1",
                &[&(now - EVIDENCE_RETENTION)],
            )
            .await?;
        Ok(usize::try_from(events).unwrap_or(0))
    }
}

/// The trace, as the three aggregates above return it.
///
/// Three arrays rather than one array of triples, each ordered by the same
/// `s.at`, so they are aligned by construction. A length mismatch is therefore
/// impossible and is nevertheless refused rather than zipped short: the arrays
/// come from one query and one ordering, so if they ever disagree the assumption
/// this reads under has stopped holding, and a Nachweis is the last place to
/// find that out by truncation.
fn samples_from(
    id: i64,
    at: &[OffsetDateTime],
    netzwirksam: &[f64],
    ceiling: &[f64],
) -> Result<Vec<ComplianceSample>, StoreError> {
    if at.len() != netzwirksam.len() || at.len() != ceiling.len() {
        return Err(StoreError::NotReadable {
            id,
            detail: format!(
                "the compliance trace came back as {} instants, {} powers and {} ceilings",
                at.len(),
                netzwirksam.len(),
                ceiling.len()
            ),
        });
    }
    Ok(at
        .iter()
        .zip(netzwirksam)
        .zip(ceiling)
        .map(|((at, netzwirksam), ceiling)| ComplianceSample {
            at: *at,
            netzwirksam: Power::new(*netzwirksam),
            ceiling: Power::new(*ceiling),
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::{AssetId, Power, Slot};
    use hems_grid::evidence::Action;
    use hems_grid::para14a::ControlMode;
    use hems_service::testdb::Postgres;
    use rust_decimal::Decimal;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

    /// The smallest step either side of a transaction time, for the `as_of`
    /// boundaries. `recorded_at <= as_of` is inclusive, so a test that means
    /// "just before this version" has to say so rather than reusing the instant.
    const MOMENT: time::Duration = time::Duration::microseconds(1);

    async fn store() -> (Postgres, Store) {
        let fixture = Postgres::start(MIGRATIONS).await;
        let store = Store::new(fixture.db.clone());
        (fixture, store)
    }

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
        e.assets = vec![AssetId::new("wallbox").expect("a literal identifier")];
        e.samples = (0..3)
            .map(|i| ComplianceSample {
                at: received + time::Duration::minutes(i),
                netzwirksam: Power::from_kw(3.0),
                ceiling: Power::from_kw(4.2),
            })
            .collect();
        e
    }

    #[tokio::test]
    async fn a_quarter_hour_survives_the_round_trip_to_the_last_digit() {
        // The property the `NUMERIC` column exists for: a settlement quantity
        // that went through an `f64` is a settlement nobody can reproduce (P3).
        let (_fixture, store) = store().await;
        let written = QuarterHour {
            grid_draw: Decimal::new(1_234_567, 6),
            ..quarter(NOW, 0)
        };
        store
            .put_quarter_hour("site-1", &written, NOW)
            .await
            .unwrap();

        let read = store
            .quarter_hours("site-1", Some(NOW), Some(NOW + time::Duration::hours(1)))
            .await
            .unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].grid_draw, Decimal::new(1_234_567, 6));
        // The **scale** as well as the value: `NUMERIC` carries it, and a
        // quantity that came back as `1.23456700` would be the same number and a
        // different document.
        assert_eq!(read[0].grid_draw.to_string(), "1.234567");
    }

    #[tokio::test]
    async fn the_storage_system_s_own_registers_survive_the_round_trip() {
        // `Z3V¼`/`Z3E¼`, the pair Basisfall A4 is *defined* by. They were
        // modelled in `hems-grid`, required by `abgrenzung_month`, configurable
        // in this daemon — and had no column, so every A4 site read back `None`
        // and was refused a settlement it had declared. A round trip that fills
        // only the fields it happens to care about cannot notice that: this one
        // asserts the whole `QuarterHour`.
        let (_fixture, store) = store().await;
        let written = QuarterHour {
            storage_consumption: Some(Decimal::new(2_000, 3)),
            storage_generation: Some(Decimal::new(1_700, 3)),
            ..quarter(NOW, 4_000)
        };
        store
            .put_quarter_hour("site-1", &written, NOW)
            .await
            .unwrap();

        let read = store.quarter_hours("site-1", None, None).await.unwrap();
        assert_eq!(read, vec![written], "every register, not the ones we asked");
    }

    #[tokio::test]
    async fn a_store_that_is_not_separately_metered_stays_absent() {
        // The other half, and the one that decides a refusal: `NULL` is "no
        // separate meter" and is *not* zero. A zero would be a settlement
        // claiming the battery stood still, which is a claim about the
        // household rather than about the metering.
        let (_fixture, store) = store().await;
        store
            .put_quarter_hour("site-1", &quarter(NOW, 4_000), NOW)
            .await
            .unwrap();

        let read = store.quarter_hours("site-1", None, None).await.unwrap();
        assert_eq!(read[0].storage_consumption, None);
        assert_eq!(read[0].storage_generation, None);
    }

    #[tokio::test]
    async fn the_database_can_add_the_registers_up_itself() {
        // What the `TEXT` column made impossible, and the reason the type
        // changed: a settlement over a year is 35 040 rows, and summing them
        // used to mean parsing 35 040 strings in this process.
        //
        // The `SUM` below is written out in the test and **not** in the daemon,
        // and that is deliberate: the table is versioned, so an aggregate that
        // does not pick one version per slot counts a corrected quarter hour
        // twice. Every read in `histd` goes through `quarter_hours_as_of`, which
        // does. This test writes one version of each.
        let (fixture, store) = store().await;
        for i in 0..4 {
            store
                .put_quarter_hour(
                    "site-1",
                    &quarter(NOW + time::Duration::minutes(15 * i), 1_000),
                    NOW,
                )
                .await
                .unwrap();
        }
        let client = fixture.db.get().await.expect("a connection");
        let total: Decimal = client
            .query_one(
                "SELECT SUM(grid_draw_kwh) FROM quarter_hour WHERE site_id = $1",
                &[&"site-1"],
            )
            .await
            .expect("a sum")
            .get(0);
        assert_eq!(total, Decimal::new(4_000, 3), "exactly four kilowatt-hours");
    }

    #[tokio::test]
    async fn a_restated_register_supersedes_the_one_before_it_without_erasing_it() {
        // Both halves of the bitemporality the table is keyed for, and the
        // second one is why the key has three columns. An ordinary read gets the
        // correction; a settlement being reproduced gets what it was actually
        // computed from. A table that upserted could answer only the first, and
        // the schema claimed to answer both.
        let (_fixture, store) = store().await;
        let later = NOW + time::Duration::days(1);
        let window = (Some(NOW), Some(NOW + time::Duration::hours(1)));

        // A substitute value, and then the real reading a fortnight later.
        store
            .put_quarter_hour("site-1", &quarter(NOW, 1_000), NOW)
            .await
            .unwrap();
        store
            .put_quarter_hour("site-1", &quarter(NOW, 2_000), later)
            .await
            .unwrap();

        let now = store
            .quarter_hours("site-1", window.0, window.1)
            .await
            .unwrap();
        assert_eq!(now.len(), 1, "one slot, not two — the newest version of it");
        assert_eq!(now[0].grid_draw, Decimal::new(2_000, 3));

        // …and the Nachweis that was handed over before the correction arrived.
        let then = store
            .quarter_hours_as_of("site-1", window.0, window.1, Some(later - MOMENT))
            .await
            .unwrap();
        assert_eq!(then.len(), 1);
        assert_eq!(
            then[0].grid_draw,
            Decimal::new(1_000, 3),
            "a settlement under dispute is reproduced from the registers it was \
             computed from, not from the ones that replaced them"
        );
    }

    #[tokio::test]
    async fn a_batch_the_box_placed_twice_is_written_once() {
        // The box's outbox retries when an acknowledgement is lost, which sends
        // the same slot at the same `recorded_at` again. With the version in the
        // key that has to be idempotent rather than an error — and it must not
        // become a second version either, or a household would grow a register
        // history out of its own network.
        let (fixture, store) = store().await;
        for _ in 0..3 {
            store
                .put_quarter_hour("site-1", &quarter(NOW, 1_000), NOW)
                .await
                .unwrap();
        }
        let client = fixture.db.get().await.expect("a connection");
        let versions: i64 = client
            .query_one("SELECT COUNT(*) FROM quarter_hour", &[])
            .await
            .expect("a count")
            .get(0);
        assert_eq!(versions, 1, "a retry is not a restatement");
    }

    #[tokio::test]
    async fn an_as_of_before_the_first_version_sees_nothing() {
        // The boundary that decides whether `as_of` is a filter or a fiction: a
        // question about a day before the box had ever reported has to come back
        // empty rather than falling through to the current value.
        let (_fixture, store) = store().await;
        store
            .put_quarter_hour("site-1", &quarter(NOW, 1_000), NOW)
            .await
            .unwrap();
        let read = store
            .quarter_hours_as_of("site-1", None, None, Some(NOW - MOMENT))
            .await
            .unwrap();
        assert!(read.is_empty(), "nothing was known yet: {read:?}");
    }

    #[tokio::test]
    async fn a_days_registers_go_in_one_statement_and_all_come_back() {
        // Ninety-six rows through one `UNNEST`. A day's registers are one
        // *fact*, and a settlement that could observe half of them is one that
        // can be run on half a day.
        let (_fixture, store) = store().await;
        let quarters: Vec<QuarterHour> = (0..96)
            .map(|i| quarter(NOW + time::Duration::minutes(15 * i), 500 + i))
            .collect();
        store
            .put_quarter_hours("site-1", &quarters, NOW)
            .await
            .unwrap();
        let read = store.quarter_hours("site-1", None, None).await.unwrap();
        assert_eq!(read.len(), 96);
        assert_eq!(read, quarters, "every register of every row");
    }

    #[tokio::test]
    async fn a_battery_meter_that_drops_out_mid_day_writes_nulls_only_where_it_was_quiet() {
        // The realistic shape of a partly metered day, and the one a batch write
        // can get wrong on its own: `UNNEST` takes the storage registers as an
        // array of `Option`, so a day where the battery's meter answered for
        // half the quarter hours has to arrive as values *and* nulls in one
        // statement. Written as `unwrap_or_default()` anywhere along that path it
        // would arrive as values and **zeros**, and a household on Basisfall A4
        // would be settled on a battery that stood still all afternoon instead of
        // being refused.
        let (_fixture, store) = store().await;
        let quarters: Vec<QuarterHour> = (0..96)
            .map(|i| QuarterHour {
                storage_consumption: (i < 48).then(|| Decimal::new(120, 3)),
                storage_generation: (i < 48).then(|| Decimal::new(100, 3)),
                ..quarter(NOW + time::Duration::minutes(15 * i), 500 + i)
            })
            .collect();
        store
            .put_quarter_hours("site-1", &quarters, NOW)
            .await
            .unwrap();

        let read = store.quarter_hours("site-1", None, None).await.unwrap();
        assert_eq!(
            read, quarters,
            "values where it spoke, nulls where it did not"
        );
        assert_eq!(read[47].storage_consumption, Some(Decimal::new(120, 3)));
        assert_eq!(read[48].storage_consumption, None, "a null, never a zero");
    }

    #[tokio::test]
    async fn two_sites_do_not_see_each_others_registers() {
        let (_fixture, store) = store().await;
        store
            .put_quarter_hour("site-1", &quarter(NOW, 1_000), NOW)
            .await
            .unwrap();
        store
            .put_quarter_hour("site-2", &quarter(NOW, 9_000), NOW)
            .await
            .unwrap();
        let read = store
            .quarter_hours("site-1", Some(NOW), Some(NOW + time::Duration::hours(1)))
            .await
            .unwrap();
        assert_eq!(read[0].grid_draw, Decimal::new(1_000, 3));
    }

    #[tokio::test]
    async fn an_event_and_its_whole_trace_are_stored_together() {
        let (_fixture, store) = store().await;
        let id = store
            .put_control_event("site-1", &event(NOW, Some(NOW + time::Duration::hours(1))))
            .await
            .unwrap();
        assert_eq!(store.control_event_count("site-1").await.unwrap(), 1);
        assert_eq!(store.sample_count(id).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn an_event_survives_being_written_and_read_back_unchanged() {
        // The property the document column exists for. Reassembling a Nachweis
        // field by field would mean a renamed variant silently changed what two
        // years of evidence said.
        let (_fixture, store) = store().await;
        let written = event(NOW, Some(NOW + time::Duration::minutes(90)));
        store.put_control_event("site-1", &written).await.unwrap();
        let read = store.control_events("site-1", None, None).await.unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].event, written, "including its whole trace");
    }

    #[tokio::test]
    async fn the_two_years_run_from_the_day_the_event_closed() {
        // An event that ran for a week is documented for two years after it
        // *ended*, which is the reading that never keeps less than `[A1 7.3]`
        // asks for.
        let (_fixture, store) = store().await;
        let long = event(NOW, Some(NOW + time::Duration::days(7)));
        store.put_control_event("site-1", &long).await.unwrap();

        let two_years_after_arrival = NOW + EVIDENCE_RETENTION + time::Duration::hours(1);
        assert_eq!(store.prune(two_years_after_arrival).await.unwrap(), 0);
        assert_eq!(store.control_event_count("site-1").await.unwrap(), 1);

        let two_years_after_release =
            NOW + time::Duration::days(7) + EVIDENCE_RETENTION + time::Duration::hours(1);
        assert_eq!(store.prune(two_years_after_release).await.unwrap(), 1);
        assert_eq!(store.control_event_count("site-1").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn pruning_an_event_takes_its_trace_with_it() {
        // A compliance trace whose event has been deleted is a column of numbers
        // nobody can interpret.
        let (_fixture, store) = store().await;
        let id = store
            .put_control_event("site-1", &event(NOW, Some(NOW)))
            .await
            .unwrap();
        assert_eq!(store.sample_count(id).await.unwrap(), 3);
        store
            .prune(NOW + EVIDENCE_RETENTION + time::Duration::days(1))
            .await
            .unwrap();
        assert_eq!(store.sample_count(id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn nothing_inside_the_two_years_is_ever_pruned() {
        // The direction that matters: a service that deleted evidence early has
        // destroyed the household's own proof that it obeyed the operator.
        let (_fixture, store) = store().await;
        store
            .put_control_event("site-1", &event(NOW, Some(NOW)))
            .await
            .unwrap();
        let nearly = NOW + EVIDENCE_RETENTION - time::Duration::days(1);
        assert_eq!(store.prune(nearly).await.unwrap(), 0);
        assert_eq!(store.control_event_count("site-1").await.unwrap(), 1);
        assert_eq!(store.earliest_event("site-1").await.unwrap(), Some(NOW));
    }

    #[tokio::test]
    async fn a_fleet_writes_at_once_rather_than_behind_one_lock() {
        // The architectural reason for the change (D156). Under SQLite every
        // household's forwarded evidence went through one `Arc<Mutex<Store>>`
        // because there is one write lock; here the writes are concurrent, and
        // this fails if a future refactor puts a mutex back in front of them.
        let (_fixture, store) = store().await;
        let writes = (0..8).map(|i| {
            let store = store.clone();
            async move {
                store
                    .put_control_event(&format!("site-{i}"), &event(NOW, Some(NOW)))
                    .await
            }
        });
        // `join_all` without a `futures` dependency: eight futures, awaited
        // together rather than in sequence, which is what makes this a test
        // about concurrency rather than about a loop.
        let mut set = tokio::task::JoinSet::new();
        for write in writes {
            set.spawn(write);
        }
        while let Some(joined) = set.join_next().await {
            joined
                .expect("no write panicked")
                .expect("every household's evidence lands");
        }
        for i in 0..8 {
            assert_eq!(
                store
                    .control_event_count(&format!("site-{i}"))
                    .await
                    .unwrap(),
                1
            );
        }
    }
}
