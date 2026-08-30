//! The domain model of a home energy system.
//!
//! `hems-core` is the vocabulary every other crate in the workspace speaks. It
//! holds facts and no rules: what a site is made of, what a quarter hour is,
//! what a measurement and a command are. The regulation lives in `hems-grid`,
//! the economics in `hems-tariff`, the decisions in `hems-optimizer` and
//! `hems-realtime`.
//!
//! # Guarantees
//!
//! * **No I/O, no clock, no async.** Every function is a pure function of its
//!   arguments; time enters as a parameter. A whole winter day, DST transition
//!   included, is a unit test.
//! * **One sign convention.** See [`units`] — positive is power flowing *into*
//!   the thing being measured, so `grid == Σ assets` holds site-wide and can be
//!   tested ([`site::Site::balance_residual`]).
//! * **Commands explain themselves.** [`setpoint::Setpoint`] cannot be built
//!   without a [`setpoint::Reason`], and reasons carry an
//!   [`setpoint::Authority`] that makes "the grid limit wins" checkable rather
//!   than a convention.
//!
//! # Example
//!
//! ```
//! use hems_core::prelude::*;
//!
//! let wallbox = AssetId::new("wallbox-garage")?;
//! let limit = Setpoint::new(
//!     wallbox,
//!     Command::ConsumptionCeiling(Power::from_kw(4.2)),
//!     Reason::guard(GuardRule::Lpc),
//!     time::macros::datetime!(2026-01-15 17:04:00 UTC),
//! )?;
//!
//! assert_eq!(limit.authority(), Authority::Guard);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::doc_markdown
)]

pub mod asset;
pub mod circuit;
pub mod envelope;
pub mod error;
pub mod ids;
pub mod measurement;
pub mod plan;
pub mod setpoint;
pub mod site;
pub mod slot;
pub mod thermal;
pub mod units;

/// Everything a consumer normally wants, in one `use`.
pub mod prelude {
    pub use crate::asset::{
        Asset, AssetMeta, Battery, CapRelief, Capabilities, Chemistry, DhwTank, Evse, Fallgruppe,
        FlexibleLoad, HeatPump, HeatPumpControl, LegacyStatus, LoadKind, Meter, MeterRole, PvArray,
        Relay, SteuVeExemption,
    };
    pub use crate::circuit::{Circuit, Circuits};
    pub use crate::envelope::Envelope;
    pub use crate::error::{IdError, SetpointError, SiteError, UnitError};
    pub use crate::ids::{AssetId, CircuitId, PlanId, SiteId};
    pub use crate::measurement::{Freshness, Measurement};
    pub use crate::plan::{AssetTarget, CostBreakdown, Plan, SlotPlan};
    pub use crate::setpoint::{
        Authority, Command, FallbackCause, GuardRule, RealtimeCause, Reason, Setpoint, UserOverride,
    };
    pub use crate::site::{GeoPoint, GridConnection, Site};
    pub use crate::slot::{Horizon, SLOT, SLOTS_PER_DAY, Slot};
    pub use crate::thermal::{CopCurve, Rc2, Rc2Discrete, ThermalState};
    pub use crate::units::{
        ApparentPower, Current, Energy, NOMINAL_VOLTAGE, PerPhase, Phase, PhaseConnection,
        PhaseMode, Power, Soc, Voltage,
    };
}
