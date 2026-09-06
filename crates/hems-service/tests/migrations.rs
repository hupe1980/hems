//! The migration runner, against a real PostgreSQL.
//!
//! Three properties, and each one was a `PRAGMA user_version` test in a daemon
//! before the runner moved into the shell — where it belongs, because three
//! daemons now share it and a copy each is three chances to get the refusals
//! wrong.
//!
//! The refusals are the point. A migration runner that only ever applies things
//! is a `psql -f` with extra steps; what earns it its place is what it declines
//! to do when the database and the binary disagree about what the schema is.

use hems_service::db::Migration;
use hems_service::testdb::Postgres;

const FIRST: &str = "CREATE TABLE widget (id BIGINT PRIMARY KEY)";
const SECOND: &str = "ALTER TABLE widget ADD COLUMN label TEXT";

fn one() -> Vec<Migration> {
    vec![Migration {
        version: 1,
        description: "a widget",
        sql: FIRST,
    }]
}

fn two() -> Vec<Migration> {
    let mut set = one();
    set.push(Migration {
        version: 2,
        description: "and a label for it",
        sql: SECOND,
    });
    set
}

#[tokio::test]
async fn a_second_run_applies_nothing() {
    // The property that makes a numbered migration different from a
    // `CREATE TABLE IF NOT EXISTS`: the second start-up must not re-run the
    // first revision. `0002` here is an `ALTER TABLE`, which is not idempotent —
    // so a runner that re-ran it would fail rather than pass quietly, which is
    // exactly what makes this test able to fail.
    let fixture = Postgres::empty().await;
    hems_service::db::migrate(&fixture.db, &two())
        .await
        .expect("an empty database migrates");
    hems_service::db::migrate(&fixture.db, &two())
        .await
        .expect("and a second start-up finds nothing to do");

    let client = fixture.db.get().await.expect("a connection");
    let applied: i64 = client
        .query_one("SELECT COUNT(*) FROM schema_migration", &[])
        .await
        .expect("the version table")
        .get(0);
    assert_eq!(applied, 2, "two revisions, applied once each");
}

#[tokio::test]
async fn a_revision_added_later_is_applied_on_top() {
    // A deployment: the database is at 1, the new binary carries 2.
    let fixture = Postgres::empty().await;
    hems_service::db::migrate(&fixture.db, &one())
        .await
        .unwrap();
    hems_service::db::migrate(&fixture.db, &two())
        .await
        .unwrap();

    let client = fixture.db.get().await.expect("a connection");
    client
        .execute("INSERT INTO widget (id, label) VALUES (1, 'a')", &[])
        .await
        .expect("the column the second revision added exists");
}

#[tokio::test]
async fn an_edited_migration_is_refused_rather_than_skipped() {
    // The refusal that matters most. A revision that has changed since it was
    // applied means the database and the binary disagree about what the schema
    // is — and the binary would go on writing rows shaped for a schema nobody
    // has. Two years of § 14a evidence is the last record in this workspace that
    // should be repaired by guesswork.
    let fixture = Postgres::empty().await;
    hems_service::db::migrate(&fixture.db, &one())
        .await
        .unwrap();

    let edited = vec![Migration {
        version: 1,
        description: "a widget",
        // One character. The checksum has no opinion about which edits are
        // harmless, which is the whole point of it.
        sql: "CREATE TABLE widget (id BIGINT PRIMARY KEY )",
    }];
    assert!(matches!(
        hems_service::db::migrate(&fixture.db, &edited).await,
        Err(hems_service::DbError::Tampered { version: 1, .. })
    ));
}

#[tokio::test]
async fn a_database_from_a_newer_build_is_refused() {
    // A rolled-back deployment. Serving against it would write rows the newer
    // schema cannot read, so the old binary stops instead.
    let fixture = Postgres::empty().await;
    hems_service::db::migrate(&fixture.db, &two())
        .await
        .unwrap();

    assert!(matches!(
        hems_service::db::migrate(&fixture.db, &one()).await,
        Err(hems_service::DbError::FromTheFuture {
            found: 2,
            understood: 1
        })
    ));
}

#[tokio::test]
async fn several_replicas_starting_together_migrate_once() {
    // The property the advisory lock exists for, and the one no single-node
    // store ever had to have. `fleetd` and `histd` run more than one replica —
    // that is the whole reason they left SQLite — so "every pod runs the
    // migrations on start-up" is the normal case rather than a race somebody
    // has to avoid.
    let fixture = Postgres::empty().await;
    let set = two();
    let run = |db: hems_service::Db| {
        let set = set.clone();
        async move { hems_service::db::migrate(&db, &set).await }
    };
    let (a, b, c, d) = tokio::join!(
        run(fixture.db.clone()),
        run(fixture.db.clone()),
        run(fixture.db.clone()),
        run(fixture.db.clone()),
    );
    for outcome in [a, b, c, d] {
        outcome.expect("every replica either applies or finds nothing to do");
    }

    let client = fixture.db.get().await.expect("a connection");
    let applied: i64 = client
        .query_one("SELECT COUNT(*) FROM schema_migration", &[])
        .await
        .expect("the version table")
        .get(0);
    assert_eq!(applied, 2, "applied once, not four times");
}

#[tokio::test]
async fn a_statement_timeout_is_set_on_every_connection() {
    // The bound that survives a client going away. A Data Act export over two
    // years of a large site is a long query, and one that has lost its reader
    // holds a backend and its locks until PostgreSQL is told otherwise.
    let fixture = Postgres::empty().await;
    let client = fixture.db.get().await.expect("a connection");
    let setting: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .expect("the session setting")
        .get(0);
    assert_ne!(setting, "0", "a serving replica needs a bound");
}

#[tokio::test]
async fn a_migration_runs_without_the_serving_timeout_and_does_not_leak_that() {
    // Two halves of one trap, and both are invisible until the day they matter.
    //
    // Every pooled connection carries `statement_timeout` so an abandoned query
    // cannot hold a backend for ever — and `migrate` takes its connection from
    // that same pool. A migration is not a request: adding a column to a
    // fleet's two years of registers legitimately takes longer than any *query*
    // may, and killed half way it leaves a daemon refusing to start against a
    // schema it just failed to apply. So the migration transaction clears the
    // bound.
    //
    // The second half is why it is `SET LOCAL`. `RecyclingMethod::Fast` does
    // not reset a session, so a plain `SET` would hand that connection back to
    // the pool **unbounded** — and the next two-year export to be given it
    // would have no timeout at all. That is strictly worse than the bug being
    // fixed, because it fails only under load and only sometimes.
    //
    // The first assertion is inside the migration, in the transaction it
    // actually runs in, because that is the only place the question can be
    // asked honestly.
    const CHECKS_ITS_OWN_TIMEOUT: &str = "DO $$ BEGIN \
         IF current_setting('statement_timeout') <> '0' THEN \
             RAISE EXCEPTION 'a migration ran under the serving statement_timeout: %', \
                 current_setting('statement_timeout'); \
         END IF; \
     END $$; \
     CREATE TABLE slow_widget (id BIGINT PRIMARY KEY)";

    // One connection, so the pool cannot hand the check a *different* one from
    // the one the migration ran on — which is what would make the leak half of
    // this test pass by luck.
    let fixture = Postgres::empty_with(1).await;
    hems_service::db::migrate(
        &fixture.db,
        &[Migration {
            version: 1,
            description: "a migration that inspects its own session",
            sql: CHECKS_ITS_OWN_TIMEOUT,
        }],
    )
    .await
    .expect("a migration runs unbounded");

    let client = fixture.db.get().await.expect("the same connection back");
    let setting: String = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .expect("the session setting")
        .get(0);
    assert_ne!(
        setting, "0",
        "the migration's connection went back into the pool with no bound on it"
    );
}
