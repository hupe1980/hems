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
//! # What this proves, and what it still does not (R32)
//!
//! The CEM is `s2-kit`'s own [`CemSession`] and so is the surface it dials, so
//! both ends of this socket are the same crate. That much is unchanged.
//!
//! What **did** change when the crate did is that the manager on the other end is
//! now a session engine with a **rule-numbered semantic validator** in front of
//! it, on by default. Every message this box sends is checked against the rule
//! catalogue before the CEM accepts it, and a violation arrives as
//! `CemEvent::Refused { report }` or `CemEvent::Warnings { report }` with the
//! rule that was broken named. `assert_clean` below fails the test on either.
//!
//! That is a third artefact rather than a second opinion: the catalogue is
//! written from the standard's text and is independent of the session logic on
//! *either* side, so "hems sends a description `s2-python` would refuse" is a
//! test failure here rather than a discovery at a test event. A peer that merely
//! decoded would only ever prove the far end did not crash.
//!
//! What is still open is the same shape as D119's EEBUS blind spot: a validator
//! and a session written by the same hand can be wrong together, and no
//! implementation that is not ours has yet read a byte of this.
//! `flexiblepower/s2-analyzer` and `s2-python` are the answers, and the ElaadNL
//! event (28–29 October 2026) is the multi-vendor version.

use std::sync::Arc;

use hems_core::prelude::{AssetId, Power};
use hemsd::drivers::Registry;
use hemsd::runtime::s2::{Cem, S2Settings, Surface};
use s2_kit::io::{Dialled, Driver, WebSocket};
use s2_kit::message::Message;
use s2_kit::session::{CemConfig, CemEvent, CemSession};
use s2_kit::types::Timestamp;
use s2_kit::types::common::ControlType;
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

/// A short name for an event, for a failure message.
fn event_name(e: &CemEvent) -> String {
    match e {
        CemEvent::Refused { kind, report, .. } => format!("REFUSED {kind:?}: {report:?}"),
        CemEvent::Warnings { kind, report } => format!("WARN {kind:?}: {report:?}"),
        other => format!("{other:?}")
            .split_whitespace()
            .next()
            .unwrap_or("?")
            .to_owned(),
    }
}

/// A real Customer Energy Manager, on a real socket.
///
/// `s2-kit`'s own [`CemSession`] driven by its own [`Driver`], rather than a
/// hand-written sequence of frames. That is the point rather than a convenience:
/// the engine performs the handshake in the order S2 specifies, acknowledges what
/// the standard says must be acknowledged, and — the part this file is for —
/// **validates every message the box sends against the rule catalogue** before
/// accepting it. A test that wrote its own frames would be testing the box
/// against a peer that exists nowhere.
///
/// S2's binding says nothing about authentication, so the credential rides in the
/// ordinary `Authorization` header — S2 Connect's own rule, and the same scheme
/// every other surface in this workspace uses. `WebSocket::connect` puts it
/// there, so nothing here builds a handshake request by hand.
struct Manager {
    session: CemSession,
    driver: Driver<Dialled>,
    seen: Vec<CemEvent>,
}

impl Manager {
    /// Dial the box with the household's own credential.
    async fn as_the_household(address: &str, asset: &AssetId) -> Manager {
        Self::connect(address, asset, Some(TOKEN))
            .await
            .expect("the box to accept a manager")
    }

    /// Dial with whatever credential — or none.
    async fn connect(
        address: &str,
        asset: &AssetId,
        token: Option<&str>,
    ) -> Result<Manager, String> {
        let socket = WebSocket::connect(&format!("ws://{address}/s2/{asset}"), token)
            .await
            .map_err(|e| e.to_string())?;
        let mut session = CemSession::new(CemConfig::default());
        session.open(Timestamp::now());
        Ok(Manager {
            session,
            driver: Driver::new(socket),
            seen: Vec::new(),
        })
    }

    /// Connect, let the handshake finish, and select a control type.
    ///
    /// The preamble every test but the credential ones share. It is a method
    /// rather than four copies because the handshake is the **engine's** now: a
    /// test that spelled it out would be asserting the library's behaviour in
    /// seven places instead of hems's in one.
    async fn ready(address: &str, asset: &AssetId, control: ControlType) -> Manager {
        let mut manager = Self::as_the_household(address, asset).await;
        manager
            .until("describe itself", |e| {
                matches!(e, CemEvent::ResourceDescribed(_)).then_some(())
            })
            .await;
        manager
            .session
            .select_control_type(control, Timestamp::now())
            .expect("the selection to go out");
        // …and **wait for it to be active**. The engine validates outbound
        // messages, so an instruction sent before the box has acknowledged the
        // selection is refused at this end with `NotAllowed` — which is the
        // right answer and means a test that raced ahead would be testing the
        // CEM's own guard rather than the box.
        manager
            .until("activate the control type", |e| {
                matches!(e, CemEvent::Ready { .. }).then_some(())
            })
            .await;
        manager
    }

    /// Step the session until `want` matches, or fail rather than hang.
    async fn until<T>(&mut self, what: &str, mut want: impl FnMut(&CemEvent) -> Option<T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                while let Some(event) = self.session.poll_event() {
                    let found = want(&event);
                    self.seen.push(event);
                    if let Some(found) = found {
                        return found;
                    }
                }
                self.driver
                    .step(&mut self.session)
                    .await
                    .expect("the box to stay on the wire");
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the box did not {what} within five seconds; saw: {:?}",
                self.seen.iter().map(event_name).collect::<Vec<_>>()
            )
        })
    }

    /// Let the conversation run on for a moment without waiting for anything.
    ///
    /// What it is for is [`Manager::assert_clean`]: a message the validator
    /// objects to may arrive *after* the thing a test was waiting for, and a test
    /// that stopped reading the moment it had its answer would never see it.
    async fn settle(&mut self) {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            loop {
                while let Some(event) = self.session.poll_event() {
                    self.seen.push(event);
                }
                if self.driver.step(&mut self.session).await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    /// Fail if the validator had anything to say about what the **box** sent.
    ///
    /// The whole value of driving a real CEM. A rule-numbered refusal or warning
    /// against one of this box's own messages is a defect in this product, and it
    /// is one that "the far end did not crash" would never have found (R32).
    ///
    /// There is no allowlist. Every rule in the catalogue is a failure here with
    /// no way to opt out, which is what makes the check mean anything: the one
    /// disagreement this workspace ever had — `S2-RMD-003`, whether a battery may
    /// be storage, load *and* generator for electricity — was settled by the
    /// standard's own `maxItems: 3` on `roles`, and the rule was narrowed to the
    /// `(role, commodity)` pair rather than tolerated here (D201).
    fn assert_clean(&self) {
        for event in &self.seen {
            let (what, report) = match event {
                CemEvent::Refused { kind, report, .. } => (format!("refused {kind:?}"), report),
                CemEvent::Warnings { kind, report } => (format!("objects to {kind:?}"), report),
                _ => continue,
            };
            if let Some(violation) = report.violations().first() {
                panic!(
                    "the manager {what}: [{}] {} at {} — {:?}",
                    violation.rule.0, violation.message, violation.path, violation.severity
                );
            }
        }
    }
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
    let mut manager = Manager::as_the_household(&address, &battery).await;

    // 1–3. The handshake is the **engine's**, in the order S2 specifies: the
    //      Resource Manager speaks first because it is the constrained side, the
    //      manager picks a version, and everything but an acknowledgement is
    //      acknowledged. A regression in hems's ordering is therefore caught by a
    //      peer that knows the rule, not by a script that happened to expect it.
    let details = manager
        .until("describe itself", |e| match e {
            CemEvent::ResourceDescribed(d) => Some(d.clone()),
            _ => None,
        })
        .await;
    assert!(
        details
            .available_control_types
            .contains(&ControlType::FillRateBasedControl),
        "a battery is a store with a fill level and a rate, so it is FRBC: {:?}",
        details.available_control_types
    );

    // 4. The manager selects it, and the box sends the system description that
    //    tells it what the actuator and the two operation modes are called.
    manager
        .session
        .select_control_type(ControlType::FillRateBasedControl, Timestamp::now())
        .expect("the selection to go out");

    let description = manager
        .until("send its system description", |e| match e {
            CemEvent::Description {
                message: Message::FrbcSystemDescription(d),
                ..
            } => Some(d.clone()),
            _ => None,
        })
        .await;
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

    // 5. …and the thing all of that was for. `instruct` validates it against the
    //    rule catalogue on the way out, so an instruction hems could not
    //    legitimately be sent is a failure here rather than a refusal on the wire.
    manager
        .session
        .instruct(
            s2_kit::types::frbc::Instruction::builder()
                .id(s2_kit::types::Id::generate())
                .actuator_id(actuator.id)
                .operation_mode(charge.id)
                .operation_mode_factor(0.5)
                .execution_time(Timestamp::now())
                .abnormal_condition(false)
                .build(),
            Timestamp::now(),
        )
        .expect("the instruction to go out");
    manager.settle().await;

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

    // And the manager's own validator had nothing to say about any of it. This is
    // the assertion the previous library could not carry: every handshake,
    // description, status and measurement this box sent was checked against the
    // rule catalogue, and a violation would name the rule it broke.
    manager.assert_clean();
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
    let mut manager = Manager::ready(&address, &battery, ControlType::FillRateBasedControl).await;

    manager
        .session
        .instruct(
            s2_kit::types::frbc::Instruction::builder()
                .id(s2_kit::types::Id::generate())
                .actuator_id(s2_kit::types::Id::generate())
                .operation_mode(s2_kit::types::Id::generate())
                .operation_mode_factor(0.5)
                .execution_time(Timestamp::now())
                .abnormal_condition(false)
                .build(),
            Timestamp::now(),
        )
        .expect("the instruction to go out");
    manager.settle().await;

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
        Manager::connect(
            &address,
            &AssetId::new("a-device-nobody-owns").expect("a valid identifier"),
            Some(TOKEN)
        )
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
    let mut manager = Manager::ready(&address, &battery, ControlType::FillRateBasedControl).await;

    let description = manager
        .until("send its system description", |e| match e {
            CemEvent::Description {
                message: Message::FrbcSystemDescription(d),
                ..
            } => Some(d.clone()),
            _ => None,
        })
        .await;
    let actuator = description.actuators.first().expect("one actuator");
    let charge = actuator
        .operation_modes
        .iter()
        .find(|m| m.elements.iter().any(|e| e.fill_rate.end_of_range > 0.0))
        .expect("a mode that fills it");

    let instruction = s2_kit::types::Id::generate();
    manager
        .session
        .instruct(
            s2_kit::types::frbc::Instruction::builder()
                .id(instruction)
                .actuator_id(actuator.id)
                .operation_mode(charge.id)
                .operation_mode_factor(1.0)
                .execution_time(Timestamp::now())
                .abnormal_condition(false)
                .build(),
            Timestamp::now(),
        )
        .expect("the instruction to go out");
    manager.settle().await;

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
        manager
            .until("report the abort", |e| match e {
                CemEvent::InstructionStatus(update)
                    if update.status_type == s2_kit::types::common::InstructionStatus::Aborted =>
                {
                    Some(update.clone())
                }
                _ => None,
            })
            .await
    })
    .await
    .expect("the manager to be told its instruction was overridden");

    assert_eq!(
        update.instruction_id, instruction,
        "S2's `ABORTED` is *started and could not be completed*, which is exactly \
         what a network operator's reduction does to an accepted instruction — and \
         the update names the instruction it aborted, not merely a status"
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
        Manager::connect(&address, &battery, None).await.is_err(),
        "a Customer Energy Manager with no credential opened a session"
    );
    assert!(
        Manager::connect(&address, &battery, Some("a-guess"))
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

/// An instruction scheduled for later does not move the household now.
///
/// `execution_time` means "when to start; in the past means as soon as
/// possible", so a time in the future is a schedule. A Resource Manager that
/// acts on receipt fires an aggregator's whole day at once, on every household
/// at once (D212). Not checkable from a message: it is a **session** behaviour,
/// and a test that sets the execution time to `now` cannot tell the two
/// behaviours apart.
#[tokio::test]
async fn an_instruction_for_later_does_not_move_the_household_now() {
    let (address, cem, site) = a_box_that_can_be_managed().await;
    let battery = battery(&site);
    let mut manager = Manager::ready(&address, &battery, ControlType::FillRateBasedControl).await;
    let (actuator, charge) = charge_mode(&mut manager).await;

    manager
        .session
        .instruct(
            s2_kit::types::frbc::Instruction::builder()
                .id(s2_kit::types::Id::generate())
                .actuator_id(actuator)
                .operation_mode(charge)
                .operation_mode_factor(1.0)
                .execution_time(Timestamp::from(
                    time::OffsetDateTime::now_utc() + time::Duration::hours(2),
                ))
                .abnormal_condition(false)
                .build(),
            Timestamp::now(),
        )
        .expect("a scheduled instruction is a perfectly good one");

    // It is accepted — it is well formed and names a described actuator — and it
    // is *not* carried out. `until` rather than `settle` first, because `settle`
    // drains events without matching and would swallow the status this is about.
    let accepted = manager
        .until("accept the instruction", |e| match e {
            CemEvent::InstructionStatus(update) => Some(update.status_type),
            _ => None,
        })
        .await;
    assert_eq!(
        accepted,
        s2_kit::types::common::InstructionStatus::Accepted,
        "a schedule is accepted, not refused"
    );
    manager.settle().await;
    assert!(
        !cem.active(time::OffsetDateTime::now_utc())
            .await
            .contains_key(&battery),
        "an instruction two hours out must not be in the map the arbiter reads"
    );
    manager.assert_clean();
}

/// A manager that selects `NO_SELECTION` hands the resource back.
///
/// A **state** rather than a capability — "to be used if no control type is or
/// has been selected" — and how an aggregator finishes a dispatch window without
/// dropping the connection it still wants the measurements on. The hold has to
/// go at once, because a manager's instruction ranks above the box's own plan
/// (D213).
#[tokio::test]
async fn a_manager_that_stops_driving_stops_being_obeyed() {
    let (address, cem, site) = a_box_that_can_be_managed().await;
    let battery = battery(&site);
    let mut manager = Manager::ready(&address, &battery, ControlType::FillRateBasedControl).await;
    let (actuator, charge) = charge_mode(&mut manager).await;

    manager
        .session
        .instruct(
            s2_kit::types::frbc::Instruction::builder()
                .id(s2_kit::types::Id::generate())
                .actuator_id(actuator)
                .operation_mode(charge)
                .operation_mode_factor(1.0)
                .execution_time(Timestamp::now())
                .abnormal_condition(false)
                .build(),
            Timestamp::now(),
        )
        .expect("the instruction to go out");
    manager.settle().await;
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
    .expect("the household to be under instruction first");

    manager
        .session
        .select_control_type(ControlType::NoSelection, Timestamp::now())
        .expect("handing a resource back is a thing a manager may do");
    manager.settle().await;

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status = cem.status(time::OffsetDateTime::now_utc()).await;
            if status.connected.is_empty() && status.instructing.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the hold to be released the moment the manager let go");

    manager.assert_clean();
}

/// The actuator and its charge mode, read off the description rather than
/// assumed — which is the whole point of a standard that names things by ID.
async fn charge_mode(manager: &mut Manager) -> (s2_kit::types::Id, s2_kit::types::Id) {
    let description = manager
        .until("send its system description", |e| match e {
            CemEvent::Description {
                message: Message::FrbcSystemDescription(d),
                ..
            } => Some(d.clone()),
            _ => None,
        })
        .await;
    let actuator = description
        .actuators
        .first()
        .expect("a battery has one actuator");
    let charge = actuator
        .operation_modes
        .iter()
        .find(|m| m.elements.iter().any(|e| e.fill_rate.end_of_range > 0.0))
        .expect("a battery has a mode that fills it");
    (actuator.id, charge.id)
}
