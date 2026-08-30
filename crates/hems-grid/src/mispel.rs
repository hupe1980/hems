//! MiSpeL — Marktintegration von Speichern und Ladepunkten, BK 618-25-02.
//!
//! From **1 October 2026** a household battery that has ever been charged from
//! the grid, or a bidirectional charge point, keeps its levy privileges and its
//! EEG support only if the energy flowing through it is *separated* — which
//! quantity was green and which was grey, quarter hour by quarter hour, over a
//! whole calendar month. Get it wrong and the household loses the Umlage
//! privilege on its grid draw, the market premium on its feed-in, or both.
//!
//! The Festlegung offers three options and this module implements the two that
//! need arithmetic:
//!
//! | Option | Who it is for | Here |
//! |---|---|---|
//! | **Ausschließlichkeit** | a store that is never charged from the grid | nothing to compute — it is `Battery::grid_charging_allowed == false` |
//! | **Abgrenzung** (Anlage 1) | anything else | [`abgrenzung_month`], formulas (1)–(33) |
//! | **Pauschal** (Anlage 2) | solar ≤ 30 kWp | [`pauschal_year`], formulas (P1)–(P15) |
//!
//! Citations are `[MiSpeL A1 (n)]` for formula `n` of Anlage 1 and
//! `[MiSpeL A2 (Pn)]` for Anlage 2
//! (`specs/bnetza/mispel-anlage1-abgrenzungsoption-arbeitsstand-20260805.pdf`,
//! `specs/bnetza/mispel-anlage2-pauschaloption-arbeitsstand-20260805.pdf`).
//!
//! # Why this is exact arithmetic and not control
//!
//! Every number here becomes money or a Nachweis, so it is
//! [`rust_decimal::Decimal`] throughout — principle P3 of the concept. The
//! optimiser's `f64` view of the same site is a *forecast*; this is the record.
//! The two must never be confused, and the type system is what stops them being.
//!
//! # The Festlegung is not final
//!
//! The published text is an Arbeitsstand of 05.08.2026 with Bekanntgabe planned
//! for 01.10.2026, so every result carries the [`RuleSet`] that produced it, and
//! a rule set knows the day it starts to apply. A Nachweis that cannot say which
//! rules produced it is not evidence of anything.

use core::fmt;

use hems_core::prelude::Slot;
use rust_decimal::Decimal;
use thiserror::Error;
use time::Date;

/// The storage round-trip efficiency the Festlegung *presumes* wherever a charge
/// point makes a measured one impossible, `[MiSpeL A1 (14)A2,A3,A4]`.
pub const PRESUMED_EFFICIENCY: Decimal = Decimal::from_parts(85, 0, 0, false, 2);

/// The yearly feed-in per kilowatt of installed solar power that the
/// Pauschaloption treats as supportable, `[MiSpeL A2 (P1)]` (§ 19 Abs. 3c EEG).
pub const PAUSCHAL_KWH_PER_KW: Decimal = Decimal::from_parts(500, 0, 0, false, 0);

/// The factor behind the indifference band of a bidirectional charge point,
/// `[MiSpeL A2 (P2)P2]`.
pub const PAUSCHAL_EVSE_FACTOR: Decimal = Decimal::from_parts(5, 0, 0, false, 1);

/// The coefficient of the storage-to-solar size ratio, kWh/kW,
/// `[MiSpeL A2 (P2)P1]`.
pub const PAUSCHAL_STORAGE_COEFFICIENT: Decimal = Decimal::from_parts(1, 0, 0, false, 1);

/// The largest solar installation the Pauschaloption is open to, kWp
/// (§ 19 Abs. 3c EEG).
pub const PAUSCHAL_MAX_KWP: Decimal = Decimal::from_parts(30, 0, 0, false, 0);

/// Which version of the Festlegung a result was computed under.
///
/// The formulas below are read from an **Arbeitsstand**. A Nachweis that cannot
/// name the rule set that produced it is not evidence of anything, so the
/// version travels with every result — and a new Festlegung is a new variant
/// here, which makes every place that has to think about the difference fail to
/// compile until it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
// The lint fires because `rust_decimal`'s `Decimal` has `unsafe` methods and
// this module's constants are built with `from_parts`; nothing here is unsafe
// and nothing about deserialising a fieldless enum can be.
#[allow(clippy::unsafe_derive_deserialize)]
#[non_exhaustive]
pub enum RuleSet {
    /// BK 618-25-02, Arbeitsstand 05.08.2026, Bekanntgabe planned 01.10.2026.
    #[default]
    Arbeitsstand20260805,
}

impl RuleSet {
    /// The day these rules start to apply.
    #[must_use]
    pub const fn effective_from(self) -> Date {
        match self {
            RuleSet::Arbeitsstand20260805 => time::macros::date!(2026 - 10 - 01),
        }
    }

    /// The document this version was read from.
    #[must_use]
    pub const fn version(self) -> &'static str {
        match self {
            RuleSet::Arbeitsstand20260805 => "BK 618-25-02, Arbeitsstand 05.08.2026",
        }
    }

    /// Whether these rules govern `day`.
    #[must_use]
    pub fn applies_on(self, day: Date) -> bool {
        day >= self.effective_from()
    }
}

impl fmt::Display for RuleSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.version())
    }
}

/// Which Basisfall of Anlage 1 the installation's metering concept matches,
/// `[MiSpeL A1 4.1]`.
///
/// The case is a property of the **meters**, not of the devices: it says which
/// of `Z2` and `Z3` exist and therefore which formulas can be evaluated at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Basisfall {
    /// A1 — a storage system, no charge point. The round-trip efficiency can be
    /// *measured*, which is the only case in which it is.
    A1,
    /// A2 — a bidirectional charge point, no storage system.
    A2,
    /// A3 — both, on one meter. The simplified alternative to A4.
    A3,
    /// A4 — both, with the storage system separately metered at `Z3`, which is
    /// what makes its losses privilegeable.
    A4,
}

impl Basisfall {
    /// Whether the round-trip efficiency is measured (A1) or presumed at 85 %
    /// (everything with a charge point in it), `[MiSpeL A1 (14)]`.
    #[must_use]
    pub const fn efficiency_is_measured(self) -> bool {
        matches!(self, Basisfall::A1)
    }

    /// Whether storage losses can be privileged at all, `[MiSpeL A1 (19)]`.
    ///
    /// Only where the storage system's own consumption and generation are
    /// separately visible: A1 has no charge point to confuse them, A4 has a
    /// dedicated meter. In A2 and A3 the answer is always zero.
    #[must_use]
    pub const fn losses_are_privilegeable(self) -> bool {
        matches!(self, Basisfall::A1 | Basisfall::A4)
    }

    /// Whether the case needs the separate storage meter `Z3`.
    #[must_use]
    pub const fn needs_z3(self) -> bool {
        matches!(self, Basisfall::A4)
    }
}

impl fmt::Display for Basisfall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Basisfall::A1 => "A1",
            Basisfall::A2 => "A2",
            Basisfall::A3 => "A3",
            Basisfall::A4 => "A4",
        })
    }
}

/// One quarter hour of measured energy, in kilowatt-hours.
///
/// The names are the Festlegung's own — `Z1NB¼`, `Z1NE¼`, `Z2V¼`, `Z2E¼`,
/// `Z3V¼`, `Z3E¼` — because a Nachweis that renames its inputs is a Nachweis
/// somebody has to translate before they can check it.
///
/// Every quantity is a **non-negative magnitude**: these are register
/// differences from real meters, not the signed control values of `hems-core`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct QuarterHour {
    /// The quarter hour.
    pub slot: Slot,
    /// `Z1NB¼` — grid draw at the Entnahmestelle.
    pub grid_draw: Decimal,
    /// `Z1NE¼` — grid feed-in at the Einspeisestelle.
    pub grid_feed_in: Decimal,
    /// `Z2V¼` — consumption of the storage system and/or charge point.
    pub device_consumption: Decimal,
    /// `Z2E¼` — generation from the storage system and/or charge point.
    pub device_generation: Decimal,
    /// `Z3V¼` — consumption of the storage system alone. Only case A4.
    #[cfg_attr(feature = "serde", serde(default))]
    pub storage_consumption: Option<Decimal>,
    /// `Z3E¼` — generation from the storage system alone. Only case A4.
    #[cfg_attr(feature = "serde", serde(default))]
    pub storage_generation: Option<Decimal>,
    /// `AW¼` — the anzulegender Wert of the generating plant in this quarter
    /// hour, ct/kWh. Zero in the hours § 51 EEG switches support off.
    pub anzulegender_wert: Decimal,
    /// `SP¼` — the spot price in this quarter hour, ct/kWh. Only the
    /// Pauschaloption reads it, `[MiSpeL A2 (P5)]`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub spot_price: Decimal,
}

impl QuarterHour {
    /// A quarter hour with nothing in it — the starting point for a builder-ish
    /// construction in tests and in a driver that fills in what it has.
    #[must_use]
    pub fn empty(slot: Slot) -> Self {
        Self {
            slot,
            grid_draw: Decimal::ZERO,
            grid_feed_in: Decimal::ZERO,
            device_consumption: Decimal::ZERO,
            device_generation: Decimal::ZERO,
            storage_consumption: None,
            storage_generation: None,
            anzulegender_wert: Decimal::ZERO,
            spot_price: Decimal::ZERO,
        }
    }

    /// `(1)¼ = MIN[Z1NB¼ ; Z2V¼]` — the grid electricity the store took in this
    /// quarter hour, `[MiSpeL A1 (1)]`.
    ///
    /// This single `MIN` is the **Speichervorrang** (`[MiSpeL A1 2.1.5]`): grid
    /// draw is attributed to the store before it is attributed to anything else,
    /// which is the legally willed priority and not an accounting convenience.
    #[must_use]
    pub fn simultaneous_grid_charging(&self) -> Decimal {
        self.grid_draw.min(self.device_consumption)
    }

    /// `(2)¼ = MIN[Z1NE¼ ; Z2E¼]` — the feed-in that came out of the store in
    /// this quarter hour, `[MiSpeL A1 (2)]`.
    #[must_use]
    pub fn simultaneous_device_feed_in(&self) -> Decimal {
        self.grid_feed_in.min(self.device_generation)
    }

    /// `(24)¼ = WENN[AW¼ > 0 ; 1 ; 0]` — whether support is payable at all in
    /// this quarter hour, `[MiSpeL A1 (24)]`.
    ///
    /// Zero in a negative-price quarter hour under § 51 EEG, which is exactly
    /// the coupling that makes the § 51 hours worth planning around.
    #[must_use]
    pub fn supported(&self) -> bool {
        self.anzulegender_wert > Decimal::ZERO
    }

    /// `(P5)¼ = WENN[SP¼ ≥ 0 ; 1 ; 0]`, `[MiSpeL A2 (P5)]`.
    #[must_use]
    pub fn spot_non_negative(&self) -> bool {
        self.spot_price >= Decimal::ZERO
    }
}

/// Why a set of quarter hours could not be evaluated.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MispelError {
    /// No quarter hours at all. An empty month is not a zero month; it is a
    /// missing measurement, and the difference matters to a Nachweis.
    #[error("no quarter-hour values were supplied")]
    NoValues,
    /// A negative register difference. Meters count upwards; a negative
    /// difference is a reading error or a sign convention mix-up, and both are
    /// worth failing on rather than silently clamping.
    #[error("quarter hour {slot} has a negative value for {field}")]
    NegativeValue {
        /// Which quarter hour.
        slot: Slot,
        /// Which quantity.
        field: &'static str,
    },
    /// Case A4 needs the separate storage meter, `[MiSpeL A1 3.2.4]`.
    #[error("Basisfall A4 requires the separately metered storage values Z3V/Z3E")]
    MissingStorageMeter,
    /// The Pauschaloption is open only to solar up to 30 kWp.
    #[error("the Pauschaloption needs a solar installation of at most 30 kWp, got {0} kWp")]
    TooLargeForPauschal(Decimal),
    /// A denominator that is zero: nothing came out of the store all month, so
    /// the shares of `(14)A1`, `(18)` and `(30)` are undefined rather than zero.
    #[error("{0} is zero, so the share it is the denominator of is undefined")]
    UndefinedShare(&'static str),
}

/// The Abgrenzungsoption for one calendar month, `[MiSpeL A1 4.2]`.
///
/// Every intermediate figure is kept, numbered as in the Festlegung, because a
/// Nachweis is checked by somebody re-deriving it and the only useful form of it
/// is the one that shows the working.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Abgrenzung {
    /// Which metering concept this was computed under.
    pub fall: Basisfall,
    /// Which version of the Festlegung.
    pub rules: RuleSet,
    /// `(3)` total grid draw in the month.
    pub grid_draw: Decimal,
    /// `(4)` total grid feed-in in the month.
    pub grid_feed_in: Decimal,
    /// `(5)` total consumption of storage and/or charge point.
    pub device_consumption: Decimal,
    /// `(6)` total generation of storage and/or charge point.
    pub device_generation: Decimal,
    /// `(9)` grid electricity that went into the store.
    pub grid_charged: Decimal,
    /// `(10)` plant electricity that went into the store.
    pub plant_charged: Decimal,
    /// `(11)` base value of simultaneous feed-in out of the store.
    pub device_feed_in_base: Decimal,
    /// `(12)` **Fremdtankstrom**: energy the car brought back from somebody
    /// else's charge point, which may be neither settled nor supported here.
    pub foreign_charge: Decimal,
    /// `(13)` feed-in out of the store that may be considered at all.
    pub device_feed_in_considered: Decimal,
    /// `(14)` round-trip efficiency — measured in A1, presumed at 0,85 otherwise.
    pub efficiency: Decimal,
    /// `(15)` the green share of what came out of the store.
    pub renewable_storage_output: Decimal,
    /// `(16)` **saldierungsfähige Netzeinspeisung** — the feed-in that reduces
    /// the levies on the grid draw.
    pub settleable_feed_in: Decimal,
    /// `(17)` storage losses in the month, where they can be seen at all.
    pub storage_losses: Decimal,
    /// `(18)` the settleable share of the store's output.
    pub settleable_share: Decimal,
    /// `(19)` the privilegeable part of the storage losses.
    pub privilegeable_losses: Decimal,
    /// `(20)` the quantity that reduces the levies.
    pub levy_reducing: Decimal,
    /// `(21)` **umlagebelasteter Netzbezug** — what levies are still owed on.
    pub levied_grid_draw: Decimal,
    /// `(26)` supportable feed-in straight from the plant.
    pub supported_direct: Decimal,
    /// `(28)` supportable feed-in out of the store, before the AW>0 share.
    pub supportable_storage_feed_in: Decimal,
    /// `(30)` the AW>0 share of the store's feed-in.
    pub supported_share_of_storage: Decimal,
    /// `(31)` supportable feed-in out of the store.
    pub supported_storage: Decimal,
    /// `(32)` **förderfähige Netzeinspeisung** for the month.
    pub supported_feed_in: Decimal,
}

impl Abgrenzung {
    /// How much of the month's grid draw escaped the levies, as a fraction.
    ///
    /// The one number a household wants out of all of this.
    #[must_use]
    pub fn levy_relief_share(&self) -> Decimal {
        if self.grid_draw.is_zero() {
            Decimal::ZERO
        } else {
            self.levy_reducing / self.grid_draw
        }
    }
}

/// Evaluate the Abgrenzungsoption over one calendar month, `[MiSpeL A1 4.2]`.
///
/// `quarters` must be the quarter hours of exactly one calendar month; the
/// Festlegung sums `∑M` over that period and nothing here can check it for you.
///
/// # Errors
/// [`MispelError`] when a value is negative, when case A4 is asked for without
/// the separate storage meter, or when a share's denominator is zero.
pub fn abgrenzung_month(
    fall: Basisfall,
    rules: RuleSet,
    quarters: &[QuarterHour],
) -> Result<Abgrenzung, MispelError> {
    if quarters.is_empty() {
        return Err(MispelError::NoValues);
    }
    validate(fall, quarters)?;

    // ── Monthly sums, (3)–(8) ───────────────────────────────────────────────
    let sum = |f: fn(&QuarterHour) -> Decimal| -> Decimal { quarters.iter().map(f).sum() };
    let grid_draw = sum(|q| q.grid_draw); // (3)
    let grid_feed_in = sum(|q| q.grid_feed_in); // (4)
    let device_consumption = sum(|q| q.device_consumption); // (5)
    let device_generation = sum(|q| q.device_generation); // (6)

    // (9), (10): what the store was charged with, split by where it came from.
    // The `MIN` inside (1)¼ is the Speichervorrang, applied per quarter hour and
    // only then summed — summing first and taking the minimum afterwards is the
    // mistake that makes a month of perfectly ordinary operation look like
    // arbitrage.
    let grid_charged: Decimal = quarters
        .iter()
        .map(QuarterHour::simultaneous_grid_charging)
        .sum();
    let plant_charged = device_consumption - grid_charged;

    // (11), (12), (13)
    let device_feed_in_base: Decimal = quarters
        .iter()
        .map(QuarterHour::simultaneous_device_feed_in)
        .sum();
    let foreign_charge = (device_generation - device_consumption).max(Decimal::ZERO);
    let device_feed_in_considered = (device_feed_in_base - foreign_charge).max(Decimal::ZERO);

    // (14): measured only where nothing but a battery is on the meter. With a
    // charge point in the picture the car's own losses are invisible, so the
    // Festlegung fixes the figure at 85 % rather than inviting a computed one
    // that would be wrong in a direction nobody could audit.
    let efficiency = if fall.efficiency_is_measured() {
        if device_consumption.is_zero() {
            return Err(MispelError::UndefinedShare("(5) device consumption"));
        }
        device_generation / device_consumption
    } else {
        PRESUMED_EFFICIENCY
    };

    // (15), (16)
    let renewable_storage_output = efficiency * plant_charged;
    let settleable_feed_in =
        (device_feed_in_considered - renewable_storage_output).max(Decimal::ZERO);

    // (17)–(19): losses, and the part of them that shares the settleable fate of
    // the output they belong to.
    let storage_losses = if fall.losses_are_privilegeable() {
        let (consumed, generated) = match fall {
            Basisfall::A4 => (
                sum(|q| q.storage_consumption.unwrap_or_default()),
                sum(|q| q.storage_generation.unwrap_or_default()),
            ),
            _ => (device_consumption, device_generation),
        };
        (consumed - generated).max(Decimal::ZERO)
    } else {
        Decimal::ZERO
    };
    let settleable_share = if device_generation.is_zero() {
        Decimal::ZERO
    } else {
        settleable_feed_in / device_generation
    };
    let privilegeable_losses = if fall.losses_are_privilegeable() {
        settleable_share * storage_losses
    } else {
        Decimal::ZERO
    };

    // (20), (21)
    let levy_reducing = (settleable_feed_in + privilegeable_losses).min(grid_draw);
    let levied_grid_draw = grid_draw - levy_reducing;

    // (23)–(26): support for what the plant fed in directly, counted only in the
    // quarter hours where the anzulegender Wert is positive.
    let supported_direct: Decimal = quarters
        .iter()
        .filter(|q| q.supported())
        .map(|q| (q.grid_feed_in - q.simultaneous_device_feed_in()).max(Decimal::ZERO))
        .sum();

    // (27)–(31): support for what the store fed in, scaled by the share of the
    // store's feed-in that fell in supported quarter hours.
    let supported_storage_feed_in: Decimal = quarters
        .iter()
        .filter(|q| q.supported())
        .map(QuarterHour::simultaneous_device_feed_in)
        .sum();
    let supportable_storage_feed_in = device_feed_in_considered.min(renewable_storage_output);
    let supported_share_of_storage = if device_feed_in_base.is_zero() {
        Decimal::ZERO
    } else {
        supported_storage_feed_in / device_feed_in_base
    };
    let supported_storage = supported_share_of_storage * supportable_storage_feed_in;

    Ok(Abgrenzung {
        fall,
        rules,
        grid_draw,
        grid_feed_in,
        device_consumption,
        device_generation,
        grid_charged,
        plant_charged,
        device_feed_in_base,
        foreign_charge,
        device_feed_in_considered,
        efficiency,
        renewable_storage_output,
        settleable_feed_in,
        storage_losses,
        settleable_share,
        privilegeable_losses,
        levy_reducing,
        levied_grid_draw,
        supported_direct,
        supportable_storage_feed_in,
        supported_share_of_storage,
        supported_storage,
        supported_feed_in: supported_direct + supported_storage,
    })
}

fn validate(fall: Basisfall, quarters: &[QuarterHour]) -> Result<(), MispelError> {
    for q in quarters {
        for (value, field) in [
            (q.grid_draw, "Z1NB"),
            (q.grid_feed_in, "Z1NE"),
            (q.device_consumption, "Z2V"),
            (q.device_generation, "Z2E"),
        ] {
            if value < Decimal::ZERO {
                return Err(MispelError::NegativeValue {
                    slot: q.slot,
                    field,
                });
            }
        }
        if fall.needs_z3() && (q.storage_consumption.is_none() || q.storage_generation.is_none()) {
            return Err(MispelError::MissingStorageMeter);
        }
    }
    Ok(())
}

/// Which Basisfall of Anlage 2 the installation matches, `[MiSpeL A2 4.1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PauschalFall {
    /// P1 — a storage system, no charge point.
    P1,
    /// P2 — a bidirectional charge point, no storage system.
    P2,
    /// P3 — both.
    P3,
}

/// The installation the Pauschaloption is applied to.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PauschalPlant {
    /// Which case.
    pub fall: PauschalFall,
    /// `Pinst` — installed solar power behind the Einspeisestelle, kWp.
    pub solar_kwp: Decimal,
    /// `SKinst` — installed storage capacity behind it, kWh. Ignored in P2.
    pub storage_kwh: Decimal,
}

impl PauschalPlant {
    /// `(P2)` — the size ratio the indifference band is built from,
    /// `[MiSpeL A2 (P2)]`.
    ///
    /// The smaller the store is against the roof, the *wider* the band: with
    /// little capacity there is little room for the arbitrage the band exists to
    /// keep out of the settleable quantity.
    ///
    /// # Errors
    /// [`MispelError::UndefinedShare`] when a case that needs a storage capacity
    /// is given none.
    pub fn ratio(&self) -> Result<Decimal, MispelError> {
        let storage = || -> Result<Decimal, MispelError> {
            if self.storage_kwh.is_zero() {
                return Err(MispelError::UndefinedShare("SKinst storage capacity"));
            }
            Ok(PAUSCHAL_STORAGE_COEFFICIENT * self.solar_kwp / self.storage_kwh)
        };
        match self.fall {
            PauschalFall::P1 => storage(),
            PauschalFall::P2 => Ok(PAUSCHAL_EVSE_FACTOR),
            // The more favourable of the two applies, `[MiSpeL A2 (P2)P3]`.
            PauschalFall::P3 => Ok(storage()?.min(PAUSCHAL_EVSE_FACTOR)),
        }
    }
}

/// The Pauschaloption for one calendar year, `[MiSpeL A2 4.2]`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Pauschal {
    /// The installation.
    pub plant: PauschalPlant,
    /// Which version of the Festlegung.
    pub rules: RuleSet,
    /// `(P1)` the cap on supportable feed-in, kWh.
    pub support_cap: Decimal,
    /// `(P3)` the indifference band: feed-in that is neither supported nor
    /// settleable.
    pub indifference_band: Decimal,
    /// `(P4)` the threshold above which feed-in becomes settleable.
    pub settlement_threshold: Decimal,
    /// `(P7)` total grid draw in the year.
    pub grid_draw: Decimal,
    /// `(P8)` feed-in in quarter hours with a non-negative spot price.
    pub feed_in_at_non_negative_prices: Decimal,
    /// `(P10)` **saldierungsfähige Netzeinspeisung** for the year.
    pub settleable_feed_in: Decimal,
    /// `(P11)` **umlagebelasteter Netzbezug** for the year.
    pub levied_grid_draw: Decimal,
    /// `(P14)` feed-in in supported quarter hours.
    pub feed_in_when_supported: Decimal,
    /// `(P15)` **förderfähige Netzeinspeisung** for the year.
    pub supported_feed_in: Decimal,
}

/// Evaluate the Pauschaloption over one calendar year, `[MiSpeL A2 4.2]`.
///
/// Where the Abgrenzungsoption follows the electrons quarter by quarter, this
/// draws two flat lines through the year's feed-in — one at 500 kWh/kW, one an
/// indifference band above it — and asks only which side of them a kilowatt-hour
/// fell on. It is open to solar installations up to 30 kWp, which is most German
/// roofs, and it needs one meter rather than three.
///
/// # Errors
/// [`MispelError`] when there are no values, when a value is negative, when the
/// installation is too large for the option, or when a case that needs a storage
/// capacity is given none.
pub fn pauschal_year(
    plant: PauschalPlant,
    rules: RuleSet,
    quarters: &[QuarterHour],
) -> Result<Pauschal, MispelError> {
    if quarters.is_empty() {
        return Err(MispelError::NoValues);
    }
    if plant.solar_kwp > PAUSCHAL_MAX_KWP {
        return Err(MispelError::TooLargeForPauschal(plant.solar_kwp));
    }
    for q in quarters {
        for (value, field) in [(q.grid_draw, "Z1NB"), (q.grid_feed_in, "Z1NE")] {
            if value < Decimal::ZERO {
                return Err(MispelError::NegativeValue {
                    slot: q.slot,
                    field,
                });
            }
        }
    }

    let support_cap = plant.solar_kwp * PAUSCHAL_KWH_PER_KW; // (P1)
    let indifference_band = plant.ratio()? * support_cap; // (P3)
    let settlement_threshold = support_cap + indifference_band; // (P4)

    let grid_draw: Decimal = quarters.iter().map(|q| q.grid_draw).sum(); // (P7)
    // (P6)/(P8): a negative spot price makes the quarter hour worthless for
    // settlement, which is § 51 EEG arriving through a different door.
    let feed_in_at_non_negative_prices: Decimal = quarters
        .iter()
        .filter(|q| q.spot_non_negative())
        .map(|q| q.grid_feed_in)
        .sum();

    // (P9), (P10), (P11)
    let above_threshold =
        (feed_in_at_non_negative_prices - settlement_threshold).max(Decimal::ZERO);
    let settleable_feed_in = above_threshold.min(grid_draw);
    let levied_grid_draw = grid_draw - settleable_feed_in;

    // (P13)–(P15)
    let feed_in_when_supported: Decimal = quarters
        .iter()
        .filter(|q| q.supported())
        .map(|q| q.grid_feed_in)
        .sum();
    let supported_feed_in = feed_in_when_supported.min(support_cap);

    Ok(Pauschal {
        plant,
        rules,
        support_cap,
        indifference_band,
        settlement_threshold,
        grid_draw,
        feed_in_at_non_negative_prices,
        settleable_feed_in,
        levied_grid_draw,
        feed_in_when_supported,
        supported_feed_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromPrimitive;

    const RULES: RuleSet = RuleSet::Arbeitsstand20260805;

    fn dec(v: f64) -> Decimal {
        Decimal::from_f64(v).unwrap().round_dp(6)
    }

    fn slot(i: i64) -> Slot {
        Slot::containing(time::macros::datetime!(2026-10-01 00:00:00 UTC)).offset(i)
    }

    /// A quarter hour: (draw, feed-in, device consumption, device generation, AW).
    fn q(i: i64, nb: f64, ne: f64, v: f64, e: f64, aw: f64) -> QuarterHour {
        QuarterHour {
            grid_draw: dec(nb),
            grid_feed_in: dec(ne),
            device_consumption: dec(v),
            device_generation: dec(e),
            anzulegender_wert: dec(aw),
            ..QuarterHour::empty(slot(i))
        }
    }

    // ── (1)/(2): the Speichervorrang ────────────────────────────────────────

    #[test]
    fn the_storage_priority_is_a_minimum_per_quarter_hour_and_not_over_the_month() {
        // Two quarter hours: in the first the house draws 4 kWh and the store
        // takes 1; in the second it draws nothing and the store takes 4 from the
        // roof. Per quarter hour only 1 kWh was charged from the grid. Summing
        // first — 4 kWh of draw against 5 kWh of charging — would claim 4, and
        // turn a perfectly ordinary day into three kilowatt-hours of arbitrage.
        let month = [q(0, 4.0, 0.0, 1.0, 0.0, 8.0), q(1, 0.0, 0.0, 4.0, 0.0, 8.0)];
        let r = abgrenzung_month(Basisfall::A1, RULES, &month).unwrap();
        assert_eq!(r.grid_charged, dec(1.0));
        assert_eq!(r.plant_charged, dec(4.0));
    }

    // ── A worked month, checked figure by figure ────────────────────────────

    /// A month of a single battery (A1): charged 100 kWh from the roof and
    /// 20 kWh from the grid, gave back 102 kWh, of which 60 kWh reached the grid
    /// at the same time as the store was producing.
    fn worked_month() -> Vec<QuarterHour> {
        let mut month = Vec::new();
        // 20 quarter hours of grid charging, 1 kWh each, house drawing 3 kWh.
        for i in 0..20 {
            month.push(q(i, 3.0, 0.0, 1.0, 0.0, 8.0));
        }
        // 100 quarter hours charging from the roof, 1 kWh each.
        for i in 20..120 {
            month.push(q(i, 0.0, 0.0, 1.0, 0.0, 8.0));
        }
        // 102 quarter hours discharging 1 kWh, 60 of which feed 1 kWh to the
        // grid at the same moment.
        for i in 120..222 {
            let ne = if i < 180 { 1.0 } else { 0.0 };
            month.push(q(i, 0.0, ne, 0.0, 1.0, 8.0));
        }
        month
    }

    #[test]
    fn a_worked_month_reproduces_every_figure_of_anlage_1() {
        let r = abgrenzung_month(Basisfall::A1, RULES, &worked_month()).unwrap();
        assert_eq!(r.grid_draw, dec(60.0), "(3)");
        assert_eq!(r.grid_feed_in, dec(60.0), "(4)");
        assert_eq!(r.device_consumption, dec(120.0), "(5)");
        assert_eq!(r.device_generation, dec(102.0), "(6)");
        assert_eq!(r.grid_charged, dec(20.0), "(9)");
        assert_eq!(r.plant_charged, dec(100.0), "(10)");
        assert_eq!(r.device_feed_in_base, dec(60.0), "(11)");
        assert_eq!(
            r.foreign_charge,
            Decimal::ZERO,
            "(12): no car, no free lunch"
        );
        assert_eq!(r.device_feed_in_considered, dec(60.0), "(13)");
        // (14)A1 = 102 / 120 = 0,85 — measured, and it happens to land on the
        // presumed figure, which is why the presumption is 0,85.
        assert_eq!(r.efficiency, dec(0.85), "(14)A1");
        assert_eq!(r.renewable_storage_output, dec(85.0), "(15)");
        // (16) = MAX[60 − 85 ; 0] = 0. The store gave back less than the roof put
        // into it, so nothing it fed in was grey and nothing is settleable.
        assert_eq!(r.settleable_feed_in, Decimal::ZERO, "(16)");
        assert_eq!(r.storage_losses, dec(18.0), "(17)A1");
        assert_eq!(r.privilegeable_losses, Decimal::ZERO, "(19)");
        assert_eq!(
            r.levied_grid_draw,
            dec(60.0),
            "(21): all of it still levied"
        );
        // Every quarter hour is supported here, so the whole feed-in counts.
        assert_eq!(
            r.supported_direct,
            Decimal::ZERO,
            "(26): all of it via the store"
        );
        assert_eq!(r.supported_feed_in, dec(60.0), "(32)");
    }

    #[test]
    fn a_month_of_grid_arbitrage_produces_a_settleable_quantity() {
        // The case the Festlegung exists for: the store is charged from the grid
        // in cheap hours and emptied into it in dear ones. None of that is green,
        // so all of it is settleable against the levies on the draw.
        let mut month = Vec::new();
        for i in 0..40 {
            month.push(q(i, 2.0, 0.0, 2.0, 0.0, 8.0)); // 80 kWh in, all from the grid
        }
        for i in 40..108 {
            month.push(q(i, 0.0, 1.0, 0.0, 1.0, 8.0)); // 68 kWh out, all to the grid
        }
        let r = abgrenzung_month(Basisfall::A1, RULES, &month).unwrap();
        assert_eq!(r.grid_charged, dec(80.0));
        assert_eq!(r.plant_charged, Decimal::ZERO);
        assert_eq!(
            r.renewable_storage_output,
            Decimal::ZERO,
            "(15): nothing green"
        );
        assert_eq!(r.settleable_feed_in, dec(68.0), "(16)");
        // (17) = 80 − 68 = 12 kWh of losses, and (18) = 68/68 = 1, so all of them
        // are privilegeable.
        assert_eq!(r.storage_losses, dec(12.0), "(17)");
        assert_eq!(r.settleable_share, Decimal::ONE, "(18)");
        assert_eq!(r.privilegeable_losses, dec(12.0), "(19)");
        // (20) = MIN[68 + 12 ; 80] = 80, so the whole draw escapes the levies.
        assert_eq!(r.levy_reducing, dec(80.0), "(20)");
        assert_eq!(r.levied_grid_draw, Decimal::ZERO, "(21)");
        assert_eq!(r.levy_relief_share(), Decimal::ONE);
    }

    #[test]
    fn the_levy_reduction_can_never_exceed_the_draw_it_reduces() {
        // (20)'s MIN. A month that feeds in far more than it draws must not
        // produce a negative levied quantity — which would be a refund.
        let mut month = vec![q(0, 1.0, 0.0, 1.0, 0.0, 8.0)];
        for i in 1..50 {
            month.push(q(i, 0.0, 1.0, 0.0, 1.0, 8.0));
        }
        let r = abgrenzung_month(Basisfall::A1, RULES, &month).unwrap();
        assert_eq!(r.levy_reducing, r.grid_draw);
        assert_eq!(r.levied_grid_draw, Decimal::ZERO);
        assert!(r.levied_grid_draw >= Decimal::ZERO);
    }

    #[test]
    fn a_negative_price_quarter_hour_carries_no_support() {
        // § 51 EEG through (24)¼: the anzulegender Wert is zero, so the feed-in
        // of that quarter hour is worth nothing in support — while still counting
        // in every settlement figure, because the levies do not care about the
        // spot price.
        let supported = [q(0, 0.0, 5.0, 0.0, 0.0, 8.0)];
        let unsupported = [q(0, 0.0, 5.0, 0.0, 0.0, 0.0)];
        let a = abgrenzung_month(Basisfall::A2, RULES, &supported).unwrap();
        let b = abgrenzung_month(Basisfall::A2, RULES, &unsupported).unwrap();
        assert_eq!(a.supported_feed_in, dec(5.0));
        assert_eq!(b.supported_feed_in, Decimal::ZERO);
        assert_eq!(a.grid_feed_in, b.grid_feed_in);
    }

    #[test]
    fn energy_a_car_brought_from_somewhere_else_is_taken_back_out() {
        // (12) Fremdtankstrom. The car was charged at work, comes home full and
        // feeds the house: the store produced more than it was ever given here,
        // and the difference may be neither settled nor supported.
        let month = [q(0, 0.0, 0.0, 2.0, 0.0, 8.0), q(1, 0.0, 6.0, 0.0, 6.0, 8.0)];
        let r = abgrenzung_month(Basisfall::A2, RULES, &month).unwrap();
        assert_eq!(r.foreign_charge, dec(4.0), "(12) = MAX[6 − 2 ; 0]");
        assert_eq!(r.device_feed_in_considered, dec(2.0), "(13) = 6 − 4");
    }

    #[test]
    fn a_charge_point_gets_the_presumed_efficiency_and_no_privilegeable_losses() {
        let month = [q(0, 2.0, 0.0, 2.0, 0.0, 8.0), q(1, 0.0, 1.0, 0.0, 1.0, 8.0)];
        for fall in [Basisfall::A2, Basisfall::A3] {
            let r = abgrenzung_month(fall, RULES, &month).unwrap();
            assert_eq!(r.efficiency, PRESUMED_EFFICIENCY, "(14) in {fall}");
            assert_eq!(r.privilegeable_losses, Decimal::ZERO, "(19) in {fall}");
        }
    }

    #[test]
    fn case_a4_measures_the_storage_losses_on_their_own_meter() {
        // A3 and A4 differ in exactly one thing: A4 can see the *battery's* own
        // consumption and generation, so its losses are privilegeable while the
        // charge point's are not.
        let month: Vec<QuarterHour> = (0..2)
            .map(|i| QuarterHour {
                storage_consumption: Some(dec(if i == 0 { 2.0 } else { 0.0 })),
                storage_generation: Some(dec(if i == 0 { 0.0 } else { 1.7 })),
                ..q(
                    i,
                    if i == 0 { 4.0 } else { 0.0 },
                    if i == 0 { 0.0 } else { 4.0 },
                    if i == 0 { 4.0 } else { 0.0 },
                    if i == 0 { 0.0 } else { 4.0 },
                    8.0,
                )
            })
            .collect();
        let a4 = abgrenzung_month(Basisfall::A4, RULES, &month).unwrap();
        assert_eq!(a4.storage_losses, dec(0.3), "(17)A4 = 2,0 − 1,7");
        assert!(a4.privilegeable_losses > Decimal::ZERO);

        let a3 = abgrenzung_month(Basisfall::A3, RULES, &month).unwrap();
        assert_eq!(a3.privilegeable_losses, Decimal::ZERO, "(19)A2,A3");
    }

    #[test]
    fn case_a4_refuses_to_guess_at_a_meter_it_does_not_have() {
        let month = [q(0, 1.0, 0.0, 1.0, 0.0, 8.0)];
        assert_eq!(
            abgrenzung_month(Basisfall::A4, RULES, &month),
            Err(MispelError::MissingStorageMeter)
        );
    }

    #[test]
    fn an_empty_month_is_a_missing_measurement_and_not_a_zero() {
        assert_eq!(
            abgrenzung_month(Basisfall::A1, RULES, &[]),
            Err(MispelError::NoValues)
        );
        assert_eq!(
            pauschal_year(
                PauschalPlant {
                    fall: PauschalFall::P1,
                    solar_kwp: dec(9.8),
                    storage_kwh: dec(10.0)
                },
                RULES,
                &[]
            ),
            Err(MispelError::NoValues)
        );
    }

    #[test]
    fn a_meter_that_counts_backwards_is_refused_rather_than_clamped() {
        let mut month = vec![q(0, 1.0, 0.0, 1.0, 0.0, 8.0)];
        month[0].grid_draw = dec(-1.0);
        assert!(matches!(
            abgrenzung_month(Basisfall::A1, RULES, &month),
            Err(MispelError::NegativeValue { field: "Z1NB", .. })
        ));
    }

    // ── The Pauschaloption ──────────────────────────────────────────────────

    fn plant(fall: PauschalFall, kwp: f64, kwh: f64) -> PauschalPlant {
        PauschalPlant {
            fall,
            solar_kwp: dec(kwp),
            storage_kwh: dec(kwh),
        }
    }

    #[test]
    fn the_pauschal_thresholds_come_out_of_the_size_of_the_installation() {
        // 9,8 kWp and 10 kWh: the support cap is 4 900 kWh, the ratio is
        // 0,1 × 9,8 / 10 = 0,098, so the indifference band is 480,2 kWh and the
        // settlement threshold 5 380,2 kWh.
        let p = plant(PauschalFall::P1, 9.8, 10.0);
        assert_eq!(p.ratio().unwrap(), dec(0.098));
        let year = [q(0, 0.0, 0.0, 0.0, 0.0, 8.0)];
        let r = pauschal_year(p, RULES, &year).unwrap();
        assert_eq!(r.support_cap, dec(4900.0), "(P1)");
        assert_eq!(r.indifference_band, dec(480.2), "(P3)");
        assert_eq!(r.settlement_threshold, dec(5380.2), "(P4)");
    }

    #[test]
    fn a_smaller_store_widens_the_indifference_band() {
        // The whole idea of (P2)P1: less capacity against the same roof means
        // less room for arbitrage, so more of the feed-in is presumed innocent —
        // and a *wider* band before anything counts as settleable.
        let small = plant(PauschalFall::P1, 10.0, 5.0).ratio().unwrap();
        let large = plant(PauschalFall::P1, 10.0, 20.0).ratio().unwrap();
        assert!(small > large, "{small} should exceed {large}");
    }

    #[test]
    fn a_charge_point_takes_the_flat_half_and_both_take_the_better_of_the_two() {
        assert_eq!(
            plant(PauschalFall::P2, 10.0, 0.0).ratio().unwrap(),
            PAUSCHAL_EVSE_FACTOR
        );
        // P3 takes the *smaller* ratio, which is the narrower band and therefore
        // the more settleable feed-in — the favourable one for the household.
        let both = plant(PauschalFall::P3, 10.0, 20.0);
        assert_eq!(both.ratio().unwrap(), dec(0.05));
    }

    #[test]
    fn only_feed_in_above_the_threshold_and_at_non_negative_prices_is_settleable() {
        let p = plant(PauschalFall::P1, 1.0, 10.0);
        // (P1) = 500, ratio = 0,01, (P3) = 5, (P4) = 505.
        let mut year: Vec<QuarterHour> = (0..600)
            .map(|i| {
                let mut h = q(i, 1.0, 1.0, 0.0, 0.0, 8.0);
                h.spot_price = dec(5.0);
                h
            })
            .collect();
        // Fifty of them at a negative spot price: they feed in, and they count
        // for nothing in the settlement.
        for h in year.iter_mut().take(50) {
            h.spot_price = dec(-2.0);
        }
        let r = pauschal_year(p, RULES, &year).unwrap();
        assert_eq!(r.feed_in_at_non_negative_prices, dec(550.0), "(P8)");
        assert_eq!(r.settleable_feed_in, dec(45.0), "(P10) = 550 − 505");
        assert_eq!(r.levied_grid_draw, dec(555.0), "(P11) = 600 − 45");
        assert_eq!(r.supported_feed_in, dec(500.0), "(P15) capped at (P1)");
    }

    #[test]
    fn the_pauschal_option_refuses_a_roof_it_is_not_open_to() {
        let year = [q(0, 0.0, 0.0, 0.0, 0.0, 8.0)];
        assert_eq!(
            pauschal_year(plant(PauschalFall::P1, 45.0, 10.0), RULES, &year),
            Err(MispelError::TooLargeForPauschal(dec(45.0)))
        );
    }

    #[test]
    fn the_rule_set_says_when_it_applies() {
        assert!(!RULES.applies_on(time::macros::date!(2026 - 09 - 30)));
        assert!(RULES.applies_on(time::macros::date!(2026 - 10 - 01)));
    }
}
