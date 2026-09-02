//! The collector, over the wire, with the report a box actually sends.
//!
//! `hemsd` produces a `DayKpis`; this service consumes one. The type is shared,
//! so the two cannot drift — but a shared type still leaves a wire, and this is
//! the test that says the wire works.
//!
//! The wire is a **signed CloudEvent**, so every request here is signed
//! the way a box signs one, and three of the tests are about what happens when
//! it is not.

use std::sync::Arc;

use hems_core::prelude::CostBreakdown;
use hems_core::report::DayKpis;
use obsd::api::{Observed, router};
use obsd::fleet::Fleet;
use time::macros::date;
use tokio::sync::RwLock;

/// The secret the box and this fleet share in these tests.
const SECRET: &str = "whsec_the-test-fleet";
/// A second household's own key, so "who signed this" is a question with two
/// possible answers rather than one.
const SECRET_FOR_HAUS_1: &str = "whsec_haus-1";
/// The credential an operator reads the fleet view with.
const OPERATOR: &str = "tok-operator";

/// A day a box would send after a January reduction it respected.
fn good_day(site: &str, day: u8) -> DayKpis {
    DayKpis {
        site: site.into(),
        date: time::Date::from_calendar_date(2026, time::Month::January, day).unwrap(),
        imported_kwh: 55.7,
        exported_kwh: 0.3,
        produced_kwh: 8.4,
        self_sufficiency: 0.13,
        economics: Some(hems_core::report::Economics {
            cost: CostBreakdown {
                energy_eur: 21.08,
                wear_eur: 0.62,
                discomfort_eur: 0.19,
                stored_eur: 0.14,
                ..CostBreakdown::default()
            },
            baseline: CostBreakdown {
                energy_eur: 24.12,
                ..CostBreakdown::default()
            },
        }),
        respected_the_grid: true,
        control_events: 1,
        forecast: Some(hems_core::report::ForecastScores {
            pv_coverage: 0.81,

            pv_crps: 192.0,

            load_coverage: 0.85,

            load_crps: 18.0,
        }),
        ..DayKpis::default()
    }
}

/// Start the service on an ephemeral port; returns the address and the stopper.
async fn start() -> (
    std::net::SocketAddr,
    hems_service::shutdown::ShutdownTrigger,
) {
    start_with_a_site_credential(None).await
}

/// The same service, additionally accepting one household's own credential.
///
/// The default harness configures an operator and nothing else, which is why
/// nothing here noticed what a *site* token could reach.
async fn start_with_a_site_credential(
    site: Option<(&str, &str)>,
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
    let fleet = Arc::new(RwLock::new(Fleet::new(60)));
    let (signal, trigger) = hems_service::Shutdown::channel();
    let server = hems_service::Server::new(
        hems_service::identity!(),
        settings,
        hems_service::Health::new(),
        router(Observed::new(
            fleet,
            time::Duration::days(2),
            // Every site the tests report for, each with a key of its own — which
            // is the point: two sites sharing one key would make "who signed this"
            // unanswerable, and the daemon refuses that configuration.
            (0..10)
                .map(|i| format!("site-{i}"))
                .chain(std::iter::once("haus-2".to_owned()))
                .map(|site| {
                    let key = format!("{SECRET}-{site}");
                    (site, key)
                })
                .chain(std::iter::once((
                    "haus-1".to_owned(),
                    SECRET_FOR_HAUS_1.to_owned(),
                )))
                .collect(),
            hems_events::webhook::DEFAULT_TOLERANCE,
            hems_service::Credentials::resolve(
                &site
                    .map(|(name, token)| (name.to_owned(), hems_service::Secret::literal(token)))
                    .into_iter()
                    .collect(),
                &std::collections::BTreeMap::new(),
                &[hems_service::OperatorCredential {
                    token: hems_service::Secret::literal(OPERATOR),
                    tenant: "*".into(),
                }],
            )
            .unwrap(),
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
    body: Option<&str>,
    headers: &[(&str, String)],
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let extra: String = headers
        .iter()
        .map(|(n, v)| format!("{n}: {v}\r\n"))
        .collect();
    let head = match body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: h\r\nContent-Type: application/cloudevents+json\r\n\
             {extra}Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        ),
        None => {
            format!("{method} {path} HTTP/1.1\r\nHost: h\r\n{extra}Connection: close\r\n\r\n")
        }
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

/// The body and headers a box sends for one day.
fn signed_report(
    day: &DayKpis,
    secret: &str,
    at: time::OffsetDateTime,
) -> (String, Vec<(&'static str, String)>) {
    let event = hems_events::Event::new(
        hems_events::SITE_DAY_REPORTED,
        format!("hems://sites/{}", day.site),
        format!("{}:{}", day.site, day.date),
        at,
        day.clone(),
    )
    .about(day.date.to_string());
    let body = String::from_utf8(event.to_bytes().unwrap()).unwrap();
    let signature = hems_events::webhook::sign(secret.as_bytes(), &event.id, at, body.as_bytes());
    (body, signature.headers().to_vec())
}

/// The same, at this instant and under the fleet's own secret.
fn report_now(day: &DayKpis) -> (String, Vec<(&'static str, String)>) {
    signed_report(
        day,
        &format!("{SECRET}-{}", day.site),
        time::OffsetDateTime::now_utc(),
    )
}

/// The header an operator reads with.
fn operator() -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {OPERATOR}"))]
}

/// `POST /v1/days` with a report a box would have signed.
async fn post_day(address: std::net::SocketAddr, day: &DayKpis) -> u16 {
    let (body, headers) = report_now(day);
    request(address, "POST", "/v1/days", Some(&body), &headers)
        .await
        .0
}

#[tokio::test]
async fn a_day_reported_over_the_wire_reaches_the_summary() {
    let (address, trigger) = start().await;
    assert_eq!(post_day(address, &good_day("site-1", 15)).await, 202);

    let (status, summary) = request(address, "GET", "/v1/fleet", None, &operator()).await;
    assert_eq!(status, 200);
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["sites"], 1);
    assert_eq!(summary["measured_days"], 1);
    // €24,12 − €22,03 = €2,09, which is the reference winter day's own saving.
    let saving = summary["saving_eur"].as_f64().unwrap();
    assert!((saving - 2.09).abs() < 0.005, "{saving}");
    trigger.trigger();
}

#[tokio::test]
async fn a_breach_arrives_as_a_named_finding_and_not_as_a_percentage() {
    let (address, trigger) = start().await;
    for i in 0..5 {
        post_day(address, &good_day(&format!("site-{i}"), 15)).await;
    }
    let mut bad = good_day("site-9", 15);
    bad.respected_the_grid = false;
    bad.worst_overshoot_w = 850.0;
    post_day(address, &bad).await;

    let (_, summary) = request(address, "GET", "/v1/fleet", None, &operator()).await;
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    let breached = summary["breached"].as_array().unwrap();
    assert_eq!(breached.len(), 1);
    assert_eq!(breached[0]["site"], "site-9");
    assert_eq!(breached[0]["date"], "2026-01-15");
    assert!(breached[0]["detail"].as_str().unwrap().contains("850"));
    trigger.trigger();
}

#[tokio::test]
async fn a_site_can_be_asked_about_on_its_own() {
    let (address, trigger) = start().await;
    for day in 15..18 {
        post_day(address, &good_day("site-1", day)).await;
    }
    let (status, days) = request(address, "GET", "/v1/sites/site-1", None, &operator()).await;
    assert_eq!(status, 200);
    let days: Vec<DayKpis> = serde_json::from_str(&days).unwrap();
    assert_eq!(days.len(), 3);
    assert_eq!(days[0].date, date!(2026 - 01 - 15));

    let (status, _) = request(address, "GET", "/v1/sites/nobody", None, &operator()).await;
    assert_eq!(status, 404);
    trigger.trigger();
}

#[tokio::test]
async fn a_report_with_a_field_this_build_does_not_know_is_refused() {
    // `deny_unknown_fields`, and it is the whole point of sharing the type: a
    // box running a newer build that renamed a field must fail loudly here
    // rather than have the fleet silently default it and average a zero.
    //
    // It is signed correctly, so this is a `400` and not a `401`: the box is who
    // it says it is and is sending something this build cannot read, which is a
    // different problem from an intruder and deserves a different answer.
    let (address, trigger) = start().await;
    let (body, headers) = report_now(&good_day("site-1", 15));
    let mut event: serde_json::Value = serde_json::from_str(&body).unwrap();
    event["data"]["invented_field"] = serde_json::json!(1);
    let tampered = serde_json::to_string(&event).unwrap();
    // Re-sign it: the point of this test is the schema, not the signature.
    let at = time::OffsetDateTime::now_utc();
    let id = headers[0].1.clone();
    let key = format!("{SECRET}-site-1");
    let signature = hems_events::webhook::sign(key.as_bytes(), &id, at, tampered.as_bytes());
    let (status, _) = request(
        address,
        "POST",
        "/v1/days",
        Some(&tampered),
        &signature.headers(),
    )
    .await;
    assert_eq!(
        status, 400,
        "a schema the fleet does not understand is refused"
    );
    trigger.trigger();
}

#[tokio::test]
async fn an_unsigned_report_is_refused() {
    // What the signature is for: without it, anybody who can reach this
    // endpoint can write a household into — or out of — the list of sites that
    // did not respect a network operator's reduction.
    let (address, trigger) = start().await;
    let (body, _) = report_now(&good_day("site-1", 15));
    let (status, _) = request(address, "POST", "/v1/days", Some(&body), &[]).await;
    assert_eq!(status, 401);

    let (_, summary) = request(address, "GET", "/v1/fleet", None, &operator()).await;
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(summary["sites"], 0, "a refused report must not be recorded");
    trigger.trigger();
}

#[tokio::test]
async fn a_report_edited_after_signing_is_refused() {
    // Not the box's build being newer — the body being changed on the way. The
    // signature is over the bytes, so a compliant day rewritten into a breach
    // does not verify.
    let (address, trigger) = start().await;
    let (body, headers) = report_now(&good_day("site-1", 15));
    let edited = body.replace(
        "\"respected_the_grid\":true",
        "\"respected_the_grid\":false",
    );
    assert_ne!(edited, body, "the field has to be in the body to be edited");
    let (status, _) = request(address, "POST", "/v1/days", Some(&edited), &headers).await;
    assert_eq!(status, 401);
    trigger.trigger();
}

#[tokio::test]
async fn a_captured_report_stops_working() {
    // Replay: the exact bytes and the exact signature a box sent, six minutes
    // later. Without the timestamp inside the signed content, re-sending
    // yesterday's breach every hour would be a supported operation.
    let (address, trigger) = start().await;
    let day = good_day("site-1", 15);
    let stale = time::OffsetDateTime::now_utc() - time::Duration::minutes(6);
    let (body, headers) = signed_report(&day, &format!("{SECRET}-site-1"), stale);
    let (status, _) = request(address, "POST", "/v1/days", Some(&body), &headers).await;
    assert_eq!(status, 401);
    trigger.trigger();
}

#[tokio::test]
async fn an_event_of_another_type_does_not_become_a_day() {
    // A correctly signed message from a box, of a type this endpoint does not
    // read. Signed by us and still not a day report — the two checks are
    // independent and both have to hold.
    let (address, trigger) = start().await;
    let at = time::OffsetDateTime::now_utc();
    let event = hems_events::Event::new(
        hems_events::SITE_PLAN_PUBLISHED,
        "hems://sites/site-1",
        "site-1:plan",
        at,
        good_day("site-1", 15),
    );
    let body = String::from_utf8(event.to_bytes().unwrap()).unwrap();
    let key = format!("{SECRET}-site-1");
    let signature = hems_events::webhook::sign(key.as_bytes(), &event.id, at, body.as_bytes());
    let (status, _) = request(
        address,
        "POST",
        "/v1/days",
        Some(&body),
        &signature.headers(),
    )
    .await;
    assert_eq!(status, 400);
    trigger.trigger();
}

#[tokio::test]
async fn the_fleet_view_is_not_served_without_a_credential() {
    // `/v1/fleet` carries what every household spent and drew, and the named
    // list of those that did not respect a network operator's reduction. Writing
    // is authenticated by a signature; reading is a different caller and needs
    // its own credential.
    let (address, trigger) = start().await;
    post_day(address, &good_day("site-1", 15)).await;
    for path in ["/v1/fleet", "/v1/sites/site-1"] {
        assert_eq!(
            request(address, "GET", path, None, &[]).await.0,
            401,
            "{path}"
        );
        assert_eq!(
            request(
                address,
                "GET",
                path,
                None,
                &[("authorization", "Bearer tok-invented".to_owned())]
            )
            .await
            .0,
            401,
            "{path}"
        );
    }
    trigger.trigger();
}

#[tokio::test]
async fn one_household_cannot_read_the_whole_fleet() {
    // `/v1/fleet` carries `breached` — the named list of households that did not
    // respect a network operator's reduction — and `below_minimum`. A site's own
    // box credential says "I am this household"; it says nothing about any
    // other, and a summary over all of them is not this household's data.
    let (address, trigger) = start_with_a_site_credential(Some(("haus-1", "tok-haus-1"))).await;

    let own = [("authorization", "Bearer tok-haus-1".to_owned())];
    let (status, _) = request(address, "GET", "/v1/fleet", None, &own).await;
    assert_eq!(
        status, 403,
        "a household's own token must not read the fleet summary"
    );

    // Its own days, on the other hand, are exactly what it may read.
    let (status, _) = request(address, "GET", "/v1/sites/haus-1", None, &own).await;
    assert_ne!(status, 403, "and it is not locked out of its own record");

    // …and another household's are not.
    let (status, _) = request(address, "GET", "/v1/sites/haus-2", None, &own).await;
    assert_eq!(status, 403);

    let (status, _) = request(address, "GET", "/v1/fleet", None, &operator()).await;
    assert_eq!(status, 200, "the operator still reads it");

    trigger.trigger();
}

#[tokio::test]
async fn a_box_cannot_report_a_day_as_another_household() {
    // The signature says the bytes were not edited. It does **not** say who sent
    // them, because the secret is the fleet's rather than the household's — so
    // any box that can sign can attribute a day to any site. What that buys an
    // attacker is the one thing `obsd` exists to hold: `breached` is the named
    // list of households that did not respect a network operator's reduction,
    // and a forged day writes to it.
    let (address, trigger) = start().await;

    // `haus-1`'s box, holding the fleet secret, reports a breach for `haus-2`.
    let mut forged = good_day("haus-2", 15);
    forged.respected_the_grid = false;
    let (body, headers) =
        signed_report(&forged, SECRET_FOR_HAUS_1, time::OffsetDateTime::now_utc());
    let (status, _) = request(address, "POST", "/v1/days", Some(&body), &headers).await;
    assert_eq!(
        status, 401,
        "a box may report its own day and nobody else's — a signature over a \
         shared secret authenticates the bytes, never the sender"
    );

    // And the fleet view did not learn about a breach nobody committed.
    let (status, summary) = request(address, "GET", "/v1/fleet", None, &operator()).await;
    assert_eq!(status, 200);
    let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
    assert_eq!(
        summary["breached"].as_array().map(Vec::len),
        Some(0),
        "nothing was recorded: {summary}"
    );

    // Its own day, under its own secret, is taken.
    let (body, headers) = signed_report(
        &good_day("haus-1", 15),
        SECRET_FOR_HAUS_1,
        time::OffsetDateTime::now_utc(),
    );
    let (status, _) = request(address, "POST", "/v1/days", Some(&body), &headers).await;
    assert_eq!(status, 202);

    trigger.trigger();
}
