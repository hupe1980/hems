//! Who may read a household's record, and who may write it.
//!
//! These routes serve the two documents this daemon exists for: a network
//! operator's Nachweis `[A1 7.2]`, and the household's Data Act Article 4
//! export — everything the product generated, which is when the shower ran and
//! which fortnight nobody was in. Authorisation is therefore part of what the
//! service *is*, and every rule below is checked over a real socket.

use std::collections::BTreeMap;
use std::sync::Arc;

use hems_core::prelude::{GuardRule, Power};
use hems_grid::evidence::ControlEvent;
use hems_grid::mispel::QuarterHour;
use hems_grid::para14a::ControlMode;
use hems_service::{Credentials, Secret};
use histd::Db;
use histd::api::{History, router};
use time::macros::datetime;

const NOW: time::OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);

const HAUS_1: &str = "tok-haus-1";
const HAUS_2: &str = "tok-haus-2";
const NETZ: &str = "tok-netzbetreiber";

fn credentials() -> Credentials {
    let sites: BTreeMap<String, Secret> = [
        ("haus-1".to_owned(), Secret::literal(HAUS_1)),
        ("haus-2".to_owned(), Secret::literal(HAUS_2)),
    ]
    .into_iter()
    .collect();
    Credentials::resolve(
        &sites,
        &std::collections::BTreeMap::new(),
        &[hems_service::OperatorCredential {
            token: Secret::literal(NETZ),
            tenant: "*".into(),
        }],
    )
    .unwrap()
}

fn event() -> ControlEvent {
    ControlEvent::received(
        GuardRule::Lpc,
        ControlMode::Ems,
        Power::from_kw(4.2),
        Power::from_kw(10.5),
        NOW,
    )
}

async fn start() -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    drop(listener);

    // A record already exists for `haus-1`, so a refusal below is a refusal
    // rather than an empty answer that happens to look like one.
    let path = std::env::temp_dir().join(format!(
        "hems-histd-auth-{}-{:?}.sqlite",
        std::process::id(),
        std::thread::current().id()
    ));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let db = Db::at(&path);
    let mut store = db.connect().unwrap();
    store.put_control_event("haus-1", &event()).unwrap();
    store
        .put_quarter_hour(
            "haus-1",
            &QuarterHour::empty(hems_core::prelude::Slot::containing(NOW)),
            NOW,
        )
        .unwrap();

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
            db.clone(),
            Arc::new(std::sync::Mutex::new(store)),
            credentials(),
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

/// One HTTP/1.1 request, so the test needs no client dependency.
async fn request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    let head = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: h\r\n{auth}Content-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: h\r\n{auth}Connection: close\r\n\r\n"),
    };
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    raw.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn nothing_is_served_without_a_credential() {
    let (address, trigger) = start().await;
    for path in [
        "/v1/sites/haus-1/export",
        "/v1/sites/haus-1/nachweis",
        "/v1/sites/haus-1/quarter-hours",
    ] {
        assert_eq!(
            request(address, "GET", path, None, None).await,
            401,
            "{path} answered without a token"
        );
        assert_eq!(
            request(address, "GET", path, Some("tok-invented"), None).await,
            401,
            "{path} answered a token nobody issued"
        );
    }
    trigger.trigger();
}

#[tokio::test]
async fn one_households_credential_does_not_reach_anothers_record() {
    // `403` and not `404`: the caller is somebody, just not this somebody, and
    // an operator debugging a rollout has to be able to tell those apart.
    let (address, trigger) = start().await;
    for path in [
        "/v1/sites/haus-1/export",
        "/v1/sites/haus-1/nachweis",
        "/v1/sites/haus-1/quarter-hours",
    ] {
        assert_eq!(
            request(address, "GET", path, Some(HAUS_2), None).await,
            403,
            "{path} served haus-2 the record of haus-1"
        );
    }
    assert_eq!(
        request(
            address,
            "POST",
            "/v1/sites/haus-1/events",
            Some(HAUS_2),
            Some(&serde_json::to_string(&event()).unwrap()),
        )
        .await,
        403,
        "haus-2 wrote evidence into haus-1's record"
    );
    trigger.trigger();
}

#[tokio::test]
async fn a_box_reads_and_writes_its_own_record() {
    let (address, trigger) = start().await;
    for path in [
        "/v1/sites/haus-1/export",
        "/v1/sites/haus-1/nachweis",
        "/v1/sites/haus-1/quarter-hours",
    ] {
        assert_eq!(
            request(address, "GET", path, Some(HAUS_1), None).await,
            200,
            "{path}"
        );
    }
    assert_eq!(
        request(
            address,
            "POST",
            "/v1/sites/haus-1/events",
            Some(HAUS_1),
            Some(&serde_json::to_string(&event()).unwrap()),
        )
        .await,
        201
    );
    trigger.trigger();
}

#[tokio::test]
async fn an_operator_reads_the_nachweis_and_never_the_data_act_export() {
    // The distinction the two documents are for. `[A1 7.2]` is the record of
    // what the operator commanded and what the connection point drew, and it is
    // theirs to check. Article 4 of Regulation (EU) 2023/2854 is a right of the
    // **user**, and a fleet token is not a household.
    let (address, trigger) = start().await;
    assert_eq!(
        request(
            address,
            "GET",
            "/v1/sites/haus-1/nachweis",
            Some(NETZ),
            None
        )
        .await,
        200
    );
    assert_eq!(
        request(address, "GET", "/v1/sites/haus-1/export", Some(NETZ), None).await,
        403,
        "an operator was handed the household's whole consumption record"
    );
    trigger.trigger();
}

#[tokio::test]
async fn an_operator_may_not_write_the_record_it_is_judged_by() {
    // An operator that could write the evidence of its own control actions is
    // marking its own homework, and `[A1 7.2]` puts the record with the
    // household for exactly that reason.
    let (address, trigger) = start().await;
    assert_eq!(
        request(
            address,
            "POST",
            "/v1/sites/haus-1/events",
            Some(NETZ),
            Some(&serde_json::to_string(&event()).unwrap()),
        )
        .await,
        403
    );
    assert_eq!(
        request(
            address,
            "POST",
            "/v1/sites/haus-1/quarter-hours",
            Some(NETZ),
            Some("[]"),
        )
        .await,
        403
    );
    trigger.trigger();
}
