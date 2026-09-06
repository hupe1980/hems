//! What survives a restart, and the only place `fleetd` writes SQL.
//!
//! # Why there is a database here at all
//!
//! Two of the facts this daemon holds cannot be re-derived from its
//! configuration, and losing either breaks a property the daemon claims:
//!
//! * the **credential** a box was issued. It exists nowhere else — the box holds
//!   the other copy — so a registry that forgot it is a fleet of boxes
//!   presenting a token nothing recognises, on every route, until somebody
//!   re-commissions each one by hand;
//! * the fact that a site **has** enrolled. The enrolment secret is single-use
//!   because a secret that still works once the box is in the field is a
//!   credential sitting in an installer's notes. If "already enrolled" lives
//!   only in memory then a restart makes every one of those secrets usable
//!   again, and the property is a comment rather than a mechanism.
//!
//! What is deliberately **not** here is the configured half — which sites exist,
//! their secrets, the configuration each should run and the signature over it.
//! That is the operator's *intent*, it is declared in the daemon's own
//! configuration, and copying it into a database would give two answers to one
//! question with no rule for which wins.
//!
//! # Why PostgreSQL for something this small
//!
//! It is a handful of rows per box, and the old argument for SQLite (D87) was
//! exactly that: an enrolment is a few hundred bytes written once in a box's
//! life. What that argument left out is that **size is not why a fleet service
//! needs a server** (D156). `fleetd` is the daemon a box talks to first, so it
//! is the one that must be up while another is being deployed — and a service
//! whose state is a file on one node cannot run a second replica, cannot fail
//! over, and cannot be upgraded without every box in the field losing the
//! endpoint it enrols against.
//!
//! The single-use enrolment makes it sharper. That property is a primary-key
//! collision, and a primary key is only single-use across the *whole* service:
//! two replicas with a file each would each accept the same enrolment secret
//! once. Under SQLite the daemon could not be replicated, so the hole did not
//! exist; making it replicable is what makes the shared database load-bearing
//! rather than a matter of taste.

use std::collections::BTreeMap;

use hems_service::db::{Db, Migration};
use thiserror::Error;
use time::OffsetDateTime;

/// The schema this binary carries.
///
/// `include_str!` rather than a path read at run time, so the container needs no
/// files beside the binary. `hems_service::db::migrate` checksums each revision,
/// so an edit to one already applied is refused rather than skipped.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "enrolments and what each box last reported",
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
}

impl From<deadpool_postgres::PoolError> for StoreError {
    fn from(e: deadpool_postgres::PoolError) -> Self {
        Self::Pool(e.to_string())
    }
}

impl StoreError {
    /// Whether this is the unique-violation a second enrolment attempt raises.
    ///
    /// The single-use property is a primary key, so "already enrolled" arrives
    /// as SQLSTATE `23505` rather than as a check somebody wrote. Asked by
    /// SQLSTATE and not by matching the message: the text is the server's, it is
    /// localised, and a registry that decided policy from it would change its
    /// mind when somebody set `lc_messages`.
    #[must_use]
    pub fn is_already_taken(&self) -> bool {
        matches!(self, Self::Sql(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION))
    }
}

/// One box's credential, as it was issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolment {
    /// The credential it presents from now on.
    pub token: String,
    /// When it was adopted.
    pub enrolled_at: OffsetDateTime,
}

/// What a box last said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The configuration version it says it is running.
    pub running_version: String,
    /// When it said so.
    pub last_seen: OffsetDateTime,
}

/// The fleet's durable half.
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

    /// Record that a site has enrolled, and with which credential.
    ///
    /// The insert is **not** an upsert. `site` is the primary key, so a second
    /// enrolment collides here rather than being caught by a map a restart
    /// emptied — the single-use property is the constraint and not a check.
    ///
    /// # Errors
    /// [`StoreError::Sql`], including the primary-key collision.
    pub async fn enrol(&self, site: &str, enrolment: &Enrolment) -> Result<(), StoreError> {
        let client = self.db.get().await?;
        client
            .execute(
                "INSERT INTO enrolment (site, token, enrolled_at) VALUES ($1, $2, $3)",
                &[&site, &enrolment.token, &enrolment.enrolled_at],
            )
            .await?;
        Ok(())
    }

    /// Record what a box says it is running.
    ///
    /// An upsert, unlike an enrolment: this is the one row a box overwrites
    /// every time it reports.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub async fn report(&self, site: &str, report: &Report) -> Result<(), StoreError> {
        let client = self.db.get().await?;
        client
            .execute(
                "INSERT INTO running (site, running_version, last_seen) VALUES ($1, $2, $3)
                 ON CONFLICT (site) DO UPDATE SET
                     running_version = EXCLUDED.running_version,
                     last_seen       = EXCLUDED.last_seen",
                &[&site, &report.running_version, &report.last_seen],
            )
            .await?;
        Ok(())
    }

    /// Every credential this fleet has issued.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn enrolments(&self) -> Result<BTreeMap<String, Enrolment>, StoreError> {
        let client = self.db.get().await?;
        let rows = client
            .query("SELECT site, token, enrolled_at FROM enrolment", &[])
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get(0),
                    Enrolment {
                        token: row.get(1),
                        enrolled_at: row.get(2),
                    },
                )
            })
            .collect())
    }

    /// What every box has last said about itself.
    ///
    /// # Errors
    /// [`StoreError::Sql`] or [`StoreError::Pool`].
    pub async fn reports(&self) -> Result<BTreeMap<String, Report>, StoreError> {
        let client = self.db.get().await?;
        let rows = client
            .query("SELECT site, running_version, last_seen FROM running", &[])
            .await?;
        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get(0),
                    Report {
                        running_version: row.get(1),
                        last_seen: row.get(2),
                    },
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    async fn store() -> (hems_service::testdb::Postgres, Store) {
        let fixture = hems_service::testdb::Postgres::start(MIGRATIONS).await;
        let store = Store::new(fixture.db.clone());
        (fixture, store)
    }

    #[tokio::test]
    async fn a_credential_outlives_the_process_that_issued_it() {
        // The whole reason this file exists. What is written here is the only
        // copy the fleet has — the box holds the other — so a restart that lost
        // it would leave every enrolled household presenting a token nothing
        // recognises, on every route, with no way back but a site visit.
        let (_fixture, store) = store().await;
        store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-1".into(),
                    enrolled_at: NOW,
                },
            )
            .await
            .unwrap();

        let back = store.enrolments().await;
        assert_eq!(
            back.unwrap().get("site-1"),
            Some(&Enrolment {
                token: "tok-1".into(),
                enrolled_at: NOW,
            })
        );
    }

    #[tokio::test]
    async fn the_single_use_secret_is_a_constraint_and_not_a_check() {
        // In memory the second enrolment was refused by a map, and a restart
        // emptied the map — so every enrolment secret an installer had written
        // down became usable again, silently, on every deploy. The primary key
        // is what makes that impossible rather than unlikely.
        let (_fixture, store) = store().await;
        let first = Enrolment {
            token: "tok-1".into(),
            enrolled_at: NOW,
        };
        store.enrol("site-1", &first).await.unwrap();

        let again = store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-2".into(),
                    enrolled_at: NOW + time::Duration::hours(1),
                },
            )
            .await;
        assert!(again.is_err(), "a site enrols once");
        assert_eq!(
            store.enrolments().await.unwrap().get("site-1"),
            Some(&first),
            "and the first credential is the one that survives — a second \
             attempt must never rotate a working box's token"
        );
    }

    #[tokio::test]
    async fn a_report_is_the_one_row_a_box_overwrites() {
        let (_fixture, store) = store().await;
        store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-1".into(),
                    enrolled_at: NOW,
                },
            )
            .await
            .unwrap();
        store
            .report(
                "site-1",
                &Report {
                    running_version: "6".into(),
                    last_seen: NOW,
                },
            )
            .await
            .unwrap();
        store
            .report(
                "site-1",
                &Report {
                    running_version: "7".into(),
                    last_seen: NOW + time::Duration::hours(1),
                },
            )
            .await
            .unwrap();

        let reports = store.reports().await.unwrap();
        assert_eq!(reports.len(), 1, "one box, one row");
        assert_eq!(
            reports.get("site-1"),
            Some(&Report {
                running_version: "7".into(),
                last_seen: NOW + time::Duration::hours(1),
            })
        );
    }

    #[tokio::test]
    async fn a_box_that_has_not_reported_has_no_row() {
        // "Has not said yet" and "is running version zero" are different facts.
        // A default would collapse them, and the second is what a rollout
        // dashboard would then show for a box that has never once answered.
        let (_fixture, store) = store().await;
        store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-1".into(),
                    enrolled_at: NOW,
                },
            )
            .await
            .unwrap();

        assert!(store.reports().await.unwrap().is_empty());
        assert_eq!(store.enrolments().await.unwrap().len(), 1);
    }
}
