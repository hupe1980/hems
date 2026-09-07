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
//! # `redb`, and every value a document
//!
//! A key-value store with ordered ranges, which is what this record always was:
//! it has **no joins**, its only aggregates are the backlog's counts, and every
//! ordering is on the natural key (D169). What SQL bought it was a way to lose a
//! field — `Z3V¼`/`Z3E¼` existed in `QuarterHour`, had no column, and were
//! dropped through four layers while everything compiled (D165). A
//! `TableDefinition` over a serde document cannot do that: the value *is* the
//! type, and adding a field to it cannot lose one.
//!
//! What `redb` does not give is a schema, so the refusal has to be explicit —
//! [`StoreError::NotReadable`] where a stored document is not one this build can
//! parse, named with the row rather than swallowed.
//!
//! # The outbox
//!
//! A box records **first** and forwards **second**. [`Store::pending_events`]
//! and [`Store::pending_quarter_hours`] are what the fleet has not acknowledged
//! and [`Store::mark_forwarded`] is the acknowledgement, so a box offline for a
//! week keeps its own two years and reconciles when it comes back. The other
//! order makes the WAN a dependency of the record, and the day a network
//! operator asks about is the day the link was down.
//!
//! "Not yet acknowledged" is a **set** rather than a scan: three small tables
//! holding the keys still owed, written in the same transaction as the row
//! itself, so they cannot come to disagree with it. Everything else is answered
//! from a key's own order — a Nachweis window from `EVENTS_BY_RECEIVED`, a
//! register range from the register table — and the two-year sweep walks the
//! events, of which two years holds a few thousand.
//!
//! Forwarded is not deleted: the two years are the household's, so [`Store::prune`]
//! follows the retention window and never an acknowledgement.
//!
//! # One household, one process
//!
//! The edge is a single daemon, so there is no `site_id` here: a key holding the
//! same value in every row is a join key for a join nobody makes. The site's
//! name belongs to the report that leaves the box, not to its own record.

use std::path::Path;

use hems_core::prelude::{Power, Slot};
use hems_grid::evidence::{ComplianceSample, ControlEvent};
use hems_grid::mispel::QuarterHour;
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use thiserror::Error;
use time::OffsetDateTime;

/// How long a § 14a control event is kept, `[A1 7.3]`.
pub const RETENTION: time::Duration = time::Duration::days(2 * 365);

/// The schema this build writes and understands.
///
/// `redb` has no schema of its own, so the revision is a row: a box downgraded
/// onto a file a newer build wrote is refused rather than left to interpret
/// documents it may not understand.
const SCHEMA: u64 = 1;

// ── The tables ──────────────────────────────────────────────────────────────
//
// Values are `serde_json` documents of the types this workspace already
// exchanges, so a field added to one of them travels without a schema change —
// which is the whole reason this is not a column list (D165, D169).

/// `"schema"`, and the two counters that hand out row identifiers.
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// The quarter-hour registers, keyed by the slot's start as Unix seconds.
///
/// An instant, not a local string: a household's own day boundary is
/// `metering`'s question, and a wall-clock key would ask it twice, differently,
/// twice a year. The key's order *is* the slot order, so a window and the
/// retention sweep are both range scans with no index behind them.
const REGISTERS: TableDefinition<i64, &[u8]> = TableDefinition::new("registers");

/// The registers the fleet has not acknowledged.
const REGISTERS_OWED: TableDefinition<i64, ()> = TableDefinition::new("registers_owed");

/// One § 14a control event, `[A1 7.2]`, keyed by the identifier an
/// acknowledgement names.
const EVENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("events");

/// `(received_at, id)`, so a Nachweis window is a range scan.
const EVENTS_BY_RECEIVED: TableDefinition<(i64, u64), ()> =
    TableDefinition::new("events_by_received");

/// The events the fleet has not acknowledged.
const EVENTS_OWED: TableDefinition<u64, ()> = TableDefinition::new("events_owed");

/// The minute-resolution trace, keyed `(event, instant)`.
///
/// A table of its own rather than a field on the event, so each fact has one
/// home and a trace of ten thousand samples is not re-parsed to answer "how
/// many". The composite key makes one event's trace a range scan, and deleting
/// it with the event an explicit drain — `redb` has no cascade, and an explicit
/// one is a line of code rather than a property somebody has to remember.
const SAMPLES: TableDefinition<(u64, i64), &[u8]> = TableDefinition::new("samples");

/// What the box has learned about its own house, one document per model.
const LEARNED: TableDefinition<&str, &[u8]> = TableDefinition::new("learned");

/// The box's EEBUS identity and trust store. One row, under `"self"`.
const IDENTITY: TableDefinition<&str, &[u8]> = TableDefinition::new("identity");

/// The failsafe a network operator wrote, per direction.
const FAILSAFE: TableDefinition<&str, &[u8]> = TableDefinition::new("failsafe");

/// The CloudEvents queued for the fleet.
const OUTBOUND: TableDefinition<u64, &[u8]> = TableDefinition::new("outbound");

/// The CloudEvents the fleet has not taken.
const OUTBOUND_OWED: TableDefinition<u64, ()> = TableDefinition::new("outbound_owed");

/// A CloudEvent's own id to the row that carries it, so a re-reported day
/// amends one message rather than queueing a second.
const OUTBOUND_BY_EVENT: TableDefinition<&str, u64> = TableDefinition::new("outbound_by_event");

/// Why the store could not answer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database itself.
    #[error("the box's store failed: {0}")]
    Sql(String),
    /// A value could not be turned into a document to store.
    #[error("the record could not be serialised: {detail}")]
    NotSerialisable {
        /// What `serde` said.
        detail: String,
    },
    /// A stored document is not one this build can read.
    ///
    /// Named with the row rather than swallowed: one unreadable event in two
    /// years of them is a fact an operator has to be told, not a gap in a
    /// Nachweis nobody can account for. It matters more without a schema than
    /// with one — nothing but this checks that a document still parses.
    #[error("the stored record {id} cannot be read by this build: {detail}")]
    NotReadable {
        /// Which row.
        id: i64,
        /// What `serde` said.
        detail: String,
    },
    /// A factory reset was asked for while the fleet has not taken everything.
    ///
    /// Refused rather than performed, because a reset is the one operation here
    /// that destroys `[A1 7.3]` evidence, and evidence that has not been
    /// forwarded exists **only** on this box. An installer resetting a box
    /// before it has drained its outbox would erase the record of a reduction a
    /// network operator can ask about for two years — and would find out two
    /// years later.
    ///
    /// [`Store::factory_reset_discarding_evidence`] is the way through for a box
    /// that will never see a WAN again, and it is named so that choosing it is a
    /// decision rather than a retry.
    #[error(
        "the fleet has not taken {events} control event(s), {quarter_hours} \
         quarter hour(s) and {outbound} report(s) yet; a factory reset now would \
         erase evidence that exists nowhere else"
    )]
    EvidenceNotForwarded {
        /// Control events still owed to the fleet.
        events: usize,
        /// Quarter-hour registers still owed.
        quarter_hours: usize,
        /// Day reports still owed.
        outbound: usize,
    },
    /// The database is at a revision this build does not know.
    ///
    /// A downgraded box. Two years of § 14a evidence is the last record in this
    /// workspace that should be repaired by guesswork.
    #[error("the store is at schema revision {found}, and this build understands {understood}")]
    FromTheFuture {
        /// What the file says.
        found: u64,
        /// The newest revision this build carries.
        understood: u64,
    },
}

/// Every `redb` error arrives as [`StoreError::Sql`], because a caller can do
/// nothing different about a transaction, a table and a commit.
macro_rules! sql {
    ($e:expr) => {
        ($e).map_err(|e| StoreError::Sql(e.to_string()))
    };
}

/// The box's EEBUS identity, as it is stored.
///
/// The SKI is derived from the key rather than stored beside it: two fields that
/// can disagree about one identity is one field too many, and the derivation is
/// `eebus::cert::ski_from_public_key`.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredIdentity {
    /// The SHIP ID the certificate carries as its common name.
    pub ship_id: String,
    /// The private key, PKCS#8 PEM.
    pub key_pem: String,
    /// The trusted peers, as `eebus::runtime::TrustStore`'s own JSON.
    pub trusted: String,
}

impl core::fmt::Debug for StoredIdentity {
    /// Never the key. It is the one value in this store whose leak would let
    /// another device be this household to a network operator, and a `Debug`
    /// that printed it would put it in the first log line somebody pastes into
    /// a bug report.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoredIdentity")
            .field("ship_id", &self.ship_id)
            .field("trusted", &self.trusted)
            .finish_non_exhaustive()
    }
}

/// A failsafe the network operator wrote, as it is stored.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredFailsafe {
    /// The power the household restrains itself to.
    pub watts: f64,
    /// How long it holds that for once it starts, seconds.
    pub minimum_s: i64,
}

/// What has not reached the fleet yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Backlog {
    /// Control events waiting.
    pub events: usize,
    /// Quarter hours waiting.
    pub quarter_hours: usize,
    /// CloudEvents waiting — day reports, so far.
    pub outbound: usize,
}

impl Backlog {
    /// Whether the box is up to date with the fleet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events == 0 && self.quarter_hours == 0 && self.outbound == 0
    }
}

/// One CloudEvent waiting for the fleet.
///
/// The **body** and not a signed request: a Standard Webhooks signature covers
/// the timestamp and a receiver refuses one older than five minutes, so a
/// signature made when the row was written is worthless by the time a box back
/// from an outage sends it. It is signed at each attempt, over these bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundEvent {
    /// The row identifier, which is what an acknowledgement names.
    pub id: i64,
    /// The CloudEvent id, which is also the `webhook-id`.
    pub event_id: String,
    /// The CloudEvents `type`, so a drain can route without parsing the body.
    pub event_type: String,
    /// The exact bytes the signature covers.
    pub body: Vec<u8>,
    /// How many attempts have been made.
    pub attempts: i64,
}

/// One quarter hour as the **box** records it: the MiSpeL registers a settlement
/// is computed from, and — beside them rather than inside them — what the roof
/// produced.
///
/// [`QuarterHour`] is the Festlegung's own register set and stays that. Its names
/// are `Z1NB¼`, `Z1NE¼`, `Z2V¼`, `Z2E¼`, and a Nachweis that renames its inputs
/// is one somebody has to translate before they can check it — and `Z2E¼` is the
/// **storage system and charge point's** generation, which is not the roof and is
/// not close to it. Read as the roof it makes a household running off its own sun
/// report a self-sufficiency of nought all summer, because its export exceeds its
/// battery's discharge and the subtraction floors at zero.
///
/// The absence of a production meter is `None` rather than zero: a box with none
/// has not measured a dark roof (D124).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Recorded {
    /// The registers a settlement is computed from.
    pub registers: QuarterHour,
    /// What the roof produced in this quarter hour, kWh — `None` where the box
    /// has no production measurement to read.
    ///
    /// A decimal **string** on the wire and on disk, like every other quantity
    /// a settlement is computed from: a JSON number is a `double` to every
    /// reader that has ever parsed one (P3).
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub production: Option<rust_decimal::Decimal>,
}

/// One event as it is held.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredEvent {
    /// The row identifier, which is what an acknowledgement names.
    pub id: i64,
    /// The event, with its compliance trace re-attached.
    pub event: ControlEvent,
}

/// A register row as it is stored: the record, and the two instants the store
/// itself owns.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RegisterRow {
    recorded: Recorded,
    recorded_at: i64,
    #[serde(default)]
    forwarded_at: Option<i64>,
}

/// An event row: the event **without** its trace, and what the store knows about
/// it that the event does not.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EventRow {
    event: ControlEvent,
    received_at: i64,
    expires_at: i64,
    #[serde(default)]
    forwarded_at: Option<i64>,
}

/// One sample of the trace.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct SampleRow {
    netzwirksam: f64,
    ceiling: f64,
}

/// A queued CloudEvent as it is stored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct OutboundRow {
    event_id: String,
    event_type: String,
    body: Vec<u8>,
    attempts: i64,
    created_at: i64,
    #[serde(default)]
    forwarded_at: Option<i64>,
    #[serde(default)]
    last_error: Option<String>,
}

fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(value).map_err(|e| StoreError::NotSerialisable {
        detail: e.to_string(),
    })
}

fn decode<T: serde::de::DeserializeOwned>(id: i64, bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::NotReadable {
        id,
        detail: e.to_string(),
    })
}

/// The box's own record.
pub struct Store {
    db: Database,
}

impl Store {
    /// Open — or create — the database at `path`, and bring the schema up.
    ///
    /// # Errors
    /// [`StoreError::Sql`], or [`StoreError::FromTheFuture`] for a file written
    /// by a newer build.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db = sql!(Database::create(path))?;
        Self::from_database(db)
    }

    /// A store in memory, for a test.
    ///
    /// # Errors
    /// As [`Store::open`].
    pub fn in_memory() -> Result<Self, StoreError> {
        let db =
            sql!(Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()))?;
        Self::from_database(db)
    }

    /// Create every table and settle the schema revision.
    ///
    /// The tables are opened in one write transaction whether they exist or
    /// not: `redb` creates on first open, and a store whose tables appeared
    /// lazily would answer "no such table" to a read on a fresh box rather than
    /// an empty result.
    fn from_database(db: Database) -> Result<Self, StoreError> {
        let store = Self { db };
        let write = sql!(store.db.begin_write())?;
        {
            sql!(write.open_table(REGISTERS))?;
            sql!(write.open_table(REGISTERS_OWED))?;
            sql!(write.open_table(EVENTS))?;
            sql!(write.open_table(EVENTS_BY_RECEIVED))?;
            sql!(write.open_table(EVENTS_OWED))?;
            sql!(write.open_table(SAMPLES))?;
            sql!(write.open_table(LEARNED))?;
            sql!(write.open_table(IDENTITY))?;
            sql!(write.open_table(FAILSAFE))?;
            sql!(write.open_table(OUTBOUND))?;
            sql!(write.open_table(OUTBOUND_OWED))?;
            sql!(write.open_table(OUTBOUND_BY_EVENT))?;

            let mut meta = sql!(write.open_table(META))?;
            let found = sql!(meta.get("schema"))?.map(|v| v.value());
            match found {
                Some(found) if found > SCHEMA => {
                    return Err(StoreError::FromTheFuture {
                        found,
                        understood: SCHEMA,
                    });
                }
                Some(_) => {}
                None => {
                    sql!(meta.insert("schema", SCHEMA))?;
                }
            }
        }
        sql!(write.commit())?;
        Ok(store)
    }

    /// The next identifier from a counter, inside the caller's transaction.
    fn next_id(meta: &mut redb::Table<'_, &str, u64>, counter: &str) -> Result<u64, StoreError> {
        let next = sql!(meta.get(counter))?.map_or(1, |v| v.value() + 1);
        sql!(meta.insert(counter, next))?;
        Ok(next)
    }

    /// The box's EEBUS identity and the peers it trusts, if it has been given
    /// one.
    ///
    /// # Errors
    /// [`StoreError`] where the read fails.
    pub fn eebus_identity(&self) -> Result<Option<StoredIdentity>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let table = sql!(read.open_table(IDENTITY))?;
        match sql!(table.get("self"))? {
            Some(bytes) => Ok(Some(decode(0, bytes.value())?)),
            None => Ok(None),
        }
    }

    /// Keep it. The SKI follows the key, so a box that regenerated one on every
    /// boot would be a different device to its network operator every morning.
    ///
    /// # Errors
    /// [`StoreError`] where the write fails.
    pub fn put_eebus_identity(
        &self,
        identity: &StoredIdentity,
        _now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let bytes = encode(identity)?;
        let write = sql!(self.db.begin_write())?;
        {
            let mut table = sql!(write.open_table(IDENTITY))?;
            sql!(table.insert("self", bytes.as_slice()))?;
        }
        sql!(write.commit())
    }

    /// The failsafe a network operator last wrote, for `direction`.
    ///
    /// `None` where no operator has ever written one, which is the ordinary
    /// case: the configured value stands until somebody changes it.
    ///
    /// # Errors
    /// [`StoreError`] where the read fails.
    pub fn eebus_failsafe(&self, direction: &str) -> Result<Option<StoredFailsafe>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let table = sql!(read.open_table(FAILSAFE))?;
        match sql!(table.get(direction))? {
            Some(bytes) => Ok(Some(decode(0, bytes.value())?)),
            None => Ok(None),
        }
    }

    /// Keep it, so the next power cut does not undo the operator's write.
    ///
    /// # Errors
    /// [`StoreError`] where the write fails.
    pub fn put_eebus_failsafe(
        &self,
        direction: &str,
        failsafe: &StoredFailsafe,
        _now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let bytes = encode(failsafe)?;
        let write = sql!(self.db.begin_write())?;
        {
            let mut table = sql!(write.open_table(FAILSAFE))?;
            sql!(table.insert(direction, bytes.as_slice()))?;
        }
        sql!(write.commit())
    }

    /// Keep what the box has learned about its own house.
    ///
    /// Overwrites: there is one current model per name, and a history of a
    /// forecast's own past states is not something anybody asks a box for.
    ///
    /// # Errors
    /// [`StoreError`] where the write fails, or where the model cannot be
    /// serialised — which would be a defect in `hems-forecast` rather than a
    /// runtime condition, and is worth an error rather than a silent skip
    /// because a box that quietly stopped remembering its roof would look
    /// exactly like one that had just been installed.
    pub fn put_learned<T: serde::Serialize>(
        &self,
        name: &str,
        model: &T,
        _now: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let bytes = encode(model)?;
        let write = sql!(self.db.begin_write())?;
        {
            let mut table = sql!(write.open_table(LEARNED))?;
            sql!(table.insert(name, bytes.as_slice()))?;
        }
        sql!(write.commit())
    }

    /// Read one back.
    ///
    /// `None` where the box has never stored one. A document that no longer
    /// parses is an **error** rather than a `None`: a box that silently forgot
    /// its roof because the model's shape moved would look exactly like one
    /// that had just been installed, and would spend a fortnight relearning
    /// what it already knew.
    ///
    /// # Errors
    /// [`StoreError`] where the read fails or the document has moved on.
    pub fn learned<T: serde::de::DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let table = sql!(read.open_table(LEARNED))?;
        match sql!(table.get(name))? {
            Some(bytes) => Ok(Some(decode(0, bytes.value())?)),
            None => Ok(None),
        }
    }

    /// Write one quarter hour's registers.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn put_quarter_hour(
        &self,
        quarter: &Recorded,
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.put_quarter_hours(std::slice::from_ref(quarter), recorded_at)
    }

    /// Write many in one transaction.
    ///
    /// A day is one *fact*: a settlement that can observe half of it is a
    /// settlement that can be run on half a day. A restated register is owed to
    /// the fleet again — it is a different number from the one they were given.
    ///
    /// # Errors
    /// [`StoreError`]. Nothing is written if any row fails.
    pub fn put_quarter_hours(
        &self,
        quarters: &[Recorded],
        recorded_at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        if quarters.is_empty() {
            return Ok(());
        }
        let write = sql!(self.db.begin_write())?;
        {
            let mut table = sql!(write.open_table(REGISTERS))?;
            let mut owed = sql!(write.open_table(REGISTERS_OWED))?;
            for quarter in quarters {
                let slot = quarter.registers.slot.start().unix_timestamp();
                let row = RegisterRow {
                    recorded: *quarter,
                    recorded_at: recorded_at.unix_timestamp(),
                    forwarded_at: None,
                };
                sql!(table.insert(slot, encode(&row)?.as_slice()))?;
                sql!(owed.insert(slot, ()))?;
            }
        }
        sql!(write.commit())
    }

    /// Write a control event and its trace, and return the identifier an
    /// acknowledgement names.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn put_control_event(&mut self, event: &ControlEvent) -> Result<i64, StoreError> {
        // Two years from the day it *closed*, not from the day it arrived: an
        // event that ran for a week is documented for two years after it ended.
        let expires_at = event.released_at.unwrap_or(event.received_at) + RETENTION;
        let received_at = event.received_at.unix_timestamp();
        let mut document = event.clone();
        document.samples.clear();

        let write = sql!(self.db.begin_write())?;
        let id;
        {
            let mut meta = sql!(write.open_table(META))?;
            id = Self::next_id(&mut meta, "next_event")?;
            let row = EventRow {
                event: document,
                received_at,
                expires_at: expires_at.unix_timestamp(),
                forwarded_at: None,
            };
            let mut events = sql!(write.open_table(EVENTS))?;
            sql!(events.insert(id, encode(&row)?.as_slice()))?;
            let mut by_received = sql!(write.open_table(EVENTS_BY_RECEIVED))?;
            sql!(by_received.insert((received_at, id), ()))?;
            let mut owed = sql!(write.open_table(EVENTS_OWED))?;
            sql!(owed.insert(id, ()))?;
            let mut samples = sql!(write.open_table(SAMPLES))?;
            for s in &event.samples {
                let row = SampleRow {
                    netzwirksam: s.netzwirksam.get(),
                    ceiling: s.ceiling.get(),
                };
                sql!(samples.insert((id, s.at.unix_timestamp()), encode(&row)?.as_slice()))?;
            }
        }
        sql!(write.commit())?;
        Ok(i64::try_from(id).unwrap_or(i64::MAX))
    }

    /// Every control event on record, oldest first, with its trace re-attached.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn control_events(&self) -> Result<Vec<StoredEvent>, StoreError> {
        self.events_in(i64::MIN, i64::MAX, usize::MAX, false)
    }

    /// The events the fleet has not acknowledged, oldest first, at most `limit`.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn pending_events(&self, limit: usize) -> Result<Vec<StoredEvent>, StoreError> {
        self.events_in(i64::MIN, i64::MAX, limit, true)
    }

    /// The events received in `[from, to)`, oldest first.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn control_events_between(
        &self,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        self.events_in(
            from.unix_timestamp(),
            to.unix_timestamp(),
            usize::MAX,
            false,
        )
    }

    /// The events whose `received_at` falls in `[from, to)`, in that order.
    ///
    /// Ordered by `EVENTS_BY_RECEIVED` rather than by the primary key, because
    /// the identifier is a counter and two events can arrive in an order the
    /// counter does not reflect once a clock has been corrected.
    fn events_in(
        &self,
        from: i64,
        to: i64,
        limit: usize,
        owed_only: bool,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let by_received = sql!(read.open_table(EVENTS_BY_RECEIVED))?;
        let events = sql!(read.open_table(EVENTS))?;
        let owed = sql!(read.open_table(EVENTS_OWED))?;
        let samples = sql!(read.open_table(SAMPLES))?;

        let mut out = Vec::new();
        for entry in sql!(by_received.range((from, u64::MIN)..(to, u64::MIN)))? {
            let (key, _) = sql!(entry)?;
            let (_, id) = key.value();
            if owed_only && sql!(owed.get(id))?.is_none() {
                continue;
            }
            let Some(bytes) = sql!(events.get(id))? else {
                continue;
            };
            let signed = i64::try_from(id).unwrap_or(i64::MAX);
            let row: EventRow = decode(signed, bytes.value())?;
            let mut event = row.event;
            for sample in sql!(samples.range((id, i64::MIN)..(id, i64::MAX)))? {
                let (key, value) = sql!(sample)?;
                let (_, at) = key.value();
                let s: SampleRow = decode(signed, value.value())?;
                event.samples.push(ComplianceSample {
                    at: OffsetDateTime::from_unix_timestamp(at)
                        .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                    netzwirksam: Power::new(s.netzwirksam),
                    ceiling: Power::new(s.ceiling),
                });
            }
            out.push(StoredEvent { id: signed, event });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// The registers the fleet has not acknowledged, oldest first, at most
    /// `limit`.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn pending_quarter_hours(&self, limit: usize) -> Result<Vec<Recorded>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let owed = sql!(read.open_table(REGISTERS_OWED))?;
        let registers = sql!(read.open_table(REGISTERS))?;
        let mut out = Vec::new();
        for entry in sql!(owed.iter())? {
            let (slot, _) = sql!(entry)?;
            let Some(bytes) = sql!(registers.get(slot.value()))? else {
                continue;
            };
            let row: RegisterRow = decode(slot.value(), bytes.value())?;
            out.push(row.recorded);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Every quarter hour on record, oldest first.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn quarter_hours(&self) -> Result<Vec<Recorded>, StoreError> {
        self.registers_in(i64::MIN, i64::MAX)
    }

    /// The quarter hours whose slot starts in `[from, to)`, oldest first.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn quarter_hours_between(
        &self,
        from: OffsetDateTime,
        to: OffsetDateTime,
    ) -> Result<Vec<Recorded>, StoreError> {
        self.registers_in(from.unix_timestamp(), to.unix_timestamp())
    }

    fn registers_in(&self, from: i64, to: i64) -> Result<Vec<Recorded>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let registers = sql!(read.open_table(REGISTERS))?;
        let mut out = Vec::new();
        for entry in sql!(registers.range(from..to))? {
            let (slot, bytes) = sql!(entry)?;
            let row: RegisterRow = decode(slot.value(), bytes.value())?;
            out.push(row.recorded);
        }
        Ok(out)
    }

    /// Queue a CloudEvent for the fleet.
    ///
    /// Keyed on the CloudEvent's own id, which is derived from what the report
    /// is *about* — the site and the day — so a box re-reporting a day it has
    /// corrected is amending one message rather than sending a second. The
    /// attempt count resets with the body: what was stuck was the old document.
    ///
    /// Returns the row identifier, which is what an acknowledgement names. It is
    /// stable across a re-queue of the same `event_id`, because the row is.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn queue_event(
        &mut self,
        event_id: &str,
        event_type: &str,
        body: &[u8],
        at: OffsetDateTime,
    ) -> Result<i64, StoreError> {
        let write = sql!(self.db.begin_write())?;
        let id;
        {
            let mut by_event = sql!(write.open_table(OUTBOUND_BY_EVENT))?;
            let existing = sql!(by_event.get(event_id))?.map(|found| found.value());
            id = if let Some(found) = existing {
                found
            } else {
                let mut meta = sql!(write.open_table(META))?;
                let fresh = Self::next_id(&mut meta, "next_outbound")?;
                sql!(by_event.insert(event_id, fresh))?;
                fresh
            };
            let row = OutboundRow {
                event_id: event_id.to_owned(),
                event_type: event_type.to_owned(),
                body: body.to_vec(),
                attempts: 0,
                created_at: at.unix_timestamp(),
                forwarded_at: None,
                last_error: None,
            };
            let mut outbound = sql!(write.open_table(OUTBOUND))?;
            sql!(outbound.insert(id, encode(&row)?.as_slice()))?;
            let mut owed = sql!(write.open_table(OUTBOUND_OWED))?;
            sql!(owed.insert(id, ()))?;
        }
        sql!(write.commit())?;
        Ok(i64::try_from(id).unwrap_or(i64::MAX))
    }

    /// The CloudEvents the fleet has not taken, oldest first, at most `limit`.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn pending_outbound(&self, limit: usize) -> Result<Vec<OutboundEvent>, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let owed = sql!(read.open_table(OUTBOUND_OWED))?;
        let outbound = sql!(read.open_table(OUTBOUND))?;
        let mut rows: Vec<(i64, u64, OutboundRow)> = Vec::new();
        for entry in sql!(owed.iter())? {
            let (id, _) = sql!(entry)?;
            let id = id.value();
            let Some(bytes) = sql!(outbound.get(id))? else {
                continue;
            };
            let signed = i64::try_from(id).unwrap_or(i64::MAX);
            let row: OutboundRow = decode(signed, bytes.value())?;
            rows.push((row.created_at, id, row));
        }
        // Oldest first, by the instant the row was queued and then by its
        // identifier — the order the outbox drained in before, and the one that
        // keeps a re-reported day in the place its first attempt had.
        rows.sort_by_key(|(created, id, _)| (*created, *id));
        Ok(rows
            .into_iter()
            .take(limit)
            .map(|(_, id, row)| OutboundEvent {
                id: i64::try_from(id).unwrap_or(i64::MAX),
                event_id: row.event_id,
                event_type: row.event_type,
                body: row.body,
                attempts: row.attempts,
            })
            .collect())
    }

    /// Record that the fleet has taken these CloudEvents.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn mark_sent(&mut self, ids: &[i64], at: OffsetDateTime) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        self.close_outbound(ids, at, None)
    }

    /// Record that an attempt on this row failed, and why.
    ///
    /// On the row rather than only in a log: "which of my reports is stuck, and
    /// on what" is asked days after the log line has rotated away.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn mark_attempted(&mut self, id: i64, error: &str) -> Result<(), StoreError> {
        let write = sql!(self.db.begin_write())?;
        {
            let mut outbound = sql!(write.open_table(OUTBOUND))?;
            let key = u64::try_from(id).unwrap_or(0);
            let existing = sql!(outbound.get(key))?.map(|b| b.value().to_vec());
            if let Some(bytes) = existing {
                let mut row: OutboundRow = decode(id, &bytes)?;
                row.attempts += 1;
                row.last_error = Some(error.to_owned());
                sql!(outbound.insert(key, encode(&row)?.as_slice()))?;
            }
        }
        sql!(write.commit())
    }

    /// Give up on a row the fleet will never take.
    ///
    /// A permanent refusal — a `4xx` that is not a rate limit — means `obsd`
    /// has read the document and will not have it. Retrying is a box asking the
    /// same rejected question every five minutes for ever, so the row is marked
    /// forwarded with the refusal on it: out of the backlog, still on record,
    /// and visible to anybody asking what happened to that day.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn abandon_event(
        &mut self,
        id: i64,
        error: &str,
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        self.close_outbound(&[id], at, Some(error))
    }

    /// Take rows out of the outbox, optionally recording why.
    fn close_outbound(
        &mut self,
        ids: &[i64],
        at: OffsetDateTime,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let write = sql!(self.db.begin_write())?;
        {
            let mut outbound = sql!(write.open_table(OUTBOUND))?;
            let mut owed = sql!(write.open_table(OUTBOUND_OWED))?;
            for id in ids {
                let key = u64::try_from(*id).unwrap_or(0);
                let existing = sql!(outbound.get(key))?.map(|b| b.value().to_vec());
                if let Some(bytes) = existing {
                    let mut row: OutboundRow = decode(*id, &bytes)?;
                    row.forwarded_at = Some(at.unix_timestamp());
                    if let Some(error) = error {
                        // The attempt that was refused still happened, and
                        // "which of my reports is stuck, and on what" is asked
                        // days after the log line has rotated away.
                        row.attempts += 1;
                        row.last_error = Some(error.to_owned());
                    }
                    sql!(outbound.insert(key, encode(&row)?.as_slice()))?;
                }
                sql!(owed.remove(key))?;
            }
        }
        sql!(write.commit())
    }

    /// Record that the fleet has taken these events and registers.
    ///
    /// One transaction over both, because a partial acknowledgement is a claim
    /// about rows nobody can point at.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn mark_forwarded(
        &mut self,
        events: &[i64],
        slots: &[Slot],
        at: OffsetDateTime,
    ) -> Result<(), StoreError> {
        let write = sql!(self.db.begin_write())?;
        {
            let mut event_rows = sql!(write.open_table(EVENTS))?;
            let mut events_owed = sql!(write.open_table(EVENTS_OWED))?;
            for id in events {
                let key = u64::try_from(*id).unwrap_or(0);
                let existing = sql!(event_rows.get(key))?.map(|b| b.value().to_vec());
                if let Some(bytes) = existing {
                    let mut row: EventRow = decode(*id, &bytes)?;
                    row.forwarded_at = Some(at.unix_timestamp());
                    sql!(event_rows.insert(key, encode(&row)?.as_slice()))?;
                }
                sql!(events_owed.remove(key))?;
            }
            let mut register_rows = sql!(write.open_table(REGISTERS))?;
            let mut registers_owed = sql!(write.open_table(REGISTERS_OWED))?;
            for slot in slots {
                let key = slot.start().unix_timestamp();
                let existing = sql!(register_rows.get(key))?.map(|b| b.value().to_vec());
                if let Some(bytes) = existing {
                    let mut row: RegisterRow = decode(key, &bytes)?;
                    row.forwarded_at = Some(at.unix_timestamp());
                    sql!(register_rows.insert(key, encode(&row)?.as_slice()))?;
                }
                sql!(registers_owed.remove(key))?;
            }
        }
        sql!(write.commit())
    }

    /// What the fleet has not taken.
    ///
    /// Three counts of three small sets rather than three scans of the record:
    /// the sets hold exactly what is owed, so this is bounded by the backlog and
    /// not by two years.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn backlog(&self) -> Result<Backlog, StoreError> {
        let read = sql!(self.db.begin_read())?;
        let count = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
        Ok(Backlog {
            events: count(sql!(sql!(read.open_table(EVENTS_OWED))?.len())?),
            quarter_hours: count(sql!(sql!(read.open_table(REGISTERS_OWED))?.len())?),
            outbound: count(sql!(sql!(read.open_table(OUTBOUND_OWED))?.len())?),
        })
    }

    /// Put the box back the way it left the factory.
    ///
    /// Two callers want the same operation. `ATC_LPC_COM_PT_CSInit_002` and its
    /// LPP twin are a device-level conformance case — reset the Controllable
    /// System, read its parameters back, and the limit must come back
    /// **inactive** with the failsafe at what the parameter sheet declares — and
    /// RED/EN 18031 asks the same question from the security side, because a
    /// device that cannot be returned to a known-good state cannot be safely
    /// resold. The stake is specific here: the identity in this store is what
    /// lets a network operator's Steuerbox reduce *this* house, so a box that
    /// changed hands carrying it would leave the previous household's operator
    /// controlling the new one.
    ///
    /// It clears **every table** in one transaction, because a half-reset box
    /// has lost its evidence and kept an identity somebody still trusts. The
    /// schema row is written again afterwards, so a reset box is a commissioned
    /// box with nothing in it rather than one with no database; and
    /// [`Store::eebus_failsafe`] answering `None` is what makes `hemsd`'s
    /// `failsafe_in_force` fall back to the configured value.
    ///
    /// It **refuses** while the fleet is owed anything: unforwarded evidence
    /// exists only here. See [`StoreError::EvidenceNotForwarded`].
    ///
    /// # Errors
    /// [`StoreError::EvidenceNotForwarded`] where the fleet is still owed
    /// anything, or [`StoreError::Sql`].
    pub fn factory_reset(&mut self) -> Result<(), StoreError> {
        let owed = self.backlog()?;
        if !owed.is_empty() {
            return Err(StoreError::EvidenceNotForwarded {
                events: owed.events,
                quarter_hours: owed.quarter_hours,
                outbound: owed.outbound,
            });
        }
        self.wipe()
    }

    /// The same, for a box that will never reach the fleet again.
    ///
    /// Named for what it costs rather than for what it does, because the two
    /// callers are an installer decommissioning a dead site and an installer who
    /// has not waited for the outbox to drain, and only the first of those
    /// should find this function comfortable to type.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn factory_reset_discarding_evidence(&mut self) -> Result<(), StoreError> {
        self.wipe()
    }

    /// Empty every table in one transaction, and put the schema row back.
    fn wipe(&mut self) -> Result<(), StoreError> {
        let write = sql!(self.db.begin_write())?;
        {
            sql!(sql!(write.open_table(REGISTERS))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(REGISTERS_OWED))?.retain(|_, ()| false))?;
            sql!(sql!(write.open_table(EVENTS))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(EVENTS_BY_RECEIVED))?.retain(|_, ()| false))?;
            sql!(sql!(write.open_table(EVENTS_OWED))?.retain(|_, ()| false))?;
            sql!(sql!(write.open_table(SAMPLES))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(LEARNED))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(IDENTITY))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(FAILSAFE))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(OUTBOUND))?.retain(|_, _| false))?;
            sql!(sql!(write.open_table(OUTBOUND_OWED))?.retain(|_, ()| false))?;
            sql!(sql!(write.open_table(OUTBOUND_BY_EVENT))?.retain(|_, _| false))?;
            // The counters go with it — a reset box hands out identifiers from
            // one again, like the box it now is — and the schema row stays, so
            // the next open runs no migration.
            let mut meta = sql!(write.open_table(META))?;
            sql!(meta.retain(|_, _| false))?;
            sql!(meta.insert("schema", SCHEMA))?;
        }
        sql!(write.commit())
    }

    /// How many registers one sweep transaction may delete.
    ///
    /// A **bound on the transaction**, not on the work: the loop keeps going
    /// until nothing older is left, committing between batches.
    ///
    /// `redb` is copy-on-write, so a delete holds the old pages until its
    /// transaction commits — and one enormous transaction has to hold all of
    /// them at once. Measured: two years of registers occupy 56,6 MB, thirty
    /// daily sweeps leave it at 56,6 MB, and deleting a *year* in one
    /// transaction takes the file to **793 MB**. The daily case is the ordinary
    /// one and was never in danger; the year is a box that has been off — a
    /// holiday home, a unit back from repair — whose first sweep after it comes
    /// back would otherwise try to clear the whole backlog at once, on a
    /// gateway's flash.
    const SWEEP_BATCH: usize = 4 * 96;

    /// Delete registers older than `oldest`, a bounded batch at a time.
    fn sweep_registers(&self, oldest: i64) -> Result<(), StoreError> {
        loop {
            let doomed: Vec<i64> = {
                let read = sql!(self.db.begin_read())?;
                let registers = sql!(read.open_table(REGISTERS))?;
                let mut out = Vec::new();
                for entry in sql!(registers.range(i64::MIN..=oldest))? {
                    let (slot, _) = sql!(entry)?;
                    out.push(slot.value());
                    if out.len() >= Self::SWEEP_BATCH {
                        break;
                    }
                }
                out
            };
            if doomed.is_empty() {
                return Ok(());
            }
            let write = sql!(self.db.begin_write())?;
            {
                let mut registers = sql!(write.open_table(REGISTERS))?;
                let mut owed = sql!(write.open_table(REGISTERS_OWED))?;
                for slot in &doomed {
                    sql!(registers.remove(*slot))?;
                    sql!(owed.remove(*slot))?;
                }
            }
            sql!(write.commit())?;
        }
    }

    /// Delete every event whose two years are up, and the registers older than
    /// the same window.
    ///
    /// Returns how many events went. Their traces go with them explicitly:
    /// `redb` has no cascade, and a trace whose event has been deleted is a set
    /// of numbers nobody can interpret.
    ///
    /// The events are **scanned** rather than indexed by expiry. Two years holds
    /// a few thousand of them, this runs once a day, and an index on a value
    /// that is `released_at + two years` — not monotonic in the key, because an
    /// event that ran a week expires later than one that arrived after it —
    /// would be a second thing to keep in step for no measurable gain.
    ///
    /// # Errors
    /// [`StoreError`].
    pub fn prune(&self, now: OffsetDateTime) -> Result<usize, StoreError> {
        let cutoff = now.unix_timestamp();
        let expired: Vec<(u64, i64)> = {
            let read = sql!(self.db.begin_read())?;
            let events = sql!(read.open_table(EVENTS))?;
            let mut out = Vec::new();
            for entry in sql!(events.iter())? {
                let (id, bytes) = sql!(entry)?;
                let id = id.value();
                let row: EventRow = decode(i64::try_from(id).unwrap_or(i64::MAX), bytes.value())?;
                if row.expires_at <= cutoff {
                    out.push((id, row.received_at));
                }
            }
            out
        };

        let write = sql!(self.db.begin_write())?;
        {
            let mut events = sql!(write.open_table(EVENTS))?;
            let mut by_received = sql!(write.open_table(EVENTS_BY_RECEIVED))?;
            let mut owed = sql!(write.open_table(EVENTS_OWED))?;
            let mut samples = sql!(write.open_table(SAMPLES))?;
            for (id, received_at) in &expired {
                sql!(events.remove(*id))?;
                sql!(by_received.remove((*received_at, *id)))?;
                sql!(owed.remove(*id))?;
                sql!(samples.retain_in((*id, i64::MIN)..(*id, i64::MAX), |_, _| false))?;
            }
        }
        sql!(write.commit())?;

        self.sweep_registers((now - RETENTION).unix_timestamp())?;
        Ok(expired.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::prelude::{AssetId, GuardRule};
    use hems_grid::evidence::Action;
    use hems_grid::para14a::ControlMode;
    use rust_decimal::Decimal;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

    /// A file of its own. `redb` takes an exclusive lock on its path, and
    /// `cargo test` runs binaries in parallel, so a shared name would be one
    /// test failing on another.
    fn temp_path(what: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hems-box-{what}-{}-{n}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn quarter(at: OffsetDateTime) -> Recorded {
        Recorded {
            registers: QuarterHour {
                grid_draw: Decimal::new(1_234_567, 6),
                ..QuarterHour::empty(Slot::containing(at))
            },
            production: Some(Decimal::new(2, 1)),
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
            store.quarter_hours().unwrap()[0].registers.grid_draw,
            Decimal::new(1_234_567, 6)
        );
    }

    #[test]
    fn every_register_survives_the_round_trip_and_not_only_the_ones_asked_for() {
        // `Z3V¼`/`Z3E¼` had no column here, so the box could measure its
        // battery, write the quarter hour, read it back and hand the fleet a
        // pair of nulls — and a household declared Basisfall A4 was refused a
        // settlement for want of registers its own box had taken. A test that
        // asserts one field cannot see that; this one asserts the whole record.
        let store = Store::in_memory().unwrap();
        let written = Recorded {
            registers: QuarterHour {
                storage_consumption: Some(Decimal::new(2_000, 3)),
                storage_generation: Some(Decimal::new(1_700, 3)),
                ..quarter(NOW).registers
            },
            production: Some(Decimal::new(2, 1)),
        };
        store.put_quarter_hour(&written, NOW).unwrap();
        assert_eq!(store.quarter_hours().unwrap(), vec![written]);
    }

    #[test]
    fn a_box_with_no_battery_meter_records_no_storage_register() {
        // The null is the load-bearing half: it means "not separately metered",
        // and `hems_grid::mispel` refuses Basisfall A4 on it. A zero would be a
        // settlement claiming the battery stood still.
        let store = Store::in_memory().unwrap();
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        let read = store.quarter_hours().unwrap();
        assert_eq!(read[0].registers.storage_consumption, None);
        assert_eq!(read[0].registers.storage_generation, None);
    }

    #[test]
    fn a_factory_reset_leaves_a_box_that_looks_like_it_left_the_factory() {
        // `ATC_LPC_COM_PT_CSInit_002`: reset the Controllable System and its
        // parameters have to come back as the sheet declares them. `None` here
        // is exactly that — `failsafe_in_force` then falls back to the
        // configured value — and the identity has to be gone too, or a resold
        // box would still answer to the previous household's network operator.
        let mut store = Store::in_memory().unwrap();
        store
            .put_eebus_failsafe(
                "consumption",
                &StoredFailsafe {
                    watts: 4_200.0,
                    minimum_s: 7_200,
                },
                NOW,
            )
            .unwrap();
        store
            .put_eebus_identity(
                &StoredIdentity {
                    ship_id: "hems_test".into(),
                    key_pem: "-----BEGIN PRIVATE KEY-----".into(),
                    trusted: "[]".into(),
                },
                NOW,
            )
            .unwrap();
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        store.put_control_event(&event(NOW)).unwrap();
        // Everything the fleet is owed, taken — a reset refuses otherwise.
        let slots: Vec<Slot> = store
            .quarter_hours()
            .unwrap()
            .iter()
            .map(|r| r.registers.slot)
            .collect();
        let ids: Vec<i64> = store
            .control_events()
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        store.mark_forwarded(&ids, &slots, NOW).unwrap();

        store.factory_reset().expect("a drained box resets");

        assert_eq!(store.eebus_failsafe("consumption").unwrap(), None);
        assert_eq!(store.eebus_identity().unwrap(), None);
        assert!(store.control_events().unwrap().is_empty());
        assert!(store.quarter_hours().unwrap().is_empty());
        assert!(store.backlog().unwrap().is_empty());
        // …and it is a box with an empty database rather than one with no
        // database: the next write must not need a migration.
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        assert_eq!(store.quarter_hours().unwrap().len(), 1);
    }

    #[test]
    fn a_reset_is_refused_while_the_fleet_is_still_owed_evidence() {
        // The one operation here that destroys `[A1 7.3]` evidence, and
        // unforwarded evidence exists nowhere else. An installer who resets a
        // box before its outbox has drained erases the record of a reduction a
        // network operator may ask about for two years, and finds out two years
        // later — so this refuses, and says what is owed.
        let mut store = Store::in_memory().unwrap();
        store.put_control_event(&event(NOW)).unwrap();

        let refused = store.factory_reset().expect_err("evidence is still owed");
        assert!(
            matches!(refused, StoreError::EvidenceNotForwarded { events: 1, .. }),
            "{refused}"
        );
        assert!(
            !store.control_events().unwrap().is_empty(),
            "and a refusal has to leave the record alone"
        );

        // The way through is named for what it costs, and is a decision rather
        // than a retry.
        store
            .factory_reset_discarding_evidence()
            .expect("a box that will never see a WAN again");
        assert!(store.control_events().unwrap().is_empty());
    }

    #[test]
    fn a_backlog_larger_than_one_sweep_is_cleared_all_the_same() {
        // The sweep bounds its **transaction**, not its work: `redb` is
        // copy-on-write, so a delete holds the old pages until it commits and
        // one enormous transaction has to hold all of them at once. Measured on
        // two years of registers (56,6 MB): thirty daily sweeps leave it at
        // 56,6 MB, and deleting a year in **one** transaction took the file to
        // 793 MB — on a gateway's flash. The daily case was never in danger;
        // the year is a box that has been off and comes back.
        //
        // Bounding it introduces the obvious bug — a loop that clears one batch
        // and stops — so this writes more than a batch and asserts the lot goes.
        let store = Store::in_memory().unwrap();
        let old = NOW - RETENTION - time::Duration::days(30);
        let count = Store::SWEEP_BATCH + 50;
        let backlog: Vec<Recorded> = (0..count)
            .map(|i| {
                let at = old + time::Duration::minutes(15 * i64::try_from(i).unwrap_or(0));
                Recorded {
                    registers: QuarterHour::empty(Slot::containing(at)),
                    production: None,
                }
            })
            .collect();
        store.put_quarter_hours(&backlog, old).unwrap();
        // …and one inside the window, which must survive.
        store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        assert_eq!(store.quarter_hours().unwrap().len(), count + 1);

        store.prune(NOW).unwrap();

        let left = store.quarter_hours().unwrap();
        assert_eq!(
            left.len(),
            1,
            "a backlog of {count} has to clear in one call, not one batch of it"
        );
        assert_eq!(left[0].registers.slot, Slot::containing(NOW));
    }

    #[test]
    fn a_whole_day_goes_in_one_call() {
        // Ninety-six rows, one statement each, one commit for the lot. The
        // *atomicity* is structural — `transaction()` and one `commit()` — and
        // is deliberately not asserted here: there is no way to make this batch
        // fail part-way from outside the store, and a test that cannot fail is
        // not a test. What this pins is that every row arrives.
        let store = Store::in_memory().unwrap();
        let day: Vec<Recorded> = (0..96)
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
        let batched = Store::in_memory().unwrap();
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
                quarter_hours: 1,
                outbound: 0,
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
            .map(|q| q.registers.slot)
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
            .map(|q| q.registers.slot)
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
        restated.registers.grid_draw = Decimal::new(9_999_999, 6);
        store.put_quarter_hour(&restated, NOW).unwrap();
        assert_eq!(store.backlog().unwrap().quarter_hours, 1);
        assert_eq!(
            store.quarter_hours().unwrap()[0].registers.grid_draw,
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
        // The trace went with it. `redb` has no cascade, so this is the
        // assertion that the explicit drain in `prune` actually ran — a trace
        // whose event has been deleted is a set of numbers nobody can interpret,
        // and it would sit there for ever.
        let read = store.db.begin_read().unwrap();
        let samples = read.open_table(SAMPLES).unwrap();
        assert_eq!(
            samples.len().unwrap(),
            0,
            "the trace is drained with its event"
        );
    }

    #[test]
    fn a_store_written_by_a_newer_build_is_refused_rather_than_used() {
        // `redb` has no schema of its own, so the revision is a row — and this
        // is what makes a downgraded box refuse the file rather than interpret
        // documents it may not understand.
        let path = temp_path("from-the-future");
        {
            let store = Store::open(&path).unwrap();
            let write = store.db.begin_write().unwrap();
            {
                let mut meta = write.open_table(META).unwrap();
                meta.insert("schema", 9999_u64).unwrap();
            }
            write.commit().unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::FromTheFuture {
                found: 9999,
                understood: 1
            })
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reopened_store_still_has_everything_and_re_runs_no_migration() {
        let path = temp_path("reopen");
        {
            let mut store = Store::open(&path).unwrap();
            store.put_control_event(&event(NOW)).unwrap();
            store.put_quarter_hour(&quarter(NOW), NOW).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.control_events().unwrap().len(), 1);
        assert_eq!(store.quarter_hours().unwrap().len(), 1);
        let read = store.db.begin_read().unwrap();
        let meta = read.open_table(META).unwrap();
        assert_eq!(
            meta.get("schema").unwrap().unwrap().value(),
            SCHEMA,
            "the revision is what it was, so the next open runs no migration"
        );
        drop(read);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod outbound_tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 08:00:00 UTC);

    #[test]
    fn a_re_reported_day_amends_one_message_rather_than_queueing_two() {
        // `hemsd` derives the CloudEvent id from the site and the date, so a box
        // that recomputes yesterday and sends it again is *correcting* one
        // report. Two rows with one id would send the same day twice, and
        // `obsd` — which replaces by date — would take the older one second.
        let mut store = Store::in_memory().unwrap();
        store
            .queue_event(
                "haus-1:2026-01-14",
                "de.hems.site.day.reported",
                b"first",
                NOW,
            )
            .unwrap();
        store.mark_attempted(1, "HTTP 503").unwrap();
        store
            .queue_event(
                "haus-1:2026-01-14",
                "de.hems.site.day.reported",
                b"corrected",
                NOW + time::Duration::hours(1),
            )
            .unwrap();

        let pending = store.pending_outbound(10).unwrap();
        assert_eq!(pending.len(), 1, "one day, one message");
        assert_eq!(pending[0].body, b"corrected");
        assert_eq!(
            pending[0].attempts, 0,
            "and the attempt count resets with the body — what was stuck was \
             the document that has just been replaced"
        );
    }

    #[test]
    fn a_queued_day_survives_until_the_fleet_takes_it() {
        let mut store = Store::in_memory().unwrap();
        store
            .queue_event("haus-1:2026-01-14", "de.hems.site.day.reported", b"{}", NOW)
            .unwrap();
        assert_eq!(store.backlog().unwrap().outbound, 1);

        store.mark_attempted(1, "connection refused").unwrap();
        assert_eq!(
            store.pending_outbound(10).unwrap()[0].attempts,
            1,
            "a failed attempt is counted and the day is still queued"
        );

        store
            .mark_sent(&[1], NOW + time::Duration::hours(2))
            .unwrap();
        assert!(store.pending_outbound(10).unwrap().is_empty());
        assert_eq!(store.backlog().unwrap().outbound, 0);
    }

    #[test]
    fn a_refusal_leaves_the_backlog_without_leaving_the_record() {
        // A `4xx` that is not a rate limit is `obsd` having read the document
        // and refused it. Retrying is a box asking the same rejected question
        // every five minutes for ever; deleting the row is a day that
        // disappeared with no account of why.
        let mut store = Store::in_memory().unwrap();
        store
            .queue_event("haus-1:2026-01-14", "de.hems.site.day.reported", b"{}", NOW)
            .unwrap();
        store.abandon_event(1, "HTTP 400", NOW).unwrap();

        assert!(store.pending_outbound(10).unwrap().is_empty());
        assert_eq!(store.backlog().unwrap().outbound, 0);
        let read = store.db.begin_read().unwrap();
        let outbound = read.open_table(OUTBOUND).unwrap();
        let bytes = outbound.get(1_u64).unwrap().unwrap();
        let row: OutboundRow = serde_json::from_slice(bytes.value()).unwrap();
        assert_eq!(row.attempts, 1);
        assert_eq!(
            row.last_error.as_deref(),
            Some("HTTP 400"),
            "and why, on the row, days later"
        );
    }
}
