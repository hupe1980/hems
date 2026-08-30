//! What a kilowatt-hour costs, quarter hour by quarter hour.
//!
//! A German household bill is a stack, and every layer moves for its own
//! reasons:
//!
//! | Layer | Moves with | Where it comes from |
//! |---|---|---|
//! | Energy | the market, every 15 minutes since 01.10.2025 | § 41a EnWG dynamic tariffs, EPEX day-ahead |
//! | Network charge | the time of day, if Modul 3 is chosen | BK8-22/010-A, the operator's price sheet |
//! | Levies and taxes | the calendar year | StromStG, KWKG, § 19 StromNEV, offshore, Konzessionsabgabe |
//! | Feed-in | the support regime, and whether the price went negative | EEG §§ 19, 51 |
//!
//! [`source`] reads the published day-ahead curve out of whatever ENTSO-E,
//! SMARD, aWATTar, Tibber or Energy-Charts sent — a `&str` in, a series out, so
//! the crate still does no I/O and every source is tested against a captured
//! response including the two days a year that have 92 and 100 quarter hours.
//!
//! The optimiser needs one number per direction per slot, but the household
//! needs to see the stack, and the advisor ([`advisor`]) needs to take it apart
//! again to answer "would Modul 2 be cheaper for us?". So the layers stay
//! separate all the way through and are only summed at the edge.
//!
//! # Money is exact
//!
//! Prices and costs are [`rust_decimal::Decimal`] in cents per kilowatt-hour.
//! Optimisation runs on `f64` — a solver has no use for exact arithmetic — but
//! anything a household is shown or billed comes back through `Decimal`, and
//! the conversion happens once, at [`SlotPrice::import_f64`].

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::similar_names
)]

pub mod advisor;
pub mod levies;
pub mod source;
pub mod stack;
pub mod tariff;

pub use advisor::{Comparison, ModulChoice, compare_moduls};
pub use levies::Levies;
pub use source::{PriceBasis, PriceSeries, Source, SourceError};
pub use stack::{PriceStack, SlotPrice};
pub use tariff::{EnergyPrice, FeedIn, NetworkCharge, Tariff};
