//! The hot queries use their indexes, and keep using them.
//!
//! A query that stops using an index does not fail, log, or look different from
//! one that does. It returns the right answer and reads a thousand times more
//! rows to get it, and the first symptom is a settlement export timing out on
//! the one household with two years of history.
//!
//! The trap is specific and cheap to fall into. `tokio-postgres` prepares and
//! caches statements, and PostgreSQL switches a cached statement to a **generic**
//! plan after five executions — one with no parameter values, so an optional
//! bound written `$2 IS NULL OR ts >= $2` collapses from an `Index Cond` into a
//! `Filter` and a one-day window reads a household's whole record.
//!
//! # The SQL and the session setting are a pair, and both are tested
//!
//! `quarter_hour` carries two overlapping indexes: the key
//! `(site_id, slot_start, recorded_at DESC)` for a household's window, and
//! `(slot_start)` for the retention sweep across every household. With no
//! parameter values a generic plan cannot tell which is more selective, and it
//! picks the wrong one — so there is no SQL that is right under every plan mode.
//! The answer is `plan_cache_mode = force_custom_plan` on every pooled
//! connection (`hems_service::db::prepare_session`), and the last test here
//! asserts that setting is present so removing it breaks the pair visibly.
//!
//! # The query under test is the daemon's own, columns and all
//!
//! The table is versioned — a restated register is a new row — so every read is
//! `DISTINCT ON (slot_start) … ORDER BY slot_start, recorded_at DESC` over the
//! eight payload columns a settlement needs. Asking `EXPLAIN` about a tidier
//! query than that produces a tidier plan than production gets: with only
//! `slot_start` selected this planner returns an index-only scan and no sort,
//! which is a plan the daemon never runs. So the statement below carries the
//! whole column list.

use hems_service::testdb::Postgres;

/// Enough rows, across enough sites, that a sequential scan is not the cheapest
/// plan by accident. A planner given a hundred rows will read them all whatever
/// the SQL says, and a test that passed on that would be testing nothing.
const SITES: i32 = 50;
const ROWS: i32 = 40_000;

async fn loaded() -> Postgres {
    let fixture = Postgres::start(histd::store::MIGRATIONS).await;
    let client = fixture.db.get().await.expect("a connection");
    client
        .batch_execute(&format!(
            "INSERT INTO quarter_hour
             SELECT 'site-' || (g % {SITES}),
                    '2025-01-01Z'::timestamptz + ((g / {SITES}) || ' minutes')::interval,
                    1, 1, 1, 1, NULL, NULL, 1, 1, now()
             FROM generate_series(1, {ROWS}) g
             ON CONFLICT DO NOTHING;
             INSERT INTO control_event
                 (site_id, document, rule, received_at, first_ceiling_w,
                  strictest_ceiling_w, minimum_power_w, below_minimum, expires_at)
             SELECT 'site-' || (g % {SITES}), '{{}}'::jsonb, 'lpc',
                    '2025-01-01Z'::timestamptz + ((g / {SITES}) || ' hours')::interval,
                    4200, 4200, 10500, false, 'infinity'::timestamptz
             FROM generate_series(1, 5000) g;
             ANALYZE quarter_hour;
             ANALYZE control_event;"
        ))
        .await
        .expect("a loaded table");
    fixture
}

/// The plan a **serving** connection produces for `sql`.
///
/// Prepared and executed exactly as `tokio-postgres` does it, on a connection
/// from the pool — so the session settings under test are the ones the daemon
/// actually runs with rather than a psql default.
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
async fn one_sites_window_keeps_every_bound_in_its_index_condition() {
    // The query behind `/v1/sites/{site}/quarter-hours` and behind every
    // settlement. The assertion is on the **`Index Cond`**, not merely on the
    // index being named: the failure mode this exists for is precisely an index
    // scan whose range moved into a `Filter`.
    //
    // The statement is the daemon's own, `DISTINCT ON` and all: the table is
    // versioned, so the query that actually runs picks one `recorded_at` per
    // slot, and a plan test for a simpler query than the one in production
    // proves nothing about production.
    let fixture = loaded().await;
    let plan = plan_of(
        &fixture,
        "window_q",
        "text, timestamptz, timestamptz, timestamptz",
        "SELECT DISTINCT ON (slot_start)
                slot_start, grid_draw_kwh, grid_feed_in_kwh,
                device_consumption_kwh, device_generation_kwh,
                storage_consumption_kwh, storage_generation_kwh,
                anzulegender_wert_ct, spot_price_ct
         FROM quarter_hour
         WHERE site_id = $1 AND slot_start >= $2 AND slot_start < $3
           AND recorded_at <= $4
         ORDER BY slot_start, recorded_at DESC",
        "'site-3', '2025-01-01Z', '2025-01-02Z', 'infinity'",
    )
    .await;

    assert!(
        plan.contains("quarter_hour_version"),
        "the window query left its key index:\n{plan}"
    );
    assert!(
        plan.contains("Index Cond: ((site_id = ") && plan.contains("slot_start <"),
        "all three columns have to be in the index condition — a range that has \
         become a Filter reads the household's whole record for one day of it:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan"),
        "a settlement window must never be a sequential scan:\n{plan}"
    );
    // The `as_of` bound is asserted separately, because it is the one that was
    // added last and is the one an `IS NULL OR` rewrite would quietly demote to
    // a `Filter` — which on a household with two years of history means reading
    // every version of every quarter hour to answer a question about one day.
    assert!(
        plan.contains("recorded_at <="),
        "the version bound has to be in the index condition too:\n{plan}"
    );
    // What is deliberately *not* asserted: that there is no `Sort`. The
    // settlement read wants eight payload columns, so it cannot be index-only,
    // so PostgreSQL takes a bitmap scan and sorts the window to pick versions.
    // Measured, that is 4 ms for 800 rows and an in-memory sort for a two-year
    // export — against a response the caller materialises in full anyway.
}

#[tokio::test]
async fn one_sites_events_are_an_index_scan() {
    // The query behind a Nachweis `[A1 7.2]`.
    let fixture = loaded().await;
    let plan = plan_of(
        &fixture,
        "events_q",
        "text, timestamptz, timestamptz",
        "SELECT id FROM control_event
         WHERE site_id = $1 AND received_at >= $2 AND received_at < $3
         ORDER BY received_at, id",
        "'site-3', '2025-01-01Z', '2025-01-02Z'",
    )
    .await;

    assert!(
        plan.contains("control_event_by_site"),
        "the Nachweis query left its index:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan"),
        "a Nachweis must never be a sequential scan:\n{plan}"
    );
}

#[tokio::test]
async fn the_retention_sweep_does_not_read_the_whole_table() {
    // `[A1 7.3]`'s sweep runs daily over every household and deletes **one day**
    // of a two-year record. Without an index on `slot_start` that is a full scan
    // of the fleet's whole settlement record to remove a fraction of a per cent
    // of it — which is why that index exists even though it competes with the
    // key index on the query above.
    //
    // The cutoff matters to what this proves: asking to delete most of the table
    // correctly plans as a sequential scan, and a test written that way would
    // fail for the right reason and the wrong cause.
    let fixture = loaded().await;
    let client = fixture.db.get().await.expect("a connection");
    let rows = client
        .query(
            "EXPLAIN (COSTS OFF) DELETE FROM quarter_hour WHERE slot_start <= '2025-01-01 00:30:00Z'",
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
        plan.contains("quarter_hour_by_slot"),
        "the retention sweep left its index:\n{plan}"
    );
}

#[tokio::test]
async fn a_serving_connection_replans_with_its_parameters() {
    // The setting that makes the two queries above optimal *together*: with both
    // indexes present and no parameter values, a generic plan cannot tell which
    // is more selective. `prepare_session` forces a custom plan on every
    // connection the pool hands out, which is the whole reason a window query
    // and an export can be one statement.
    let fixture = Postgres::start(histd::store::MIGRATIONS).await;
    let client = fixture.db.get().await.expect("a connection");
    let mode: String = client
        .query_one("SHOW plan_cache_mode", &[])
        .await
        .expect("the session setting")
        .get(0);
    assert_eq!(mode, "force_custom_plan");
}
