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
//! # Why SQLite
//!
//! One writer, a handful of rows per box, and a file. The same argument
//! `histd` makes (D87), one size smaller: an enrolment is a few hundred bytes
//! written once in a box's life, and a report is one row a box overwrites. A
//! Postgres for that is a server, a pool and a system library added to something
//! that fits in a file — and `bundled` keeps `just ci` clone-and-run with every
//! query below exercised against a real database rather than a mock.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, params};
use thiserror::Error;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The revisions, oldest first.
///
/// `mako`'s layout — `services/<daemon>/migrations/NNNN_*.sql`, a new file per
/// revision, never an edit to one that has shipped. `mako` is PostgreSQL and
/// calls `sqlx::migrate!`; this is SQLite, so the applied revision lives in
/// `user_version` and the runner below is the twenty lines that reads it.
const MIGRATIONS: &[(i32, &str)] = &[(1, include_str!("../migrations/0001_schema.sql"))];

/// Why the store could not answer.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The database itself.
    #[error("the fleet store failed: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A stored timestamp is not the RFC 3339 it was written as.
    ///
    /// Nothing else writes these columns, so it means the file has been edited
    /// by something else. An error rather than a fallback to `now`, because a
    /// box that appears to have reported this instant is a box nobody will look
    /// at.
    #[error("the stored timestamp {value:?} for site {site} is not RFC 3339")]
    NotATimestamp {
        /// Which site's row.
        site: String,
        /// What was in it.
        value: String,
    },
    /// The file was written by a newer build.
    #[error("this database is at revision {found}, and this build understands {understood}")]
    FromTheFuture {
        /// What the file says.
        found: i32,
        /// The newest revision this build has.
        understood: i32,
    },
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
#[derive(Debug)]
pub struct Store {
    connection: Connection,
}

impl Store {
    /// Open `path`, applying any revision it is behind.
    ///
    /// `:memory:` opens a private database, which is what the tests use.
    ///
    /// # Errors
    /// [`StoreError::Sql`] if a revision cannot be applied, or
    /// [`StoreError::FromTheFuture`] if the file was written by a newer build.
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
    fn migrate(&self) -> Result<(), StoreError> {
        // Every `PRAGMA` on every open, and none of them in a migration file:
        // `journal_mode` is persistent and cannot be set inside a transaction,
        // and the other two are per *connection*, so a migration would set them
        // for the connection that created the schema and for nothing after.
        self.connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        let at: i32 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let newest = MIGRATIONS.last().map_or(0, |(v, _)| *v);
        if at > newest {
            // A downgraded daemon. Refusing is the only safe answer: this build
            // would write rows the newer revision cannot read back.
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

    /// Record that a site has enrolled, and with which credential.
    ///
    /// The insert is **not** an upsert. `site` is the primary key, so a second
    /// enrolment collides here rather than being caught by a map a restart
    /// emptied — the single-use property is the constraint and not a check.
    ///
    /// # Errors
    /// [`StoreError::Sql`], including the primary-key collision.
    pub fn enrol(&self, site: &str, enrolment: &Enrolment) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO enrolment (site, token, enrolled_at) VALUES (?1, ?2, ?3)",
            params![
                site,
                enrolment.token,
                enrolment
                    .enrolled_at
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| enrolment.enrolled_at.to_string())
            ],
        )?;
        Ok(())
    }

    /// Record what a box says it is running.
    ///
    /// An upsert, unlike an enrolment: this is the one row a box overwrites
    /// every time it reports.
    ///
    /// # Errors
    /// [`StoreError::Sql`].
    pub fn report(&self, site: &str, report: &Report) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO running (site, running_version, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(site) DO UPDATE SET running_version = ?2, last_seen = ?3",
            params![
                site,
                report.running_version,
                report
                    .last_seen
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| report.last_seen.to_string())
            ],
        )?;
        Ok(())
    }

    /// Every credential this fleet has issued.
    ///
    /// # Errors
    /// [`StoreError::Sql`], or [`StoreError::NotATimestamp`] for a row nothing
    /// in this daemon could have written.
    pub fn enrolments(&self) -> Result<BTreeMap<String, Enrolment>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT site, token, enrolled_at FROM enrolment")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (site, token, at) = row?;
            let enrolled_at = parse(&site, &at)?;
            out.insert(site, Enrolment { token, enrolled_at });
        }
        Ok(out)
    }

    /// What every box has last said about itself.
    ///
    /// # Errors
    /// [`StoreError::Sql`], or [`StoreError::NotATimestamp`].
    pub fn reports(&self) -> Result<BTreeMap<String, Report>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT site, running_version, last_seen FROM running")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (site, running_version, at) = row?;
            let last_seen = parse(&site, &at)?;
            out.insert(
                site,
                Report {
                    running_version,
                    last_seen,
                },
            );
        }
        Ok(out)
    }
}

/// One stored timestamp.
fn parse(site: &str, value: &str) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| StoreError::NotATimestamp {
        site: site.to_owned(),
        value: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-06-21 12:00:00 UTC);

    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }

    #[test]
    fn a_credential_outlives_the_process_that_issued_it() {
        // The whole reason this file exists. What is written here is the only
        // copy the fleet has — the box holds the other — so a restart that lost
        // it would leave every enrolled household presenting a token nothing
        // recognises, on every route, with no way back but a site visit.
        let store = store();
        store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-1".into(),
                    enrolled_at: NOW,
                },
            )
            .unwrap();

        let back = store.enrolments();
        assert_eq!(
            back.unwrap().get("site-1"),
            Some(&Enrolment {
                token: "tok-1".into(),
                enrolled_at: NOW,
            })
        );
    }

    #[test]
    fn the_single_use_secret_is_a_constraint_and_not_a_check() {
        // In memory the second enrolment was refused by a map, and a restart
        // emptied the map — so every enrolment secret an installer had written
        // down became usable again, silently, on every deploy. The primary key
        // is what makes that impossible rather than unlikely.
        let store = store();
        let first = Enrolment {
            token: "tok-1".into(),
            enrolled_at: NOW,
        };
        store.enrol("site-1", &first).unwrap();

        let again = store.enrol(
            "site-1",
            &Enrolment {
                token: "tok-2".into(),
                enrolled_at: NOW + time::Duration::hours(1),
            },
        );
        assert!(again.is_err(), "a site enrols once");
        assert_eq!(
            store.enrolments().unwrap().get("site-1"),
            Some(&first),
            "and the first credential is the one that survives — a second \
             attempt must never rotate a working box's token"
        );
    }

    #[test]
    fn a_report_is_the_one_row_a_box_overwrites() {
        let store = store();
        store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-1".into(),
                    enrolled_at: NOW,
                },
            )
            .unwrap();
        store
            .report(
                "site-1",
                &Report {
                    running_version: "6".into(),
                    last_seen: NOW,
                },
            )
            .unwrap();
        store
            .report(
                "site-1",
                &Report {
                    running_version: "7".into(),
                    last_seen: NOW + time::Duration::hours(1),
                },
            )
            .unwrap();

        let reports = store.reports().unwrap();
        assert_eq!(reports.len(), 1, "one box, one row");
        assert_eq!(
            reports.get("site-1"),
            Some(&Report {
                running_version: "7".into(),
                last_seen: NOW + time::Duration::hours(1),
            })
        );
    }

    #[test]
    fn a_box_that_has_not_reported_has_no_row() {
        // "Has not said yet" and "is running version zero" are different facts.
        // A default would collapse them, and the second is what a rollout
        // dashboard would then show for a box that has never once answered.
        let store = store();
        store
            .enrol(
                "site-1",
                &Enrolment {
                    token: "tok-1".into(),
                    enrolled_at: NOW,
                },
            )
            .unwrap();

        assert!(store.reports().unwrap().is_empty());
        assert_eq!(store.enrolments().unwrap().len(), 1);
    }

    #[test]
    fn opening_an_up_to_date_store_applies_nothing() {
        let file =
            std::env::temp_dir().join(format!("hems-fleetd-migrate-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&file);
        Store::open(&file).unwrap();
        let again = Store::open(&file).unwrap();
        let at: i32 = again
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(at, MIGRATIONS.last().unwrap().0);
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let file =
            std::env::temp_dir().join(format!("hems-fleetd-future-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&file);
        {
            let store = Store::open(&file).unwrap();
            store
                .connection
                .execute_batch("PRAGMA user_version = 99")
                .unwrap();
        }
        assert!(matches!(
            Store::open(&file),
            Err(StoreError::FromTheFuture {
                found: 99,
                understood: 1
            })
        ));
        let _ = std::fs::remove_file(&file);
    }
}
