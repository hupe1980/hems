//! The box's own record reaching the fleet, over a real socket.
//!
//! `hemsd` records first and forwards second: `[A1 7.3]` gives a network
//! operator two years to ask about a control event, and G3 says the house is
//! never worse off when the cloud is gone. That makes the *forwarding* the part
//! nothing else exercises — the store keeps the rows either way, and a backlog
//! that never drains looks exactly like a box with nothing to say.
//!
//! It looked exactly like that for a while. The drain sent control events and
//! never the quarter-hour **registers**, which are the other half of what
//! `histd` holds and what MiSpeL's Abgrenzung and § 42c's allocation are
//! computed from: `mark_forwarded(&accepted, &[], now)` passed an empty slot
//! list on every round, `pending_quarter_hours` had no caller outside its own
//! unit tests, and `Backlog::is_empty` could never become true on a running box.
//! Nothing failed, and the fleet's register half was fed by nothing.
//!
//! The fleet here is two `axum` routes in the shape `histd` serves, for the same
//! reason `planned_house.rs` fakes `tariffd`: what is under test is the box's
//! drain, and standing up the real daemon would test its store instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::routing::post;
use hems_core::prelude::{AssetId, Power, Slot};
use hems_core::setpoint::GuardRule;
use hems_grid::evidence::ControlEvent;
use hems_grid::mispel::QuarterHour;
use hems_grid::para14a::ControlMode;
use hems_service::{Secret, Shutdown};
use hemsd::runtime::outbox::{HistdSettings, Outbox};
use hemsd::store::{Recorded, Store};
use rust_decimal::Decimal;
use tokio::sync::Mutex;

const NOW: time::OffsetDateTime = time::macros::datetime!(2026-01-15 17:00:00 UTC);

/// What the stand-in fleet was given.
#[derive(Default)]
struct Landed {
    events: AtomicUsize,
    registers: AtomicUsize,
}

/// Two routes in the shape `histd` serves, and the counts they saw.
async fn histd_on_loopback() -> (String, Arc<Landed>, Shutdown) {
    let landed = Arc::new(Landed::default());
    let for_events = Arc::clone(&landed);
    let for_registers = Arc::clone(&landed);
    let app = Router::new()
        .route(
            "/v1/sites/{site}/events",
            post(move |body: axum::Json<serde_json::Value>| {
                let seen = Arc::clone(&for_events);
                async move {
                    let _ = body;
                    seen.events.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::ACCEPTED
                }
            }),
        )
        .route(
            "/v1/sites/{site}/quarter-hours",
            post(move |body: axum::Json<Vec<QuarterHour>>| {
                let seen = Arc::clone(&for_registers);
                async move {
                    seen.registers.fetch_add(body.0.len(), Ordering::SeqCst);
                    axum::http::StatusCode::ACCEPTED
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = format!("http://{}", listener.local_addr().expect("its address"));
    let (signal, trigger) = Shutdown::channel();
    let stop = signal.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(stop.wait())
            .await;
    });
    std::mem::forget(trigger);
    (address, landed, signal)
}

fn register(i: i64) -> Recorded {
    Recorded {
        registers: QuarterHour {
            grid_draw: Decimal::new(15, 2),
            grid_feed_in: Decimal::new(5, 2),
            ..QuarterHour::empty(Slot::containing(NOW + time::Duration::minutes(15 * i)))
        },
        production: Some(Decimal::new(10, 2)),
    }
}

fn event() -> ControlEvent {
    let mut e = ControlEvent::received(
        GuardRule::Lpc,
        ControlMode::Ems,
        Power::from_kw(4.2),
        Power::from_kw(10.5),
        NOW,
    );
    e.applied_at = Some(NOW);
    e.released_at = Some(NOW + time::Duration::minutes(90));
    e.assets = vec![AssetId::new("wallbox").unwrap()];
    e
}

#[tokio::test]
async fn both_halves_of_the_record_reach_the_fleet_and_leave_the_backlog() {
    let (address, landed, _shutdown) = histd_on_loopback().await;

    let mut store = Store::in_memory().expect("a store");
    store.put_control_event(&event()).expect("an event");
    let day: Vec<Recorded> = (0..8).map(register).collect();
    store.put_quarter_hours(&day, NOW).expect("its registers");
    assert_eq!(
        store.backlog().expect("a backlog").quarter_hours,
        8,
        "owed before the drain"
    );

    let outbox = Outbox::new(&HistdSettings {
        url: Some(address),
        site: Some("haus-1".to_owned()),
        token: Some(Secret::literal("tok")),
        every_s: 300,
        batch: 50,
    })
    .expect("a client")
    .expect("a configured fleet");

    let store = Arc::new(Mutex::new(store));
    let sent = outbox.drain(&store, NOW).await.expect("a drain");

    assert_eq!(sent, 9, "one event and eight registers");
    assert_eq!(landed.events.load(Ordering::SeqCst), 1);
    assert_eq!(
        landed.registers.load(Ordering::SeqCst),
        8,
        "and the registers went as one batch, which is the shape `histd` serves"
    );

    // Forwarded is not deleted: the two years are the household's, and an
    // acknowledgement moves a row out of the backlog and never out of the store.
    let backlog = store.lock().await.backlog().expect("a backlog");
    assert!(backlog.is_empty(), "nothing is still owed: {backlog:?}");
    assert_eq!(
        store.lock().await.quarter_hours().expect("the rows").len(),
        8,
        "and every row is still on the box"
    );

    // A second round has nothing to say, rather than saying it all again.
    assert_eq!(outbox.drain(&store, NOW).await.expect("a drain"), 0);
    assert_eq!(landed.registers.load(Ordering::SeqCst), 8);
}

#[tokio::test]
async fn a_fleet_that_refuses_the_registers_keeps_them_owed() {
    // The property the whole design rests on: a record is safe on the box first.
    // A refusal is a warning and a retry, never a row that quietly stops
    // existing.
    let mut store = Store::in_memory().expect("a store");
    let day: Vec<Recorded> = (0..4).map(register).collect();
    store.put_quarter_hours(&day, NOW).expect("its registers");

    let outbox = Outbox::new(&HistdSettings {
        // A port nothing is listening on.
        url: Some("http://127.0.0.1:1".to_owned()),
        site: Some("haus-1".to_owned()),
        token: Some(Secret::literal("tok")),
        every_s: 300,
        batch: 50,
    })
    .expect("a client")
    .expect("a configured fleet");

    let store = Arc::new(Mutex::new(store));
    assert_eq!(outbox.drain(&store, NOW).await.expect("a drain"), 0);
    assert_eq!(
        store
            .lock()
            .await
            .backlog()
            .expect("a backlog")
            .quarter_hours,
        4,
        "still owed"
    );
}
