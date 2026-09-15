//! Being managed by a Customer Energy Manager, and not only describing what
//! could be.
//!
//! `hems-flex` holds the whole of S2 (EN 50491-12-2) and holds it sans-I/O:
//! which control type each asset belongs to, the description a manager is sent,
//! the session that has the conversation, and the arithmetic that reads an
//! instruction back into watts. **This is the socket under it**, and being
//! driveable is the half that matters: S2's argument is that a device which says
//! what it can do can be planned by software that has never heard of it, and a
//! device that says so and then declines the consequence has made the argument
//! and kept the benefit for itself.
//!
//! The socket and nothing else. Every decision is `hems-flex`'s and
//! every *control* decision is the arbiter's — the same seam `runtime/ship.rs`
//! keeps for EEBUS, for the same reason: there is exactly one copy of the S2
//! state machine in the product, and it is the one the unit tests drive.
//!
//! # The path names the resource, because the wire does not
//!
//! S2 puts **one Resource Manager on one connection**: `ResourceManagerDetails`
//! identifies a single resource and the CEM selects a single control type for
//! it. A household is several resources, and no S2 message carries a resource
//! identifier — so one connection cannot carry a whole house, and something
//! outside the protocol has to say which resource a connection is about.
//!
//! The WebSocket path is that something: a manager opens `ws://<box>/s2/<asset>`
//! for each resource it drives, under the asset identifier the household's own
//! configuration already uses. The alternative — a listening port per resource —
//! is the same information expressed as a firewall rule, and it changes whenever
//! a household buys a battery. It rides on the shell's own listener, so the box
//! binds one socket and `matched-path` labels the route rather than every
//! asset's name.
//!
//! # It does not obey; it asks (D143)
//!
//! A decoded instruction becomes a [`hems_realtime::CemRequest`] in a map the
//! control loop reads, and the arbiter treats it as one more voice with an
//! opinion — above the plan the box makes for itself, below the person in the
//! kitchen, and below the guard absolutely. A manager cannot raise a § 14a
//! ceiling by asking nicely, because what it writes is a *desire* and
//! `[BK6-22-300 A1 4.6 S. 3]` is applied after every desire in the system.
//!
//! # Who may connect, and what that bounds
//!
//! A manager presents a credential the household issued it **by name**, which it
//! can list and withdraw (`runtime::access`, D185, D188). This route is inside
//! the same gate as every other one the daemon adds.
//!
//! The ranking is what bounds the worst case: everything a connected manager
//! does goes through the guard, so it cannot exceed a § 14a ceiling, a fuse, a
//! § 9 EEG cap or a device rating, cannot outlast its instruction's expiry, and
//! cannot overrule a person who presses *pause*. What it can do is spend a
//! household's money badly inside those bounds — which is why the surface is
//! **off by default**.
//!
//! # A manager that stops talking stops deciding
//!
//! Every request expires. A crashed manager, a cut cable or a lapsed certificate
//! would otherwise hold this household wherever it last said for as long as the
//! box runs — the § 14a failsafe's own failure with the ownership reversed. On
//! expiry the asset returns to the box's own plan with nothing to cancel, which
//! is how a household override behaves and for the same reason.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Response};
use hems_core::prelude::{Asset, AssetId, Envelope, Site};
use hems_realtime::CemRequest;
use s2_kit::message::Message;
use time::OffsetDateTime;
use tokio::sync::RwLock;

use crate::runtime::transport::Shared;

/// How the box lets a Customer Energy Manager drive it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct S2Settings {
    /// Whether the `/s2/{asset}` surface exists at all.
    ///
    /// **Off by default**, and that is a statement rather than caution. A
    /// household that has not connected a manager gains nothing from a socket
    /// that accepts one, and the box's own planner is what it is for. Turning it
    /// on is the household saying it has delegated the optimising to somebody
    /// else — which is a decision, and decisions are configuration.
    pub enabled: bool,
    /// How long an instruction is believed, in seconds.
    ///
    /// A quarter of an hour by default: one market interval, which is the grain
    /// a manager plans on and therefore the longest a silence can be mistaken
    /// for an intention. See the module note for why this is not optional.
    pub instruction_for_secs: u64,
    /// How often the household's own measurements are published to a connected
    /// manager, in seconds.
    ///
    /// Five. S2 owes a manager a status while the connection is up, and a
    /// manager planning against a fill level it last saw a minute ago is a
    /// manager planning against a different house.
    pub report_every_secs: u64,
}

impl Default for S2Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            instruction_for_secs: 15 * 60,
            report_every_secs: 5,
        }
    }
}

impl S2Settings {
    fn instruction_for(&self) -> time::Duration {
        time::Duration::seconds(i64::try_from(self.instruction_for_secs).unwrap_or(i64::MAX))
    }
}

/// What a connected manager is asking of this household, shared with the
/// control loop.
///
/// One lock over the whole map rather than one per asset: the control loop reads
/// all of it once a tick and a session writes one entry when an instruction
/// arrives, so the contended case is a read against a write that happens a few
/// times a quarter hour.
#[derive(Debug, Clone, Default)]
pub struct Cem {
    held: Arc<RwLock<Held>>,
}

/// An instruction this box accepted, and whether the manager has been told what
/// became of it.
#[derive(Debug, Clone)]
struct Outstanding {
    /// The instruction's own identifier, which is what an `InstructionStatusUpdate`
    /// is about.
    id: s2_kit::types::Id,
    /// Whether the manager has already been told this one was overridden.
    ///
    /// Once, not once a second: a § 14a reduction lasts minutes and a status
    /// update every tick would be a Resource Manager shouting.
    told: bool,
}

/// The state behind [`Cem`].
#[derive(Debug, Default)]
struct Held {
    requests: BTreeMap<AssetId, CemRequest>,
    /// The instruction behind each live request.
    outstanding: BTreeMap<AssetId, Outstanding>,
    /// Assets whose S2 instruction the guard is currently overriding, and which
    /// rule is doing it.
    ///
    /// Written by the control loop on every tick, read by the session that owes
    /// the manager an answer. **This is the half an S2 implementation usually
    /// leaves out**, and leaving it out is not a cosmetic gap: an instruction is
    /// answered `Accepted` the moment it decodes, and if a network operator's
    /// reduction then overrides it the manager is never told. An aggregator that
    /// has sold flexibility on the strength of an `Accepted` is selling
    /// something the grid has already taken back.
    overridden: BTreeMap<AssetId, String>,
    /// Assets a manager is connected to and has selected a control type for.
    active: BTreeSet<AssetId>,
    /// Instructions this Resource Manager refused — an unknown actuator, a
    /// factor outside `[0, 1]`, a mode nobody described.
    ///
    /// Counted rather than only logged, because it is invisible from the
    /// manager's side: every one of them was answered politely, and a CEM that
    /// keeps addressing an actuator this household does not have is a
    /// commissioning fault nobody would otherwise see.
    refused: u64,
    /// Instructions this Resource Manager accepted and the arbiter cannot act
    /// on — an SG Ready mode, a programme start.
    ///
    /// The honest name for a boundary rather than a silence. The arbiter decides
    /// a *power* per asset; a relay state and the placement of a dishwasher
    /// programme are decided elsewhere (by the driver and by the planner
    /// respectively), and a manager driving those is being acknowledged and not
    /// carried out. A number that starts climbing is the feature somebody wants
    /// next, and it says so out loud instead of being invisible.
    unactioned: u64,
}

impl Cem {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What a manager is asking for now, as the arbiter wants it.
    ///
    /// Expired entries are dropped on the way past rather than swept on a timer
    /// of their own — the same rule [`crate::runtime::overrides::Overrides`]
    /// follows, and for the same reason: a sweep is a second place for the two
    /// to disagree about what *now* is.
    pub async fn active(&self, now: OffsetDateTime) -> BTreeMap<AssetId, CemRequest> {
        let mut held = self.held.write().await;
        held.requests.retain(|_, r| r.is_live(now));
        held.requests.clone()
    }

    /// What the surface should say about itself.
    pub async fn status(&self, now: OffsetDateTime) -> CemStatus {
        let mut held = self.held.write().await;
        held.requests.retain(|_, r| r.is_live(now));
        CemStatus {
            connected: held.active.iter().map(ToString::to_string).collect(),
            instructing: held.requests.keys().map(ToString::to_string).collect(),
            overridden: held
                .overridden
                .iter()
                .filter(|(asset, _)| held.requests.contains_key(*asset))
                .map(|(asset, rule)| (asset.to_string(), rule.clone()))
                .collect(),
            refused: held.refused,
            unactioned: held.unactioned,
        }
    }

    async fn instruct(&self, asset: AssetId, request: CemRequest, id: s2_kit::types::Id) {
        let mut held = self.held.write().await;
        held.requests.insert(asset.clone(), request);
        held.outstanding
            .insert(asset, Outstanding { id, told: false });
    }

    /// Which assets the guard is currently overriding a manager on, named by the
    /// rule doing it.
    ///
    /// Called by the control loop once a tick with the whole map, so an override
    /// that has *lifted* disappears by not being mentioned.
    pub async fn note_overrides(&self, overridden: BTreeMap<AssetId, String>) {
        self.held.write().await.overridden = overridden;
    }

    /// The instruction this asset owes the manager an abort for, if it owes one.
    ///
    /// Takes it: the answer is given once per instruction, not once per tick.
    async fn owed_abort(&self, asset: &AssetId) -> Option<(s2_kit::types::Id, String)> {
        let mut held = self.held.write().await;
        let rule = held.overridden.get(asset)?.clone();
        let outstanding = held.outstanding.get_mut(asset)?;
        if outstanding.told {
            return None;
        }
        outstanding.told = true;
        Some((outstanding.id, rule))
    }

    async fn opened(&self, asset: &AssetId) {
        self.held.write().await.active.insert(asset.clone());
    }

    /// A session ended: the manager stops deciding for that asset **now**
    /// rather than when its last instruction would have expired.
    ///
    /// A closed connection is a manager that has said something, and what it has
    /// said is that it is no longer managing. Letting the last instruction run
    /// out its quarter hour would keep a household at a setpoint chosen by a
    /// process that is gone.
    async fn closed(&self, asset: &AssetId) {
        let mut held = self.held.write().await;
        held.active.remove(asset);
        held.requests.remove(asset);
        held.outstanding.remove(asset);
        held.overridden.remove(asset);
    }

    async fn refused(&self) {
        self.held.write().await.refused += 1;
    }

    async fn unactioned(&self) {
        self.held.write().await.unactioned += 1;
    }
}

/// What the local API says about the managers connected to this box.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CemStatus {
    /// Assets a manager has opened a session on and chosen a control type for.
    pub connected: Vec<String>,
    /// Assets a manager currently has an unexpired instruction on.
    pub instructing: Vec<String>,
    /// Assets where the guard is overriding what the manager asked for, and the
    /// rule doing it.
    ///
    /// On the household's own screen as well as on the wire, because it is the
    /// sentence that explains a bill: *your manager asked for this and your
    /// network operator would not allow it.*
    pub overridden: std::collections::BTreeMap<String, String>,
    /// Instructions refused since the box started.
    pub refused: u64,
    /// Instructions accepted that the arbiter does not act on.
    pub unactioned: u64,
}

/// Everything a session needs that is not in the message.
#[derive(Clone)]
pub struct Surface {
    site: Arc<Site>,
    registry: Shared,
    cem: Cem,
    settings: S2Settings,
}

impl Surface {
    /// The S2 surface of one household.
    #[must_use]
    pub fn new(site: Arc<Site>, registry: Shared, cem: Cem, settings: S2Settings) -> Self {
        Self {
            site,
            registry,
            cem,
            settings,
        }
    }
}

/// The route a manager connects to.
///
/// Merged into the shell's own router, so the S2 surface is on the socket the
/// box already binds.
pub fn router(surface: Surface) -> axum::Router {
    axum::Router::new()
        .route("/s2/{asset}", axum::routing::any(upgrade))
        .with_state(surface)
}

/// Accept the WebSocket, or refuse an asset this household does not have.
async fn upgrade(
    State(surface): State<Surface>,
    Path(asset): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Ok(id) = AssetId::new(&asset) else {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("`{asset}` is not a valid asset identifier"),
        )
            .into_response();
    };
    if surface.site.asset(&id).is_none() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            format!("this household has no asset called `{id}`"),
        )
            .into_response();
    }
    upgrade.on_upgrade(move |socket| serve(surface, id, socket))
}

/// One connection, for as long as it lasts.
///
/// The pump is four lines and the order of them is the protocol: whatever the
/// session has queued goes out, one message comes in, the session is told about
/// it, and whatever it produced goes out on the next turn. The **Resource
/// Manager speaks first** — S2 makes its handshake the one that must carry a
/// version list — so [`hems_flex::Session::open`] is called before anything is
/// read.
async fn serve(surface: Surface, asset: AssetId, mut socket: WebSocket) {
    let now = OffsetDateTime::now_utc();
    let Some(mut session) = session_for(&surface.site, &asset, now) else {
        tracing::warn!(%asset, "this asset has no S2 description, so there is nothing to manage");
        return;
    };
    tracing::info!(%asset, control_type = ?session.control_type(), "a Customer Energy Manager connected");
    session.open();

    let mut reports = tokio::time::interval(std::time::Duration::from_secs(
        surface.settings.report_every_secs.max(1),
    ));
    reports.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Everything queued, before anything is awaited: a handshake that sat in
        // an outbox until the peer happened to say something would be a session
        // neither side opens.
        while let Some(message) = session.poll_transmit() {
            let Ok(text) = serde_json::to_string(&message) else {
                tracing::error!(%asset, "an S2 message this box built could not be encoded");
                continue;
            };
            if socket
                .send(WsMessage::Text(axum::extract::ws::Utf8Bytes::from(text)))
                .await
                .is_err()
            {
                break;
            }
        }

        tokio::select! {
            incoming = socket.recv() => {
                let now = OffsetDateTime::now_utc();
                match incoming {
                    Some(Ok(WsMessage::Text(text))) => match serde_json::from_str::<Message>(&text) {
                        Ok(message) => session.on_message(&message, now),
                        Err(error) => {
                            // Not a session fault. A frame this box cannot parse
                            // is a manager speaking a dialect of the schema, and
                            // tearing the connection down would take a household
                            // out of its manager's reach for one bad message.
                            tracing::warn!(%asset, %error, "a frame from the manager was not an S2 message");
                        }
                    },
                    // A close frame, a ping/pong the library already answered, or
                    // a binary frame S2's JSON binding does not use.
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::info!(%asset, %error, "the manager's connection ended");
                        break;
                    }
                }
            }
            _ = reports.tick() => {
                let now = OffsetDateTime::now_utc();
                let report = report_of(&surface, &asset, now).await;
                session.report(&report, now);
                // …and the answer to the question an `Accepted` leaves open.
                // S2 answers an instruction twice on purpose — "I read it" is
                // about the wire and "I will do it" is about the household — and
                // this is the second answer arriving late, which is the only
                // honest time for it: a network operator's reduction lands after
                // the instruction was accepted, and a manager that is never told
                // is a manager selling flexibility the grid has taken back.
                if let Some((instruction, rule)) = surface.cem.owed_abort(&asset).await {
                    tracing::info!(
                        %asset, %rule,
                        "telling the manager its instruction was overridden"
                    );
                    session.instruction_became(
                        instruction,
                        s2_kit::types::common::InstructionStatus::Aborted,
                        now,
                    );
                }
            }
        }

        if apply(&surface, &asset, session.drain(), OffsetDateTime::now_utc()).await {
            break;
        }
    }

    surface.cem.closed(&asset).await;
    tracing::info!(%asset, "the Customer Energy Manager's session ended");
}

/// The session this household would offer for one asset.
///
/// Built per connection rather than kept, because a description is a statement
/// about *now*: a charge point with a car on it is a store and the same charge
/// point without one is an envelope, and a manager that reconnects after the car
/// left must be told about the charge point that is actually there.
fn session_for(site: &Site, asset: &AssetId, now: OffsetDateTime) -> Option<hems_flex::Session> {
    let modes = BTreeMap::new();
    let context = hems_flex::DescribeContext::new(now, now + time::Duration::days(1), &modes);
    hems_flex::sessions_for(site, &context)
        .into_iter()
        .find(|s| s.asset() == asset)
}

/// What the household knows about one resource.
async fn report_of(surface: &Surface, asset: &AssetId, now: OffsetDateTime) -> hems_flex::Report {
    let observed = {
        let mut registry = surface.registry.lock().await;
        registry.observe(None, now)
    };
    let Some(measurement) = observed.state.asset(asset) else {
        // A resource nobody metered reports nothing rather than reporting zero —
        // the same rule the guard applies to a silent device, and the same
        // reason: a manager told a battery is at 0 kWh will plan to fill it.
        return hems_flex::Report::default();
    };
    hems_flex::Report {
        power: measurement.power,
        fill: fill_of(surface.site.asset(asset), measurement),
    }
}

/// How full a store is, in the unit its own S2 description declared.
///
/// Every description states its `fill_level_range` in kilowatt-hours — of
/// electricity for a battery, of heat for a tank — so this has to produce the
/// same unit or a manager will plan against a number that is not on the scale it
/// was given.
///
/// **The charge point is not here**, and that is a consequence of how it is
/// described rather than an omission. A wallbox becomes an S2 *store* only when
/// a car with a departure time is on it ([`hems_flex::DescribeContext`]), and
/// this surface describes one without — so it is offered as a `PEBC` envelope,
/// which a manager bounds rather than fills, and an envelope has no fill level
/// to report. Offering it as a store would mean answering *how many
/// kilowatt-hours is it holding*, and a state of charge with no pack size behind
/// it cannot: the capacity is the planner's input, not the site's.
fn fill_of(asset: Option<&Asset>, measurement: &hems_core::prelude::Measurement) -> Option<f64> {
    match asset? {
        Asset::Battery(b) => Some(measurement.soc?.fraction() * b.capacity.kwh()),
        Asset::Dhw(t) => Some(t.stored_heat(measurement.temperature_c?).kwh()),
        _ => None,
    }
}

/// Turn what the session decided into what the arbiter reads.
///
/// Returns whether the session is over.
async fn apply(
    surface: &Surface,
    asset: &AssetId,
    events: Vec<hems_flex::SessionEvent>,
    now: OffsetDateTime,
) -> bool {
    use hems_flex::{Instructed, SessionEvent};
    let until = now + surface.settings.instruction_for();
    let mut closed = false;
    for event in events {
        match event {
            SessionEvent::Ready(control) => {
                tracing::info!(%asset, ?control, "the manager selected a control type");
                surface.cem.opened(asset).await;
            }
            SessionEvent::Instructed {
                wanted,
                instruction,
                ..
            } => match wanted {
                Instructed::Power(power) => {
                    tracing::debug!(%asset, %power, "the manager asked for a power");
                    surface
                        .cem
                        .instruct(asset.clone(), CemRequest::power(power, until), instruction)
                        .await;
                }
                Instructed::Envelope(envelope_instruction) => {
                    let instruction_id = instruction;
                    match envelope_of(&surface.site, asset, &envelope_instruction) {
                        Some(envelope) => {
                            tracing::debug!(%asset, ?envelope, "the manager sent an envelope");
                            surface
                                .cem
                                .instruct(
                                    asset.clone(),
                                    CemRequest::envelope(envelope, until),
                                    instruction_id,
                                )
                                .await;
                        }
                        None => surface.cem.unactioned().await,
                    }
                }
                // Acknowledged on the wire and not acted on here: see
                // [`Held::unactioned`] for where each of these is decided
                // instead, and why counting it is the honest answer.
                Instructed::SgReady(_) | Instructed::Start(_) => {
                    tracing::info!(
                        %asset,
                        "the manager sent an instruction the arbiter does not act on"
                    );
                    surface.cem.unactioned().await;
                }
            },
            SessionEvent::Refused { reason, .. } => {
                tracing::warn!(%asset, %reason, "an instruction from the manager was refused");
                surface.cem.refused().await;
            }
            SessionEvent::Released => {
                // The same clearing a closed connection gets, and for the same
                // reason: a manager has said something, and what it has said is
                // that it is no longer managing this resource. The *connection*
                // stays up — it may want the measurements, and it may select
                // again — so this is deliberately not `closed = true`.
                tracing::info!(%asset, "the manager handed the resource back");
                surface.cem.closed(asset).await;
            }
            SessionEvent::Closed(reason) => {
                tracing::warn!(%asset, %reason, "the S2 session ended on a protocol fault");
                closed = true;
            }
        }
    }
    closed
}

/// The interval a `PEBC` instruction leaves this asset, in the load convention.
fn envelope_of(
    site: &Site,
    asset: &AssetId,
    instruction: &s2_kit::types::pebc::Instruction,
) -> Option<Envelope> {
    let asset = site.asset(asset)?;
    let mode = asset.meta().phases.default_mode();
    hems_flex::envelope_interval(instruction, asset, mode).ok()
}
