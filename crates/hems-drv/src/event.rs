//! What a driver reports upwards.

use hems_core::prelude::{AssetId, Measurement, Power};
use time::{Duration, OffsetDateTime};

use crate::link::LinkState;

/// Something a driver observed or concluded.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "kind"))]
pub enum DriverEvent {
    /// The device reported what it is doing.
    Measured(Measurement),
    /// The network operator changed what the connection point may do.
    GridLimit(GridLimit),
    /// A command was answered.
    ///
    /// Reported even when it succeeded, because the § 14a evidence record
    /// `[A1 7.2]` is a record of what was *commanded and confirmed*, and a
    /// driver that only reported failures would leave the operator's Nachweis
    /// with nothing in it on a compliant day.
    Command(CommandOutcome),
    /// The grid connection point published how far PV feed-in is curtailed.
    ///
    /// EEBUS MGCP scenario 1 `[MGCP-011]`, and it is deliberately **not** a
    /// [`GridLimit`]: what crosses the wire is a *percentage* of the building's
    /// cumulated nominal PV peak power, and the sum it applies to is a property
    /// of the installation that no EEBUS message carries. Only the two together
    /// are a number of watts. A driver reports the factor and the site turns it
    /// into a ceiling, because the driver does not know the roof.
    FeedInFactor(FeedInFactor),
    /// A car was plugged in, unplugged, or said something new about itself.
    ///
    /// Not a [`DriverEvent::Measured`], and the distinction is the point: a
    /// charge point with no car on the end of it is working perfectly and has
    /// no state of charge to report, so an absent measurement would be
    /// indistinguishable from a socket nobody is listening to. Presence is a
    /// fact about the *session*, and the planner needs it before it needs any
    /// number: a car that is not there cannot be charged at any price.
    Vehicle(VehiclePresence),
    /// A device announced that it *could* consume, and on what terms.
    ///
    /// The one event in this list that is neither a measurement nor a limit. A
    /// ceiling can only ask an appliance to do less, and an appliance already
    /// under one does nothing when told again — so a plan that has worked out
    /// the house will be cheaper if the compressor runs *now* has had no way to
    /// say so. EEBUS OHPCF is that lever, and this is what it says.
    Flexibility(Flexibility),
    /// The network operator changed the values this household falls back to
    /// when it stops hearing from them (`[LPC-021]`, `[LPP-021]`).
    Failsafe(Failsafe),
    /// The link changed state.
    Link(LinkState),
}

/// What a household restrains itself to when the operator goes quiet.
///
/// Configuration the **operator writes**, not a measurement, and the only thing
/// in the § 14a exchange whose value outlives the session that set it: the
/// failsafe is what the box holds when there is no session at all. A box that
/// kept it only in memory would come back from a power cut on whatever its own
/// file says, discarding a value the network operator wrote — which is
/// `ATC_LPC_COM_PT_CSInit_003`, and is a certification failure as much as a
/// household one.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Failsafe {
    /// Which direction it bounds.
    pub direction: LimitDirection,
    /// The power the household restrains itself to.
    pub power: Power,
    /// How long it must hold that for once it starts.
    #[cfg_attr(feature = "serde", serde(with = "crate::event::seconds"))]
    pub minimum: Duration,
    /// When the operator wrote it.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
}

/// A process a device is offering to run, and the terms it attaches.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Flexibility {
    /// Which asset.
    pub asset: AssetId,
    /// What it would draw while running. `None` where the device published no
    /// figure, which is a device that can be started and not budgeted for.
    pub power: Option<Power>,
    /// How long it must run once started.
    ///
    /// The planner has carried a minimum runtime since the compressor model was
    /// written and has taken it from configuration. This is the same number
    /// arriving from the machine.
    #[cfg_attr(feature = "serde", serde(default, with = "crate::event::opt_seconds"))]
    pub min_run: Option<Duration>,
    /// And how long it must then rest before it may start again.
    #[cfg_attr(feature = "serde", serde(default, with = "crate::event::opt_seconds"))]
    pub min_rest: Option<Duration>,
    /// Whether the offered process is running or paused right now.
    pub running: bool,
    /// Whether the manager may abort it once started.
    pub stoppable: bool,
    /// Whether it may be paused and resumed.
    pub pausable: bool,
    /// When the device said so.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
}

impl Flexibility {
    /// Whether two offers say the same thing, ignoring when they said it.
    ///
    /// The registry is edge-driven and a re-plan is not free: a compressor
    /// restating an unchanged offer on every notification is not news.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.asset == other.asset
            && self.power == other.power
            && self.min_run == other.min_run
            && self.min_rest == other.min_rest
            && self.running == other.running
            && self.stoppable == other.stoppable
            && self.pausable == other.pausable
    }
}

/// An optional duration, on the wire as whole seconds.
#[cfg(feature = "serde")]
pub(crate) mod opt_seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::Duration;

    // `&Option<T>` rather than `Option<&T>`, which clippy would prefer: serde's
    // `with` contract is what fixes this signature, and a helper that took the
    // idiomatic one is a helper serde cannot call.
    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(value: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(d) => s.serialize_some(&d.whole_seconds()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<i64>::deserialize(d)?.map(Duration::seconds))
    }
}

/// A duration, on the wire as whole seconds.
#[cfg(feature = "serde")]
pub(crate) mod seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(value.whole_seconds())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::seconds(i64::deserialize(d)?))
    }
}

/// Whether there is a car on the charge point, and what it has said.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VehiclePresence {
    /// Whether a car is plugged in.
    ///
    /// EEBUS EVCC scenario 1 has no message for this at all: an `EV` entity
    /// *appearing* underneath the `EVSE` entity is the message, and scenario 8
    /// is the entity going away again. So this is read off the peer's own entity
    /// tree rather than out of a payload.
    pub connected: bool,
    /// How full the battery is, `0..=1` — `None` on a car that cannot say.
    ///
    /// A car on IEC 61851 has a pilot wire and nothing else: it cannot be asked
    /// its state of charge, and a box that assumed one would plan a charge for a
    /// battery it invented.
    pub soc: Option<f64>,
    /// The battery's usable size in watt-hours, where the car published one.
    pub capacity_wh: Option<f64>,
    /// When this was learned.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
}

/// How far the connection point says PV feed-in is curtailed `[MGCP-011]`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FeedInFactor {
    /// The factor, as a percentage in `[0, 100]`.
    ///
    /// `P_PV,feed-in ≤ percent/100 × Σ P_PV,AC,nom`. A percentage is not
    /// actionable on its own, and treating one as a power is the mistake this
    /// type exists to make impossible.
    pub percent: f64,
    /// When it arrived.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
}

/// A limit the network operator has set on the connection point.
///
/// Carries its own **direction** and **duration** rather than being applied to
/// whatever the caller assumes. A reduction has an end: `[LPC-909]` sends one
/// with the limit, and stretching today's ninety minutes across a whole horizon
/// plans the house under a limit that lapsed before teatime.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GridLimit {
    /// Whether it bounds what the site may draw or what it may feed in.
    pub direction: LimitDirection,
    /// The ceiling, as a non-negative magnitude. `None` releases the limit.
    pub ceiling: Option<Power>,
    /// How long it is valid for, where the operator said.
    ///
    /// `None` is "until further notice", which is not the same as "for ever":
    /// the failsafe releases on its own minimum if the operator then goes quiet.
    #[cfg_attr(feature = "serde", serde(default, with = "duration_secs_opt"))]
    pub duration: Option<Duration>,
    /// When it arrived.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
    /// Whether this is the operator asking, or the failsafe applying because it
    /// has gone quiet.
    ///
    /// The two look identical at the connection point and are entirely
    /// different events in the evidence record: one is a control action the
    /// operator has to be able to account for, the other is the household
    /// restraining itself because nobody is talking to it.
    pub source: LimitSource,
}

/// Which way a limit points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LimitDirection {
    /// What the site may draw — EEBUS LPC, § 14a EnWG.
    Consumption,
    /// What it may feed in — EEBUS LPP, § 9 EEG.
    Production,
}

/// Who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LimitSource {
    /// The network operator, over the wire.
    Operator,
    /// Nobody: the peer went quiet and the failsafe applies.
    Failsafe,
}

/// What a device answered when it was told something.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CommandOutcome {
    /// Whether the device accepted it.
    pub accepted: bool,
    /// What the device says it will actually do, where it says.
    ///
    /// Hardware clips: a wallbox told 7 A on a 6–16 A range may answer 7 A, and
    /// one told 3 A answers 6 A or nothing. The difference between commanded
    /// and confirmed is a number worth having, because it is where a plan and a
    /// house quietly stop agreeing.
    pub confirmed: Option<Power>,
    /// When.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
    /// What the device said, where it said anything.
    #[cfg_attr(feature = "serde", serde(default))]
    pub detail: Option<String>,
}

/// An optional [`Duration`] as whole seconds.
#[cfg(feature = "serde")]
mod duration_secs_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::Duration;

    /// Write the duration in seconds, or `null`.
    ///
    /// # Errors
    /// Never: an `i64` of seconds always serialises.
    #[expect(
        clippy::ref_option,
        reason = "serde's `with` module contract fixes this signature"
    )]
    pub fn serialize<S: Serializer>(v: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(d) => s.serialize_some(&d.whole_seconds()),
            None => s.serialize_none(),
        }
    }

    /// Read a duration from whole seconds, or `null`.
    ///
    /// # Errors
    /// When the input is neither a number nor `null`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<i64>::deserialize(d)?.map(Duration::seconds))
    }
}
