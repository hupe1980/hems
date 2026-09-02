//! A box arriving, being adopted, asking what to run, and being offered an
//! update it can check for itself.

use std::collections::BTreeMap;
use std::sync::Arc;

use ed25519_dalek::{Signer, SigningKey};
use fleetd::api::{Fleet, router};
use fleetd::config::SiteEntry;
use fleetd::registry::Registry;
use hems_service::update::{Manifest, Release, SignedConfig};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

const ARTEFACT: &[u8] = b"a gateway image";

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

fn release() -> Release {
    let manifest = Manifest {
        component: "hemsd".into(),
        version: "0.2.0".into(),
        url: "https://updates.example/hemsd-0.2.0".into(),
        sha256: hex::encode(Sha256::digest(ARTEFACT)),
        size_bytes: ARTEFACT.len() as u64,
        published_at: time::macros::datetime!(2026-06-21 12:00:00 UTC),
    };
    let signature = signing_key().sign(&manifest.signing_payload().unwrap());
    Release {
        manifest,
        signature: hex::encode(signature.to_bytes()),
    }
}

/// The configuration document for a site, signed the way an operator signs one.
///
/// `fleetd` never holds the signing key: it is applied here, off the fleet
/// server, and `fleetd` carries only the result. A `fleetd` an attacker owns can
/// therefore serve no configuration any box will accept.
fn signed_config(site: &str, version: &str, config: &str) -> SignedConfig {
    let mut c = SignedConfig {
        site: site.into(),
        version: version.into(),
        config: config.into(),
        signature: String::new(),
    };
    c.signature = hex::encode(signing_key().sign(&c.signing_payload().unwrap()).to_bytes());
    c
}

fn sites() -> BTreeMap<String, SiteEntry> {
    let c = signed_config("site-1", "7", "listen = \"0.0.0.0:8080\"\n");
    [(
        "site-1".to_owned(),
        SiteEntry {
            enrolment_secret: hems_service::Secret::literal("installer-secret"),
            config: c.config,
            config_version: c.version,
            config_signature: c.signature,
        },
    )]
    .into_iter()
    .collect()
}

async fn start() -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    start_with(sites()).await
}

async fn start_with(
    sites: BTreeMap<String, SiteEntry>,
) -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    start_on(sites, std::path::Path::new(":memory:")).await
}

/// A daemon whose durable half is `store_path`.
///
/// Two of these in a row over one path is a restart, which is the only way to
/// test the property the store exists for.
async fn start_on(
    sites: BTreeMap<String, SiteEntry>,
    store_path: &std::path::Path,
) -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    drop(listener);
    let settings = hems_service::Settings {
        listen: bound,
        shutdown_grace_s: 2,
        ..hems_service::Settings::default()
    };
    let store = fleetd::store::Store::open(store_path).unwrap();
    let registry = Arc::new(RwLock::new(Registry::restore(
        sites,
        store.enrolments().unwrap(),
        store.reports().unwrap(),
    )));
    let store = Arc::new(std::sync::Mutex::new(store));
    let releases = [("hemsd".to_owned(), release())].into_iter().collect();
    let (signal, trigger) = hems_service::Shutdown::channel();
    let server = hems_service::Server::new(
        hems_service::identity!(),
        settings,
        hems_service::Health::new(),
        router(Fleet::new(
            registry,
            releases,
            hems_service::Credentials::default().with_operator(OPERATOR),
            store,
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

async fn request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
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
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (
        status,
        raw.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned(),
    )
}

#[tokio::test]
async fn a_box_is_adopted_told_what_to_run_and_reports_back() {
    let (address, trigger) = start().await;

    let (status, body) = request(
        address,
        "POST",
        "/v1/enrol",
        None,
        Some(r#"{"site":"site-1","secret":"installer-secret"}"#),
    )
    .await;
    assert_eq!(status, 200);
    let enrolled: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = enrolled["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 64, "256 bits of hexadecimal");

    let (status, body) = request(address, "GET", "/v1/config", Some(&token), None).await;
    assert_eq!(status, 200);
    let config: SignedConfig = serde_json::from_str(&body).unwrap();
    assert_eq!(config.version, "7");
    // The box checks it against the key it was **built** with, so what the fleet
    // served is checkable without trusting the fleet.
    let public = hex::encode(signing_key().verifying_key().to_bytes());
    assert!(
        config
            .verify_for("site-1", &public)
            .unwrap()
            .contains("listen"),
        "the document a box would run"
    );

    // Before the box reports, the fleet does not claim it has converged.
    let (_, body) = request(address, "GET", "/v1/fleet", Some(OPERATOR), None).await;
    let states: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(states[0]["converged"], false);

    let (status, _) = request(
        address,
        "POST",
        "/v1/config/running",
        Some(&token),
        Some(r#"{"config_version":"7"}"#),
    )
    .await;
    assert_eq!(status, 204);
    let (_, body) = request(address, "GET", "/v1/fleet", Some(OPERATOR), None).await;
    let states: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(states[0]["converged"], true);
    trigger.trigger();
}

/// The credential an operator reads the roster with.
const OPERATOR: &str = "tok-operator";

#[tokio::test]
async fn the_roster_is_not_public() {
    // It lists every household this fleet has adopted, the version each is on
    // and when it was last heard from — which is to say which households exist,
    // which are running an old build and which are unreachable right now. A
    // box's own enrolment credential does not read it either: that one says
    // "I am this household", and this is about every household.
    let (address, trigger) = start().await;

    let (status, _) = request(address, "GET", "/v1/fleet", None, None).await;
    assert_eq!(status, 401, "no credential, no roster");

    let (status, _) = request(address, "GET", "/v1/fleet", Some("tok-nobody"), None).await;
    assert_eq!(status, 401, "and not somebody else's either");

    let (status, _) = request(address, "GET", "/v1/fleet", Some(OPERATOR), None).await;
    assert_eq!(status, 200, "an operator's credential does");
    trigger.trigger();
}

#[tokio::test]
async fn a_configuration_from_a_fleet_server_that_lies_is_not_accepted() {
    // The property the signature buys, and the reason it is not enough for this
    // endpoint to be authenticated: a `fleetd` an attacker owns is still a
    // `fleetd` no box will take a configuration from.
    let (address, trigger) = start_with(sites_with_a_tampered_document()).await;
    let (_, body) = request(
        address,
        "POST",
        "/v1/enrol",
        None,
        Some(r#"{"site":"site-1","secret":"installer-secret"}"#),
    )
    .await;
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, body) = request(address, "GET", "/v1/config", Some(&token), None).await;
    assert_eq!(status, 200, "the fleet serves it happily");
    let config: SignedConfig = serde_json::from_str(&body).unwrap();
    let public = hex::encode(signing_key().verifying_key().to_bytes());
    assert!(
        config.verify_for("site-1", &public).is_err(),
        "and the box refuses it"
    );
    trigger.trigger();
}

/// A site whose document has been edited after it was signed.
fn sites_with_a_tampered_document() -> BTreeMap<String, SiteEntry> {
    let mut sites = sites();
    let entry = sites.get_mut("site-1").unwrap();
    entry.config = "listen = \"0.0.0.0:1\"\n".into();
    sites
}

#[tokio::test]
async fn an_unsigned_configuration_is_not_published_at_all() {
    // An operator who has not signed a document has not published one. Serving
    // it unsigned would need the box to have a "trust it anyway" path, and that
    // path is the one an attacker aims at.
    let mut sites = sites();
    sites.get_mut("site-1").unwrap().config_signature = String::new();
    let (address, trigger) = start_with(sites).await;
    let (_, body) = request(
        address,
        "POST",
        "/v1/enrol",
        None,
        Some(r#"{"site":"site-1","secret":"installer-secret"}"#),
    )
    .await;
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, _) = request(address, "GET", "/v1/config", Some(&token), None).await;
    assert_eq!(status, 503);
    trigger.trigger();
}

#[tokio::test]
async fn every_bad_enrolment_gets_the_same_answer() {
    // An endpoint that says "no such site" for one input and "wrong secret" for
    // another will enumerate the fleet for anybody who asks patiently.
    let (address, trigger) = start().await;
    for body in [
        r#"{"site":"nobody","secret":"installer-secret"}"#,
        r#"{"site":"site-1","secret":"wrong"}"#,
    ] {
        let (status, answer) = request(address, "POST", "/v1/enrol", None, Some(body)).await;
        assert_eq!(status, 401, "{body}");
        assert!(answer.trim().is_empty(), "and it says nothing: {answer}");
    }
    trigger.trigger();
}

#[tokio::test]
async fn nothing_is_served_without_a_credential() {
    let (address, trigger) = start().await;
    for path in ["/v1/config", "/v1/releases/hemsd"] {
        assert_eq!(
            request(address, "GET", path, None, None).await.0,
            401,
            "{path}"
        );
        assert_eq!(
            request(address, "GET", path, Some("invented"), None)
                .await
                .0,
            401,
            "{path}"
        );
    }
    trigger.trigger();
}

#[tokio::test]
async fn the_offered_release_verifies_against_the_key_the_box_was_built_with() {
    // The whole point of signing: the box trusts a key, not a server. A
    // compromised `fleetd` can serve a manifest, and no box will accept it.
    let (address, trigger) = start().await;
    let (_, body) = request(
        address,
        "POST",
        "/v1/enrol",
        None,
        Some(r#"{"site":"site-1","secret":"installer-secret"}"#),
    )
    .await;
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, body) = request(address, "GET", "/v1/releases/hemsd", Some(&token), None).await;
    assert_eq!(status, 200);
    let offered: Release = serde_json::from_str(&body).unwrap();

    let trusted = hex::encode(signing_key().verifying_key().to_bytes());
    let manifest = offered.verify(&trusted).expect("signed by us");
    assert_eq!(manifest.version, "0.2.0");
    offered
        .check_artefact(ARTEFACT)
        .expect("the right artefact");
    offered.check_newer("0.1.0").expect("newer than what runs");

    // …and a box built with somebody else's key refuses it, which is the same
    // sentence read from the other side.
    let stranger = hex::encode(
        SigningKey::from_bytes(&[1u8; 32])
            .verifying_key()
            .to_bytes(),
    );
    assert!(offered.verify(&stranger).is_err());

    // A release for a component this fleet does not carry is a 404, not an
    // empty manifest a box might act on.
    assert_eq!(
        request(address, "GET", "/v1/releases/nothing", Some(&token), None)
            .await
            .0,
        404
    );
    trigger.trigger();
}

#[tokio::test]
async fn a_restart_does_not_orphan_the_fleet() {
    // The registry used to be entirely in memory, seeded from TOML. Restarting
    // the daemon threw away every credential it had ever issued — and the box
    // holding the other copy could not enrol again, because its single-use
    // secret was spent. This is that property, and it is the reason `fleetd`
    // has a database at all.
    let file =
        std::env::temp_dir().join(format!("hems-fleetd-restart-{}.sqlite", std::process::id()));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
    }

    let (address, trigger) = start_on(sites(), &file).await;
    let (status, body) = request(
        address,
        "POST",
        "/v1/enrol",
        None,
        Some(r#"{"site":"site-1","secret":"installer-secret"}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let token = serde_json::from_str::<serde_json::Value>(&body).unwrap()["token"]
        .as_str()
        .expect("a minted credential")
        .to_owned();

    let (status, _) = request(
        address,
        "POST",
        "/v1/config/running",
        Some(&token),
        Some(r#"{"config_version":"7"}"#),
    )
    .await;
    assert_eq!(status, 204);
    trigger.trigger();

    // …and now the daemon comes back.
    let (address, trigger) = start_on(sites(), &file).await;

    let (status, body) = request(address, "GET", "/v1/config", Some(&token), None).await;
    assert_eq!(
        status, 200,
        "the box's credential still works after a restart: {body}"
    );

    let (status, _) = request(
        address,
        "POST",
        "/v1/enrol",
        None,
        Some(r#"{"site":"site-1","secret":"installer-secret"}"#),
    )
    .await;
    assert_eq!(
        status, 401,
        "and the single-use secret is still spent — otherwise every enrolment \
         secret an installer wrote down becomes live again on every deploy"
    );

    let (status, body) = request(address, "GET", "/v1/fleet", Some(OPERATOR), None).await;
    assert_eq!(status, 200);
    let roster: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        roster[0]["running_version"], "7",
        "and the rollout answer survived too: a fleet that forgot it would \
         report a completed rollout as untouched"
    );
    assert_eq!(roster[0]["converged"], true);

    trigger.trigger();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
    }
}
