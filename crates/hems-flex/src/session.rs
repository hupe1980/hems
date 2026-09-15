//! Being a Resource Manager, not only describing one.
//!
//! [`describe`](crate::describe) says what a household's devices *are* in S2's
//! vocabulary. This is the other half: the conversation a Customer Energy
//! Manager actually has with one of them — the handshake, the control type it
//! chooses, the statuses it is owed while the connection is up, and the
//! instructions it sends back.
//!
//! Until this existed hems could describe its flexibility to a CEM and could not
//! be managed by one, which is a strange half of a standard to implement: S2's
//! whole argument is that a device that says what it can do can be planned by
//! software that has never heard of it, and a device that says so and then
//! ignores the answer has made the argument and declined the consequence.
//!
//! # Sans-I/O, like everything else that decides
//!
//! No socket, no clock, no task. Messages in, messages out, and `now` is a
//! parameter — the same contract [`hems_drv`] holds a protocol driver to, for
//! the same reason: a handshake, a rejected instruction and a CEM that selects a
//! control type nobody offered are all unit tests rather than a WebSocket and a
//! sleep. `s2_kit::connection` is the async transport for anyone who wants
//! one; `hemsd` owns the socket and hands the bytes here.
//!
//! [`hems_drv`]: https://docs.rs/hems-drv
//!
//! # One session is one resource
//!
//! S2 puts one Resource Manager on one connection: `ResourceManagerDetails`
//! identifies a single resource and the CEM selects a single control type for
//! it. A household is therefore several sessions rather than one multiplexed
//! one, and [`crate::describe_site`] is what produces their offers. Doing it the
//! other way — one connection carrying the whole house — would need a resource
//! identifier on every message that S2 does not have.
//!
//! # What the RM owes, and what it may refuse
//!
//! Every message except a `ReceptionStatus` is answered with one, and the answer
//! is not a formality: `INVALID_CONTENT` is how a Resource Manager says *that
//! actuator is not mine* to a manager that has confused two devices, and a
//! session that answered `OK` to everything would let a CEM believe it was
//! driving something.
//!
//! An **instruction is answered twice** — a `ReceptionStatus` for the message
//! and an [`InstructionStatusUpdate`] for the decision — because they are
//! different questions. "I read it" is about the wire; "I will do it" is about
//! the household, and a hot-water tank that has just been told to heat while its
//! own thermostat holds it off has received the message perfectly and is not
//! going to carry it out.
//!
//! # What this deliberately does not do
//!
//! It does not *obey* (D143). A decoded instruction becomes a [`SessionEvent`],
//! and what happens to it is the arbiter's decision: a CEM is one more voice with an
//! opinion about a device, and it ranks below the guard exactly as the planner
//! does. An S2 session that wrote setpoints straight to hardware would be a
//! second control plane with no § 14a precedence in it.

use std::collections::VecDeque;

use hems_core::prelude::{AssetId, Power};
use hems_device::sg_ready::SgReadyState;
use s2_kit::message::Message;
use s2_kit::types::Id;
use s2_kit::types::common::{
    ControlType, EnergyManagementRole, Handshake, InstructionStatus, InstructionStatusUpdate,
    PowerMeasurement, PowerValue, ReceptionStatus, ReceptionStatusValues, ResourceManagerDetails,
    RevokableObjects, RevokeObject,
};
use s2_kit::types::{frbc, ombc, pebc, ppbc};
use thiserror::Error;
use time::OffsetDateTime;

use crate::describe::{
    BatteryDescription, DhwDescription, EvDescription, HeatPumpDescription, ProgrammeDescription,
};
use crate::instruct::{self, InstructError};

/// An instant, in the shape S2 wants it.
///
/// One line, and it used to be six. `s2_kit` put `chrono::DateTime` in its
/// public API, so this crate carried `chrono` purely to name the type on a wire
/// field. `s2-kit` has its own `Timestamp` and a `From<time::OffsetDateTime>`
/// behind the `time` feature, so the conversion is the library's and `time` is
/// once again the only calendar in the product (D201).
fn utc(at: OffsetDateTime) -> s2_kit::types::Timestamp {
    at.into()
}

/// The protocol version this Resource Manager negotiates.
///
/// `0.0.2-beta`, which is what `s2_kit` sent and what `s2-kit`'s own note calls
/// "what every deployed implementation still negotiates" — so the wire bytes did
/// not move when the library did. S2 JSON v1.0.0 is the published tag and is a
/// deliberate *later* decision: changing it is an interoperability change rather
/// than a dependency upgrade, and it belongs with the ElaadNL event where there
/// is something on the other end to disagree with (R32).
fn ours() -> s2_kit::types::ProtocolVersion {
    s2_kit::types::ProtocolVersion::V0_0_2_BETA
}

/// What this Resource Manager is offering on this connection.
///
/// One variant per control type this crate can describe, carrying the same
/// description [`crate::describe_site`] built — so the IDs an instruction names
/// are the IDs that were sent, and a session cannot answer an instruction
/// against a description somebody else produced.
#[derive(Debug, Clone)]
pub enum Offer {
    /// A stationary battery: `FRBC`.
    Battery(Box<BatteryDescription>),
    /// A charge point with a car and a departure time: `FRBC`.
    Ev(Box<EvDescription>),
    /// A hot-water tank: `FRBC`.
    Dhw(Box<DhwDescription>),
    /// Anything that only needs a bound — an inverter, a charge point with no
    /// deadline, a heat pump that takes a ceiling: `PEBC`.
    Envelope(Box<pebc::PowerConstraints>),
    /// An SG Ready heat pump: `OMBC`.
    HeatPump(Box<HeatPumpDescription>),
    /// An appliance loaded with a programme: `PPBC`.
    Programme(Box<ProgrammeDescription>),
}

impl Offer {
    /// The control type this offer can be driven in.
    #[must_use]
    pub const fn control_type(&self) -> ControlType {
        match self {
            Self::Battery(_) | Self::Ev(_) | Self::Dhw(_) => ControlType::FillRateBasedControl,
            Self::Envelope(_) => ControlType::PowerEnvelopeBasedControl,
            Self::HeatPump(_) => ControlType::OperationModeBasedControl,
            Self::Programme(_) => ControlType::PowerProfileBasedControl,
        }
    }

    /// The description to send once the CEM has selected that control type.
    ///
    /// `PEBC` is the one that sends nothing here: its constraints are the
    /// description, and S2 has the RM publish them as their own message rather
    /// than as a `SystemDescription`.
    #[must_use]
    fn system_description(&self) -> Message {
        match self {
            Self::Battery(d) => d.system.clone().into(),
            Self::Ev(d) => d.system.clone().into(),
            Self::Dhw(d) => d.system.clone().into(),
            Self::Envelope(c) => (**c).clone().into(),
            Self::HeatPump(d) => d.system.clone().into(),
            Self::Programme(d) => d.definition.clone().into(),
        }
    }
}

/// Where a session has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Nothing has been sent. [`Session::open`] is what starts it.
    Idle,
    /// Our `Handshake` is out; the CEM's has not arrived.
    AwaitingHandshake,
    /// The CEM has said hello; it has not yet said which version it chose.
    AwaitingVersion,
    /// Our `ResourceManagerDetails` is out; the CEM has not chosen a control
    /// type.
    AwaitingControlType,
    /// Running. A CEM may send instructions and expects statuses.
    Active(ControlType),
    /// Over, and why.
    Closed(CloseReason),
}

impl SessionState {
    /// Whether the CEM may send instructions and expects statuses.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }
}

/// Why a session ended.
///
/// Every one of these is a **protocol** fault. A dropped socket is not here:
/// that is the transport's, and `hemsd` reconnects from it — a session torn down
/// because a packet was lost would take a household out of a manager's reach for
/// a TCP reset, which is the mistake `hems-drv/eebus` records in D-numbered form
/// about the § 14a failsafe.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloseReason {
    /// The CEM chose a protocol version this Resource Manager did not offer.
    ///
    /// It cannot: the RM's `Handshake` carries the versions it supports and the
    /// CEM picks from that list. One that picks something else is one this
    /// session has no agreed encoding with, and continuing would be guessing.
    #[error("the CEM selected S2 version {selected:?}, which was not offered")]
    UnsupportedVersion {
        /// What the CEM said it had chosen.
        selected: String,
    },
    /// The CEM selected a control type this resource does not offer.
    #[error("the CEM selected {selected:?}, and this resource offers {offered:?}")]
    UnofferedControlType {
        /// What was asked for.
        selected: ControlType,
        /// What this resource can be driven in.
        offered: ControlType,
    },
    /// A message arrived at a point in the handshake where it cannot belong.
    #[error("a {kind} arrived while the session was {state}")]
    OutOfOrder {
        /// Which message.
        kind: &'static str,
        /// What the session was waiting for.
        state: &'static str,
    },
}

/// Something the household has to act on.
///
/// Deliberately **not** a command: see the module note. The arbiter decides what
/// a CEM's opinion is worth against the guard's, and it is the only thing that
/// may.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// The CEM chose a control type and the description has been sent.
    Ready(ControlType),
    /// An instruction this Resource Manager accepted.
    Instructed {
        /// The asset it is about.
        asset: AssetId,
        /// What the CEM asked for.
        wanted: Instructed,
        /// The instruction's own identifier, for the status update that follows
        /// once the household has actually done it.
        instruction: Id,
    },
    /// An instruction this Resource Manager refused, and why.
    ///
    /// Reported rather than swallowed: a CEM that keeps addressing an actuator
    /// this household does not have is a commissioning fault, and it is
    /// invisible from the CEM's side because it is being answered politely every
    /// time.
    Refused {
        /// The asset the session is about.
        asset: AssetId,
        /// Why.
        reason: InstructError,
    },
    /// The CEM handed the resource back.
    ///
    /// `SelectControlType { NO_SELECTION }` — a **state** rather than a
    /// capability, *"to be used if no control type is or has been selected"* —
    /// which is how a manager stops driving without dropping the connection: an
    /// aggregator finishing a dispatch window, a building manager handing a flat
    /// back to its tenant.
    ///
    /// Reported rather than swallowed, because a CEM's instruction ranks **above
    /// the box's own plan**: a resource that stays selected after its manager let
    /// go is a household obeying somebody who has stopped asking (D213).
    Released,
    /// The session ended.
    Closed(CloseReason),
}

/// What a CEM asked a resource to do, in hems's own units.
#[derive(Debug, Clone, PartialEq)]
pub enum Instructed {
    /// A power, load convention — positive draws, negative feeds in.
    Power(Power),
    /// A power **envelope** rather than a value.
    ///
    /// Handed on undecoded, and that is the honest shape: which end of an
    /// envelope binds is a property of the *asset* — an inverter is bounded from
    /// below and a wallbox from above — and a session holds a description rather
    /// than an asset. [`crate::envelope_command`] is what turns it into a
    /// command, and its caller has both.
    Envelope(Box<pebc::Instruction>),
    /// An SG Ready contact state.
    SgReady(SgReadyState),
    /// Start the appliance's programme at this instant.
    ///
    /// An instant already past means "as soon as possible", which S2 states
    /// explicitly and this does not silently correct — the session reads no
    /// clock.
    Start(OffsetDateTime),
}

/// What the household knows about this resource now.
///
/// Handed in by whoever is holding the measurements; the session turns it into
/// the messages S2 owes a CEM while a connection is up. Every field is optional
/// because a box is allowed not to know: a resource nobody metered reports
/// nothing rather than reporting zero, which is the same rule the guard applies
/// to a silent device and for the same reason.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Report {
    /// What a meter saw, load convention.
    pub power: Option<Power>,
    /// How full the store is, in the unit its description used — kilowatt-hours
    /// for a battery and a car, kilowatt-hours of heat for a tank.
    pub fill: Option<f64>,
}

/// A Resource Manager's side of one S2 connection.
#[derive(Debug, Clone)]
pub struct Session {
    asset: AssetId,
    details: ResourceManagerDetails,
    offer: Offer,
    ratings: Ratings,
    state: SessionState,
    outbox: VecDeque<Message>,
    events: Vec<SessionEvent>,
    /// The operation mode reported last, so a status can name the one before it
    /// — which S2 requires of every status but the first.
    active_mode: Option<Id>,
    /// Instructions accepted and not yet due, earliest first.
    ///
    /// S2's data model says of `execution_time`: *"When to start. In the past
    /// means as soon as possible."* So an instruction is a **schedule**, and a
    /// Resource Manager that acts on receipt fires a manager's whole day at once
    /// (D212).
    pending: Vec<Scheduled>,
}

/// An accepted instruction waiting for its moment.
#[derive(Debug, Clone, PartialEq)]
struct Scheduled {
    at: OffsetDateTime,
    wanted: Instructed,
    instruction: Id,
}

/// How many instructions may wait at once.
///
/// A manager scheduling a day of quarter hours needs ninety-six; one that has
/// lost track of what it has sent must not be able to exhaust a gateway box's
/// memory through a socket. Two days' worth, and the two-hundredth is refused
/// rather than silently dropped, because a queue that discards is a manager
/// planning against a household that is quietly not listening.
const MAX_PENDING: usize = 192;

impl Session {
    /// A session for one resource, not yet opened.
    #[must_use]
    pub fn new(
        asset: AssetId,
        details: ResourceManagerDetails,
        offer: Offer,
        ratings: Ratings,
    ) -> Self {
        Self {
            asset,
            details,
            offer,
            ratings,
            state: SessionState::Idle,
            outbox: VecDeque::new(),
            events: Vec::new(),
            active_mode: None,
            pending: Vec::new(),
        }
    }

    /// Release every instruction whose moment has come.
    ///
    /// Called from every entry point that carries a clock — `on_message` and
    /// `report` — so a caller cannot forget it, and exposed as well because a
    /// household that goes quiet still has a schedule to keep: a manager that
    /// sends nothing between 16:00 and 18:00 has still asked for something at
    /// 17:00.
    ///
    /// Emitting is what makes an instruction *happen*: the event reaches the
    /// arbiter, and `Started` is the status S2 defines for exactly this moment —
    /// `Accepted` said the household intends to carry it out, this says it has
    /// begun.
    pub fn advance(&mut self, now: OffsetDateTime) {
        if !matches!(self.state, SessionState::Active(_)) {
            return;
        }
        let due: Vec<Scheduled> = {
            let (due, later) = std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|s| s.at <= now);
            self.pending = later;
            due
        };
        for s in due {
            self.start(s, now);
        }
    }

    /// Put an instruction into effect and tell the manager it has begun.
    fn start(&mut self, s: Scheduled, now: OffsetDateTime) {
        self.events.push(SessionEvent::Instructed {
            asset: self.asset.clone(),
            wanted: s.wanted,
            instruction: s.instruction,
        });
        self.outbox.push_back(
            InstructionStatusUpdate::builder()
                .instruction_id(s.instruction)
                .status_type(InstructionStatus::Started)
                .timestamp(utc(now))
                .build()
                .into(),
        );
    }

    /// Which asset this session is about.
    #[must_use]
    pub const fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// Where it has got to.
    #[must_use]
    pub const fn state(&self) -> &SessionState {
        &self.state
    }

    /// The control type this resource can be driven in.
    #[must_use]
    pub const fn control_type(&self) -> ControlType {
        self.offer.control_type()
    }

    /// Begin: queue the `Handshake` that opens an S2 conversation.
    ///
    /// The **RM speaks first**, and its handshake is the one that must carry a
    /// version list — S2 makes it mandatory for the Resource Manager and
    /// optional for the CEM, because the RM is the constrained side and the
    /// manager is the one that adapts.
    pub fn open(&mut self) {
        if !matches!(self.state, SessionState::Idle) {
            return;
        }
        self.outbox.push_back(
            Handshake::builder()
                .role(EnergyManagementRole::Rm)
                .supported_protocol_versions(vec![ours()])
                .build()
                .into(),
        );
        self.state = SessionState::AwaitingHandshake;
    }

    /// The next message to put on the wire, if any.
    pub fn poll_transmit(&mut self) -> Option<Message> {
        self.outbox.pop_front()
    }

    /// Everything that has happened since this was last called.
    pub fn drain(&mut self) -> Vec<SessionEvent> {
        std::mem::take(&mut self.events)
    }

    /// Feed in one message from the CEM.
    pub fn on_message(&mut self, message: &Message, now: OffsetDateTime) {
        // A `ReceptionStatus` is the one message that is never itself
        // acknowledged, and answering one would be an infinite politeness.
        if matches!(message, Message::ReceptionStatus(_)) {
            return;
        }
        // Where the acknowledgement goes in the queue, taken **before** the
        // handler runs and inserted there afterwards.
        //
        // `handle` may queue the consequences of the message — a
        // `SelectControlType` puts the system description on the outbox — and
        // pushing the `ReceptionStatus` after them sends those consequences
        // *ahead of* the acknowledgement that accepted the message they answer.
        // A CEM that tracks session state reads that as a control-type message
        // arriving before its selection was accepted and refuses it
        // (`S2-STATE-001`, citing S2 Connect's normative state table), which is
        // the correct reading: until the RM says `OK`, the manager has not been
        // told its selection took.
        //
        // This was the order for five versions and nothing caught it, because
        // the interop test's CEM was a hand-written frame reader with no session
        // state of its own. Driving a real `CemSession` found it on the first
        // run (D201).
        // Anything already due goes out *before* this message is handled, so an
        // instruction for 17:00 and a message arriving at 17:05 leave in the
        // order they happened rather than the order they were spoken.
        self.advance(now);
        let at = self.outbox.len();
        let status = self.handle(message, now);
        if let Some(id) = message.id() {
            self.outbox.insert(
                at,
                ReceptionStatus {
                    subject_message_id: id,
                    status: status.0,
                    diagnostic_label: status.1,
                }
                .into(),
            );
        }
    }

    /// Publish what the household knows about this resource.
    ///
    /// A no-op until the CEM has chosen a control type: a status before the
    /// selection is a message about a description the manager has not been sent.
    pub fn report(&mut self, report: &Report, now: OffsetDateTime) {
        self.advance(now);
        let SessionState::Active(control) = self.state else {
            return;
        };
        if let Some(power) = report.power {
            self.outbox.push_back(
                PowerMeasurement::builder()
                    .measurement_timestamp(utc(now))
                    .values(vec![PowerValue {
                        commodity_quantity: self.quantity(),
                        value: power.get(),
                    }])
                    .build()
                    .into(),
            );
        }
        if control != ControlType::FillRateBasedControl {
            self.report_modes(report, now);
            return;
        }
        if let Some(fill) = report.fill {
            self.outbox.push_back(frbc::StorageStatus::new(fill).into());
        }
        self.report_modes(report, now);
    }

    /// The `ActuatorStatus` or `Status` a measured power implies.
    ///
    /// The inverse of [`crate::instruct`], and it has to be: a CEM that is told
    /// "charge mode, factor 0,5" and then reads a power measurement has to be
    /// able to reconcile the two, and two different arithmetics for one fact is
    /// how it stops being able to.
    fn report_modes(&mut self, report: &Report, now: OffsetDateTime) {
        let Some(power) = report.power else {
            return;
        };
        let Some((mode, factor)) = self.activity(power) else {
            return;
        };
        let previous = self.active_mode.filter(|m| *m != mode);
        self.active_mode = Some(mode);
        let transition = previous.as_ref().map(|_| utc(now));
        let message: Message = match &self.offer {
            Offer::Battery(d) => frbc::ActuatorStatus {
                actuator_id: d.actuator,
                active_operation_mode_id: mode,
                previous_operation_mode_id: previous,
                operation_mode_factor: factor,
                message_id: Id::generate(),
                transition_timestamp: transition,
            }
            .into(),
            Offer::Ev(d) => frbc::ActuatorStatus {
                actuator_id: d.actuator,
                active_operation_mode_id: mode,
                previous_operation_mode_id: previous,
                operation_mode_factor: factor,
                message_id: Id::generate(),
                transition_timestamp: transition,
            }
            .into(),
            Offer::Dhw(d) => frbc::ActuatorStatus {
                actuator_id: d.actuator,
                active_operation_mode_id: mode,
                previous_operation_mode_id: previous,
                operation_mode_factor: factor,
                message_id: Id::generate(),
                transition_timestamp: transition,
            }
            .into(),
            Offer::HeatPump(_) => ombc::Status {
                active_operation_mode_id: mode,
                previous_operation_mode_id: previous,
                operation_mode_factor: factor,
                message_id: Id::generate(),
                transition_timestamp: transition,
            }
            .into(),
            // PEBC has no operation modes and PPBC's status is about a
            // *sequence* rather than a rate — see `progress`.
            Offer::Envelope(_) | Offer::Programme(_) => return,
        };
        self.outbox.push_back(message);
    }

    /// Which operation mode a measured power puts this resource in, and how far
    /// into it.
    fn activity(&self, power: Power) -> Option<(Id, f64)> {
        let fraction = |limit: Power| {
            if limit <= Power::ZERO {
                0.0
            } else {
                (power.abs().get() / limit.get()).clamp(0.0, 1.0)
            }
        };
        match &self.offer {
            Offer::Battery(d) => Some(if power < Power::ZERO {
                (d.discharge, fraction(self.ratings.discharge))
            } else {
                (d.charge, fraction(self.ratings.charge))
            }),
            Offer::Ev(d) => {
                if power < Power::ZERO {
                    // A one-way charge point has no discharge mode to name, and
                    // naming the charging one for a negative power would be a
                    // status a CEM cannot reconcile with the measurement beside
                    // it. It should not happen; if it does, saying nothing is
                    // the honest answer.
                    Some((d.discharge?, fraction(self.ratings.discharge)))
                } else {
                    Some((d.charge, fraction(self.ratings.charge)))
                }
            }
            Offer::Dhw(d) => Some((d.heat, fraction(self.ratings.charge))),
            // An SG Ready unit's state is a contact position rather than
            // something a meter can be inverted into: two states draw the same
            // power and mean different things. The box reports the state it
            // commanded, through `Session::in_mode`.
            Offer::HeatPump(_) | Offer::Envelope(_) | Offer::Programme(_) => None,
        }
    }

    /// Say which discrete mode this resource is actually in.
    ///
    /// For an SG Ready heat pump, and it is a *setter* rather than something
    /// inferred from a measurement: states 2 and 3 can draw the same kilowatt
    /// and mean different things to the unit, so only whoever closed the
    /// contacts knows which one it is in. Everything else reports its mode from
    /// its power, because for a store the two are the same fact.
    pub fn in_mode(&mut self, state: SgReadyState, now: OffsetDateTime) {
        let Offer::HeatPump(d) = &self.offer else {
            return;
        };
        let Some((mode, _)) = d.modes.iter().find(|(_, s)| *s == state) else {
            return;
        };
        let mode = *mode;
        if self.active_mode.as_ref() == Some(&mode) {
            return;
        }
        let previous = self.active_mode.replace(mode);
        if !self.state.is_active() {
            return;
        }
        self.outbox.push_back(
            ombc::Status {
                active_operation_mode_id: mode,
                previous_operation_mode_id: previous,
                operation_mode_factor: 1.0,
                message_id: Id::generate(),
                transition_timestamp: previous.map(|_| utc(now)),
            }
            .into(),
        );
    }

    /// Say where an appliance's programme has got to.
    ///
    /// `PPBC`'s status is about a **sequence** rather than a rate, so it has no
    /// place in [`Session::report`]'s measurement path: a dishwasher is not half
    /// on. It is a setter for the same reason [`Session::in_mode`] is — only the
    /// box knows whether the machine was started — and the case that matters is
    /// the boring one: an appliance **waiting** is exactly what a CEM has to see
    /// to schedule it, and the status an implementation forgets to send.
    ///
    /// `progress` is how far into the sequence it is, where it is running.
    pub fn programme_is(
        &mut self,
        status: ppbc::PowerSequenceStatus,
        progress: Option<s2_kit::types::Duration>,
    ) {
        let Offer::Programme(d) = &self.offer else {
            return;
        };
        if !self.state.is_active() {
            return;
        }
        let message = profile_status(d, status, progress);
        self.outbox.push_back(message.into());
    }

    /// Tell the CEM what became of an instruction it sent.
    ///
    /// The second half of the pair the module note describes: acceptance says
    /// the household intends to carry it out, and this says whether it did.
    /// A CEM planning against a resource whose instructions all end `ABORTED` is
    /// planning against a household that is quietly refusing, and only this says
    /// so.
    pub fn instruction_became(
        &mut self,
        instruction: Id,
        status: InstructionStatus,
        now: OffsetDateTime,
    ) {
        if !self.state.is_active() {
            return;
        }
        self.outbox.push_back(
            InstructionStatusUpdate::builder()
                .instruction_id(instruction)
                .status_type(status)
                .timestamp(utc(now))
                .build()
                .into(),
        );
    }

    /// The commodity this resource's power is measured in.
    fn quantity(&self) -> s2_kit::types::common::CommodityQuantity {
        self.details
            .provides_power_measurement_types
            .first()
            .copied()
            .unwrap_or(s2_kit::types::common::CommodityQuantity::ElectricPower3PhaseSymmetric)
    }

    /// Handle one message, returning the `ReceptionStatus` it earns.
    fn handle(
        &mut self,
        message: &Message,
        now: OffsetDateTime,
    ) -> (ReceptionStatusValues, Option<String>) {
        match (&self.state, message) {
            (SessionState::AwaitingHandshake, Message::Handshake(_)) => {
                self.state = SessionState::AwaitingVersion;
                (ReceptionStatusValues::Ok, None)
            }
            (SessionState::AwaitingVersion, Message::HandshakeResponse(response)) => {
                if response.selected_protocol_version == ours() {
                    self.outbox.push_back(self.details.clone().into());
                    self.state = SessionState::AwaitingControlType;
                    (ReceptionStatusValues::Ok, None)
                } else {
                    let reason = CloseReason::UnsupportedVersion {
                        selected: response.selected_protocol_version.as_str().to_owned(),
                    };
                    let label = reason.to_string();
                    self.close(reason);
                    (ReceptionStatusValues::InvalidContent, Some(label))
                }
            }
            (
                SessionState::AwaitingControlType | SessionState::Active(_),
                Message::SelectControlType(select),
            ) => self.select(select.control_type),
            (SessionState::Active(_), Message::RevokeObject(revoke)) => self.revoke(revoke, now),
            (SessionState::Active(_), _) => self.instructed(message, now),
            (state, message) => {
                // Out of order. The session ends rather than guessing: S2's
                // handshake is what fixes the encoding, and a message that
                // arrives before it is a message this side may be reading under
                // the wrong version of the schema.
                let reason = CloseReason::OutOfOrder {
                    kind: kind_of(message),
                    state: name_of(state),
                };
                let label = reason.to_string();
                self.close(reason);
                (ReceptionStatusValues::InvalidContent, Some(label))
            }
        }
    }

    /// The CEM chose a control type — or chose to stop driving.
    ///
    /// Handled in **both** the selecting and the active state, because S2 lets a
    /// manager change its mind and the two interesting cases are the same
    /// message: re-selecting what is already running is idempotent, and
    /// `NO_SELECTION` is a manager letting go.
    fn select(&mut self, selected: ControlType) -> (ReceptionStatusValues, Option<String>) {
        let offered = self.offer.control_type();
        if selected == ControlType::NoSelection {
            // Not an error in either state, and not a close. The connection
            // stays up — a manager that is not driving may still want the
            // measurements and forecasts S2 keeps flowing — and the resource
            // goes back to the household's own plan.
            let was_active = matches!(self.state, SessionState::Active(_));
            self.pending.clear();
            self.state = SessionState::AwaitingControlType;
            self.active_mode = None;
            if was_active {
                self.events.push(SessionEvent::Released);
            }
            return (ReceptionStatusValues::Ok, None);
        }
        if selected == offered {
            self.outbox.push_back(self.offer.system_description());
            self.state = SessionState::Active(offered);
            self.events.push(SessionEvent::Ready(offered));
            return (ReceptionStatusValues::Ok, None);
        }
        let reason = CloseReason::UnofferedControlType { selected, offered };
        let label = reason.to_string();
        self.close(reason);
        (ReceptionStatusValues::InvalidContent, Some(label))
    }

    /// The CEM withdrew something it had sent.
    ///
    /// Only instructions matter here, and only ones still **waiting**: one
    /// already carried out is not revocable — it is a thing that happened, and
    /// the honest answer is the `Aborted` the arbiter's own override path
    /// already sends. A revocation for an identifier this session is not holding
    /// is answered `OK` rather than refused, because a manager withdrawing
    /// something it is unsure about is being careful rather than wrong.
    ///
    fn revoke(
        &mut self,
        revoke: &RevokeObject,
        now: OffsetDateTime,
    ) -> (ReceptionStatusValues, Option<String>) {
        let instruction = matches!(
            revoke.object_type,
            RevokableObjects::FrbcInstruction
                | RevokableObjects::OmbcInstruction
                | RevokableObjects::PebcInstruction
                | RevokableObjects::DdbcInstruction
                | RevokableObjects::PpbcScheduleInstruction
        );
        if !instruction {
            return (ReceptionStatusValues::Ok, None);
        }
        if let Some(i) = self
            .pending
            .iter()
            .position(|s| s.instruction == revoke.object_id)
        {
            self.pending.remove(i);
            self.outbox.push_back(
                InstructionStatusUpdate::builder()
                    .instruction_id(revoke.object_id)
                    .status_type(InstructionStatus::Revoked)
                    .timestamp(utc(now))
                    .build()
                    .into(),
            );
        }
        (ReceptionStatusValues::Ok, None)
    }

    /// An instruction on an active session.
    fn instructed(
        &mut self,
        message: &Message,
        now: OffsetDateTime,
    ) -> (ReceptionStatusValues, Option<String>) {
        // The signed power an FRBC factor implies, against the ratings the
        // caller gave this session — one arithmetic with `instruct`, so a
        // session and a planner cannot disagree about what a factor meant.
        let power = |direction: instruct::Direction, factor: f64| match direction {
            instruct::Direction::In => Power::new(self.ratings.charge.get() * factor),
            instruct::Direction::Out => Power::new(-self.ratings.discharge.get() * factor),
        };
        let decoded: Result<(Id, Instructed), InstructError> = match (&self.offer, message) {
            (Offer::Battery(d), Message::FrbcInstruction(i)) => {
                instruct::actuator_factor(&d.actuator, &d.charge, Some(&d.discharge), i)
                    .map(|(id, dir, f)| (id, Instructed::Power(power(dir, f))))
            }
            (Offer::Ev(d), Message::FrbcInstruction(i)) => {
                instruct::actuator_factor(&d.actuator, &d.charge, d.discharge.as_ref(), i)
                    .map(|(id, dir, f)| (id, Instructed::Power(power(dir, f))))
            }
            (Offer::Dhw(d), Message::FrbcInstruction(i)) => {
                instruct::actuator_factor(&d.actuator, &d.heat, None, i)
                    .map(|(id, dir, f)| (id, Instructed::Power(power(dir, f))))
            }
            (Offer::HeatPump(d), Message::OmbcInstruction(i)) => {
                instruct::heat_pump_state(d, i).map(|s| (i.id, Instructed::SgReady(s)))
            }
            (Offer::Programme(d), Message::PpbcScheduleInstruction(i)) => {
                instruct::programme_start(d, i).map(|at| (i.id, Instructed::Start(at)))
            }
            (Offer::Envelope(_), Message::PebcInstruction(i)) => {
                Ok((i.id, Instructed::Envelope(i.clone())))
            }
            // Everything else on an active session is either a message a CEM may
            // legitimately send and this RM has nothing to do with, or one it
            // may not. Both are answered `OK` and ignored: refusing a
            // `PowerForecast` because we do not use one would be a Resource
            // Manager insisting a manager stop being helpful.
            _ => return (ReceptionStatusValues::Ok, None),
        };

        match decoded {
            Ok((instruction, wanted)) => {
                // Accepted first, always: the instruction is well formed and
                // names something this resource described, whenever it is for.
                self.outbox.push_back(
                    InstructionStatusUpdate::builder()
                        .instruction_id(instruction)
                        .status_type(InstructionStatus::Accepted)
                        .timestamp(utc(now))
                        .build()
                        .into(),
                );
                let at = execution_time(message).unwrap_or(now);
                let s = Scheduled {
                    at,
                    wanted,
                    instruction,
                };
                if at <= now {
                    // "In the past means as soon as possible."
                    self.start(s, now);
                } else if self.pending.len() >= MAX_PENDING {
                    let label = format!(
                        "this resource is already holding {MAX_PENDING} scheduled instructions"
                    );
                    self.outbox.push_back(
                        InstructionStatusUpdate::builder()
                            .instruction_id(instruction)
                            .status_type(InstructionStatus::Rejected)
                            .timestamp(utc(now))
                            .build()
                            .into(),
                    );
                    return (ReceptionStatusValues::TemporaryError, Some(label));
                } else {
                    // Kept in time order, so `advance` can stop at the first one
                    // that is not due and a schedule is carried out in the
                    // order it describes rather than the order it arrived.
                    let i = self.pending.partition_point(|p| p.at <= at);
                    self.pending.insert(i, s);
                }
                (ReceptionStatusValues::Ok, None)
            }
            Err(reason) => {
                let label = reason.to_string();
                if let Some(id) = instruction_id(message) {
                    self.outbox.push_back(
                        InstructionStatusUpdate::builder()
                            .instruction_id(id)
                            .status_type(InstructionStatus::Rejected)
                            .timestamp(utc(now))
                            .build()
                            .into(),
                    );
                }
                self.events.push(SessionEvent::Refused {
                    asset: self.asset.clone(),
                    reason,
                });
                (ReceptionStatusValues::InvalidContent, Some(label))
            }
        }
    }

    fn close(&mut self, reason: CloseReason) {
        // A schedule belongs to the manager that made it. One that has gone —
        // or has been refused — must not go on moving a household's battery
        // from a queue nobody can revoke, which is the § 14a failsafe's own
        // failure mode with the ownership reversed.
        self.pending.clear();
        self.events.push(SessionEvent::Closed(reason.clone()));
        self.state = SessionState::Closed(reason);
    }

    /// How many accepted instructions are waiting for their moment.
    #[must_use]
    pub fn scheduled(&self) -> usize {
        self.pending.len()
    }
}

/// What a resource can move, so a measured power can be read back as a fraction
/// of an operation mode.
///
/// The ratings live on the *asset* rather than in the description — S2 describes
/// a fill rate in kWh/s and a factor is relative to the mode's own range — so
/// they are a parameter here rather than a field. A caller passing the wrong
/// ones reports a factor that does not match the power measurement beside it,
/// which is why the two are produced in one call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Ratings {
    /// The most it can draw.
    pub charge: Power,
    /// The most it can give back. Zero for anything one-way.
    pub discharge: Power,
}

impl Ratings {
    /// A one-way resource.
    #[must_use]
    pub const fn draws(charge: Power) -> Self {
        Self {
            charge,
            discharge: Power::ZERO,
        }
    }

    /// A two-way one.
    #[must_use]
    pub const fn both(charge: Power, discharge: Power) -> Self {
        Self { charge, discharge }
    }
}

/// The instruction identifier a message carries, where it is one.
fn instruction_id(message: &Message) -> Option<Id> {
    match message {
        Message::FrbcInstruction(i) => Some(i.id),
        Message::OmbcInstruction(i) => Some(i.id),
        Message::PebcInstruction(i) => Some(i.id),
        Message::PpbcScheduleInstruction(i) => Some(i.id),
        Message::DdbcInstruction(i) => Some(i.id),
        _ => None,
    }
}

/// When an instruction says it should start.
///
/// Every control type spells the field the same way and means the same thing by
/// it — *"in the past means as soon as possible"* — so this is the one place
/// that reads it. A `PPBC.ScheduleInstruction` is deliberately **not** here: its
/// execution time is the whole of what it decides and it travels in the event as
/// [`Instructed::Start`], for the planner to place rather than the session to
/// wait out. A dishwasher is scheduled, not switched.
fn execution_time(message: &Message) -> Option<OffsetDateTime> {
    let at = match message {
        Message::FrbcInstruction(i) => i.execution_time,
        Message::OmbcInstruction(i) => i.execution_time,
        Message::PebcInstruction(i) => i.execution_time,
        Message::DdbcInstruction(i) => i.execution_time,
        _ => return None,
    };
    OffsetDateTime::try_from(at).ok()
}

/// A message's name, for a diagnostic a human will read.
fn kind_of(message: &Message) -> &'static str {
    match message {
        Message::Handshake(_) => "Handshake",
        Message::HandshakeResponse(_) => "HandshakeResponse",
        Message::SelectControlType(_) => "SelectControlType",
        Message::ResourceManagerDetails(_) => "ResourceManagerDetails",
        Message::FrbcInstruction(_) => "FRBC.Instruction",
        Message::OmbcInstruction(_) => "OMBC.Instruction",
        Message::PebcInstruction(_) => "PEBC.Instruction",
        Message::PpbcScheduleInstruction(_) => "PPBC.ScheduleInstruction",
        Message::RevokeObject(_) => "RevokeObject",
        _ => "message",
    }
}

/// A state's name, for the same diagnostic.
const fn name_of(state: &SessionState) -> &'static str {
    match state {
        SessionState::Idle => "not yet opened",
        SessionState::AwaitingHandshake => "waiting for the CEM's handshake",
        SessionState::AwaitingVersion => "waiting for a protocol version",
        SessionState::AwaitingControlType => "waiting for a control type",
        SessionState::Active(_) => "active",
        SessionState::Closed(_) => "closed",
    }
}

/// The `PPBC.PowerProfileStatus` for an appliance that has not been scheduled.
///
/// Its own function because a profile status is about a *sequence container*
/// rather than about a rate, so it has no place in [`Session::report`]'s
/// measurement path — and because an appliance that is waiting is exactly the
/// case a CEM needs to see, and the one an implementation forgets.
#[must_use]
pub fn profile_status(
    description: &ProgrammeDescription,
    status: ppbc::PowerSequenceStatus,
    into: Option<s2_kit::types::Duration>,
) -> ppbc::PowerProfileStatus {
    let chosen = matches!(
        status,
        ppbc::PowerSequenceStatus::Scheduled
            | ppbc::PowerSequenceStatus::Executing
            | ppbc::PowerSequenceStatus::Finished
    )
    .then(|| description.sequence);
    ppbc::PowerProfileStatus {
        message_id: Id::generate(),
        sequence_container_status: vec![ppbc::PowerSequenceContainerStatus {
            power_profile_id: description.definition.id,
            sequence_container_id: description.container,
            selected_sequence_id: chosen,
            status,
            progress: into,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::describe::{describe_battery, describe_heat_pump};
    use crate::resource_manager_details;
    use hems_core::asset::{Asset, AssetMeta, Battery, Capabilities, HeatPump, HeatPumpControl};
    use hems_core::prelude::{CircuitId, Energy, PhaseConnection, PhaseMode, Soc};
    use s2_kit::types::common::{HandshakeResponse, SelectControlType};
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-01-15 12:00 UTC);

    fn battery() -> Battery {
        Battery {
            meta: AssetMeta::new(
                AssetId::new("battery").unwrap(),
                CircuitId::new("main").unwrap(),
                PhaseConnection::Three,
                Power::from_kw(5.0),
            )
            .with_capabilities(Capabilities::MEASURE | Capabilities::SET_POWER),
            capacity: Energy::from_kwh(10.0),
            max_charge: Power::from_kw(5.0),
            max_discharge: Power::from_kw(5.0),
            efficiency_charge: 0.95,
            efficiency_discharge: 0.95,
            soc_min: Soc::new(0.05).unwrap(),
            soc_max: Soc::FULL,
            reserve_soc: Soc::new(0.1).unwrap(),
            grid_charging_allowed: true,
        }
    }

    fn battery_session() -> (Session, BatteryDescription) {
        let b = battery();
        let description = describe_battery(&b, T0);
        let asset = Asset::Battery(b.clone());
        let session = Session::new(
            b.meta.id.clone(),
            resource_manager_details(&asset, PhaseMode::Three, false),
            Offer::Battery(Box::new(description.clone())),
            Ratings::both(b.max_charge, b.max_discharge),
        );
        (session, description)
    }

    /// Everything the session sent, drained.
    fn sent(session: &mut Session) -> Vec<Message> {
        let mut out = Vec::new();
        while let Some(m) = session.poll_transmit() {
            out.push(m);
        }
        out
    }

    fn version() -> s2_kit::types::ProtocolVersion {
        ours()
    }

    /// The CEM's own handshake. S2 makes its version list *optional* — the RM is
    /// the constrained side — so this deliberately sends none.
    fn cem_handshake() -> Message {
        Handshake::builder()
            .role(EnergyManagementRole::Cem)
            // **Absent**, not empty. S2 makes the list optional because the RM
            // is the constrained side, and `s2-kit` types it as
            // `Option<Vec<_>>` — so the distinction the schema draws is one this
            // test can now express rather than approximate with an empty vector.
            .build()
            .into()
    }

    /// Drive the handshake to the point where a CEM may instruct.
    fn negotiate(session: &mut Session, control: ControlType) -> Vec<Message> {
        session.open();
        let opening = sent(session);
        session.on_message(&cem_handshake(), T0);
        session.on_message(
            &HandshakeResponse::builder()
                .selected_protocol_version(version())
                .build()
                .into(),
            T0,
        );
        session.on_message(
            &SelectControlType {
                control_type: control,
                message_id: Id::generate(),
            }
            .into(),
            T0,
        );
        let mut all = opening;
        all.extend(sent(session));
        all
    }

    /// A whole conversation, with no socket in it.
    ///
    /// The point of the module: every step of an S2 negotiation is an assertion
    /// rather than a WebSocket and a sleep.
    #[test]
    fn a_whole_negotiation_is_a_unit_test() {
        let (mut session, _) = battery_session();
        assert_eq!(*session.state(), SessionState::Idle);

        session.open();
        let opening = sent(&mut session);
        assert!(
            matches!(opening.as_slice(), [Message::Handshake(h)]
                if h.role == EnergyManagementRole::Rm
                    && h.supported_protocol_versions.as_ref().is_some_and(|v| !v.is_empty())),
            "the RM speaks first, and its handshake is the one that must list versions: {opening:?}"
        );
        assert_eq!(*session.state(), SessionState::AwaitingHandshake);

        session.on_message(&cem_handshake(), T0);
        assert_eq!(*session.state(), SessionState::AwaitingVersion);

        session.on_message(
            &HandshakeResponse::builder()
                .selected_protocol_version(version())
                .build()
                .into(),
            T0,
        );
        assert_eq!(*session.state(), SessionState::AwaitingControlType);
        let after_version = sent(&mut session);
        assert!(
            after_version
                .iter()
                .any(|m| matches!(m, Message::ResourceManagerDetails(_))),
            "the details go out once the version is agreed: {after_version:?}"
        );

        session.on_message(
            &SelectControlType {
                control_type: ControlType::FillRateBasedControl,
                message_id: Id::generate(),
            }
            .into(),
            T0,
        );
        assert_eq!(
            *session.state(),
            SessionState::Active(ControlType::FillRateBasedControl)
        );
        let after_select = sent(&mut session);
        assert!(
            after_select
                .iter()
                .any(|m| matches!(m, Message::FrbcSystemDescription(_))),
            "the description follows the selection: {after_select:?}"
        );
        assert_eq!(
            session.drain(),
            vec![SessionEvent::Ready(ControlType::FillRateBasedControl)]
        );
    }

    /// Every message is acknowledged, and a `ReceptionStatus` is not.
    ///
    /// S2 makes the acknowledgement mandatory, and answering one would be an
    /// infinite politeness.
    #[test]
    fn every_message_is_acknowledged_except_an_acknowledgement() {
        let (mut session, _) = battery_session();
        session.open();
        let _ = sent(&mut session);

        session.on_message(&cem_handshake(), T0);
        let out = sent(&mut session);
        assert!(
            out.iter().any(
                |m| matches!(m, Message::ReceptionStatus(r) if r.status == ReceptionStatusValues::Ok)
            ),
            "{out:?}"
        );

        session.on_message(
            &ReceptionStatus {
                subject_message_id: Id::generate(),
                status: ReceptionStatusValues::Ok,
                diagnostic_label: None,
            }
            .into(),
            T0,
        );
        assert!(
            sent(&mut session).is_empty(),
            "acknowledging an acknowledgement never terminates"
        );
    }

    /// A CEM that selects a control type this resource does not offer is
    /// refused, and told which one it should have asked for.
    #[test]
    fn a_control_type_nobody_offered_ends_the_session_and_says_what_was_offered() {
        let (mut session, _) = battery_session();
        session.open();
        let _ = sent(&mut session);
        session.on_message(&cem_handshake(), T0);
        session.on_message(
            &HandshakeResponse::builder()
                .selected_protocol_version(version())
                .build()
                .into(),
            T0,
        );
        let _ = sent(&mut session);

        session.on_message(
            &SelectControlType {
                control_type: ControlType::PowerProfileBasedControl,
                message_id: Id::generate(),
            }
            .into(),
            T0,
        );
        assert!(matches!(
            session.state(),
            SessionState::Closed(CloseReason::UnofferedControlType {
                offered: ControlType::FillRateBasedControl,
                ..
            })
        ));
        let out = sent(&mut session);
        assert!(
            out.iter().any(|m| matches!(m, Message::ReceptionStatus(r)
                if r.status == ReceptionStatusValues::InvalidContent
                && r.diagnostic_label.as_ref().is_some_and(|l| l.contains("FillRateBasedControl")))),
            "the refusal has to name what was on offer, or a CEM cannot correct itself: {out:?}"
        );
    }

    /// A version the RM never offered ends the session rather than being
    /// guessed at.
    #[test]
    fn a_version_that_was_never_offered_is_refused() {
        let (mut session, _) = battery_session();
        session.open();
        let _ = sent(&mut session);
        session.on_message(&cem_handshake(), T0);
        session.on_message(
            &HandshakeResponse::builder()
                .selected_protocol_version(s2_kit::types::ProtocolVersion::new("9.9.9"))
                .build()
                .into(),
            T0,
        );
        assert!(matches!(
            session.state(),
            SessionState::Closed(CloseReason::UnsupportedVersion { .. })
        ));
    }

    /// An instruction is answered **twice**, and the two answers are different
    /// questions.
    #[test]
    fn an_instruction_is_read_and_then_decided_on() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let _ = session.drain();

        let id = Id::generate();
        session.on_message(
            &frbc::Instruction {
                id,
                message_id: Id::generate(),
                actuator_id: description.actuator,
                operation_mode: description.discharge,
                operation_mode_factor: 0.5,
                execution_time: utc(T0),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );

        let events = session.drain();
        assert_eq!(
            events,
            vec![SessionEvent::Instructed {
                asset: AssetId::new("battery").unwrap(),
                // Half of a 5 kW discharge, load convention.
                wanted: Instructed::Power(Power::from_kw(-2.5)),
                instruction: id,
            }]
        );

        let out = sent(&mut session);
        assert!(
            out.iter().any(|m| matches!(m, Message::ReceptionStatus(_))),
            "the wire is answered: {out:?}"
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::InstructionStatusUpdate(u)
                if u.instruction_id == id && u.status_type == InstructionStatus::Accepted)),
            "and so is the household: {out:?}"
        );
    }

    /// An instruction for later is carried out **later**.
    ///
    /// `execution_time` is "when to start; in the past means as soon as
    /// possible", so a time in the future is a schedule rather than a
    /// suggestion. A test that sets it to the session's own `now` cannot tell
    /// the two behaviours apart (D212).
    #[test]
    fn an_instruction_for_later_is_not_carried_out_now() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let _ = session.drain();

        let id = Id::generate();
        let later = T0 + time::Duration::hours(2);
        session.on_message(
            &frbc::Instruction {
                id,
                message_id: Id::generate(),
                actuator_id: description.actuator,
                operation_mode: description.discharge,
                operation_mode_factor: 0.5,
                execution_time: utc(later),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );

        assert_eq!(
            session.drain(),
            vec![],
            "an instruction two hours out must not move the battery now"
        );
        let out = sent(&mut session);
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::InstructionStatusUpdate(u)
                if u.instruction_id == id && u.status_type == InstructionStatus::Accepted)),
            "…and it is still accepted, because it is a perfectly good instruction: {out:?}"
        );
    }

    /// …and it *is* carried out when its moment comes.
    ///
    /// The other half, without which the fix above is indistinguishable from
    /// dropping the instruction on the floor.
    #[test]
    fn an_instruction_for_later_is_carried_out_then() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let _ = session.drain();

        let id = Id::generate();
        let later = T0 + time::Duration::hours(2);
        session.on_message(
            &frbc::Instruction {
                id,
                message_id: Id::generate(),
                actuator_id: description.actuator,
                operation_mode: description.discharge,
                operation_mode_factor: 0.5,
                execution_time: utc(later),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );
        assert_eq!(session.scheduled(), 1);
        let _ = session.drain();
        let _ = sent(&mut session);

        // A minute early is still early.
        session.advance(later - time::Duration::minutes(1));
        assert_eq!(session.drain(), vec![]);

        session.advance(later);
        assert_eq!(
            session.drain(),
            vec![SessionEvent::Instructed {
                asset: AssetId::new("battery").unwrap(),
                wanted: Instructed::Power(Power::from_kw(-2.5)),
                instruction: id,
            }]
        );
        assert_eq!(session.scheduled(), 0, "and it is not carried out twice");
        let out = sent(&mut session);
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::InstructionStatusUpdate(u)
                if u.instruction_id == id && u.status_type == InstructionStatus::Started)),
            "`Started` is the status S2 has for exactly this moment: {out:?}"
        );
    }

    /// A manager that hands the resource back is heard.
    ///
    /// `NO_SELECTION` is a **state**, not a capability: "to be used if no
    /// control type is or has been selected". Before this it was read as an
    /// unoffered control type and tore the session down at selection time, and
    /// was answered `OK` and ignored on an active one — so a household went on
    /// obeying a manager that had stopped asking, at a rank *above its own
    /// plan*.
    #[test]
    fn a_manager_that_selects_no_control_type_hands_the_resource_back() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let later = T0 + time::Duration::hours(2);
        session.on_message(
            &frbc::Instruction {
                id: Id::generate(),
                message_id: Id::generate(),
                actuator_id: description.actuator,
                operation_mode: description.discharge,
                operation_mode_factor: 1.0,
                execution_time: utc(later),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );
        assert_eq!(session.scheduled(), 1);
        let _ = session.drain();
        let _ = sent(&mut session);

        session.on_message(
            &SelectControlType::builder()
                .control_type(ControlType::NoSelection)
                .build()
                .into(),
            T0,
        );

        assert!(
            session.drain().contains(&SessionEvent::Released),
            "the arbiter has to be told, or the hold outlives the manager"
        );
        assert_eq!(session.scheduled(), 0, "and the schedule goes with it");
        assert!(
            !matches!(session.state(), SessionState::Closed(_)),
            "the connection stays up: a manager that is not driving may still \
             want the measurements S2 keeps flowing"
        );
        session.advance(later);
        assert_eq!(
            session.drain(),
            vec![],
            "nothing it was holding moves the household afterwards"
        );

        // …and it can pick the resource up again without reconnecting.
        session.on_message(
            &SelectControlType::builder()
                .control_type(ControlType::FillRateBasedControl)
                .build()
                .into(),
            T0,
        );
        assert!(
            session
                .drain()
                .contains(&SessionEvent::Ready(ControlType::FillRateBasedControl)),
            "selecting again is how a manager comes back"
        );
    }

    /// A manager may withdraw an instruction it has not had carried out yet.
    ///
    /// Only load-bearing since instructions started waiting for their execution
    /// time: before that there was never anything to revoke, so `RevokeObject`
    /// fell through the catch-all and a manager that scheduled a discharge for
    /// the evening and thought better of it was ignored.
    #[test]
    fn a_scheduled_instruction_can_be_withdrawn() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let id = Id::generate();
        let later = T0 + time::Duration::hours(2);
        session.on_message(
            &frbc::Instruction {
                id,
                message_id: Id::generate(),
                actuator_id: description.actuator,
                operation_mode: description.discharge,
                operation_mode_factor: 1.0,
                execution_time: utc(later),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );
        assert_eq!(session.scheduled(), 1);
        let _ = session.drain();
        let _ = sent(&mut session);

        session.on_message(
            &RevokeObject::builder()
                .object_type(RevokableObjects::FrbcInstruction)
                .object_id(id)
                .build()
                .into(),
            T0,
        );
        assert_eq!(session.scheduled(), 0);
        let out = sent(&mut session);
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::InstructionStatusUpdate(u)
                if u.instruction_id == id && u.status_type == InstructionStatus::Revoked)),
            "`Revoked` is the status S2 has for this: {out:?}"
        );

        session.advance(later);
        assert_eq!(
            session.drain(),
            vec![],
            "and a withdrawn instruction does not move the household afterwards"
        );
    }

    /// A manager that has lost track of what it sent must not exhaust the box.
    #[test]
    fn a_schedule_is_bounded_and_the_refusal_is_told_to_the_manager() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let instruct = |session: &mut Session, n: i64| -> Id {
            let id = Id::generate();
            session.on_message(
                &frbc::Instruction {
                    id,
                    message_id: Id::generate(),
                    actuator_id: description.actuator,
                    operation_mode: description.charge,
                    operation_mode_factor: 1.0,
                    execution_time: utc(T0 + time::Duration::minutes(15 * n)),
                    abnormal_condition: false,
                }
                .into(),
                T0,
            );
            id
        };
        for n in 1..=i64::try_from(MAX_PENDING).unwrap() {
            let _ = instruct(&mut session, n);
        }
        assert_eq!(session.scheduled(), MAX_PENDING);
        let _ = sent(&mut session);

        let refused = instruct(&mut session, 9_999);
        assert_eq!(session.scheduled(), MAX_PENDING, "the queue held");
        let out = sent(&mut session);
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::InstructionStatusUpdate(u)
                if u.instruction_id == refused && u.status_type == InstructionStatus::Rejected)),
            "a dropped instruction the manager was not told about is a manager \
             planning against a household that is quietly not listening: {out:?}"
        );
    }

    /// What became of an instruction reaches the CEM, and only while the session
    /// is up.
    ///
    /// `instruction_became` had no caller and no test: it is the second half of
    /// the pair — acceptance says the household intends to carry an instruction
    /// out, this says whether it did — and a CEM planning against a resource
    /// whose instructions all end `ABORTED` is planning against a household that
    /// is quietly refusing. A method nothing calls and nothing checks is a
    /// message nobody would notice going missing.
    #[test]
    fn what_became_of_an_instruction_is_reported_while_the_session_is_up() {
        let (mut session, _description) = battery_session();
        let id = Id::generate();

        // Before the handshake there is nobody to tell, and the message must not
        // queue up waiting for one: a status update about an instruction the
        // peer never sent is a protocol error at the far end.
        let fresh = {
            let (mut s, _) = battery_session();
            s.instruction_became(id, InstructionStatus::Started, T0);
            sent(&mut s)
        };
        assert!(
            fresh.is_empty(),
            "nothing is queued for a peer that is not there: {fresh:?}"
        );

        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let _ = sent(&mut session);

        session.instruction_became(id, InstructionStatus::Started, T0);
        session.instruction_became(id, InstructionStatus::Aborted, T0);
        let out = sent(&mut session);
        let updates: Vec<_> = out
            .iter()
            .filter_map(|m| match m {
                Message::InstructionStatusUpdate(u) if u.instruction_id == id => {
                    Some(u.status_type)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            updates,
            vec![InstructionStatus::Started, InstructionStatus::Aborted],
            "both, in order, and about the instruction they name: {out:?}"
        );
    }

    /// An instruction naming somebody else's actuator is refused on both
    /// channels, and the refusal reaches the box as an event.
    ///
    /// A CEM that keeps addressing an actuator this household does not have is a
    /// commissioning fault, and it is invisible from its own side because it is
    /// being answered politely every time.
    #[test]
    fn an_instruction_for_another_actuator_is_refused_and_reported() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let _ = session.drain();

        let id = Id::generate();
        session.on_message(
            &frbc::Instruction {
                id,
                message_id: Id::generate(),
                actuator_id: Id::generate(),
                operation_mode: description.charge,
                operation_mode_factor: 1.0,
                execution_time: utc(T0),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );

        assert_eq!(
            session.drain(),
            vec![SessionEvent::Refused {
                asset: AssetId::new("battery").unwrap(),
                reason: InstructError::UnknownActuator,
            }]
        );
        let out = sent(&mut session);
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::InstructionStatusUpdate(u)
                if u.instruction_id == id && u.status_type == InstructionStatus::Rejected)),
            "{out:?}"
        );
    }

    /// A status says the same thing the power measurement beside it does.
    ///
    /// The inverse of `instruct`, and it has to be exact: a CEM reconciles the
    /// two, and two arithmetics for one fact is how it stops being able to.
    #[test]
    fn a_status_and_the_measurement_beside_it_agree() {
        let (mut session, description) = battery_session();
        let _ = negotiate(&mut session, ControlType::FillRateBasedControl);
        let _ = sent(&mut session);

        session.report(
            &Report {
                power: Some(Power::from_kw(-2.5)),
                fill: Some(6.4),
            },
            T0,
        );
        let out = sent(&mut session);

        let measured = out.iter().find_map(|m| match m {
            Message::PowerMeasurement(p) => p.values.first().map(|v| v.value),
            _ => None,
        });
        assert_eq!(measured, Some(-2500.0));

        let status = out.iter().find_map(|m| match m {
            Message::FrbcActuatorStatus(s) => Some(s),
            _ => None,
        });
        let status = status.expect("an actuator status");
        assert_eq!(status.active_operation_mode_id, description.discharge);
        assert!((status.operation_mode_factor - 0.5).abs() < 1e-9);

        assert!(
            out.iter()
                .any(|m| matches!(m, Message::FrbcStorageStatus(s) if (s.present_fill_level - 6.4).abs() < 1e-9)),
            "a store owes its fill level: {out:?}"
        );
    }

    /// A status before the CEM has chosen a control type is not sent.
    ///
    /// It would be a message about a description the manager has not been sent.
    #[test]
    fn nothing_is_reported_before_a_control_type_is_chosen() {
        let (mut session, _) = battery_session();
        session.open();
        let _ = sent(&mut session);
        session.report(
            &Report {
                power: Some(Power::from_kw(1.0)),
                fill: Some(5.0),
            },
            T0,
        );
        assert!(sent(&mut session).is_empty());
    }

    /// An SG Ready state is **told**, not inferred.
    ///
    /// Two of its states can draw the same kilowatt and mean different things to
    /// the unit, so only whoever closed the contacts knows which one it is in.
    #[test]
    fn a_contact_state_is_reported_by_whoever_closed_the_contacts() {
        let hp = HeatPump {
            meta: AssetMeta::new(
                AssetId::new("waermepumpe").unwrap(),
                CircuitId::new("main").unwrap(),
                PhaseConnection::Three,
                Power::from_kw(3.0),
            )
            .with_capabilities(Capabilities::MEASURE | Capabilities::SET_MODE),
            electrical_nominal: Power::from_kw(3.0),
            heating_rod: None,
            cooling_electrical: None,
            control: HeatPumpControl::SgReady,
            modulating: false,
            comfort_min_c: 20.0,
            comfort_max_c: 23.0,
            cop: hems_core::prelude::CopCurve::air_source(),
        };
        let description = describe_heat_pump(&hp, Power::from_kw(22.0), T0);
        let asset = Asset::HeatPump(hp.clone());
        let mut session = Session::new(
            hp.meta.id.clone(),
            resource_manager_details(&asset, PhaseMode::Three, false),
            Offer::HeatPump(Box::new(description.clone())),
            Ratings::draws(hp.electrical_nominal),
        );
        let _ = negotiate(&mut session, ControlType::OperationModeBasedControl);
        let _ = sent(&mut session);

        // A measurement alone says nothing about which state it is in.
        session.report(
            &Report {
                power: Some(Power::from_kw(3.0)),
                fill: None,
            },
            T0,
        );
        let measured = sent(&mut session);
        assert!(
            !measured.iter().any(|m| matches!(m, Message::OmbcStatus(_))),
            "a power cannot be inverted into a contact position: {measured:?}"
        );

        session.in_mode(SgReadyState::Boost, T0);
        let out = sent(&mut session);
        let status = out
            .iter()
            .find_map(|m| match m {
                Message::OmbcStatus(s) => Some(s),
                _ => None,
            })
            .expect("a status once somebody says which state it is");
        assert_eq!(
            description.state_of(&status.active_operation_mode_id),
            Some(SgReadyState::Boost)
        );
    }

    /// An appliance waiting is a status a CEM has to be sent.
    ///
    /// It is the boring case and the one an implementation forgets: a manager
    /// cannot schedule a dishwasher it has not been told is waiting.
    #[test]
    fn an_appliance_says_it_is_waiting_and_then_that_it_is_running() {
        use hems_core::asset::{FlexibleLoad, LoadKind, Programme};
        let programme = Programme::from_steps([Power::from_kw(2.0), Power::from_kw(0.4)]);
        let load = FlexibleLoad {
            meta: AssetMeta::new(
                AssetId::new("spuelmaschine").unwrap(),
                CircuitId::new("main").unwrap(),
                PhaseConnection::Single {
                    phase: hems_core::prelude::Phase::L1,
                },
                Power::from_kw(2.0),
            )
            .with_capabilities(Capabilities::MEASURE),
            nominal: Power::from_kw(2.0),
            kind: LoadKind::Shiftable(programme.clone()),
        };
        let description = crate::describe::describe_programme(
            &load,
            &programme,
            T0,
            T0 + time::Duration::hours(8),
        );
        let asset = Asset::Load(load.clone());
        let mut session = Session::new(
            load.meta.id.clone(),
            resource_manager_details(&asset, PhaseMode::Single, false),
            Offer::Programme(Box::new(description.clone())),
            Ratings::draws(programme.peak()),
        );
        let _ = negotiate(&mut session, ControlType::PowerProfileBasedControl);
        let _ = sent(&mut session);

        // A power measurement says nothing about a sequence: a dishwasher is
        // not half on.
        session.report(
            &Report {
                power: Some(Power::from_kw(2.0)),
                fill: None,
            },
            T0,
        );
        let measured = sent(&mut session);
        assert!(
            !measured
                .iter()
                .any(|m| matches!(m, Message::PpbcPowerProfileStatus(_))),
            "{measured:?}"
        );

        session.programme_is(ppbc::PowerSequenceStatus::NotScheduled, None);
        let waiting = sent(&mut session);
        let status = waiting
            .iter()
            .find_map(|m| match m {
                Message::PpbcPowerProfileStatus(s) => s.sequence_container_status.first(),
                _ => None,
            })
            .expect("a profile status");
        assert_eq!(status.status, ppbc::PowerSequenceStatus::NotScheduled);
        assert_eq!(
            status.selected_sequence_id, None,
            "nothing is selected until something is scheduled"
        );
        assert_eq!(status.sequence_container_id, description.container);

        session.programme_is(ppbc::PowerSequenceStatus::Executing, None);
        let running = sent(&mut session);
        let status = running
            .iter()
            .find_map(|m| match m {
                Message::PpbcPowerProfileStatus(s) => s.sequence_container_status.first(),
                _ => None,
            })
            .expect("a profile status");
        assert_eq!(
            status.selected_sequence_id.as_ref(),
            Some(&description.sequence),
            "a running appliance names the sequence it is running"
        );
    }

    /// A message out of order ends the session rather than being read under a
    /// schema version nobody agreed.
    #[test]
    fn a_message_before_the_handshake_ends_the_session() {
        let (mut session, _) = battery_session();
        session.open();
        let _ = sent(&mut session);
        session.on_message(
            &SelectControlType {
                control_type: ControlType::FillRateBasedControl,
                message_id: Id::generate(),
            }
            .into(),
            T0,
        );
        assert!(matches!(
            session.state(),
            SessionState::Closed(CloseReason::OutOfOrder { .. })
        ));
    }

    /// Everything a session puts on the wire survives the standard's own schema.
    ///
    /// The same assertion `SiteDescription::messages` earns: a message that
    /// cannot be serialised is a message that cannot be sent, and round-tripping
    /// checks the whole module against the schema for the price of one test.
    #[test]
    fn every_message_a_session_sends_round_trips_through_json() {
        let (mut session, description) = battery_session();
        let mut out = negotiate(&mut session, ControlType::FillRateBasedControl);
        session.report(
            &Report {
                power: Some(Power::from_kw(2.0)),
                fill: Some(7.5),
            },
            T0,
        );
        session.on_message(
            &frbc::Instruction {
                id: Id::generate(),
                message_id: Id::generate(),
                actuator_id: description.actuator,
                operation_mode: description.charge,
                operation_mode_factor: 0.4,
                execution_time: utc(T0),
                abnormal_condition: false,
            }
            .into(),
            T0,
        );
        out.extend(sent(&mut session));
        assert!(out.len() > 6, "there should be a conversation here");
        for message in out {
            let json = serde_json::to_string(&message).expect("every S2 message serialises");
            let back: Message = serde_json::from_str(&json).expect("and comes back");
            assert_eq!(back, message);
        }
    }
}
