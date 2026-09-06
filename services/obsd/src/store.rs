//! Where the fleet's days live.
//!
//! One row per site per day. It has to be durable and shared rather than a map
//! in the process (D157): a box sends a day once and keeps no second copy, so a
//! restart would discard every report — including the named list of households
//! that did not respect a network operator's reduction — with nothing able to
//! rebuild it; and two replicas each holding their own would answer about half a
//! fleet with a denominator claiming the whole one.
//!
//! [`crate::fleet::Fleet`] stays a **pure function** of the days it is given —
//! no clock, no socket, `now` a parameter — which is what makes "one household
//! in ten thousand breached a limit and the summary says so" a unit test. This
//! module is only where the days come from.

use std::collections::BTreeMap;

use hems_core::report::DayKpis;
use hems_service::SiteScope;
use hems_service::db::{Db, Migration};
use thiserror::Error;
use time::{Date, OffsetDateTime};

/// The schema this binary carries.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "one row per site per day",
    sql: include_str!("../migrations/0001_schema.sql"),
}];

/// Why the store could not answer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database itself.
    #[error("the fleet store failed: {0}")]
    Sql(#[from] tokio_postgres::Error),
    /// No connection could be taken from the pool.
    #[error("no database connection was available: {0}")]
    Pool(String),
    /// A day could not be turned into a document to store.
    #[error("the day could not be serialised: {detail}")]
    NotSerialisable {
        /// What `serde` said.
        detail: String,
    },
    /// A stored day is not one this build can read.
    ///
    /// A rolled-back deployment. Named with the row rather than swallowed: one
    /// unreadable day in a fleet's record is a fact an operator has to be told,
    /// and a summary that silently skipped it would have a denominator nobody
    /// can reproduce.
    #[error("the stored day {site}/{day} cannot be read by this build: {detail}")]
    NotReadable {
        /// Which site.
        site: String,
        /// Which day.
        day: Date,
        /// What `serde` said.
        detail: String,
    },
}

impl From<deadpool_postgres::PoolError> for StoreError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        Self::Pool(e.to_string())
    }
}

/// The fleet's days, on disk.
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

    /// The pool underneath, for the readiness watcher.
    #[must_use]
    pub const fn db(&self) -> &Db {
        &self.db
    }

    /// Take in one day.
    ///
    /// A day already on record is **replaced**, not appended: a box that
    /// re-sends yesterday after a reconnect is correcting itself, and a fleet
    /// that counted it twice would double one household's saving inside an
    /// average. The primary key is what makes that a rule rather than a habit.
    ///
    /// # Errors
    /// [`StoreError::Sql`], [`StoreError::Pool`] or
    /// [`StoreError::NotSerialisable`].
    pub async fn record(&self, day: &DayKpis, at: OffsetDateTime) -> Result<(), StoreError> {
        let document = serde_json::to_value(day).map_err(|e| StoreError::NotSerialisable {
            detail: e.to_string(),
        })?;
        let client = self.db.get().await?;
        client
            .execute(
                "INSERT INTO site_day (site, day, document, reported_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (site, day) DO UPDATE SET
                     document    = EXCLUDED.document,
                     reported_at = EXCLUDED.reported_at",
                &[&day.site, &day.date, &document, &at],
            )
            .await?;
        Ok(())
    }

    /// The days from `since` onwards for the households one caller may see, and
    /// when each of them last reported.
    ///
    /// The whole window in **one** query. A summary is an aggregate over every
    /// household in scope, so a query per site would be a round trip per
    /// household on a route an operator refreshes.
    ///
    /// # The scope is a predicate, not a filter
    ///
    /// `scope` reaches the `WHERE` clause. A tenant-scoped summary that read
    /// every household's rows and discarded most of them afterwards would be
    /// doing precisely what D112 forbids — computing an aggregate over the whole
    /// fleet and narrowing at the end — and on a shared deployment it would pull
    /// one tenant's day reports through another tenant's request to do it.
    /// [`SiteScope::Every`] is the only scope that reads everything, and it is a
    /// named variant somebody had to configure.
    ///
    /// `last_report` is the newest `reported_at` the site has, which is what
    /// "has this box gone quiet" is answered from — and it is deliberately not
    /// the newest *day*: a box forwarding a backlog after a week offline reports
    /// old days at a recent instant, and it is not quiet.
    ///
    /// # Errors
    /// [`StoreError::Sql`], [`StoreError::Pool`] or [`StoreError::NotReadable`].
    pub async fn history(
        &self,
        since: Date,
        scope: &SiteScope,
    ) -> Result<BTreeMap<String, SiteDays>, StoreError> {
        // The `SELECT` list both arms share, so a column added to one cannot be
        // missed by the other.
        const COLUMNS: &str = "SELECT site, day, document, reported_at FROM site_day";

        let client = self.db.get().await?;
        // **Two statements, and the scope is never an optional bound** (D166).
        //
        // Written as one with `($2::text[] IS NULL OR site = ANY($2))`, the
        // tenant predicate leaves the `Index Cond` for a `Filter` the moment
        // PostgreSQL switches the cached statement to a generic plan — and the
        // query then reads every *other* tenant's window to answer about one.
        // That is the boundary D112 draws, so it is an authorisation failure and
        // not only a slow one. `force_custom_plan` prevents it and is set, but a
        // tenant boundary resting on a session setting configured in another
        // crate is one assertion away from not holding. There is no sentinel for
        // "every site" the way there is for an unbounded instant (D161), so this
        // is the two queries the one statement stood in for.
        // `tests/query_plans.rs` holds both.
        let named = scope.named().map(|s| {
            s.into_iter()
                .map(std::borrow::ToOwned::to_owned)
                .collect::<Vec<String>>()
        });
        let rows = match &named {
            Some(sites) => {
                client
                    .query(
                        &format!("{COLUMNS} WHERE site = ANY($2) AND day >= $1 ORDER BY site, day"),
                        &[&since, sites],
                    )
                    .await?
            }
            None => {
                client
                    .query(
                        &format!("{COLUMNS} WHERE day >= $1 ORDER BY site, day"),
                        &[&since],
                    )
                    .await?
            }
        };

        let mut out: BTreeMap<String, SiteDays> = BTreeMap::new();
        for row in &rows {
            let site: String = row.get(0);
            let day: Date = row.get(1);
            let document: serde_json::Value = row.get(2);
            let reported_at: OffsetDateTime = row.get(3);
            let kpis: DayKpis =
                serde_json::from_value(document).map_err(|e| StoreError::NotReadable {
                    site: site.clone(),
                    day,
                    detail: e.to_string(),
                })?;
            let entry = out.entry(site).or_default();
            entry.days.insert(day, kpis);
            entry.last_report = Some(
                entry
                    .last_report
                    .map_or(reported_at, |a| a.max(reported_at)),
            );
        }
        Ok(out)
    }

    /// Delete every day older than `before`.
    ///
    /// Returns how many went. A `DELETE` rather than a bound enforced on every
    /// write, so an operator can ask how much record there is and when the
    /// oldest of it goes.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn prune(&self, before: Date) -> Result<u64, StoreError> {
        let client = self.db.get().await?;
        Ok(client
            .execute("DELETE FROM site_day WHERE day < $1", &[&before])
            .await?)
    }
}

/// The first day a summary is computed over, and the first the sweep keeps.
///
/// One function because there were three copies of it — the REST surface, the
/// MCP surface and the retention loop — and a summary computed over a wider
/// window than the sweep deletes outside would report a figure resting on days
/// that are about to vanish. `keep_days` is a `usize` from configuration and a
/// day count is an `i64`, so the conversion is saturating rather than a cast: a
/// nonsense value in a file must not wrap a date into the past.
#[must_use]
pub fn window_start(keep_days: usize, today: Date) -> Date {
    let days = i64::try_from(keep_days.max(1)).unwrap_or(i64::MAX);
    today
        .checked_sub(time::Duration::days(days))
        .unwrap_or(Date::MIN)
}

/// One site's window, as the store returns it.
#[derive(Debug, Clone, Default)]
pub struct SiteDays {
    /// The days, oldest first.
    pub days: BTreeMap<Date, DayKpis>,
    /// When this site last said anything.
    pub last_report: Option<OffsetDateTime>,
}
