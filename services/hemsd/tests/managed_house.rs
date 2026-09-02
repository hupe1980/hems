//! The box on the wall, against a device on a socket.
//!
//! Everything else `hemsd` runs is a simulated day: the drivers are handed bytes
//! by a test and the clock is a variable. This is the other half — a real
//! `TcpListener` answering real Modbus frames, the real transport task
//! connecting to it, and the real registry folding what comes back into what the
//! guard reads.
//!
//! Three things are being proved, and only the first is obvious.
//!
//! 1. **A reading gets from a socket to the guard at all.**
//! 2. **A reconnect does not corrupt the stream.** The transport tells the
//!    driver its link went and came back, which is the one fact bytes cannot
//!    carry: a half-frame left over from before the drop would make the whole
//!    new stream decode at an offset, and every reading after it would be
//!    plausible and wrong.
//! 3. **A device that stops answering stops being believed.** Not by falling
//!    over — by ageing out of [`hemsd::drivers::Registry::state`], so the guard
//!    goes back to assuming the nameplate, which is the safe assumption and an
//!    expensive one.

use std::sync::Arc;

use hems_core::prelude::{AssetId, Power};
use hems_drv::modbus::{Cadence, SunSpec};
use hems_service::Shutdown;
use hemsd::drivers::Registry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// A SunSpec inverter that answers on base 40 000, over a socket.
///
/// The register layout is the one `hems-drv`'s own device test uses — model 1,
/// then 103 with a real `W`/`W_SF` pair, then 123, then the sentinel — because
/// the point here is the *transport*, and a second invented layout would test
/// the wrong thing.
struct Inverter {
    map: std::collections::BTreeMap<u16, u16>,
}

impl Inverter {
    fn new(watts: u16) -> Self {
        let mut map = std::collections::BTreeMap::new();
        map.insert(40_000, 0x5375);
        map.insert(40_001, 0x6e53);

        map.insert(40_002, 1);
        map.insert(40_003, 66);
        for i in 0..66_u16 {
            map.insert(40_004 + i, 0);
        }

        map.insert(40_070, 103);
        map.insert(40_071, 50);
        for i in 0..50_u16 {
            map.insert(40_072 + i, 0);
        }
        map.insert(40_072 + 12, watts / 10); // W
        map.insert(40_072 + 13, 1); // W_SF: ×10¹

        map.insert(40_122, 123);
        map.insert(40_123, 24);
        for i in 0..24_u16 {
            map.insert(40_124 + i, 0);
        }
        map.insert(40_148, 0xffff);
        map.insert(40_149, 0);
        Self { map }
    }

    /// Answer one request the way a device would.
    fn answer(&self, bytes: &[u8]) -> Vec<u8> {
        let transaction = u16::from_be_bytes([bytes[0], bytes[1]]);
        let unit = bytes[6];
        let function = bytes[7];
        let address = u16::from_be_bytes([bytes[8], bytes[9]]);
        let pdu = match function {
            0x03 => {
                let count = u16::from_be_bytes([bytes[10], bytes[11]]);
                if self.map.contains_key(&address) {
                    let mut pdu = vec![0x03, u8::try_from(count * 2).expect("a short read")];
                    for i in 0..count {
                        let v = self.map.get(&(address + i)).copied().unwrap_or(0);
                        pdu.extend_from_slice(&v.to_be_bytes());
                    }
                    pdu
                } else {
                    // Illegal data address, which is how a real device ends a
                    // model walk.
                    vec![0x83, 0x02]
                }
            }
            0x10 => {
                let count = u16::from_be_bytes([bytes[10], bytes[11]]);
                let mut pdu = vec![0x10];
                pdu.extend_from_slice(&address.to_be_bytes());
                pdu.extend_from_slice(&count.to_be_bytes());
                pdu
            }
            _ => vec![function | 0x80, 0x01],
        };
        let mut out = Vec::new();
        out.extend_from_slice(&transaction.to_be_bytes());
        out.extend_from_slice(&0_u16.to_be_bytes());
        out.extend_from_slice(
            &(u16::try_from(pdu.len() + 1).expect("a short frame")).to_be_bytes(),
        );
        out.push(unit);
        out.extend_from_slice(&pdu);
        out
    }
}

/// Serve `hangup_after` requests and then close, for ever.
///
/// `usize::MAX` is a device that never hangs up; a small number is one behind a
/// flaky Wi-Fi bridge, which is the case the reconnect exists for.
async fn serve(listener: TcpListener, watts: u16, hangup_after: usize) {
    let device = Inverter::new(watts);
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = [0_u8; 512];
        let mut served = 0_usize;
        while served < hangup_after {
            let Ok(n) = stream.read(&mut buffer).await else {
                break;
            };
            if n == 0 {
                break;
            }
            // Requests can arrive together; the driver never has more than one
            // outstanding, so a whole frame per read is the honest simplification.
            if stream
                .write_all(&device.answer(&buffer[..n]))
                .await
                .is_err()
            {
                break;
            }
            served += 1;
        }
    }
}

/// A registry holding one SunSpec driver for `pv`.
fn registry() -> (Arc<Mutex<Registry>>, AssetId, hems_core::prelude::Site) {
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let asset = household.pv.clone();
    let mut registry = Registry::new();
    registry
        .register(
            Box::new(SunSpec::new(
                asset.clone(),
                1,
                Cadence {
                    poll: time::Duration::milliseconds(20),
                    timeout: time::Duration::milliseconds(200),
                },
            )),
            &household.site,
        )
        .expect("the site has a roof");
    (Arc::new(Mutex::new(registry)), asset, household.site)
}

/// Wait until `check` holds, or give up after `attempts` short sleeps.
///
/// A loop rather than one sleep: the test is about a socket and a scheduler, and
/// a fixed wait long enough to be reliable on a loaded machine is a fixed wait
/// that makes the suite slow on every other one.
async fn until(
    registry: &Arc<Mutex<Registry>>,
    attempts: usize,
    check: impl Fn(&mut Registry, time::OffsetDateTime) -> bool,
) -> bool {
    for _ in 0..attempts {
        {
            let mut held = registry.lock().await;
            if check(&mut held, time::OffsetDateTime::now_utc()) {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn a_reading_travels_from_a_socket_to_the_guard() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address").to_string();
    tokio::spawn(serve(listener, 2_310, usize::MAX));

    let (registry, asset, _site) = registry();
    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::transport::tcp(
        Arc::clone(&registry),
        asset.clone(),
        address,
        signal,
    ));

    let arrived = until(&registry, 100, |held, now| {
        held.observe(None, now)
            .state
            .asset(&asset)
            .and_then(|m| m.power)
            .is_some()
    })
    .await;
    assert!(
        arrived,
        "a measurement has to reach the registry — this is the seam between \
         `the logic is right` and `the house is managed`"
    );

    let now = time::OffsetDateTime::now_utc();
    let power = registry
        .lock()
        .await
        .observe(None, now)
        .state
        .asset(&asset)
        .and_then(|m| m.power)
        .expect("the reading that just arrived");
    // Negative, and that is the load convention rather than a slip: an inverter
    // *produces*, so what it contributes at its own connection is an outflow.
    // The guard's whole surplus arithmetic is built on that sign, and a driver
    // that reported production as a positive draw would raise a § 14a budget by
    // twice the roof.
    assert!(
        (power.get() + 2_310.0).abs() < 1.0,
        "the reading has to be the number the device published, scale factor and \
         sign convention and all: {power:?}"
    );

    let silent: Vec<_> = registry
        .lock()
        .await
        .observe(None, now)
        .silent
        .into_iter()
        .collect();
    assert!(
        silent.is_empty(),
        "a device that is answering is not silent: {silent:?}"
    );

    trigger.trigger();
}

#[tokio::test]
async fn a_reconnect_does_not_leave_half_a_frame_behind() {
    // The device hangs up after two answers, every time. Without
    // `Driver::on_link` the driver would carry its outstanding request and
    // whatever bytes it had into the next socket, and every reading after the
    // first drop would decode at an offset — plausible, and wrong.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address").to_string();
    // Enough answers for a discovery walk and a reading, and then the socket
    // goes — which is a Wi-Fi bridge, not a broken device.
    tokio::spawn(serve(listener, 2_310, 12));

    let (registry, asset, _site) = registry();
    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::transport::tcp(
        Arc::clone(&registry),
        asset.clone(),
        address,
        signal,
    ));

    // Long enough for several drops and reconnects at a one-second backoff.
    let recovered = until(&registry, 400, |held, now| {
        held.observe(None, now)
            .state
            .asset(&asset)
            .and_then(|m| m.power)
            .is_some_and(|p| (p.get() + 2_310.0).abs() < 1.0)
    })
    .await;
    assert!(
        recovered,
        "after a drop the driver has to rediscover and read the same number, \
         not decode the new stream at the old one's offset"
    );

    trigger.trigger();
}

#[tokio::test]
async fn a_device_that_stops_answering_stops_being_believed() {
    // Not by falling over. The reading ages out, the registry reports the asset
    // as silent, and the guard goes back to its conservative assumption — which
    // is what `SILENCE` is for, and what the screen has to be able to say.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address").to_string();
    tokio::spawn(serve(listener, 2_310, usize::MAX));

    let (registry, asset, _site) = registry();
    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::transport::tcp(
        Arc::clone(&registry),
        asset.clone(),
        address,
        signal.clone(),
    ));

    assert!(
        until(&registry, 100, |held, now| {
            held.observe(None, now).silent.is_empty()
        })
        .await,
        "the device answers to begin with"
    );

    // Stop the transport. Nothing else changes: the driver keeps its last
    // reading, and only its age says anything is wrong.
    trigger.trigger();

    let mut held = registry.lock().await;
    let stale = held.observe(
        None,
        time::OffsetDateTime::now_utc() + hemsd::drivers::SILENCE + time::Duration::seconds(1),
    );
    assert!(
        stale.silent.contains(&asset),
        "a reading older than SILENCE is a device nobody is hearing from, and the \
         household is entitled to be told which one"
    );
    assert!(
        stale.state.asset(&asset).is_none(),
        "and it is not handed to the guard as though it were current"
    );
}

#[tokio::test]
async fn a_grid_driver_with_no_transport_still_runs_its_clock() {
    // The trap this closes is subtler than it looks, and getting it wrong the
    // *other* way is what makes it worth a test.
    //
    // Every transition out of the LPC machine's first state is a timer. A
    // Controllable System nobody ticks therefore sits in `Init` for ever — and
    // in `Init` the effective limit is the **failsafe value**, so a box that
    // simply left an untransported driver alone would hold the household at its
    // § 14a minimum indefinitely, on the strength of an implementation accident
    // rather than anything a network operator asked for.
    //
    // Given its clock, the same machine does what `[LPC-922]` says: with no
    // Energy Guard ever in contact it releases itself, and it reports that. The
    // point is not which answer is more restrictive — it is that the state the
    // box reports is the state the machine is actually in.
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let start = time::OffsetDateTime::now_utc();
    let mut registry = Registry::new();
    registry
        .register(
            Box::new(hems_drv::eebus::Lpc::new(
                AssetId::new("netzanschluss").expect("a valid identifier"),
                hems_drv::eebus::Use::Lpc,
                Power::from_kw(10.5),
                std::time::Duration::from_secs(2 * 3600),
                start,
            )),
            &household.site,
        )
        .expect("a grid driver speaks for the connection point");

    // Untouched: nothing has been reported at all, because a state machine that
    // has not been asked anything has not said anything.
    let _ = registry.drain();
    assert_eq!(registry.limits().steuve_ceiling, None);

    // Two minutes of a clock and no Energy Guard, which is what
    // `transport::clock_only` supplies.
    registry.on_timeout(start + time::Duration::seconds(121));
    let _ = registry.drain();

    assert_eq!(
        registry.limits().steuve_ceiling,
        None,
        "a Controllable System that has never been in contact releases itself \
         rather than holding a limit nobody is maintaining — `[LPC-922]`"
    );
    assert!(
        !registry.limits().in_failsafe,
        "and it is not in the failsafe either: the failsafe is what happens when \
         a guard that *was* talking stops, which is a different event in the \
         Nachweis of `[A1 7.2]`"
    );
    // The link is what says so, and it is what the readiness probe reads.
    assert!(
        registry
            .silent(start + time::Duration::seconds(121))
            .any(|a| a.as_str() == "netzanschluss"),
        "the box has to be able to say it is not in contact with a Steuerbox"
    );
}

#[tokio::test]
async fn a_reduction_a_running_box_lived_through_reaches_its_two_year_record() {
    // The compliance obligation, end to end on the running loop rather than on
    // a simulated day. `[A1 7.2]` is a document about what a household *did*
    // while a reduction was in force and `[A1 7.3]` gives a network operator two
    // years to ask for it — so a box that manages a § 14a household and keeps no
    // record has an obligation it cannot discharge, and nothing about a working
    // screen would say so.
    //
    // The record is built by the control loop as it runs, because one
    // reconstructed afterwards from logs that were never kept is not a record.
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let start = time::OffsetDateTime::now_utc();

    // A Steuerbox that takes control and then reduces the household to 4,2 kW.
    let mut lpc = hems_drv::eebus::Lpc::new(
        AssetId::new("netzanschluss").expect("a valid identifier"),
        hems_drv::eebus::Use::Lpc,
        Power::from_kw(10.5),
        std::time::Duration::from_secs(2 * 3600),
        start,
    );
    let mut t = 0_i64;
    while t <= 120 {
        lpc.on_heartbeat(start + time::Duration::seconds(t));
        t += 60;
    }
    lpc.on_limit(
        &hems_drv::eebus::LimitWrite::active(4_200.0),
        start + time::Duration::seconds(130),
    );

    let mut registry = Registry::new();
    registry
        .register(Box::new(lpc), &household.site)
        .expect("a grid driver speaks for the connection point");
    let now = start + time::Duration::seconds(130);
    let observed = registry.observe(None, now);
    assert_eq!(
        observed.limits.steuve_ceiling,
        Some(Power::from_kw(4.2)),
        "the reduction has to reach the guard's view before anything can record it"
    );

    // What the control loop does with it, in one step: observe, then persist
    // whatever that closed.
    let mut store = hemsd::store::Store::in_memory().expect("a store");
    let mut evidence = hems_grid::evidence::EvidenceRecorder::new();

    let assets: Vec<AssetId> = Vec::new();
    evidence.observe(
        hems_grid::evidence::Observation {
            ceiling: Some(Power::from_kw(4.2)),
            rule: hems_core::setpoint::GuardRule::Lpc,
            mode: hems_grid::para14a::ControlMode::Ems,
            minimum_power: Power::from_kw(10.5),
            netzwirksam: Power::from_kw(3.0),
            applied: true,
        },
        &assets,
        now,
    );
    // …and ninety minutes later it lifts.
    let closed = evidence
        .observe(
            hems_grid::evidence::Observation {
                ceiling: None,
                rule: hems_core::setpoint::GuardRule::Lpc,
                mode: hems_grid::para14a::ControlMode::Ems,
                minimum_power: Power::from_kw(10.5),
                netzwirksam: Power::from_kw(3.0),
                applied: false,
            },
            &assets,
            now + time::Duration::minutes(90),
        )
        .cloned()
        .expect("a reduction that lifts closes its record");

    store
        .put_control_event(&closed)
        .expect("the record has to be writable");

    let kept = store.control_events().expect("and readable back");
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].event.strictest_ceiling(), Power::from_kw(4.2));
    assert!(
        kept[0].event.released_at.is_some(),
        "a closed record says when the reduction ended — an open one is not a Nachweis"
    );
    assert!(
        !store.backlog().expect("a backlog").is_empty(),
        "and it is in the outbox until the fleet acknowledges it, because the \
         household's own copy must not depend on the WAN"
    );
}

#[tokio::test]
async fn what_the_fleet_acknowledges_leaves_the_backlog_and_nothing_else_does() {
    // `Store::backlog` grew for ever, because nothing forwarded. The two halves
    // of that are separate promises and this asserts both: what `histd` takes is
    // marked and stops being a backlog, and what it refuses stays — because the
    // household's own copy must never depend on the WAN, and a row dropped
    // because a service was down is a Nachweis that cannot be produced.
    use std::sync::atomic::{AtomicUsize, Ordering};

    let taken = Arc::new(AtomicUsize::new(0));
    let refuse_after = 2_usize;
    let counter = Arc::clone(&taken);
    let app = axum::Router::new().route(
        "/v1/sites/{site}/events",
        axum::routing::post(move || {
            let counter = Arc::clone(&counter);
            async move {
                // Two land, then the fleet starts refusing — a rolling deploy,
                // an expired token, a full disk.
                if counter.fetch_add(1, Ordering::SeqCst) < refuse_after {
                    axum::http::StatusCode::CREATED
                } else {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let url = format!("http://{}", listener.local_addr().expect("its address"));
    let (serve_signal, serve_trigger) = Shutdown::channel();
    let stop = serve_signal.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(stop.wait())
            .await;
    });

    // Four closed reductions waiting to be forwarded.
    let mut store = hemsd::store::Store::in_memory().expect("a store");
    let start = time::OffsetDateTime::now_utc();
    for i in 0..4 {
        let at = start + time::Duration::hours(i);
        let mut event = hems_grid::evidence::ControlEvent::received(
            hems_core::setpoint::GuardRule::Lpc,
            hems_grid::para14a::ControlMode::Ems,
            Power::from_kw(4.2),
            Power::from_kw(10.5),
            at,
        );
        event.released_at = Some(at + time::Duration::minutes(90));
        store.put_control_event(&event).expect("it stores");
    }
    assert_eq!(store.backlog().expect("a backlog").events, 4);

    let store = Arc::new(Mutex::new(store));
    let outbox = hemsd::runtime::outbox::Outbox::new(&hemsd::runtime::outbox::HistdSettings {
        url: Some(url),
        site: Some("reference-household".into()),
        token: Some(hems_service::Secret::literal("tok-test")),
        every_s: 300,
        batch: 50,
    })
    .expect("a client")
    .expect("a configured histd");

    let forwarded = outbox
        .drain(&store, time::OffsetDateTime::now_utc())
        .await
        .expect("the store is readable");
    serve_trigger.trigger();

    assert_eq!(forwarded, 2, "what the fleet took is what is marked");
    assert_eq!(
        store.lock().await.backlog().expect("a backlog").events,
        2,
        "and what it refused is still the household's to keep — a row dropped \
         because a service was down is a Nachweis nobody can produce"
    );
}

#[tokio::test]
async fn a_boost_reaches_the_arbiter_and_still_loses_to_the_grid() {
    // The one write on the local API, and the reason it is safe. An override is
    // a *desire*: the arbiter reads it first and then narrows it into whatever
    // the grid, the fuses and the hardware leave open. So a household can say
    // "charge the car now, I do not care what it costs" — and a household in the
    // middle of a § 14a reduction that says it gets as much as the reduction
    // allows and not a watt more.
    //
    // An endpoint that set a value on a driver instead would have gone round the
    // guard, which is the one property this workspace is built to keep.
    use hemsd::runtime::overrides::Overrides;

    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let now = time::OffsetDateTime::now_utc();

    let overrides = Overrides::new();
    overrides
        .set(
            household.evse.clone(),
            hems_core::setpoint::UserOverride::Boost,
            None,
            now,
        )
        .await;
    let wanted = overrides.active(now).await;
    assert_eq!(wanted.len(), 1, "the boost is in force");

    // The arbiter, with a § 14a reduction to 4,2 kW running.
    let arbiter = hems_realtime::Arbiter::new(hems_realtime::ArbiterConfig::default());
    let limits = hems_realtime::guard::GridLimits {
        steuve_ceiling: Some(Power::from_kw(4.2)),
        steuve_since: Some(now),
        ..hems_realtime::guard::GridLimits::default()
    };
    let state = hems_realtime::guard::SiteState::default();
    let previous = std::collections::BTreeMap::new();
    let delivered = std::collections::BTreeMap::new();
    let phases = std::collections::BTreeMap::new();

    let decision = arbiter.tick(hems_realtime::Tick {
        now,
        site: &household.site,
        state: &state,
        limits: &limits,
        plan: None,
        overrides: &wanted,
        previous: &previous,
        delivered: &delivered,
        phases: &phases,
    });

    let budget = decision
        .verdict
        .steuve_budget
        .expect("a reduction is in force, so there is a budget");
    let car = decision
        .commanded
        .get(&household.evse)
        .copied()
        .unwrap_or(Power::ZERO);
    assert!(
        car <= budget + Power::new(1.0),
        "a boost may ask for everything and still gets only what § 14a leaves: \
         {car:?} against a budget of {budget:?}"
    );

    // …and it is the *boost* that got the budget, not the heat pump: that is
    // what the override is for, and a weighted allocation nobody could steer
    // would be a button that does nothing.
    let heat_pump = decision
        .commanded
        .get(&household.heat_pump)
        .copied()
        .unwrap_or(Power::ZERO);
    assert!(
        car > heat_pump,
        "the asset the household asked for has to win the share it can win: \
         car {car:?}, heat pump {heat_pump:?}"
    );
}
