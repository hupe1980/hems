//! § 14a EnWG Modul 3 — time-variable network charges.
//!
//! The network operator divides the day into windows and assigns each a tariff
//! level: **HT** (Hochlast), **ST** (Standard) and **NT** (Niedriglast). Shifting
//! consumption out of HT into NT is the cheapest flexibility a household has,
//! and unlike the wholesale price it is known a year in advance.
//!
//! The rules are BDEW's Anwendungshilfe „Für die Umsetzung von Modul 3" V1.1
//! (07.02.2025, `specs/bnetza/bdew-awh-modul-3-v1.1-20250207.pdf`), which
//! implements BK8-22/010-A:
//!
//! * available only in combination with **Modul 1**, only at a Marktlokation
//!   **without** registrierende Leistungsmessung, and only with an intelligent
//!   metering system (§ 1);
//! * windows and price levels are fixed **per calendar year** and apply to the
//!   whole network area; they may not differ between quarters (§ 2);
//! * the Hochtarif lasts at least two hours a day;
//! * the three levels have to be billed in **at least two quarters** of the
//!   year, which need not be adjacent.
//!
//! # The calendar is `metering`'s
//!
//! A Zählzeitdefinition resolves an instant to a register in the Europe/Berlin
//! calendar, with the Bundesland's statutory holidays. A list of minute ranges
//! cannot: it has no notion of the day of the week, so a Sunday afternoon
//! resolves to the weekday Hochtarif and a household is told to shift load it
//! is not being charged extra for.
//!
//! So [`Modul3Calendar`] is a [`Zaehlzeitdefinition`] plus the two facts the
//! definition does not carry — which quarters the operator bills the levels in,
//! and where the calendar was transcribed from — and validation is
//! [`metering::zaehlzeit::assess_modul_3`].
//!
//! There is no machine-readable national format for any of this: it is a PDF
//! and an Excel sheet per network operator. Recording the source is therefore
//! not decoration — when a household is billed on a calendar, "which document
//! said so" is the first question anyone asks.

use hems_core::prelude::Slot;
use metering::zaehlzeit::{HT, NT};

pub use metering::zaehlzeit::{
    Modul3Conformance, Modul3Context, Modul3Finding, Quarter, ZaehlzeitFenster, Zaehlzeitdefinition,
};

/// The day this module became orderable from network operators.
pub const MODUL3_AVAILABLE_FROM: time::Date = time::macros::date!(2025 - 04 - 01);

/// One of the three price levels.
///
/// The `metering` register identifiers `HT`/`NT`/`ST` as a closed enum, because
/// a price stack indexes on them and a typo in a string is a silent tariff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "UPPERCASE"))]
pub enum Preisstufe {
    /// Niedriglasttarif — the cheap window.
    Nt,
    /// Standardlasttarif — everything not otherwise named.
    #[default]
    St,
    /// Hochlasttarif — the expensive window.
    Ht,
}

impl Preisstufe {
    /// The level a `metering` register identifier stands for.
    ///
    /// Anything that is not [`HT`] or [`NT`] is the standard level, which is
    /// also what an unreachable register resolves to — and
    /// [`Modul3Finding::RegisterNeverReached`] is what reports that as the
    /// defect it is rather than letting it read as a cheap hour.
    #[must_use]
    pub fn from_register(register: &str) -> Self {
        match register {
            HT => Self::Ht,
            NT => Self::Nt,
            _ => Self::St,
        }
    }
}

/// One network operator's Modul 3 calendar, and where it came from.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Modul3Calendar {
    /// The Zählzeitdefinition the operator published: the windows, the
    /// fallback, the validity period, and the Bundesland whose holidays count.
    pub definition: Zaehlzeitdefinition,
    /// The calendar quarters the three levels are billed in — the operator's
    /// Wahlrecht, published on the price sheet and not a property of the
    /// windows.
    pub billed_quarters: Vec<Quarter>,
    /// Where this calendar was transcribed from — a price-sheet URL, a document
    /// hash.
    #[cfg_attr(feature = "serde", serde(default))]
    pub source: Option<String>,
}

impl Modul3Calendar {
    /// A calendar with a Hochtarif and a Niedertarif band, both `(from, to)` in
    /// minutes of the local day and either allowed to wrap past midnight.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        year: i32,
        hochtarif: (u16, u16),
        niedertarif: (u16, u16),
        billed_quarters: Vec<Quarter>,
    ) -> Self {
        let from = time::Date::from_calendar_date(year, time::Month::January, 1)
            .unwrap_or(time::Date::MIN);
        let to = time::Date::from_calendar_date(year, time::Month::December, 31)
            .unwrap_or(time::Date::MAX);
        Self {
            definition: Zaehlzeitdefinition::modul_3(id, from, hochtarif, niedertarif).until(to),
            billed_quarters,
            source: None,
        }
    }

    /// Record where the calendar was transcribed from (builder style).
    #[must_use]
    pub fn from_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Check the calendar against the Anwendungshilfe, and against the delivery
    /// point it is to be used at.
    ///
    /// Seven rules: three levels that are all reachable, a day with no gaps in
    /// it, a Hochtarif of at least two hours **on every day class the
    /// definition distinguishes**, windows identical all year, at least two
    /// billed quarters, a validity period of one calendar year, and the three
    /// preconditions of § 1.
    ///
    /// The verdict distinguishes "breaks a rule" from "could not be checked",
    /// so a curated portfolio can route on the reason rather than on a boolean.
    #[must_use]
    pub fn assess(&self, at: &Modul3Context) -> (Modul3Conformance, Vec<Modul3Finding>) {
        let ctx = match &self.billed_quarters {
            q if q.is_empty() => at.clone(),
            q => {
                let mut ctx = at.clone();
                ctx.billed_quarters = Some(q.clone());
                ctx
            }
        };
        metering::zaehlzeit::assess_modul_3(&self.definition, &ctx)
    }

    /// Whether this calendar's levels are billed in the quarter `slot` falls in.
    ///
    /// Outside those quarters the ordinary network charge applies, which is why
    /// the planner must ask before it shifts anything.
    #[must_use]
    pub fn is_billed_in(&self, slot: Slot) -> bool {
        let date = slot.local_date();
        self.definition.is_valid_on(date)
            && Quarter::of_month(u8::from(date.month()))
                .is_some_and(|q| self.billed_quarters.contains(&q))
    }

    /// The level charged in `slot`.
    ///
    /// Resolved by `metering`, so it is DST-correct and knows a Sunday from a
    /// Tuesday: the repeated hour of the long October day is inside the same
    /// window twice, the skipped hour of the short March day is inside none,
    /// and a Bundesland holiday is classified the way the price sheet means it.
    #[must_use]
    pub fn stufe_at(&self, slot: Slot) -> Preisstufe {
        if !self.is_billed_in(slot) {
            return Preisstufe::St;
        }
        self.definition
            .register_for(slot.start())
            .map_or(Preisstufe::St, Preisstufe::from_register)
    }
}

/// Whether a Marktlokation may take Modul 3 at all (§ 1 of the Anwendungshilfe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Modul3Eligibility {
    /// Modul 1 (the flat reduction) is in place — Modul 3 is only available
    /// together with it.
    pub has_modul_1: bool,
    /// An intelligent metering system is installed.
    pub has_imsys: bool,
    /// The location is billed with registrierende Leistungsmessung, which rules
    /// Modul 3 out.
    pub has_rlm: bool,
}

impl Modul3Eligibility {
    /// Whether the customer may be billed under Modul 3.
    #[must_use]
    pub const fn is_eligible(&self) -> bool {
        self.has_modul_1 && self.has_imsys && !self.has_rlm
    }

    /// The same three facts, as the context `metering`'s assessment wants them.
    #[must_use]
    pub fn as_context(&self) -> Modul3Context {
        Modul3Context::default()
            .with_modul_1(self.has_modul_1)
            .with_intelligentes_messsystem(self.has_imsys)
            .with_registrierende_leistungsmessung(self.has_rlm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn calendar() -> Modul3Calendar {
        // Hochtarif 17:00–20:00, Niedertarif 22:00–06:00 wrapping past midnight.
        Modul3Calendar::new(
            "NB-14A-3",
            2026,
            (17 * 60, 20 * 60),
            (22 * 60, 6 * 60),
            vec![Quarter::Q1, Quarter::Q4],
        )
        .from_source("https://example.invalid/preisblatt-2026.pdf")
    }

    fn eligible() -> Modul3Eligibility {
        Modul3Eligibility {
            has_modul_1: true,
            has_imsys: true,
            has_rlm: false,
        }
    }

    #[test]
    fn a_transcribed_calendar_passes_every_rule_the_anwendungshilfe_states() {
        let (verdict, findings) = calendar().assess(&eligible().as_context());
        assert_eq!(verdict, Modul3Conformance::Conforms, "{findings:?}");
    }

    #[test]
    fn a_hochtarif_shorter_than_two_hours_is_refused() {
        let short = Modul3Calendar::new(
            "NB-short",
            2026,
            (18 * 60, 19 * 60),
            (22 * 60, 6 * 60),
            vec![Quarter::Q1, Quarter::Q4],
        );
        let (verdict, findings) = short.assess(&eligible().as_context());
        assert_eq!(verdict, Modul3Conformance::Violates);
        assert!(
            findings.contains(&Modul3Finding::HochtarifBelowTwoHours),
            "{findings:?}"
        );
    }

    #[test]
    fn one_billed_quarter_is_not_enough() {
        let one = Modul3Calendar::new(
            "NB-one",
            2026,
            (17 * 60, 20 * 60),
            (22 * 60, 6 * 60),
            vec![Quarter::Q1],
        );
        let (_, findings) = one.assess(&eligible().as_context());
        assert!(
            findings.contains(&Modul3Finding::FewerThanTwoBilledQuarters),
            "{findings:?}"
        );
    }

    #[test]
    fn a_delivery_point_without_modul_1_cannot_take_modul_3() {
        let point = Modul3Eligibility {
            has_modul_1: false,
            ..eligible()
        };
        assert!(!point.is_eligible());
        let (verdict, findings) = calendar().assess(&point.as_context());
        assert_eq!(verdict, Modul3Conformance::Violates);
        assert!(
            findings.contains(&Modul3Finding::Modul1NotSelected),
            "{findings:?}"
        );
    }

    #[test]
    fn the_windows_resolve_in_local_time_and_only_in_billed_quarters() {
        let c = calendar();
        // 18:00 CET on a January Monday is Hochtarif.
        assert_eq!(
            c.stufe_at(Slot::containing(datetime!(2026-01-05 17:00 UTC))),
            Preisstufe::Ht
        );
        // 03:00 CET is the Niedertarif band, which wraps past midnight.
        assert_eq!(
            c.stufe_at(Slot::containing(datetime!(2026-01-05 2:00 UTC))),
            Preisstufe::Nt
        );
        // Midday is neither.
        assert_eq!(
            c.stufe_at(Slot::containing(datetime!(2026-01-05 11:00 UTC))),
            Preisstufe::St
        );
        // The same 18:00 in July is outside the billed quarters entirely, so
        // the ordinary network charge applies and there is nothing to shift.
        assert_eq!(
            c.stufe_at(Slot::containing(datetime!(2026-07-06 16:00 UTC))),
            Preisstufe::St
        );
    }

    #[test]
    fn a_calendar_from_another_year_charges_nothing_special() {
        let c = calendar();
        assert_eq!(
            c.stufe_at(Slot::containing(datetime!(2027-01-05 17:00 UTC))),
            Preisstufe::St
        );
    }
}
