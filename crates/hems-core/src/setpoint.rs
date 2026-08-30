//! Commands, and the reason each one exists.
//!
//! A setpoint without a reason is unexplainable after the fact, and "why did my
//! wallbox stop at 17:04?" is the single most common question a HEMS has to
//! answer — to the customer, to the installer, and to the network operator, who
//! may ask an operator to show that a § 14a reduction was actually carried out
//! (`[BK6-22-300 A1 7.2]`).
//!
//! So [`Setpoint::new`] is the only constructor and it takes a [`Reason`]. There
//! is no way to produce a command that cannot say where it came from.

use core::fmt;

use time::OffsetDateTime;

use crate::error::SetpointError;
use crate::ids::{AssetId, PlanId};
use crate::slot::Slot;
use crate::units::{Current, Power};

/// What an asset is being told to do.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(rename_all = "snake_case", tag = "kind", content = "value")
)]
pub enum Command {
    /// Track this active power, load convention: positive draws, negative feeds
    /// in or discharges.
    ActivePower(Power),
    /// Draw no more than this. A ceiling, not a target — the asset is free to
    /// use less, which is what every § 14a and § 9 EEG limit actually says.
    ConsumptionCeiling(Power),
    /// Feed in no more than this magnitude (a non-negative value).
    ProductionCeiling(Power),
    /// Charging current per used conductor (the unit a wallbox speaks).
    ChargingCurrent(Current),
    /// Use this many outer conductors (1 or 3).
    PhaseCount(u8),
    /// Enter this discrete operating mode. The `u8` is the SG Ready state 1–4
    /// or the index of an S2 `OMBC` operation mode.
    OperationMode(u8),
    /// Switch the asset on or off.
    OnOff(bool),
}

impl Command {
    /// `true` when every number in the command is finite.
    #[must_use]
    pub fn is_finite(&self) -> bool {
        match self {
            Command::ActivePower(p)
            | Command::ConsumptionCeiling(p)
            | Command::ProductionCeiling(p) => p.is_finite(),
            Command::ChargingCurrent(c) => c.is_finite(),
            Command::PhaseCount(_) | Command::OperationMode(_) | Command::OnOff(_) => true,
        }
    }
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::ActivePower(p) => write!(f, "active power {p}"),
            Command::ConsumptionCeiling(p) => write!(f, "consume at most {p}"),
            Command::ProductionCeiling(p) => write!(f, "feed in at most {p}"),
            Command::ChargingCurrent(c) => write!(f, "charging current {c}"),
            Command::PhaseCount(n) => write!(f, "{n}-phase"),
            Command::OperationMode(m) => write!(f, "operation mode {m}"),
            Command::OnOff(true) => f.write_str("on"),
            Command::OnOff(false) => f.write_str("off"),
        }
    }
}

/// The grid rules the guard plane can invoke.
///
/// The variants live here, in the dependency-free core, so that a [`Reason`]
/// can name a rule without `hems-core` depending on `hems-grid`. `hems-grid`
/// owns the rules' *content*; this enum is only their identity, and it is what
/// the UI, the evidence record and the operator see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum GuardRule {
    /// § 14a EnWG netzorientierte Steuerung: the network operator's limit on the
    /// netzwirksamer Leistungsbezug, received over EEBUS LPC or a relay.
    Lpc,
    /// § 9 EEG: the network operator's limit on feed-in, over EEBUS LPP.
    Lpp,
    /// § 9 EEG Solarspitzengesetz: the 60 % cap that applies until an intelligent
    /// metering system with a control device is in operation.
    Para9Cap,
    /// The EEBUS failsafe state, entered when the Energy Guard's heartbeat stops.
    Failsafe,
    /// A fuse or cable rating on the circuit path.
    CircuitLimit,
    /// The contractually agreed connection power.
    ContractLimit,
    /// The VDE-AR-N 4100 limit on unbalanced load (4,6 kVA) — the limit on the
    /// *installation*, so it binds the single-phase devices that can move it.
    Unbalance,
    /// The asset's own nameplate, rating, or state of charge.
    DeviceLimit,
    /// The energy held back for a power cut. Neither the planner nor the arbiter
    /// may discharge below it; only an islanded system may use it.
    BackupReserve,
}

impl fmt::Display for GuardRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            GuardRule::Lpc => "§ 14a EnWG limit",
            GuardRule::Lpp => "§ 9 EEG feed-in limit",
            GuardRule::Para9Cap => "§ 9 EEG 60 % cap",
            GuardRule::Failsafe => "EEBUS failsafe",
            GuardRule::CircuitLimit => "circuit limit",
            GuardRule::ContractLimit => "connection limit",
            GuardRule::Unbalance => "unbalance limit",
            GuardRule::DeviceLimit => "device limit",
            GuardRule::BackupReserve => "backup reserve",
        };
        f.write_str(s)
    }
}

/// Why the arbiter chose to correct a plan value inside a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RealtimeCause {
    /// Following the measured surplus rather than the forecast one.
    SurplusTracking,
    /// Covering a measured import from the store rather than from the grid.
    ///
    /// The other half of surplus tracking, and the behaviour that has to hold
    /// when there is no plan at all: a box with a full battery that imports the
    /// evening peak because its planner is gone is worse than one with no
    /// planner in the first place.
    SelfConsumption,
    /// Keeping the site's unbalance inside its limit.
    PhaseBalance,
    /// Ramping towards the target rather than jumping to it.
    RampLimit,
    /// Holding the previous value because the change was too small to be worth
    /// sending — a device asked to chase measurement noise wears out its relays
    /// for nothing.
    Hysteresis,
    /// A device left to its own controls, because nothing is limiting it.
    ///
    /// A heat pump has a thermostat and a hot-water tank has a sensor. An energy
    /// manager only ever tells them to use *less*, so an absent instruction
    /// means "no limit" — not "off". Reading it as "off", which is right for a
    /// battery and a charge point, is how a box with no plan spent a January day
    /// letting the house go cold and a June day handing out cold showers, while
    /// reporting a saving for both.
    LocalControl,
    /// A generator running at everything the weather offers, because nothing is
    /// limiting it.
    ///
    /// The default for an inverter, and it has to be said out loud. Everything
    /// else on a site answers a request for *more* power; an inverter answers a
    /// request for *less*, so reading its absent instruction as "zero" — the way
    /// an absent instruction reads for every load — tells the inverter to
    /// stop.
    MaximumPowerPoint,
}

/// A deliberate human override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum UserOverride {
    /// "Charge now", "heat now" — ignore price, respect the guard.
    Boost,
    /// Hold the asset off until further notice.
    Pause,
    /// The household is away; comfort constraints are relaxed.
    Away,
}

/// Why no plan was followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum FallbackCause {
    /// No plan covers the current slot.
    NoPlan,
    /// The plan is older than the operator allows it to be.
    PlanStale,
    /// The asset's driver is not reporting.
    DriverLost,
    /// The system clock is not synchronised, so price and window rules cannot be
    /// trusted. Grid rules still apply; tariff optimisation pauses.
    ClockUnsynchronised,
}

/// Why a setpoint has the value it has.
///
/// The variants are ordered by authority: a [`Reason::Guard`] value can never be
/// relaxed by anything below it. [`Reason::authority`] makes that order
/// machine-checkable, and `hems-realtime` asserts it on every tick.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case", tag = "source"))]
pub enum Reason {
    /// A grid or safety rule. Absolute — `[BK6-22-300 A1 4.6 S. 3]` requires that
    /// a network operator's reduction takes precedence over market-driven
    /// control whenever it is the stricter of the two.
    Guard {
        /// Which rule.
        rule: GuardRule,
        /// When the rule became active, where that is known.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339::option"))]
        since: Option<OffsetDateTime>,
    },
    /// A person asked for it.
    User(UserOverride),
    /// The optimiser's plan for this slot.
    Plan {
        /// Which plan.
        plan: PlanId,
        /// Which slot of it.
        slot: Slot,
        /// The marginal value of a kilowatt-hour in that slot, in €/kWh.
        ///
        /// The price the plan faces in that slot, not a shadow price: a true
        /// dual needs a solver that exposes them, and the pure-Rust backend
        /// does not. It explains *how much* the plan cared, and it is what the
        /// guard weights a § 14a allocation by.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        marginal_eur_per_kwh: Option<f64>,
    },
    /// A correction inside the slot the plan could not anticipate.
    Realtime(RealtimeCause),
    /// No plan was available.
    Fallback(FallbackCause),
}

/// How much authority a reason carries. Larger wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Authority {
    /// A fallback default.
    Fallback = 0,
    /// A correction within the plan's intent.
    Realtime = 1,
    /// The optimiser's plan.
    Plan = 2,
    /// A person's explicit wish.
    User = 3,
    /// A grid or safety rule. Nothing overrides it.
    Guard = 4,
}

impl Reason {
    /// The authority this reason carries.
    #[must_use]
    pub const fn authority(&self) -> Authority {
        match self {
            Reason::Guard { .. } => Authority::Guard,
            Reason::User(_) => Authority::User,
            Reason::Plan { .. } => Authority::Plan,
            Reason::Realtime(_) => Authority::Realtime,
            Reason::Fallback(_) => Authority::Fallback,
        }
    }

    /// A guard reason without a start time.
    #[must_use]
    pub const fn guard(rule: GuardRule) -> Self {
        Reason::Guard { rule, since: None }
    }

    /// A guard reason that knows when it started.
    #[must_use]
    pub const fn guard_since(rule: GuardRule, since: OffsetDateTime) -> Self {
        Reason::Guard {
            rule,
            since: Some(since),
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Guard {
                rule,
                since: Some(t),
            } => write!(f, "{rule} (since {t})"),
            Reason::Guard { rule, since: None } => write!(f, "{rule}"),
            Reason::User(o) => write!(f, "user: {o:?}"),
            Reason::Plan {
                slot,
                marginal_eur_per_kwh: Some(m),
                ..
            } => {
                write!(f, "plan for {slot} ({m:.4} €/kWh)")
            }
            Reason::Plan { slot, .. } => write!(f, "plan for {slot}"),
            Reason::Realtime(c) => write!(f, "realtime: {c:?}"),
            Reason::Fallback(c) => write!(f, "fallback: {c:?}"),
        }
    }
}

/// One command for one asset, with its reason and its time.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Setpoint {
    /// The asset this is aimed at.
    pub asset: AssetId,
    /// What to do.
    pub command: Command,
    /// Why.
    pub reason: Reason,
    /// When the decision was made.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub at: OffsetDateTime,
}

impl Setpoint {
    /// Build a setpoint, refusing a command that carries a non-finite number.
    ///
    /// This is the one place hems checks for `NaN`: quantities are constructed
    /// cheaply and infallibly everywhere else, and the gate sits where a number
    /// turns into an action on a physical device.
    ///
    /// # Errors
    /// [`SetpointError::NotFinite`] when the command contains `NaN` or infinity.
    pub fn new(
        asset: AssetId,
        command: Command,
        reason: Reason,
        at: OffsetDateTime,
    ) -> Result<Self, SetpointError> {
        if !command.is_finite() {
            return Err(SetpointError::NotFinite {
                asset: asset.to_string(),
            });
        }
        Ok(Self {
            asset,
            command,
            reason,
            at,
        })
    }

    /// The authority behind this setpoint.
    #[must_use]
    pub const fn authority(&self) -> Authority {
        self.reason.authority()
    }
}

impl fmt::Display for Setpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} — {}", self.asset, self.command, self.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-05-01 12:00:00 UTC);

    #[test]
    fn a_non_finite_command_never_becomes_a_setpoint() {
        let asset = AssetId::new("wallbox").unwrap();
        let err = Setpoint::new(
            asset.clone(),
            Command::ActivePower(Power::new_const(f64::NAN)),
            Reason::guard(GuardRule::Lpc),
            NOW,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SetpointError::NotFinite {
                asset: "wallbox".into()
            }
        );
    }

    #[test]
    fn guard_outranks_every_other_reason() {
        let guard = Reason::guard(GuardRule::Lpc).authority();
        for other in [
            Reason::User(UserOverride::Boost).authority(),
            Reason::Plan {
                plan: PlanId::new(),
                slot: Slot::containing(NOW),
                marginal_eur_per_kwh: None,
            }
            .authority(),
            Reason::Realtime(RealtimeCause::SurplusTracking).authority(),
            Reason::Fallback(FallbackCause::NoPlan).authority(),
        ] {
            assert!(guard > other, "guard must outrank {other:?}");
        }
    }

    #[test]
    fn a_setpoint_explains_itself() {
        let sp = Setpoint::new(
            AssetId::new("wallbox").unwrap(),
            Command::ConsumptionCeiling(Power::from_kw(4.2)),
            Reason::guard_since(GuardRule::Lpc, NOW),
            NOW,
        )
        .unwrap();
        let rendered = sp.to_string();
        assert!(rendered.contains("wallbox"), "{rendered}");
        assert!(rendered.contains("consume at most"), "{rendered}");
        assert!(rendered.contains("§ 14a"), "{rendered}");
    }
}
