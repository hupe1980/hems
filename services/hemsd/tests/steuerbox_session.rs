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
use hemsd::drivers::Registry;
use tokio::sync::Mutex;

/// The household's own § 14a driver, in a registry, as `hemsd run` builds it.
fn household() -> (Arc<Mutex<Registry>>, AssetId, hemsd::Household) {
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household");
    let asset = AssetId::new("netzanschluss").expect("a valid identifier");
    let mut registry = Registry::new();
    registry
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
    (Arc::new(Mutex::new(registry)), asset, household)
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
    let (registry, asset, _household) = household();

    // ── The box, as `hemsd run` starts it ───────────────────────────────────
    //
    // No store, so the identity is fresh — which is the case the daemon warns
    // about and is exactly right here: what matters is that the SKI it produces
    // is the one the Steuerbox has to trust.
    let settings = hemsd::runtime::ship::ShipSettings {
        listen: Some("127.0.0.1:0".into()),
        ..hemsd::runtime::ship::ShipSettings::default()
    };
    let (node, box_ski) =
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
        node,
        listener,
        Arc::clone(&registry),
        asset.clone(),
        signal,
    ));

    // ── The Steuerbox dials, discovers, binds and writes 4,2 kW ─────────────
    let (engine, mut guard) = steuerbox();
    let mut hub = Hub::new(guard_node, engine);
    // Bounded: an unanswered handshake is a defect, and a test that waits for
    // ever reports it as a hang rather than a failure.
    tokio::time::timeout(std::time::Duration::from_secs(20), hub.connect(address))
        .await
        .expect("the handshake has to finish")
        .expect("the SHIP handshake");

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
    let (registry, asset, _household) = household();
    let settings = hemsd::runtime::ship::ShipSettings {
        ship_id: "hems_untrusted-test".into(),
        ..hemsd::runtime::ship::ShipSettings::default()
    };
    let (node, _ski) =
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
        node,
        listener,
        Arc::clone(&registry),
        asset,
        signal,
    ));

    let (engine, mut guard) = steuerbox();
    let mut hub = Hub::new(stranger, engine);
    // TLS and the WebSocket succeed; SHIP holds the peer in the pending state —
    // which for an untrusted peer may mean the handshake never completes at all,
    // so this is bounded and either outcome is acceptable. What is *not*
    // acceptable is a limit arriving, and that is what is asserted below.
    let connected =
        tokio::time::timeout(std::time::Duration::from_secs(5), hub.connect(address)).await;

    if matches!(connected, Ok(Ok(_))) {
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
