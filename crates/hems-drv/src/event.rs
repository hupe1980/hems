//! What a driver reports upwards.

use hems_core::prelude::{Measurement, Power};
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
    /// The link changed state.
    Link(LinkState),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
