//! The Model Context Protocol surface, over a real socket.
//!
//! What is being asserted is the seam rather than the protocol: that `/mcp` is
//! mounted where the daemon says it is, that the gate refuses a caller without
//! the token, and that a tool answers from the **same** cache the REST route
//! answers from — because two query paths that can disagree are two answers to
//! one question.

use std::sync::Arc;

use hems_service::{McpAuth, McpSettings, Secret, Shutdown};
use hems_tariff::cache::PriceCache;
use hems_tariff::source::{PriceBasis, PriceSeries, Source};
use rust_decimal::Decimal;
use tokio::sync::RwLock;

/// A cache with one day of prices in it.
async fn cache() -> Arc<RwLock<PriceCache>> {
    let now = time::OffsetDateTime::now_utc();
    let first = hems_core::prelude::Slot::containing(now);
    let mut cache = PriceCache::new();
    let series = PriceSeries {
        points: (0..96)
            .map(|i| (first.offset(i), Decimal::new(1250, 2)))
            .collect(),
        source: Source::Entsoe,
        basis: PriceBasis::Wholesale,
        published_minutes: 15,
    };
    cache.merge(&series, now);
    Arc::new(RwLock::new(cache))
}

/// The daemon's own router, with the MCP surface merged in as `main` merges it.
async fn serve(
    auth: McpAuth,
) -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound = listener.local_addr().unwrap();
    drop(listener);

    let cache = cache().await;
    let (signal, trigger) = Shutdown::channel();
    let app = tariffd::api::router(tariffd::api::Prices::new(Arc::clone(&cache))).merge(
        tariffd::mcp_server::router(
            Arc::new(tariffd::mcp_server::State { cache }),
            auth,
            hems_service::mcp::cancel_on(&signal),
        ),
    );
    let server = hems_service::Server::new(
        hems_service::identity!(),
        hems_service::Settings {
            listen: bound,
            shutdown_grace_s: 2,
            ..hems_service::Settings::default()
        },
        hems_service::Health::new(),
        app,
    );
    tokio::spawn(async move { server.run_until(signal).await.unwrap() });
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(bound).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    (bound, trigger)
}

/// One MCP request, as a client makes it.
async fn call(
    address: std::net::SocketAddr,
    token: Option<&str>,
    session: Option<&str>,
    body: &str,
) -> (u16, Option<String>, String) {
    let mut request = reqwest::Client::new()
        .post(format!("http://{address}/mcp"))
        .header("content-type", "application/json")
        // The streamable-HTTP transport answers either as JSON or as an event
        // stream, and says which it may send.
        .header("accept", "application/json, text/event-stream")
        .body(body.to_owned());
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(id) = session {
        request = request.header("mcp-session-id", id);
    }
    let response = request.send().await.expect("the daemon answers");
    let status = response.status().as_u16();
    let id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (status, id, response.text().await.unwrap_or_default())
}

/// A session, initialised as the protocol requires before any tool may run.
async fn session(address: std::net::SocketAddr) -> String {
    let (status, id, body) = call(address, None, None, INITIALIZE).await;
    assert_eq!(status, 200, "initialize: {body}");
    let id = id.expect("the transport issues a session id");
    let (status, _, body) = call(
        address,
        None,
        Some(&id),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "the initialized notification is accepted: {status} {body}"
    );
    id
}

/// Everything an SSE answer carries, with the framing stripped.
fn payload(body: &str) -> String {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect::<Vec<_>>()
        .join("")
}

/// `initialize`, which every session begins with.
const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-06-18",
    "capabilities":{},
    "clientInfo":{"name":"a-test","version":"0"}}}"#;

#[tokio::test]
async fn the_surface_is_mounted_and_says_what_it_is() {
    let (address, trigger) = serve(McpAuth::open()).await;

    let (status, _, body) = call(address, None, None, INITIALIZE).await;
    assert_eq!(status, 200, "the transport answered: {body}");
    assert!(
        body.contains("tariffd"),
        "and it names itself, so a client knows what it is talking to: {body}"
    );
    assert!(
        body.contains("wholesale"),
        "the instructions are the point of the surface — an agent reading a \
         price curve has to be told it is a wholesale one: {body}"
    );

    trigger.trigger();
}

#[tokio::test]
async fn a_gated_surface_refuses_a_caller_without_the_token() {
    // The whole of the authorisation model on this daemon: the REST routes are
    // open because a published auction result is not anybody's data, and an
    // operator who puts a token on the MCP surface is rate-limiting their own
    // upstream quota rather than protecting a household.
    let (address, trigger) = serve(McpAuth::gate("tok-mcp")).await;

    let (status, ..) = call(address, None, None, INITIALIZE).await;
    assert_eq!(status, 401, "no token, no session");

    let (status, ..) = call(address, Some("tok-mc"), None, INITIALIZE).await;
    assert_eq!(status, 401, "and not a prefix of it either");

    let (status, _, body) = call(address, Some("tok-mcp"), None, INITIALIZE).await;
    assert_eq!(status, 200, "the configured token does: {body}");

    trigger.trigger();
}

#[tokio::test]
async fn the_two_surfaces_answer_from_the_same_cache() {
    // A second query path is a second thing to keep in step, and the one that
    // drifts is whichever nobody is looking at. This calls both and compares
    // the prices they return, so a change to one that does not reach the other
    // fails here rather than in somebody's plan.
    let (address, trigger) = serve(McpAuth::open()).await;

    let rest: serde_json::Value = reqwest::get(format!("http://{address}/v1/prices?slots=4"))
        .await
        .expect("the REST route answers")
        .json()
        .await
        .expect("and answers JSON");
    let over_rest: Vec<&str> = rest["points"]
        .as_array()
        .expect("points")
        .iter()
        .map(|p| p["price_ct"].as_str().expect("an exact decimal string"))
        .collect();
    assert_eq!(
        over_rest, ["12.50"; 4],
        "four quarter hours at 12,50 ct/kWh"
    );

    let id = session(address).await;
    let (status, _, body) = call(
        address,
        None,
        Some(&id),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"get_prices","arguments":{"slots":4}}}"#,
    )
    .await;
    assert_eq!(status, 200, "the tool ran: {body}");

    let answer: serde_json::Value =
        serde_json::from_str(&payload(&body)).expect("a JSON-RPC response");
    // A tool without a declared output schema answers with a text block, and
    // the text is the JSON. Nothing here declares one, so this is the form.
    let tool: serde_json::Value = serde_json::from_str(
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a text block: {body}")),
    )
    .expect("whose text is the tool's JSON");
    let over_mcp: Vec<&str> = tool["points"]
        .as_array()
        .unwrap_or_else(|| panic!("the tool returned points: {body}"))
        .iter()
        .map(|p| p["price_ct"].as_str().expect("an exact decimal string"))
        .collect();

    assert_eq!(over_mcp, over_rest, "one cache, one answer");
    assert!(
        (tool["coverage"].as_f64().expect("coverage") - 1.0).abs() < f64::EPSILON,
        "and the same coverage the REST surface reports: {body}"
    );

    trigger.trigger();
}

#[tokio::test]
async fn a_daemon_holding_household_data_will_not_open_its_surface() {
    // `tariffd` may be open; `histd` and `obsd` may not, and the two
    // constructors are what make that a decision the daemon takes at start-up
    // rather than one a configuration file gets wrong quietly.
    let open = McpSettings {
        enabled: true,
        token: None,
    };
    assert!(
        McpAuth::gated(&open).is_ok(),
        "a published auction result is nobody's data"
    );
    assert!(
        McpAuth::per_caller(&open, &hems_service::Credentials::default()).is_err(),
        "a surface over a store that accepts nothing refuses every call identically"
    );

    // …and a shared token beside a credential model is refused, because it
    // would answer every caller as the same principal.
    let with_token = McpSettings {
        enabled: true,
        token: Some(Secret::literal("tok-operator")),
    };
    let credentials = hems_service::Credentials::default().with_operator("tok-operator");
    assert!(McpAuth::per_caller(&with_token, &credentials).is_err());
}
