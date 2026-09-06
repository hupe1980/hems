//! The curated Modul 3 catalogue — one transcription per Netzgebiet.
//!
//! There is no machine-readable national format for a Zählzeitdefinition: each
//! network operator publishes a PDF or an Excel sheet, and somebody transcribes
//! it. A box can carry its own operator's calendar (`hemsd`'s
//! `[tariff.modul3]`, D126) and that is right for one household; at fleet scale
//! the same transcription should happen **once per Netzgebiet**, be checked
//! once, and be served to every box behind that operator. This module is that
//! catalogue.
//!
//! # Curated means refused, not warned about
//!
//! Every entry is validated against the BDEW Anwendungshilfe
//! ([`hems_grid::modul3::Modul3Calendar::assess`]) when the daemon starts, and
//! a violation is a start-up **error**. The argument is D126's, scaled up: a
//! box that came up on a broken calendar prices one household's year in windows
//! nobody may sell; a fleet service that served one prices a whole Netzgebiet's.
//! `Unknown` is not a refusal — the delivery-point preconditions of § 1 are
//! facts about each household, not about the catalogue, and are checked where
//! the household is known.
//!
//! # Open, like the prices
//!
//! A Modul 3 calendar is the operator's published price sheet in machine form —
//! nobody's household data — so the routes are as open as `/v1/prices`, and for
//! the same reason.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use hems_grid::modul3::{Modul3Conformance, Modul3Context};

use crate::config::Modul3Entry;

/// Why the catalogue could not be curated.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CatalogueError {
    /// A `billed_quarters` entry is not one of `Q1`–`Q4`.
    #[error(
        "{netzbetreiber}/{year}: `billed_quarters` must be drawn from Q1, Q2, Q3 and Q4, \
         and this says {given:?}"
    )]
    NotAQuarter {
        /// Whose calendar.
        netzbetreiber: String,
        /// Which year.
        year: i32,
        /// What was written.
        given: Vec<String>,
    },
    /// The calendar breaks the Anwendungshilfe.
    #[error(
        "{netzbetreiber}/{year} breaks the BDEW Anwendungshilfe: {findings} — \
         see specs/bnetza/bdew-awh-modul-3-v1.1-20250207.pdf"
    )]
    Violates {
        /// Whose calendar.
        netzbetreiber: String,
        /// Which year.
        year: i32,
        /// What it broke.
        findings: String,
    },
    /// No source document was named.
    ///
    /// Required here even though the parser leaves it optional: a catalogue is
    /// the thing a whole Netzgebiet is billed against, and when a household
    /// queries a bill the first question is which document said so.
    #[error("{netzbetreiber}/{year} names no source document")]
    NoSource {
        /// Whose calendar.
        netzbetreiber: String,
        /// Which year.
        year: i32,
    },
    /// Two entries claim the same operator and year.
    ///
    /// "Which calendar applies" would have no answer, and the shape that takes
    /// — pick one and carry on — is how two households behind one operator end
    /// up planned against different windows.
    #[error("{netzbetreiber}/{year} appears twice in the catalogue")]
    Duplicate {
        /// Whose calendar.
        netzbetreiber: String,
        /// Which year.
        year: i32,
    },
}

/// The catalogue, curated: every entry validated, no two for one operator-year.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    by_operator: BTreeMap<String, Vec<Modul3Entry>>,
}

impl Catalogue {
    /// Validate `entries` into a catalogue, refusing the first thing wrong.
    ///
    /// # Errors
    /// [`CatalogueError`] — a violation of the Anwendungshilfe, a missing
    /// source document, a quarter that is not one, or a duplicate operator-year.
    pub fn curate(entries: &[Modul3Entry]) -> Result<Self, CatalogueError> {
        let mut by_operator: BTreeMap<String, Vec<Modul3Entry>> = BTreeMap::new();
        for entry in entries {
            let netzbetreiber = entry.netzbetreiber.trim().to_owned();
            let year = entry.calendar.year;
            if entry.calendar.source.is_none() {
                return Err(CatalogueError::NoSource {
                    netzbetreiber,
                    year,
                });
            }
            let Some(calendar) = entry.calendar.calendar() else {
                return Err(CatalogueError::NotAQuarter {
                    netzbetreiber,
                    year,
                    given: entry.calendar.billed_quarters.clone(),
                });
            };
            // The delivery-point preconditions of § 1 are per household and are
            // deliberately not asserted here, so their findings come back
            // `Unknown` — which is "could not be checked", not "broken", and a
            // catalogue must not refuse to exist over a fact about a household
            // it has never met.
            let (verdict, findings) = calendar.assess(&Modul3Context {
                billed_quarters: None,
                modul_1_selected: None,
                registrierende_leistungsmessung: None,
                intelligentes_messsystem: None,
            });
            if verdict == Modul3Conformance::Violates {
                return Err(CatalogueError::Violates {
                    netzbetreiber,
                    year,
                    findings: format!("{findings:?}"),
                });
            }
            let for_operator = by_operator.entry(netzbetreiber.clone()).or_default();
            if for_operator.iter().any(|e| e.calendar.year == year) {
                return Err(CatalogueError::Duplicate {
                    netzbetreiber,
                    year,
                });
            }
            for_operator.push(entry.clone());
        }
        Ok(Self { by_operator })
    }

    /// Every calendar for one operator, or `None` where the catalogue has
    /// nobody by that name.
    #[must_use]
    pub fn for_operator(&self, netzbetreiber: &str) -> Option<&[Modul3Entry]> {
        self.by_operator
            .get(netzbetreiber.trim())
            .map(Vec::as_slice)
    }

    /// Which operators and years the catalogue covers.
    #[must_use]
    pub fn listing(&self) -> Vec<(String, Vec<i32>)> {
        self.by_operator
            .iter()
            .map(|(op, entries)| {
                (
                    op.clone(),
                    entries.iter().map(|e| e.calendar.year).collect(),
                )
            })
            .collect()
    }

    /// How many calendars the catalogue holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_operator.values().map(Vec::len).sum()
    }

    /// Whether it holds none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_operator.is_empty()
    }
}

/// The routes: a listing, and one operator's calendars.
pub fn router(catalogue: Arc<Catalogue>) -> Router {
    Router::new()
        .route("/v1/modul3", get(listing_handler))
        .route("/v1/modul3/{netzbetreiber}", get(operator_handler))
        .with_state(catalogue)
}

async fn listing_handler(State(catalogue): State<Arc<Catalogue>>) -> axum::Json<serde_json::Value> {
    let listing: Vec<serde_json::Value> = catalogue
        .listing()
        .into_iter()
        .map(|(netzbetreiber, years)| {
            serde_json::json!({ "netzbetreiber": netzbetreiber, "years": years })
        })
        .collect();
    axum::Json(serde_json::json!({ "calendars": listing }))
}

async fn operator_handler(
    State(catalogue): State<Arc<Catalogue>>,
    Path(netzbetreiber): Path<String>,
) -> Result<axum::Json<Vec<Modul3Entry>>, StatusCode> {
    // A `404` rather than an empty list: "this operator is not in the
    // catalogue" and "this operator has published no calendar" are different
    // facts, and only the first is this service's to answer.
    catalogue
        .for_operator(&netzbetreiber)
        .map(|entries| axum::Json(entries.to_vec()))
        .ok_or(StatusCode::NOT_FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcription() -> hems_grid::modul3::Transcription {
        hems_grid::modul3::Transcription {
            id: "NB-14A-3-2026".into(),
            year: 2026,
            hochtarif_minutes: [17 * 60, 20 * 60],
            niedertarif_minutes: [22 * 60, 6 * 60],
            billed_quarters: vec!["Q1".into(), "Q4".into()],
            ht_ct_per_kwh: 18.0,
            st_ct_per_kwh: 10.0,
            nt_ct_per_kwh: 4.0,
            source: Some("https://example.invalid/preisblatt-2026.pdf".into()),
        }
    }

    fn entry() -> Modul3Entry {
        Modul3Entry {
            netzbetreiber: "9900123456789".into(),
            calendar: transcription(),
        }
    }

    #[test]
    fn a_conformant_calendar_is_curated_and_served() {
        let catalogue = Catalogue::curate(&[entry()]).expect("a conformant calendar");
        assert_eq!(catalogue.len(), 1);
        let served = catalogue
            .for_operator("9900123456789")
            .expect("the operator is in the catalogue");
        assert_eq!(served[0].calendar.year, 2026);
        assert!(catalogue.for_operator("9900000000000").is_none());
    }

    #[test]
    fn a_calendar_that_breaks_the_anwendungshilfe_refuses_to_start() {
        // A Hochtarif of ninety minutes: ten minutes short of the two hours § 2
        // requires, which is exactly the quiet transcription slip the check
        // exists for.
        let mut broken = entry();
        broken.calendar.hochtarif_minutes = [17 * 60, 18 * 60 + 30];
        let err = Catalogue::curate(&[broken]).expect_err("a tariff nobody may sell");
        assert!(matches!(err, CatalogueError::Violates { .. }), "{err}");
    }

    #[test]
    fn a_calendar_with_no_source_document_is_refused() {
        let mut anonymous = entry();
        anonymous.calendar.source = None;
        let err = Catalogue::curate(&[anonymous]).expect_err("no provenance, no catalogue");
        assert!(matches!(err, CatalogueError::NoSource { .. }), "{err}");
    }

    #[test]
    fn two_calendars_for_one_operator_and_year_are_refused() {
        let err = Catalogue::curate(&[entry(), entry()])
            .expect_err("`which calendar applies` must have one answer");
        assert!(matches!(err, CatalogueError::Duplicate { .. }), "{err}");
    }

    #[test]
    fn a_quarter_that_is_not_one_is_named_rather_than_read_as_absent() {
        let mut typo = entry();
        typo.calendar.billed_quarters = vec!["Q1".into(), "Q5".into()];
        let err = Catalogue::curate(&[typo]).expect_err("Q5 is a typo, not a Wahlrecht");
        assert!(matches!(err, CatalogueError::NotAQuarter { .. }), "{err}");
    }

    #[test]
    fn two_years_for_one_operator_are_two_entries_not_a_conflict() {
        let mut next_year = entry();
        next_year.calendar.year = 2027;
        next_year.calendar.id = "NB-14A-3-2027".into();
        let catalogue =
            Catalogue::curate(&[entry(), next_year]).expect("windows are fixed per year");
        assert_eq!(
            catalogue.for_operator("9900123456789").map(<[_]>::len),
            Some(2)
        );
    }
}
