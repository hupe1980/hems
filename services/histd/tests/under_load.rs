//! The service answers while it is busy.
//!
//! The hazard a pool-backed service has is the **pool**: there are `pool_size`
//! connections and no more, and one that queued without bound behind a saturated
//! pool would report itself healthy exactly while it could not answer. So the
//! pool here is deliberately smaller than the load put on it, and two things are
//! asserted.
//!
//! * `/livez` and `/readyz` answer promptly while it is saturated. They are the
//!   probes an orchestrator restarts and routes on, and they take no connection —
//!   a design decision this pins rather than an accident.
//! * **A box's evidence write does not queue behind a household's export.**
//!   `[A1 7.2]` is a record of something with a clock on it, so one long read
//!   must not hold the store against it.

use std::collections::BTreeMap;

use hems_core::prelude::Slot;
use hems_grid::mispel::QuarterHour;
use hems_service::{Credentials, Secret};
use histd::api::{History, router};
use time::macros::datetime;

const START: time::OffsetDateTime = datetime!(2026-01-01 00:00:00 UTC);
const TOKEN: &str = "tok-haus-1";

/// How long a request may take while the service is busy.
///
/// Generous on purpose: what is being ruled out is a *queue*, which under the
/// old architecture was seconds, and the bound sits an order of magnitude below
/// that and an order above the milliseconds a healthy answer takes.
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(2_000);

/// How many exports are in flight while the probe is measured.
///
/// More than [`POOL`], so every connection is busy and the ninth caller is
/// genuinely waiting for one.
const EXPORTS: usize = 8;

/// The connections the service is given.
const POOL: usize = 4;

/// A year of registers, which is what an export actually costs.
///
/// A year rather than the full two: the point is a response large enough to hold
/// a connection for a measurable time, and 35 040 rows already is.
async fn a_year_of_registers(store: &histd::Store) {
    let quarters: Vec<QuarterHour> = (0..(365 * 96))
        .map(|i| QuarterHour::empty(Slot::containing(START + time::Duration::minutes(15 * i))))
        .collect();
    // One statement. Row by row this is thirty-five thousand round trips, which
    // is minutes before the test has measured anything.
    store
        .put_quarter_hours("haus-1", &quarters, START)
        .await
        .expect("a year of registers");
}

async fn start() -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    drop(listener);

    let sites: BTreeMap<String, Secret> = [("haus-1".to_owned(), Secret::literal(TOKEN))]
        .into_iter()
        .collect();
    let settings = hems_service::Settings {
        listen: bound,
        shutdown_grace_s: 2,
        ..hems_service::Settings::default()
    };
    let fixture = hems_service::testdb::Postgres::start_with(histd::store::MIGRATIONS, POOL).await;
    let store = histd::Store::new(fixture.db.clone());
    a_year_of_registers(&store).await;
    // Leaked on purpose: the pool has to outlive the server task, and a test
    // process ends when the test does.
    std::mem::forget(fixture);

    let (signal, trigger) = hems_service::Shutdown::channel();
    let server = hems_service::Server::new(
        hems_service::identity!(),
        settings,
        hems_service::Health::new(),
        router(History::new(
            store,
            Credentials::resolve(&sites, &std::collections::BTreeMap::new(), &[]).unwrap(),
        )),
    );
    tokio::spawn(async move { server.run_until(signal).await.unwrap() });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(bound).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    (bound, trigger)
}

async fn send(address: std::net::SocketAddr, head: String) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    String::from_utf8_lossy(&raw)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

async fn get(address: std::net::SocketAddr, path: &str, token: Option<&str>) -> u16 {
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    send(
        address,
        format!("GET {path} HTTP/1.1\r\nHost: h\r\n{auth}Connection: close\r\n\r\n"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_service_answers_while_the_exports_run() {
    let (address, trigger) = start().await;

    let exports: Vec<_> = (0..EXPORTS)
        .map(|_| tokio::spawn(get(address, "/v1/sites/haus-1/export", Some(TOKEN))))
        .collect();
    // Let them reach the handler, so what is measured below is a busy service
    // rather than one that has not started yet.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The probe an orchestrator restarts the process on.
    let probe = std::time::Instant::now();
    assert_eq!(get(address, "/livez", None).await, 200, "live");
    assert_eq!(get(address, "/readyz", None).await, 200, "ready");
    let waited = probe.elapsed();

    // And the write a box is making while a household exports.
    let body = "[]";
    let write = std::time::Instant::now();
    let status = send(
        address,
        format!(
            "POST /v1/sites/haus-1/quarter-hours HTTP/1.1\r\nHost: h\r\n\
             Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert_eq!(status, 204, "the box's write went through");
    let write_waited = write.elapsed();

    for export in exports {
        assert_eq!(export.await.unwrap(), 200, "every export was answered too");
    }
    trigger.trigger();

    assert!(
        waited < PROBE_BUDGET,
        "the health probe waited {waited:?} behind {EXPORTS} exports against a \
         pool of {POOL}, which is a service reporting itself healthy while it \
         cannot answer"
    );
    assert!(
        write_waited < PROBE_BUDGET,
        "a box's evidence write waited {write_waited:?} behind a household's export"
    );
}
