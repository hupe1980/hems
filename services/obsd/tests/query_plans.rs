//! The scoped read uses the primary key, and keeps using it.
//!
//! # Why this daemon needs a plan test and not only `histd`
//!
//! In `histd` a query that stops using its index is a settlement export getting
//! slow. Here it is a **tenant boundary**. D112 says a scoped summary must be an
//! aggregate over the households in scope rather than over the whole fleet
//! narrowed afterwards, and the place that rule is actually enforced is the
//! `WHERE` clause of one statement. A plan that moves the scope out of the
//! `Index Cond` and into a `Filter` still returns exactly the right rows — and
//! reads every other tenant's day reports to do it.
//!
//! That is not hypothetical. The query was written with the scope as an optional
//! bound, `AND ($2::text[] IS NULL OR site = ANY($2))`, which plans perfectly
//! while the planner can see the array. `tokio-postgres` prepares and caches
//! statements, and PostgreSQL switches a cached statement to a **generic** plan
//! after five executions — one with no parameter values, which cannot fold the
//! `IS NULL` away. Measured on a loaded table, the same SQL gave:
//!
//! ```text
//! custom:  Index Cond: ((site = ANY ('{site-3}')) AND (day >= …))
//! generic: Index Cond: (day >= $1)
//!          Filter:     (($2 IS NULL) OR (site = ANY ($2)))
//! ```
//!
//! `force_custom_plan` on every pooled connection prevents that, and is set. The
//! store does not rely on it for this: the scope is two statements rather than
//! one optional bound, because a tenant boundary that holds only by virtue of a
//! session setting configured in another crate is one assertion away from not
//! holding. Both are asserted below — the plans, and the setting.

use hems_service::testdb::Postgres;

/// Enough sites and days that a sequential scan is not the cheapest plan by
/// accident. A planner given a hundred rows reads them all whatever the SQL
/// says, and a test that passed on that would be testing nothing.
const SITES: i32 = 500;
const ROWS: i32 = 60_000;

async fn loaded() -> Postgres {
    let fixture = Postgres::start(obsd::store::MIGRATIONS).await;
    let client = fixture.db.get().await.expect("a connection");
    client
        .batch_execute(&format!(
            "INSERT INTO site_day
             SELECT 'site-' || (g % {SITES}),
                    DATE '2025-01-01' + ((g / {SITES}) || ' days')::interval,
                    '{{}}'::jsonb, now()
             FROM generate_series(1, {ROWS}) g
             ON CONFLICT DO NOTHING;
             ANALYZE site_day;"
        ))
        .await
        .expect("a loaded table");
    fixture
}

/// The plan a **serving** connection produces for `sql`.
///
/// Prepared and executed as `tokio-postgres` does it, on a connection from the
/// pool, so the session settings under test are the daemon's own rather than a
/// `psql` default.
async fn plan_of(fixture: &Postgres, name: &str, types: &str, sql: &str, args: &str) -> String {
    let client = fixture.db.get().await.expect("a connection");
    client
        .batch_execute(&format!("PREPARE {name}({types}) AS {sql};"))
        .await
        .expect("the statement prepares");
    let rows = client
        .query(&format!("EXPLAIN (COSTS OFF) EXECUTE {name}({args})"), &[])
        .await
        .expect("a plan");
    rows.iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_scoped_summary_reads_only_the_households_in_scope() {
    // The tenant boundary, as the planner sees it. The assertion is on the
    // **`Index Cond`** carrying `site`, not merely on an index being named: a
    // scope that has become a `Filter` returns the same rows and reads the whole
    // fleet to find them.
    let fixture = loaded().await;
    let plan = plan_of(
        &fixture,
        "scoped_q",
        "date, text[]",
        "SELECT site, day, document, reported_at FROM site_day
         WHERE site = ANY($2) AND day >= $1
         ORDER BY site, day",
        "'2025-01-01', ARRAY['site-3','site-7']",
    )
    .await;

    assert!(
        plan.contains("site_day_pkey"),
        "the scoped query left its primary key:\n{plan}"
    );
    assert!(
        plan.contains("Index Cond") && plan.contains("site = ANY"),
        "the scope has to be in the index condition — as a Filter it reads every \
         tenant's day reports to answer about one:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan"),
        "a tenant's summary must never be a sequential scan of the fleet:\n{plan}"
    );
}

#[tokio::test]
async fn the_unscoped_summary_uses_the_day_index() {
    // `SiteScope::Every` is the one scope that reads everything, and it is a
    // named variant somebody had to configure. It is a different query for that
    // reason, and it wants the other index.
    let fixture = loaded().await;
    let plan = plan_of(
        &fixture,
        "every_q",
        "date",
        "SELECT site, day, document, reported_at FROM site_day
         WHERE day >= $1
         ORDER BY site, day",
        "'2025-12-01'",
    )
    .await;

    assert!(
        plan.contains("site_day_by_day"),
        "the fleet-wide window left the index the retention sweep shares:\n{plan}"
    );
}

#[tokio::test]
async fn the_retention_sweep_does_not_read_the_whole_table() {
    // The daily sweep deletes one day out of the window. The primary key leads
    // with `site`, so it cannot answer `day < $1`, and without `site_day_by_day`
    // this is a sequential scan of the fleet's whole record to remove a fraction
    // of a per cent of it.
    //
    // The cutoff matters to what this proves: asking to delete most of the table
    // correctly plans as a sequential scan, and a test written that way would
    // fail for the right reason and the wrong cause.
    let fixture = loaded().await;
    let client = fixture.db.get().await.expect("a connection");
    let rows = client
        .query(
            "EXPLAIN (COSTS OFF) DELETE FROM site_day WHERE day < DATE '2025-01-03'",
            &[],
        )
        .await
        .expect("a plan");
    let plan: String = rows
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        plan.contains("site_day_by_day"),
        "the retention sweep left its index:\n{plan}"
    );
}

#[tokio::test]
async fn a_serving_connection_replans_with_its_parameters() {
    // The belt to the store's braces. The scope is two statements rather than an
    // optional bound so that it does not *depend* on this — but `day >= $1` is
    // still a range whose selectivity a generic plan cannot see, and every other
    // daemon in this workspace does depend on it (D161).
    let fixture = Postgres::start(obsd::store::MIGRATIONS).await;
    let client = fixture.db.get().await.expect("a connection");
    let mode: String = client
        .query_one("SHOW plan_cache_mode", &[])
        .await
        .expect("the session setting")
        .get(0);
    assert_eq!(mode, "force_custom_plan");
}
