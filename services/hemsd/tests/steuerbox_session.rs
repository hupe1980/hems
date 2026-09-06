//! A network operator's Steuerbox limits the household, over a real socket.
//!
//! This is the last seam. `crates/hems-drv/tests/eebus_spine.rs` proves the
//! protocol with the datagrams moved by hand; this one puts TCP, TLS 1.2 with
//! mutual authentication, a WebSocket upgrade and the SHIP handshake underneath
//! and runs `hemsd`'s own listener on top — so what is being asserted is that a
//! § 14a reduction reaches the guard of a running box.
//!
//! The Energy Guard here is `eebus`'s own actor, which is what a certifiable
//! Steuerbox is: writing a second one for the test would test the second one.

use std::sync::Arc;

use eebus::model::{DeviceType, EntityType};
use eebus::runtime::{Hub, HubEvent, Node, TrustStore, TrustedPeer};
use eebus::spine::{Engine, LocalDevice, LocalEntity};
use eebus::tls::ShipTls;
use eebus::usecases::limitation::{self, EnergyGuardActor, GuardEvent, LimitWrite};
use eebus::usecases::lpc;
use hems_core::prelude::{AssetId, Power};
use hems_service::Shutdown;
use hemsd::drivers::{Attached, DriverId, Registry};
use tokio::sync::Mutex;

/// The household's own § 14a driver, in a registry, as `hemsd run` builds it.
/// Settings that announce nothing.
///
/// Two reasons, and the second is the one that would bite: a `cargo test` that
/// registered a `_ship._tcp` service on the developer's own network is a side
/// effect nobody asked for, and two tests running at once would both announce
/// under the same instance name.
fn quiet() -> hemsd::runtime::ship::ShipSettings {
    hemsd::runtime::ship::ShipSettings {
        announce: false,
        ..hemsd::runtime::ship::ShipSettings::default()
    }
}

fn household() -> (Arc<Mutex<Registry>>, DriverId, AssetId, hemsd::Household) {
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let asset = AssetId::new("netzanschluss").expect("a valid identifier");
    let mut registry = Registry::new();
    let driver = registry
        .register(
            Box::new(hems_drv::eebus::Lpc::new(
                asset.clone(),
                hems_drv::eebus::Use::Lpc,
                // The household's own § 14a minimum, not a vendor default.
                Power::from_kw(10.5),
                std::time::Duration::from_secs(2 * 3600),
                time::OffsetDateTime::now_utc(),
            )),
            &household.site,
        )
        .expect("a grid driver speaks for the connection point");
    (Arc::new(Mutex::new(registry)), driver, asset, household)
}

/// The network operator's box: an Energy Guard on a `GridGuard` entity.
fn steuerbox() -> (Engine, EnergyGuardActor) {
    let mut device = LocalDevice::new("n:dso", "Steuerbox-1", DeviceType::ElectricitySupplySystem)
        .expect("a valid device address");
    device
        .add_entity(
            LocalEntity::new([1], EntityType::GridGuard)
                .with_feature(limitation::client_feature(1))
                .with_feature(limitation::device_diagnosis_feature(2)),
        )
        .expect("a fresh entity");
    let client = device.address_of(&[1], 1);
    let diagnosis = device.address_of(&[1], 2);
    let mut engine = Engine::new(device);
    engine.add_use_case([1], 1, &lpc::ENERGY_GUARD);
    let actor = EnergyGuardActor::new(
        lpc::DIRECTION,
        client,
        diagnosis,
        core::time::Duration::ZERO,
    );
    (engine, actor)
}

#[tokio::test]
async fn a_steuerbox_over_tls_reduces_a_running_household() {
    let (registry, driver, asset, _household) = household();

    // ── The box, as `hemsd run` starts it ───────────────────────────────────
    //
    // No store, so the identity is fresh — which is the case the daemon warns
    // about and is exactly right here: what matters is that the SKI it produces
    // is the one the Steuerbox has to trust.
    let settings = hemsd::runtime::ship::ShipSettings {
        listen: Some("127.0.0.1:0".into()),
        ..hemsd::runtime::ship::ShipSettings::default()
    };
    let (node, box_ski, _key) =
        hemsd::runtime::ship::identity(&settings, None, time::OffsetDateTime::now_utc())
            .await
            .expect("a box can always make itself an identity");
    let listener = node.listen("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address");

    // ── Commissioning: the installer approves each end to the other ─────────
    let guard_trust = TrustStore::new();
    let guard_node = Node::new(
        "n:dso_Steuerbox-1",
        ShipTls::new(
            eebus::cert::self_signed(eebus::cert::CertParams::new("n:dso_Steuerbox-1"))
                .expect("a certificate"),
        ),
        guard_trust.clone(),
    );
    guard_trust.remember(TrustedPeer::new(box_ski));
    node.trust_store()
        .remember(TrustedPeer::new(guard_node.ski()));

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::ship::run(
        Arc::new(node),
        listener,
        Arc::clone(&registry),
        Attached {
            driver,
            asset: asset.clone(),
        },
        // No announcement: the test dials a port it already knows, and a box
        // that registered itself on the developer's own network for the length
        // of a `cargo test` is a side effect nobody asked for.
        quiet(),
        box_ski,
        signal,
    ));

    // ── The Steuerbox dials, discovers, binds and writes 4,2 kW ─────────────
    let (engine, mut guard) = steuerbox();
    let mut hub = Hub::new(guard_node, engine);
    // `dial` is a request rather than a round trip since `eebus` 0.4: the hub
    // owns the connection and says so with `HubEvent::Connected`, which is also
    // where the negotiated SHIP version arrives. Bounded, because an unanswered
    // handshake is a defect and a test that waits for ever reports it as a hang
    // rather than a failure.
    hub.dial(address);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match hub.next().await.expect("the hub keeps running") {
                HubEvent::Connected { version, .. } => break version,
                HubEvent::HandshakeFailed { error, .. } => {
                    panic!("the SHIP handshake: {error}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the handshake has to finish");

    let mut required = Some(LimitWrite::active(4_200.0));
    let mut accepted = false;
    for _ in 0..256 {
        let now = hub.now();
        let event = match tokio::time::timeout(std::time::Duration::from_secs(10), hub.next()).await
        {
            Ok(Ok(event)) => event,
            _ => break,
        };
        let mut reports = Vec::new();
        match event {
            HubEvent::PeerDiscovered { device, .. } => {
                let remote = hub.engine().peer(&device).expect("the peer just heard");
                let peer = limitation::locate(remote, lpc::DIRECTION)
                    .expect("the household plays the Controllable System");
                guard.attach(hub.engine_mut(), peer, now);
                if let Some(limit) = required.take() {
                    guard.require(&device, Some(limit), now);
                }
            }
            HubEvent::Spine(event) => {
                reports.extend(guard.handle_event(hub.engine_mut(), &event, now));
            }
            HubEvent::Tick => reports = guard.handle_timeout(hub.engine_mut(), now),
            HubEvent::Disconnected { .. } => break,
            _ => {}
        }
        for report in reports {
            if let GuardEvent::LimitAccepted { limit, .. } = report {
                assert!((limit.watts - 4_200.0).abs() < 1.0);
                accepted = true;
            }
        }
        if accepted {
            break;
        }
        hub.wake_at(guard.poll_timeout());
    }

    assert!(
        accepted,
        "the network operator has to get an acknowledgement — under § 14a it is \
         the evidence the reduction was received"
    );

    // ── …and it reaches the guard of the running box ────────────────────────
    //
    // The whole point. Everything above is protocol; this is the number the
    // household is actually held to.
    let mut arrived = None;
    for _ in 0..100 {
        let ceiling = registry
            .lock()
            .await
            .observe(None, time::OffsetDateTime::now_utc())
            .limits
            .steuve_ceiling;
        if ceiling.is_some() && ceiling != Some(Power::from_kw(10.5)) {
            arrived = ceiling;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    trigger.trigger();

    assert_eq!(
        arrived,
        Some(Power::from_kw(4.2)),
        "a limit written by a Steuerbox over TLS has to become the ceiling the \
         guard enforces — this is the seam between `the logic is right` and `the \
         house is managed`"
    );
    assert!(
        !registry
            .lock()
            .await
            .observe(None, time::OffsetDateTime::now_utc())
            .limits
            .in_failsafe,
        "and it has to be recorded as the operator asking rather than as the \
         household restraining itself, because those are different events in \
         the Nachweis of `[A1 7.2]`"
    );
}

#[tokio::test]
async fn an_untrusted_steuerbox_completes_tls_and_is_held_short_of_the_data_phase() {
    // SHIP's whole trust model. A peer nobody has approved *has* to get as far
    // as TLS — that is how its SKI becomes visible to a user at all — and must
    // not be able to write a limit. A box that accepted one would let anybody on
    // the household's network reduce the house.
    let (registry, driver, asset, _household) = household();
    let settings = hemsd::runtime::ship::ShipSettings {
        ship_id: "hems_untrusted-test".into(),
        ..hemsd::runtime::ship::ShipSettings::default()
    };
    let (node, box_ski, _key) =
        hemsd::runtime::ship::identity(&settings, None, time::OffsetDateTime::now_utc())
            .await
            .expect("an identity");
    let listener = node.listen("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address");

    // Nobody is trusted, in either direction.
    let stranger = Node::new(
        "n:dso_Stranger-1",
        ShipTls::new(
            eebus::cert::self_signed(eebus::cert::CertParams::new("n:dso_Stranger-1"))
                .expect("a certificate"),
        ),
        TrustStore::new(),
    );

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::ship::run(
        Arc::new(node),
        listener,
        Arc::clone(&registry),
        Attached { driver, asset },
        quiet(),
        box_ski,
        signal,
    ));

    let (engine, mut guard) = steuerbox();
    let mut hub = Hub::new(stranger, engine);
    // TLS and the WebSocket succeed; SHIP holds the peer in the pending state —
    // which for an untrusted peer may mean the handshake never completes at all,
    // so this is bounded and either outcome is acceptable. What is *not*
    // acceptable is a limit arriving, and that is what is asserted below.
    hub.dial(address);
    let connected = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match hub.next().await {
                Ok(HubEvent::Connected { .. }) => break true,
                Ok(HubEvent::HandshakeFailed { .. }) | Err(_) => break false,
                Ok(_) => {}
            }
        }
    })
    .await;

    if matches!(connected, Ok(true)) {
        // Give it every chance to get somewhere, then check it did not.
        for _ in 0..32 {
            let now = hub.now();
            match tokio::time::timeout(std::time::Duration::from_millis(200), hub.next()).await {
                Ok(Ok(HubEvent::Spine(event))) => {
                    let _ = guard.handle_event(hub.engine_mut(), &event, now);
                }
                Ok(Ok(HubEvent::Tick)) => {
                    let _ = guard.handle_timeout(hub.engine_mut(), now);
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
    }
    trigger.trigger();

    let limits = registry
        .lock()
        .await
        .observe(None, time::OffsetDateTime::now_utc())
        .limits;
    assert!(
        limits.steuve_ceiling.is_none() || limits.in_failsafe,
        "an unapproved peer must not be able to put a limit on the household: \
         either nothing arrived, or what is in force is the box's own failsafe"
    );
}

#[tokio::test]
async fn a_steuerbox_approved_while_it_waits_gets_through_without_a_restart() {
    // The § 14a commissioning failure field reports call the most common one
    // there is, closed from this side. An unapproved peer completes TLS — which
    // *proves* its SKI rather than taking its word — and is held in the SHIP
    // pending state; an approval added while it waits lets that same handshake
    // through. So an installer standing in a cellar reads the SKI off the
    // Steuerbox and approves it, and the session that was already waiting
    // completes.
    //
    // The alternative it replaces is the reason this matters: a static list in a
    // TOML file, edited and then restarted — and a box that restarts loses its
    // § 14a session, its plan and its place in the control period.
    let (registry, driver, asset, _household) = household();
    let settings = hemsd::runtime::ship::ShipSettings {
        ship_id: "hems_pairing-test".into(),
        ..quiet()
    };
    let (node, box_ski, key_pem) =
        hemsd::runtime::ship::identity(&settings, None, time::OffsetDateTime::now_utc())
            .await
            .expect("an identity");
    let node = Arc::new(node);
    let listener = node.listen("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address");

    let guard_trust = TrustStore::new();
    let guard_node = Node::new(
        "n:dso_Steuerbox-2",
        ShipTls::new(
            eebus::cert::self_signed(eebus::cert::CertParams::new("n:dso_Steuerbox-2"))
                .expect("a certificate"),
        ),
        guard_trust.clone(),
    );
    // The operator's end has been given this box's SKI. This box has *not* been
    // given the operator's, which is the state an installer is actually in.
    guard_trust.remember(TrustedPeer::new(box_ski));
    let guard_ski = guard_node.ski();

    let trust = hemsd::runtime::ship::Trust::new(
        Arc::clone(&node),
        None,
        settings.ship_id.clone(),
        key_pem,
    );
    assert!(
        trust.peers().is_empty(),
        "nobody has been approved yet, which is the whole point"
    );

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::ship::run(
        node,
        listener,
        Arc::clone(&registry),
        Attached { driver, asset },
        settings,
        box_ski,
        signal,
    ));

    // The Steuerbox dials into a box that does not know it. It is held pending
    // rather than refused — that is what makes an approval possible at all.
    let (engine, _guard) = steuerbox();
    let mut hub = Hub::new(guard_node, engine);
    hub.dial(address);

    // …and while it is waiting, somebody approves it.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    trust
        .approve(
            &guard_ski.to_display_string(),
            time::OffsetDateTime::now_utc(),
        )
        .await
        .expect("a SKI read off the Steuerbox");

    let connected = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match hub.next().await {
                Ok(HubEvent::Connected { .. }) => break true,
                Ok(HubEvent::HandshakeFailed { .. }) | Err(_) => break false,
                Ok(_) => {}
            }
        }
    })
    .await;

    trigger.trigger();
    assert_eq!(
        connected,
        Ok(true),
        "the handshake that was already waiting completed, with no restart"
    );
    assert_eq!(
        trust.peers(),
        vec![guard_ski.to_display_string()],
        "and the approval is what the box will remember"
    );
}

#[tokio::test]
async fn a_mistyped_ski_is_refused_rather_than_stored() {
    // A SKI is forty hexadecimal characters read off a label by somebody in a
    // cellar. One typed wrongly and stored is a peer that will never connect and
    // nothing anywhere that says why — so it is refused at the point where a
    // person can still fix it.
    let settings = hemsd::runtime::ship::ShipSettings {
        ship_id: "hems_typo-test".into(),
        ..quiet()
    };
    let (node, _ski, key_pem) =
        hemsd::runtime::ship::identity(&settings, None, time::OffsetDateTime::now_utc())
            .await
            .expect("an identity");
    let trust =
        hemsd::runtime::ship::Trust::new(Arc::new(node), None, settings.ship_id.clone(), key_pem);

    let err = trust
        .approve("not a ski", time::OffsetDateTime::now_utc())
        .await
        .expect_err("thirty-nine characters and a space is not a SKI");
    assert!(
        matches!(err, hemsd::runtime::ship::ShipError::NotASki(_)),
        "{err}"
    );
    assert!(trust.peers().is_empty(), "and nothing was stored");
}

#[tokio::test]
async fn a_failsafe_the_operator_writes_survives_the_next_power_cut() {
    // `[LPC-021]` makes the failsafe the operator's to change, and §2.15 of the
    // implementation guide makes accepting the change mandatory — because a
    // device stuck on a factory default cannot protect anything. What that
    // implies, and what this asserts, is that the new value has to outlive the
    // process: the failsafe is precisely what restrains the household when there
    // is *no* session, and a box that came back from a power cut on its own
    // configuration file would have quietly undone the operator's write.
    //
    // That is `ATC_LPC_COM_PT_CSInit_003` on the certification list. Until this
    // test the driver discarded every `CsEvent`, so the value lived in one
    // state machine and reached nothing that could write it down.
    let (registry, driver, asset, _household) = household();
    let store = hemsd::store::Store::in_memory().expect("a store");

    let settings = hemsd::runtime::ship::ShipSettings {
        listen: Some("127.0.0.1:0".into()),
        ..hemsd::runtime::ship::ShipSettings::default()
    };
    let (node, box_ski, _key) =
        hemsd::runtime::ship::identity(&settings, None, time::OffsetDateTime::now_utc())
            .await
            .expect("a box can always make itself an identity");
    let listener = node.listen("127.0.0.1:0").await.expect("a port");
    let address = listener.local_addr().expect("its address");

    let guard_trust = TrustStore::new();
    let guard_node = Node::new(
        "n:dso_Steuerbox-2",
        ShipTls::new(
            eebus::cert::self_signed(eebus::cert::CertParams::new("n:dso_Steuerbox-2"))
                .expect("a certificate"),
        ),
        guard_trust.clone(),
    );
    guard_trust.remember(TrustedPeer::new(box_ski));
    node.trust_store()
        .remember(TrustedPeer::new(guard_node.ski()));

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(hemsd::runtime::ship::run(
        Arc::new(node),
        listener,
        Arc::clone(&registry),
        Attached {
            driver,
            asset: asset.clone(),
        },
        quiet(),
        box_ski,
        signal,
    ));

    // ── The operator writes a new failsafe: 7 kW for four hours ─────────────
    //
    // Neither is the configured value, so a box that reported the configuration
    // back would pass nothing here.
    const WRITTEN_W: f64 = 7_000.0;
    const WRITTEN_FOR: core::time::Duration = core::time::Duration::from_secs(4 * 3600);

    let (engine, mut guard) = steuerbox();
    let mut hub = Hub::new(guard_node, engine);
    hub.dial(address);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            match hub.next().await.expect("the hub keeps running") {
                HubEvent::Connected { .. } => break,
                HubEvent::HandshakeFailed { error, .. } => {
                    panic!("the SHIP handshake: {error}")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the handshake has to finish");

    let mut peer = None;
    let mut written = false;
    for _ in 0..256 {
        let now = hub.now();
        let event = match tokio::time::timeout(std::time::Duration::from_secs(10), hub.next()).await
        {
            Ok(Ok(event)) => event,
            _ => break,
        };
        match event {
            HubEvent::PeerDiscovered { device, .. } => {
                let remote = hub.engine().peer(&device).expect("the peer just heard");
                let found = limitation::locate(remote, lpc::DIRECTION)
                    .expect("the household plays the Controllable System");
                guard.attach(hub.engine_mut(), found, now);
                peer = Some(device);
            }
            HubEvent::Spine(event) => {
                let _ = guard.handle_event(hub.engine_mut(), &event, now);
            }
            HubEvent::Tick => {
                let _ = guard.handle_timeout(hub.engine_mut(), now);
            }
            HubEvent::Disconnected { .. } => break,
            _ => {}
        }
        // Only once the description read has told the guard this peer's own
        // `keyId`s: a write addressed by our numbering would land on whichever
        // configuration key that device happens to keep at that index.
        if let Some(device) = peer.as_ref()
            && !written
            && guard
                .write_failsafe_limit(hub.engine_mut(), device, WRITTEN_W, now)
                .is_some()
        {
            guard.write_failsafe_duration(hub.engine_mut(), device, WRITTEN_FOR, now);
            written = true;
        }
        hub.wake_at(guard.poll_timeout());
        if written && reported(&registry).await.is_some() {
            break;
        }
    }
    assert!(
        written,
        "the guard has to learn the peer's own key ids first"
    );

    // ── …and it reaches something that can write it down ────────────────────
    let mut seen = None;
    for _ in 0..100 {
        if let Some(found) = reported(&registry).await {
            seen = Some(found);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    trigger.trigger();

    let seen = seen.expect(
        "a failsafe the operator wrote has to leave the state machine: until it does, \
         nothing in the box can keep it",
    );
    assert!((seen.power.get() - WRITTEN_W).abs() < 1.0);
    assert_eq!(seen.minimum, time::Duration::hours(4));

    // ── The power cut ───────────────────────────────────────────────────────
    store
        .put_eebus_failsafe(
            "consumption",
            &hemsd::store::StoredFailsafe {
                watts: seen.power.get(),
                minimum_s: seen.minimum.whole_seconds(),
            },
            seen.at,
        )
        .expect("the store takes it");

    let settings = hemsd::Settings {
        drivers: vec![hemsd::config::DriverSettings::EebusLpc(
            hemsd::config::EebusSettings {
                asset: "netzanschluss".into(),
                // The configured value, which is what a box that forgot would
                // come back holding.
                failsafe_kw: Some(4.2),
                failsafe_hours: 2,
                spine_vendor: None,
                spine_unique: None,
            },
        )],
        ..hemsd::Settings::default()
    };
    let started = time::OffsetDateTime::now_utc();
    let restarted =
        hemsd::runtime::assemble(&settings, Some(&store), started).expect("the box comes back up");
    // One tick, because the driver comes up in `Init` and `[LPC-911]`'s failsafe
    // is what it holds there — a state it reports on its first timeout, not on
    // being constructed.
    let ceiling = {
        let mut guard = restarted.registry.lock().await;
        guard.on_timeout(started + time::Duration::seconds(1));
        guard
            .observe(None, started + time::Duration::seconds(1))
            .limits
            .steuve_ceiling
    };
    assert_eq!(
        ceiling,
        Some(Power::new(WRITTEN_W)),
        "a box that came back on its configured 4,2 kW would have undone a write \
         the operator is entitled to make, and restrained the household to the \
         wrong number with nobody talking to it"
    );
}

/// The failsafe the box's own driver has reported, if any.
async fn reported(registry: &Arc<Mutex<Registry>>) -> Option<hems_drv::Failsafe> {
    let mut guard = registry.lock().await;
    let _ = guard.observe(None, time::OffsetDateTime::now_utc());
    guard
        .failsafe()
        .get(&hems_drv::LimitDirection::Consumption)
        .copied()
}
