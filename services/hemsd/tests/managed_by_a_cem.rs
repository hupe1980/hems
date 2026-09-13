//! A Customer Energy Manager drives this household, over a real socket.
//!
//! `hems-flex` had the whole of S2 (EN 50491-12-2) — the descriptions, the
//! session, the instruction arithmetic — and until `hemsd::runtime::s2` existed
//! **nothing called any of it**. That is the failure mode this workspace keeps
//! finding in itself and the one its README puts first: a module that is
//! implemented, cited, tested and reached by nothing, which no property test
//! catches because a property is a statement about code that runs.
//!
//! So the defence is this file. A real WebSocket, a CEM on the other end of it,
//! the handshake S2 specifies, and an instruction that has to come out the far
//! side as something the arbiter would act on. A regression anywhere in that
//! chain — the route, the JSON binding, the session, the decode, the shared map
//! — fails here rather than on somebody's bench.
//!
//! # What this does **not** prove (R32)
//!
//! The CEM is `s2energy`'s own connector and so is the surface it dials, so both
//! ends of this socket are the same crate. What the test holds is that hems's
//! *use* of S2 is coherent end to end; what it cannot see is `s2energy` reading
//! the standard wrongly, because both sides would read it wrongly together. The
//! serialisation is generated from the official JSON Schema rather than hand-
//! written, which narrows the gap and does not close it — a schema can be
//! generated faithfully and still be wired up by a hand-written session that is
//! not.
//!
//! Closing it needs an implementation that is not ours on one end:
//! `flexiblepower/s2-analyzer` validates a live connection against the
//! standard's own schemas, and `s2-python` is a second stack. Until one of them
//! is in CI this file is the EEBUS blind spot of D119 with a different protocol
//! on it, and saying so here is cheaper than discovering it at a test event.

use std::sync::Arc;

use hems_core::prelude::{AssetId, Power};
use hemsd::drivers::Registry;
use hemsd::runtime::s2::{Cem, S2Settings, Surface};
use s2energy::common::{
    ControlType, EnergyManagementRole, Handshake, HandshakeResponse, Message, SelectControlType,
};
use s2energy::transport::websockets_json::connect_as_client;
use tokio::sync::Mutex;

/// The token this box answers to.
const TOKEN: &str = "a-token-only-this-household-has";

/// The box, serving **the surfaces `main` serves**, on a loopback port.
///
/// Through `runtime::surfaces` rather than by assembling a router of its own,
/// and that is load-bearing: the security property is a layer over the whole
/// assembly, so a test that built its own arrangement of the same parts would
/// pass while the product shipped an open port. `hems_service::Server` adds only
/// the health surface on top, which is deliberately outside the gate.
async fn a_box_that_can_be_managed() -> (String, Cem, hems_core::prelude::Site) {
    let (address, cem, site, _) = a_box_with_its_access().await;
    (address, cem, site)
}

/// The same box, with the credential set it answers to.
async fn a_box_with_its_access() -> (
    String,
    Cem,
    hems_core::prelude::Site,
    hemsd::runtime::access::LocalAccess,
) {
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household is a valid site");
    let site = household.site.clone();
    let cem = Cem::new();
    let registry = Arc::new(Mutex::new(Registry::new()));
    let surface = Surface::new(
        Arc::new(site.clone()),
        Arc::clone(&registry),
        cem.clone(),
        S2Settings {
            enabled: true,
            ..S2Settings::default()
        },
    );
    let local = hemsd::runtime::api::Local::new(
        Arc::new(Mutex::new(hemsd::runtime::control::Status::default())),
        site.id.to_string(),
        None,
        hemsd::runtime::overrides::Overrides::new(),
        None,
        hemsd::runtime::access::LocalAccess::for_testing(&site.id.to_string(), TOKEN),
        None,
    );
    // The set the server answers to is the set a test connects a manager to:
    // two of them would be a test about an arrangement the box does not serve.
    let access = hemsd::runtime::access::LocalAccess::for_testing(&site.id.to_string(), TOKEN)
        .remembering(Some(std::sync::Arc::new(Mutex::new(
            hemsd::store::Store::in_memory().expect("a store"),
        ))));
    let router = hemsd::runtime::surfaces(local, Some(surface), access.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let address = listener.local_addr().expect("its address").to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (address, cem, site, access)
}

/// A CEM's connection request, carrying the household's own credential.
///
/// S2's binding is JSON over WebSockets and says nothing about authentication,
/// so the credential rides in the ordinary `Authorization` header — the same one
/// every other surface in this workspace uses, rather than a scheme invented for
/// this socket.
fn as_the_household(address: &str, asset: &AssetId) -> http::Request<()> {
    request_to(address, asset, Some(TOKEN))
}

/// The same, with whatever credential — or none.
fn request_to(address: &str, asset: &AssetId, token: Option<&str>) -> http::Request<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    let mut request = format!("ws://{address}/s2/{asset}")
        .into_client_request()
        .expect("a valid websocket handshake");
    if let Some(token) = token {
        request.headers_mut().insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {token}")).expect("a valid header"),
        );
    }
    request
}

/// Now, in the calendar `s2energy`'s generated types use.
///
/// S2's schema puts instants in `chrono`; hems is a `time` workspace throughout
/// (`hems-flex` converts at its own edge). Rather than adding a second date
/// library to this daemon for one field, the instant is built from the Unix
/// second both agree about.
fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp_nanos(
        i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(0),
    )
}

/// The identifier of the reference household's battery.
fn battery(site: &hems_core::prelude::Site) -> AssetId {
    site.assets
        .iter()
        .find_map(|a| match a {
            hems_core::prelude::Asset::Battery(b) => Some(b.meta.id.clone()),
            _ => None,
        })
        .expect("the reference household has a battery")
}

/// Wait for the box to say something, acknowledging it, and fail rather than
/// hang.
///
/// `receive_and_confirm` is the manager's side of the rule the box keeps too:
/// every S2 message but a `ReceptionStatus` is answered with one. A test CEM
/// that read without acknowledging would be testing the box against a peer that
/// does not exist.
async fn next_message<T>(connection: &mut s2energy::connection::S2Connection<T>) -> Message
where
    T: s2energy::transport::S2Transport,
{
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        connection
            .receive_and_confirm()
            .await
            .expect("the box to stay on the wire")
    })
    .await
    .expect("the box to answer within five seconds")
}

/// The whole conversation, and then the thing it was for.
///
/// S2's order is not negotiable and each step of it is a place this could be
/// wrong: the **Resource Manager speaks first** (its handshake is the one that
/// must carry a version list, because it is the constrained side), the manager
/// picks a version, the box answers with what it is, the manager selects a
/// control type, and only then is there anything to instruct.
#[tokio::test]
async fn a_manager_connects_selects_a_control_type_and_instructs_the_battery() {
    let (address, cem, site) = a_box_that_can_be_managed().await;
    let battery = battery(&site);

    // The path is what names the resource: no S2 message carries a resource
    // identifier, so one connection cannot carry a whole house.
    let mut connection = connect_as_client(as_the_household(&address, &battery))
        .await
        .expect("the box to accept a manager");

    // 1. The box speaks first.
    let Message::Handshake(handshake) = next_message(&mut connection).await else {
        panic!("a Resource Manager opens an S2 conversation with its own handshake");
    };
    assert_eq!(handshake.role, EnergyManagementRole::Rm);
    assert!(
        !handshake.supported_protocol_versions.is_empty(),
        "S2 makes the version list mandatory for the RM: it is the constrained side"
    );

    // 2. The manager answers, choosing a version the box offered.
    connection
        .send_message(
            Handshake::builder()
                .role(EnergyManagementRole::Cem)
                .supported_protocol_versions(vec![s2energy::s2_schema_version().to_string()])
                .build(),
        )
        .await
        .expect("the manager's handshake to go out");
    connection
        .send_message(HandshakeResponse::new(
            s2energy::s2_schema_version().to_string(),
        ))
        .await
        .expect("the manager's version selection to go out");

    // 3. The box says what it is. The reception statuses for the two messages
    //    above arrive first — S2 acknowledges everything but an acknowledgement.
    let details = loop {
        match next_message(&mut connection).await {
            Message::ResourceManagerDetails(details) => break details,
            Message::ReceptionStatus(_) => {}
            other => panic!("unexpected before the details: {other:?}"),
        }
    };
    assert!(
        details
            .available_control_types
            .contains(&ControlType::FillRateBasedControl),
        "a battery is a store with a fill level and a rate, so it is FRBC: {:?}",
        details.available_control_types
    );

    // 4. The manager selects it, and the box sends the system description that
    //    tells it what the actuator and the two operation modes are called.
    connection
        .send_message(SelectControlType::new(ControlType::FillRateBasedControl))
        .await
        .expect("the selection to go out");

    let description = loop {
        match next_message(&mut connection).await {
            Message::FrbcSystemDescription(description) => break description,
            Message::ReceptionStatus(_) | Message::PowerMeasurement(_) => {}
            other => panic!("unexpected before the system description: {other:?}"),
        }
    };
    let actuator = description
        .actuators
        .first()
        .expect("a battery has one actuator");
    // The charge mode. `describe_battery` puts the modes in a known order, and
    // reading it off the description rather than assuming an identifier is the
    // whole point of a standard that names things by ID.
    let charge = actuator
        .operation_modes
        .iter()
        .find(|m| m.elements.iter().any(|e| e.fill_rate.end_of_range > 0.0))
        .expect("a battery has a mode that fills it");

    // Nothing is being asked of the household yet, and the box says so.
    assert!(
        cem.active(time::OffsetDateTime::now_utc()).await.is_empty(),
        "a session that has selected a control type has not yet instructed anything"
    );

    // 5. …and the thing all of that was for.
    connection
        .send_message(
            s2energy::frbc::Instruction::builder()
                .id(s2energy::common::Id::generate())
                .actuator_id(actuator.id.clone())
                .operation_mode(charge.id.clone())
                .operation_mode_factor(0.5)
                .execution_time(now_utc())
                .abnormal_condition(false)
                .build(),
        )
        .await
        .expect("the instruction to go out");

    // The box accepts it on the wire *and* acts on it, which are two different
    // answers to two different questions — "I read it" is about the socket and
    // "I will do it" is about the household.
    let accepted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let held = cem.active(time::OffsetDateTime::now_utc()).await;
            if let Some(request) = held.get(&battery) {
                return *request;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the instruction to reach the map the arbiter reads");

    let power = accepted.power.expect("an FRBC instruction is a rate");
    assert!(
        power > Power::ZERO,
        "half of the charge mode is a positive draw in the load convention, not {power}"
    );
    assert!(
        accepted.is_live(time::OffsetDateTime::now_utc()),
        "and it applies now"
    );
    assert!(
        !accepted.is_live(time::OffsetDateTime::now_utc() + time::Duration::hours(2)),
        "and it stops applying on its own: a manager that goes quiet must not \
         hold this household for as long as the box runs"
    );

    let status = cem.status(time::OffsetDateTime::now_utc()).await;
    assert_eq!(status.connected, vec![battery.to_string()]);
    assert_eq!(status.instructing, vec![battery.to_string()]);
    assert_eq!(status.refused, 0, "nothing here was refused");
}

/// A manager addressing an actuator this household does not have is told so, and
/// the box counts it.
///
/// Reported rather than swallowed: the refusal is invisible from the manager's
/// side, because every one of its messages is being answered politely. A CEM
/// that keeps instructing an actuator that is not here is a commissioning fault,
/// and a counter is the only thing that says so.
#[tokio::test]
async fn an_instruction_for_an_actuator_nobody_described_is_refused_and_counted() {
    let (address, cem, site) = a_box_that_can_be_managed().await;
    let battery = battery(&site);
    let mut connection = connect_as_client(as_the_household(&address, &battery))
        .await
        .expect("the box to accept a manager");

    let _ = next_message(&mut connection).await;
    connection
        .send_message(
            Handshake::builder()
                .role(EnergyManagementRole::Cem)
                .supported_protocol_versions(vec![s2energy::s2_schema_version().to_string()])
                .build(),
        )
        .await
        .expect("the handshake to go out");
    connection
        .send_message(HandshakeResponse::new(
            s2energy::s2_schema_version().to_string(),
        ))
        .await
        .expect("the version to go out");
    loop {
        if matches!(
            next_message(&mut connection).await,
            Message::ResourceManagerDetails(_)
        ) {
            break;
        }
    }
    connection
        .send_message(SelectControlType::new(ControlType::FillRateBasedControl))
        .await
        .expect("the selection to go out");

    connection
        .send_message(
            s2energy::frbc::Instruction::builder()
                .id(s2energy::common::Id::generate())
                .actuator_id(s2energy::common::Id::generate())
                .operation_mode(s2energy::common::Id::generate())
                .operation_mode_factor(0.5)
                .execution_time(now_utc())
                .abnormal_condition(false)
                .build(),
        )
        .await
        .expect("the instruction to go out");

    let refused = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = cem.status(time::OffsetDateTime::now_utc()).await;
            if status.refused > 0 {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the refusal to be counted");

    assert_eq!(refused.refused, 1);
    assert!(
        cem.active(time::OffsetDateTime::now_utc()).await.is_empty(),
        "and nothing the household does changed because of it"
    );
}

/// A manager asking about an asset this household does not have is refused at
/// the door.
///
/// Before a socket is upgraded, which is the cheap place to say no: a WebSocket
/// that completes and then discovers it has nothing to manage is a manager that
/// believes it is connected to something.
#[tokio::test]
async fn there_is_nothing_to_manage_on_an_asset_this_household_does_not_have() {
    let (address, _cem, _site) = a_box_that_can_be_managed().await;
    assert!(
        connect_as_client(as_the_household(
            &address,
            &AssetId::new("a-device-nobody-owns").expect("a valid identifier")
        ))
        .await
        .is_err(),
        "an unknown asset is a 404 rather than an S2 session about nothing"
    );
}

/// A § 14a reduction overrides an instruction, and the manager is **told**.
///
/// The half of an S2 implementation that is usually left out, and leaving it out
/// is not cosmetic. An instruction is answered `Accepted` the moment it decodes,
/// which is an answer about the *wire*; whether the household actually carries it
/// out is a different question, and a network operator's reduction arrives after
/// the first answer has been given. A manager that is never told has sold
/// flexibility the grid took back, and `[BK6-22-300 A1 4.6 S. 3]` is what took it.
///
/// The control loop is not run here — this drives the seam directly, because what
/// is under test is that the override *reaches the wire*, and the arbiter's own
/// precedence is pinned where it is decided (`hems-realtime`).
#[tokio::test]
async fn a_manager_is_told_when_the_grid_overrides_its_instruction() {
    let (address, cem, site) = a_box_that_can_be_managed().await;
    let battery = battery(&site);
    let mut connection = connect_as_client(as_the_household(&address, &battery))
        .await
        .expect("the box to accept a manager");

    let _ = next_message(&mut connection).await;
    connection
        .send_message(
            Handshake::builder()
                .role(EnergyManagementRole::Cem)
                .supported_protocol_versions(vec![s2energy::s2_schema_version().to_string()])
                .build(),
        )
        .await
        .expect("the handshake to go out");
    connection
        .send_message(HandshakeResponse::new(
            s2energy::s2_schema_version().to_string(),
        ))
        .await
        .expect("the version to go out");
    loop {
        if matches!(
            next_message(&mut connection).await,
            Message::ResourceManagerDetails(_)
        ) {
            break;
        }
    }
    connection
        .send_message(SelectControlType::new(ControlType::FillRateBasedControl))
        .await
        .expect("the selection to go out");

    let description = loop {
        if let Message::FrbcSystemDescription(description) = next_message(&mut connection).await {
            break description;
        }
    };
    let actuator = description.actuators.first().expect("one actuator");
    let charge = actuator
        .operation_modes
        .iter()
        .find(|m| m.elements.iter().any(|e| e.fill_rate.end_of_range > 0.0))
        .expect("a mode that fills it");

    connection
        .send_message(
            s2energy::frbc::Instruction::builder()
                .id(s2energy::common::Id::generate())
                .actuator_id(actuator.id.clone())
                .operation_mode(charge.id.clone())
                .operation_mode_factor(1.0)
                .execution_time(now_utc())
                .abnormal_condition(false)
                .build(),
        )
        .await
        .expect("the instruction to go out");

    // Wait until the box has taken it, then let the guard take it back — which
    // is what the control loop does on the tick a § 14a ceiling arrives.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !cem
            .active(time::OffsetDateTime::now_utc())
            .await
            .contains_key(&battery)
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the instruction to be taken");

    cem.note_overrides(
        [(battery.clone(), "§ 14a LPC".to_string())]
            .into_iter()
            .collect(),
    )
    .await;

    // And it arrives on the wire as an `InstructionStatusUpdate`, not as silence.
    let update = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Message::InstructionStatusUpdate(update) = next_message(&mut connection).await
                && update.status_type == s2energy::common::InstructionStatus::Aborted
            {
                return update;
            }
        }
    })
    .await
    .expect("the manager to be told its instruction was overridden");

    assert_eq!(
        update.status_type,
        s2energy::common::InstructionStatus::Aborted,
        "S2's `ABORTED` is *started and could not be completed*, which is exactly \
         what a network operator's reduction does to an accepted instruction"
    );

    // Once, not once a second: a reduction lasts minutes and a status update on
    // every report tick would be a Resource Manager shouting.
    assert!(
        cem.status(time::OffsetDateTime::now_utc())
            .await
            .overridden
            .contains_key(&battery.to_string()),
        "and the household's own screen says so too, for as long as it lasts"
    );
}

/// Nothing this box serves about a household is served without the household's
/// own credential.
///
/// The gate is a **layer over the whole assembly** rather than a check in each
/// handler, so what this asserts is the assembly: every surface `main` puts up,
/// refused without a token and answered with one. A route added tomorrow is
/// covered by a decision nobody has to remember to repeat — which is the defect
/// `obsd` shipped when four call sites each spelled the test themselves (D112).
///
/// The § 14a properties never depended on this: everything both surfaces produce
/// is a *desire* the guard narrows. What depended on it is a household's
/// electricity — `/v1/status` says what every device is doing, `/v1/series` is
/// the Data Act's local API — and its right to decide who drives its house.
#[tokio::test]
async fn the_box_answers_nothing_about_a_household_without_its_credential() {
    let (address, _cem, site) = a_box_that_can_be_managed().await;
    let battery = battery(&site);
    let client = reqwest::Client::new();

    for path in ["/v1/status", "/v1/overrides", "/v1/pairing"] {
        let url = format!("http://{address}{path}");

        let open = client.get(&url).send().await.expect("the box to answer");
        assert_eq!(
            open.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path} answered a caller with no credential"
        );

        let wrong = client
            .get(&url)
            .bearer_auth("not-this-household's-token")
            .send()
            .await
            .expect("the box to answer");
        assert_eq!(
            wrong.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path} answered a credential nobody issued"
        );

        let right = client
            .get(&url)
            .bearer_auth(TOKEN)
            .send()
            .await
            .expect("the box to answer");
        assert!(
            right.status().is_success(),
            "{path} refused the household's own credential: {}",
            right.status()
        );
    }

    // The write, which is the one that changes what the arbiter wants.
    let override_url = format!("http://{address}/v1/overrides/{battery}");
    assert_eq!(
        client
            .put(&override_url)
            .json(&serde_json::json!({"what": "pause"}))
            .send()
            .await
            .expect("the box to answer")
            .status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "anybody on the network could pause this household's battery"
    );

    // …and the S2 socket, which is the one that lets somebody else drive.
    assert!(
        connect_as_client(request_to(&address, &battery, None))
            .await
            .is_err(),
        "a Customer Energy Manager with no credential opened a session"
    );
    assert!(
        connect_as_client(request_to(&address, &battery, Some("a-guess")))
            .await
            .is_err(),
        "a Customer Energy Manager with the wrong credential opened a session"
    );
}

/// A household can see **which** manager is driving its house, and withdraw one.
///
/// The § 14a side has had this since the SHIP trust store: a Steuerbox is
/// trusted by SKI, shown on a screen and forgotten from one. A Customer Energy
/// Manager drove the same house holding the household's *own* credential, so a
/// household could see that something was driving its battery and not what — and
/// could revoke it only by rotating the token every other surface uses (D188).
///
/// Four claims, and the last is the one that makes it a revocation rather than a
/// label: the credential stops working, and the household's own does not.
#[tokio::test]
async fn a_manager_is_named_and_can_be_withdrawn() {
    let household = hemsd::Household::build(&hemsd::HouseholdConfig::default())
        .expect("the reference household is a valid site");
    let site = household.site.id.to_string();
    let temporary = std::env::temp_dir().join(format!(
        "hems-managers-{}.redb",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    ));
    let store = std::sync::Arc::new(Mutex::new(
        hemsd::store::Store::open(&temporary).expect("a fresh store"),
    ));
    let access = hemsd::runtime::access::LocalAccess::for_testing(&site, TOKEN)
        .remembering(Some(std::sync::Arc::clone(&store)));

    // 1. Nothing is connected until a household connects something.
    assert!(
        access
            .managers()
            .await
            .expect("a readable store")
            .is_empty()
    );

    // 2. Connecting one hands back a credential, once.
    let issued = access
        .connect_manager("aggregator-nord")
        .await
        .expect("a manager can be connected");
    assert_ne!(issued, TOKEN, "a manager does not get the household's own");
    let named = access.managers().await.expect("a readable store");
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].name, "aggregator-nord");

    // 3. It opens the box, and it is a *named* principal — which is the whole
    //    point: the subject is what a household reads on a screen.
    let authority = access
        .authority(Some(&format!("Bearer {issued}")))
        .await
        .expect("the credential the box issued is one it accepts");
    assert_eq!(authority.subject(), "cem:aggregator-nord");
    assert!(authority.may_read(&site));

    // …and it survives a restart, because a household that had to re-connect its
    // aggregator after a power cut is one whose contract depends on somebody
    // being at home.
    let after_reboot = hemsd::runtime::access::LocalAccess::for_testing(&site, TOKEN)
        .remembering(Some(std::sync::Arc::clone(&store)));
    assert_eq!(
        after_reboot.restore().await.expect("a readable store"),
        1,
        "a connected manager has to survive a reboot"
    );
    assert!(
        after_reboot
            .authority(Some(&format!("Bearer {issued}")))
            .await
            .is_some()
    );

    // 4. Withdrawing it stops the credential working — and leaves the
    //    household's own alone, which is what "without rotating everything" means.
    assert!(
        access
            .forget_manager("aggregator-nord")
            .await
            .expect("a writable store")
    );
    assert!(
        access
            .authority(Some(&format!("Bearer {issued}")))
            .await
            .is_none(),
        "a withdrawn manager went on driving the house"
    );
    assert!(
        access
            .authority(Some(&format!("Bearer {TOKEN}")))
            .await
            .is_some(),
        "withdrawing a manager must not lock the household out of its own box"
    );
    assert!(
        access
            .managers()
            .await
            .expect("a readable store")
            .is_empty()
    );

    drop(store);
    let _ = std::fs::remove_file(&temporary);
}

/// A manager drives devices; it does not take the household's Data Act export.
///
/// Article 4 of Regulation (EU) 2023/2854 is a right of the *user*, and the
/// one-second series says when they showered, cooked and went away. It is the
/// one capability a manager's credential does not carry — and a capability that
/// is granted and never checked is the defect this workspace keeps finding, so
/// this is the check (D188).
#[tokio::test]
async fn a_manager_may_drive_the_house_and_not_read_its_life() {
    let (address, _cem, site, access) = a_box_with_its_access().await;
    let manager = access
        .connect_manager("aggregator-nord")
        .await
        .expect("a manager can be connected");
    let site = site.id.to_string();

    // The capability, and then the route that has to enforce it — a capability
    // granted and never checked is the defect this workspace keeps finding.
    assert!(
        access
            .authority(Some(&format!("Bearer {TOKEN}")))
            .await
            .expect("the household's own credential")
            .may_read_everything(&site),
        "the export is the user's right"
    );
    assert!(
        !access
            .authority(Some(&format!("Bearer {manager}")))
            .await
            .expect("the manager's credential")
            .may_read_everything(&site),
        "a manager must not be able to take the household's Data Act export"
    );

    let client = reqwest::Client::new();
    let series = format!("http://{address}/v1/series/grid");
    assert_eq!(
        client
            .get(&series)
            .bearer_auth(&manager)
            .send()
            .await
            .expect("the box to answer")
            .status(),
        reqwest::StatusCode::FORBIDDEN,
        "a manager reached the one-second series, which says when this household \
         showered, cooked and went away"
    );
    // …and it can still do the thing it is for.
    assert!(
        client
            .get(format!("http://{address}/v1/status"))
            .bearer_auth(&manager)
            .send()
            .await
            .expect("the box to answer")
            .status()
            .is_success()
    );
}
