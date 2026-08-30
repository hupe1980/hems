//! The German grid rules for a home energy system — executable, cited, tested.
//!
//! Every rule here names the document it comes from, in the form
//! `[A1 4.5.2]` (Anlage 1 zum Beschluss BK6-22-300) or `[LPC-031]` (the EEBUS
//! use-case technical specification). The documents themselves are indexed in
//! `specs/README.md` with their retrieval URLs. A rule without a citation is a
//! bug.
//!
//! | Module | Rule |
//! |---|---|
//! | [`para14a`] | § 14a EnWG — which devices are controllable, under which regime, and the minimum power they keep |
//! | [`lpc`] | The EEBUS limitation state machine that carries § 14a and § 9 EEG limits the last few metres |
//! | [`para9`] | § 9 EEG — the 60 % feed-in cap and the EEBUS feed-in factor |
//! | [`modul3`] | § 14a Modul 3 — time-variable network charges |
//! | [`evidence`] | The two-year record of `[A1 7]` |
//! | [`mispel`] | Which kilowatt-hour through a store was green, quarter by quarter |
//! | [`sharing`] | § 42c — allocating a community's generation |
//!
//! Where a rule belongs to [`metering`] it is called rather than copied:
//! `P_min,14a` and the netzwirksamer Leistungsbezug, the Modul 3
//! Zählzeitdefinition and its conformance rules, the allocation identity § 42b
//! and § 42c settle on. This crate contributes what is specific to *control* —
//! grouping a site's assets into the devices the Festlegung counts, the
//! transitional regimes, the state machine, the evidence, MiSpeL, and the
//! § 42c cascade.
//!
//! # Nothing here decides anything on its own
//!
//! These functions return *facts*: this is a controllable device, this is the
//! minimum it is owed, this limit is in force. Turning them into setpoints is
//! `hems-realtime`'s guard plane, which is also where the precedence of
//! `[A1 4.6 S. 3]` — a network operator's reduction beats market-driven control
//! — is enforced and property-tested.
//!
//! ```
//! use hems_grid::para14a::{ControlMode, SteuVe, minimum_power};
//! use hems_core::prelude::{AssetId, Fallgruppe, Power};
//!
//! // A wallbox and a battery behind one energy management system.
//! let devices = [
//!     SteuVe { assets: vec![AssetId::new("wallbox")?], fallgruppe: Fallgruppe::Ladepunkt, power: Power::from_kw(11.0) },
//!     SteuVe { assets: vec![AssetId::new("battery")?], fallgruppe: Fallgruppe::Stromspeicher, power: Power::from_kw(5.0) },
//! ];
//!
//! // 4,2 kW + (2 − 1) × 0,8 × 4,2 kW = 7,56 kW — the floor the network
//! // operator may not go below.
//! let floor = minimum_power(&devices, ControlMode::Ems);
//! assert!((floor.kw() - 7.56).abs() < 1e-9);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::doc_markdown
)]

pub mod evidence;
pub mod lpc;
pub mod mispel;
pub mod modul3;
pub mod para14a;
pub mod para9;
pub mod sharing;

pub use evidence::{Action, ComplianceSample, ControlEvent, EvidenceRecorder, Observation};
pub use lpc::{Direction, LimitWrite, LpcConfig, LpcEvent, LpcMachine, LpcState, Nack, Outcome};
pub use mispel::{Abgrenzung, Basisfall, Pauschal, PauschalFall, abgrenzung_month, pauschal_year};
pub use modul3::{
    Modul3Calendar, Modul3Conformance, Modul3Context, Modul3Eligibility, Modul3Finding, Preisstufe,
    Quarter, Zaehlzeitdefinition,
};
pub use para9::{CapRelief, FeedInLimits, GenerationProfile};
pub use para14a::{
    ControlMode, Participation, SteuVe, classify_at, classify_on, is_controlled_on, minimum_power,
    netzwirksamer_leistungsbezug, participation, steuve_budget,
};
pub use sharing::{
    Allocation, Aufteilung, Community, Member, Share, allocate as allocate_sharing,
    allocate_by as allocate_sharing_by,
};
