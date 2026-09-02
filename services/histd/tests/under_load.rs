//! The service answers while it is busy.
//!
//! `rusqlite` is synchronous, and a synchronous call left inside an `async`
//! handler occupies a runtime worker for as long as the query takes. A
//! household's Data Act export is the two years of `[A1 7.3]` — measured here at
//! about 370 ms — and a runtime has as many workers as the machine has cores, so
//! a handful of concurrent exports is enough to stall *every* request. The ones
//! that matter most are `/livez` and `/readyz`: a health surface that reports a
//! healthy service exactly while it cannot answer is worse than no health
//! surface at all.
//!
//! # Why a latency bound and not "was it answered"
//!
//! Asserting only that every request comes back passes whether or not the
//! queries are on the runtime: they simply queue. So the assertion is a bound on
//! how long the health probe waits while eight full-retention exports are in
//! flight, with the worker count pinned at two. Off the runtime the probe
//! answers in single-digit milliseconds; on it, it waits for the exports to
//! drain, which is seconds. The bound sits two orders of magnitude from one and
//! comfortably below the other.

use std::collections::BTreeMap;
use std::sync::Arc;

use hems_core::prelude::Slot;
use hems_grid::mispel::QuarterHour;
use hems_service::{Credentials, Secret};
use histd::Db;
use histd::api::{History, router};
use time::macros::datetime;

const START: time::OffsetDateTime = datetime!(2026-01-01 00:00:00 UTC);
const TOKEN: &str = "tok-haus-1";

/// How long the health probe may take while the service is busy.
///
/// Off the runtime it is milliseconds; on it, it is however long eight exports
/// take to drain through two workers — about a second and a half on the machine
/// this was measured on, and more on a slower one, which is the direction that
/// keeps the test honest rather than flaky.
const PROBE_BUDGET: std::time::Duration = std::time::Duration::from_millis(750);

/// How many exports are in flight while the probe is measured.
const EXPORTS: usize = 8;

/// The full `[A1 7.3]` retention, which is what an export actually costs.
///
/// A file rather than `:memory:`, because the property under test is WAL's
/// many-readers-one-writer and an in-memory database cannot be shared between
/// connections at all.
fn two_years_at(path: &std::path::Path) -> Db {
    let db = Db::at(path);
    let mut store = db.connect().unwrap();
    let quarters: Vec<QuarterHour> = (0..(730 * 96))
        .map(|i| QuarterHour::empty(Slot::containing(START + time::Duration::minutes(15 * i))))
        .collect();
    // One transaction. Row by row this is seventy thousand commits, which is
    // fifty seconds of `fsync` before the test has measured anything.
    store.put_quarter_hours("haus-1", &quarters, START).unwrap();
    db
}

async fn start(
    path: &std::path::Path,
) -> (
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
    let (signal, trigger) = hems_service::Shutdown::channel();
    let server = hems_service::Server::new(
        hems_service::identity!(),
        settings,
        hems_service::Health::new(),
        router(History::new(
            two_years_at(path),
            Arc::new(std::sync::Mutex::new(Db::at(path).connect().unwrap())),
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
    let path = std::env::temp_dir().join(format!("hems-histd-load-{}.sqlite", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let (address, trigger) = start(&path).await;

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

    // And the write a box is making while a household exports: `[A1 7.2]` is a
    // record of something with a clock on it, so one long read must not hold the
    // store against it.
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
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }

    assert!(
        waited < PROBE_BUDGET,
        "the health probe waited {waited:?} behind {EXPORTS} exports, which is \
         a service reporting itself healthy while it cannot answer"
    );
    assert!(
        write_waited < PROBE_BUDGET,
        "a box's evidence write waited {write_waited:?} behind a household's export"
    );
}
