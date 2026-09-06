//! The Data Act export, the § 14a Nachweis, and the MiSpeL settlement.
//!
//! Three exports with three different readers, and they are not the same
//! document.
//!
//! **The household's** — Regulation (EU) 2023/2854 Article 4 gives a user the
//! right to the data their connected product generates, "in a comprehensive,
//! structured, commonly used and machine-readable format", and free of charge.
//! So it is JSON, it is everything, and it is one request.
//!
//! **The network operator's** — `[A1 7.2]` asks a narrower question about a
//! wider window: what did you command, when, and what did the connection point
//! draw while it lasted. It is one event at a time and it carries the whole
//! minute-resolution trace, because an operator checking a reduction is checking
//! the trace.
//!
//! **The MiSpeL settlement** — from 01.10.2026 a storage system that has ever
//! been charged from the grid, or a bidirectional charge point, keeps its levy
//! privileges (§ 21 EnFG) and its EEG support only if the energy through it is
//! *separated*: which quantity was green, which grey, quarter hour by quarter
//! hour, summed over a calendar month. `hems-grid::mispel` is that arithmetic
//! and this is the document it produces. Until it existed the box wrote the
//! registers, this service kept them for two years, and **nothing ever settled
//! them** — the whole formula set was reachable only from a test.
//!
//! # Why both are generated rather than stored
//!
//! A stored export is a third copy that drifts. Both of these are a query over
//! the same two tables, and the tables are the record.
//!
//! # And why neither of them writes SQL
//!
//! Both go through [`Store`]'s own reads and serialise the domain
//! types, so a Nachweis is the record rather than a second rendering of it.
//! Assembling the document column by column would make it a second rendering,
//! and the two would agree until one of them was changed.

use hems_grid::mispel::{Basisfall, PauschalPlant, QuarterHour, RuleSet};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use time::{Month, OffsetDateTime};

use crate::config::MispelSettings;
use crate::store::{Store, StoreError};

/// Everything one site has on record, for a household exercising Article 4.
///
/// # Errors
/// [`StoreError::Sql`], or [`StoreError::NotReadable`] for a stored event this
/// build cannot parse.
pub fn data_act(store: &Store, site: &str) -> Result<Value, StoreError> {
    let quarters = store.quarter_hours(site, None, None)?;
    Ok(json!({
        "site": site,
        "produced_at": OffsetDateTime::now_utc().unix_timestamp(),
        "notice": "Regulation (EU) 2023/2854 Article 4: the data this product generated, \
                   in full, machine-readable, and free of charge.",
        "quarter_hours": quarters.iter().map(quarter_hour).collect::<Vec<_>>(),
        "control_events": events(store, site, None, None)?,
    }))
}

/// One site's § 14a evidence over a window, for a network operator.
///
/// # Errors
/// As [`data_act`].
pub fn nachweis(
    store: &Store,
    site: &str,
    from: Option<OffsetDateTime>,
    to: Option<OffsetDateTime>,
) -> Result<Value, StoreError> {
    Ok(json!({
        "site": site,
        "basis": "BK6-22-300 Anlage 1, Ziffer 7.2",
        "retention": "two years from the end of each event, Ziffer 7.3",
        "produced_at": OffsetDateTime::now_utc().unix_timestamp(),
        "events": events(store, site, from, to)?,
    }))
}

/// The events over a window, each as the document that was stored.
fn events(
    store: &Store,
    site: &str,
    from: Option<OffsetDateTime>,
    to: Option<OffsetDateTime>,
) -> Result<Vec<Value>, StoreError> {
    store
        .control_events(site, from, to)?
        .into_iter()
        .map(|stored| {
            // `ControlEvent`'s own `serde` form, so the document an operator is
            // handed is the one the box wrote — instants as RFC 3339, powers in
            // watts, every ceiling in sequence — plus the two facts the store
            // knows and the event does not.
            let mut value =
                serde_json::to_value(&stored.event).map_err(|e| StoreError::NotReadable {
                    id: stored.id,
                    detail: e.to_string(),
                })?;
            if let Some(map) = value.as_object_mut() {
                map.insert("id".into(), json!(stored.id));
                map.insert(
                    "expires_at".into(),
                    json!(stored.expires_at.unix_timestamp()),
                );
                // Derived, and in the document because an operator reading a
                // Nachweis is asking exactly these two questions and should not
                // have to fold a list of ceilings to answer them.
                map.insert(
                    "strictest_ceiling_w".into(),
                    json!(stored.event.strictest_ceiling().get()),
                );
                map.insert("below_minimum".into(), json!(stored.event.below_minimum()));
            }
            Ok(value)
        })
        .collect()
}

/// One quarter hour's registers.
///
/// Decimal **strings**, not numbers. A JSON number is a `double` to every reader
/// that has ever parsed one, and a settlement quantity that has been through one
/// is a settlement quantity nobody can reproduce.
fn quarter_hour(q: &QuarterHour) -> Value {
    json!({
        "slot_start": q.slot.start().unix_timestamp(),
        "grid_draw_kwh": q.grid_draw.to_string(),
        "grid_feed_in_kwh": q.grid_feed_in.to_string(),
        "device_consumption_kwh": q.device_consumption.to_string(),
        "device_generation_kwh": q.device_generation.to_string(),
        "anzulegender_wert_ct": q.anzulegender_wert.to_string(),
        "spot_price_ct": q.spot_price.to_string(),
    })
}

/// Why a MiSpeL settlement could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum MispelExportError {
    /// The site has declared no option, so there is nothing to settle it under.
    ///
    /// Refused rather than defaulted: every option produces a different
    /// Nachweis from the same registers, and one computed under a Basisfall the
    /// household is not on is arithmetically perfect and about a different
    /// installation.
    #[error(
        "site {site} has declared no MiSpeL option; add a `[mispel.{site}]` \
         section naming the one the Anlagenbetreiber chose"
    )]
    Undeclared {
        /// Which site.
        site: String,
    },
    /// The window asked for is not a period the chosen option settles over.
    #[error("{0}")]
    WrongWindow(String),
    /// The month or year asked for is not one.
    #[error("{year}-{month:?} is not a calendar month")]
    NotACalendarMonth {
        /// The year given.
        year: i32,
        /// The month given.
        month: u8,
    },
    /// The arithmetic refused the values.
    #[error("the settlement refused these registers: {0}")]
    Arithmetic(#[from] hems_grid::mispel::MispelError),
    /// The store could not be read.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// How much of the settled period the box actually has registers for.
///
/// A Nachweis is a legal document and the Festlegung sums `∑M` over a whole
/// period, so **the denominator has to be visible**. A quarter hour the box
/// could not price gets no register at all (that is deliberate — a register
/// carries two prices, and a zero in either is the figure that says § 51 EEG
/// switched support off), so a month legitimately has gaps, and a settlement
/// summed over 2 800 of 2 976 quarter hours under-reports every quantity in it
/// while looking complete.
///
/// This is the same rule the forecast scores learned the hard way: a score
/// whose denominator is invisible cannot fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Completeness {
    /// How many quarter hours the period has, in the Europe/Berlin calendar —
    /// so the long October day has 100 and the short March day 92.
    pub expected: usize,
    /// How many the box has a register for.
    pub present: usize,
}

impl Completeness {
    /// Whether every quarter hour of the period is on record.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.present >= self.expected
    }
}

/// The Ausschließlichkeitsoption's Nachweis: nothing to settle, and a figure
/// that says so.
///
/// The claim is a **statement about the registers** — a store that is never
/// charged from the grid — and `(1)¼ = MIN[Z1NB¼ ; Z2V¼]` is what measures it
/// `[MiSpeL A1 2.1.5]`: the option holds exactly while that is zero in every
/// quarter hour of the period.
///
/// So this computes it. Returning the declaration alone made the one option
/// whose Nachweis is a *promise* the one option nothing checked, and a household
/// would have learnt that its levy privilege had lapsed from its network
/// operator rather than from its own box. The planner refuses to schedule a
/// simultaneous charge (D142); this is what says none happened.
///
/// # Errors
/// [`MispelExportError`] where the window is not a real calendar period, or the
/// store cannot be read.
fn exclusivity(
    store: &Store,
    site: &str,
    rules: RuleSet,
    year: i32,
    month: Option<u8>,
) -> Result<Value, MispelExportError> {
    let (from, to) = match month {
        Some(m) => calendar_month(year, m)?,
        None => calendar_year(year)?,
    };
    let quarters = store.quarter_hours(site, Some(from), Some(to))?;
    let complete = Completeness {
        expected: quarter_hours_between(from, to),
        present: quarters.len(),
    };
    let simultaneous: Decimal = quarters
        .iter()
        .map(hems_grid::mispel::QuarterHour::simultaneous_grid_charging)
        .sum();
    // Reported rather than merely summed: a household whose claim has lapsed
    // needs the quarter hour, not a total it has to go looking for.
    let breaches: Vec<Value> = quarters
        .iter()
        .filter(|q| q.simultaneous_grid_charging() > Decimal::ZERO)
        .map(|q| {
            json!({
                "slot": q.slot.start().to_string(),
                "kwh": q.simultaneous_grid_charging().to_string(),
            })
        })
        .collect();
    let held = simultaneous.is_zero();
    Ok(json!({
        "site": site,
        "option": "ausschliesslichkeit",
        "rule_set": rules.version(),
        "effective_from": rules.effective_from().to_string(),
        "year": year,
        "month": month,
        "settled": false,
        "complete": complete.is_complete(),
        "quarter_hours": { "expected": complete.expected, "present": complete.present },
        // `(1)¼` summed over the period. Zero is the claim; anything else is
        // grid electricity that entered the store, and the option does not
        // cover the period it happened in.
        "gleichzeitiger_netzbezug_kwh": simultaneous.to_string(),
        "held": held,
        "breaches": breaches,
        "notice": if held {
            "Ausschließlichkeitsoption: no quarter hour of this period shows grid draw \
             and storage charging at the same time, so no energy through the store is \
             grey and there is nothing to separate."
        } else {
            "Ausschließlichkeitsoption: the registers show grid draw and storage \
             charging in the same quarter hour. The option does not cover this period; \
             the Abgrenzungs- or Pauschaloption has to be settled instead."
        },
    }))
}

/// One site's MiSpeL settlement over a calendar period, `[MiSpeL A1 4.2]` /
/// `[MiSpeL A2 4.2]`.
///
/// `month` is required for the Abgrenzungsoption, which settles per calendar
/// month, and refused for the Pauschaloption, which settles per calendar year.
/// The window is built from `metering::calendar`, not from the caller: the
/// arithmetic requires exactly one calendar period and says it cannot check
/// that for you, so the one thing to do is take the ability to get it wrong
/// away from whoever is asking. A month boundary at a fixed `+01:00` would
/// lose an hour every March and double one every October — on the two months a
/// settlement is most likely to be queried.
///
/// # Errors
/// [`MispelExportError`] where the site has declared no option, the window does
/// not suit the declared one, or the arithmetic refuses the registers.
pub fn mispel(
    store: &Store,
    site: &str,
    declared: Option<MispelSettings>,
    year: i32,
    month: Option<u8>,
) -> Result<Value, MispelExportError> {
    let declared = declared.ok_or_else(|| MispelExportError::Undeclared {
        site: site.to_owned(),
    })?;
    let rules = RuleSet::default();

    // Ausschließlichkeit has nothing to *separate* — a store never charged from
    // the grid holds no grey energy — but it is not therefore a period with
    // nothing to say. See [`exclusivity`].
    if matches!(declared, MispelSettings::Ausschliesslichkeit) {
        return exclusivity(store, site, rules, year, month);
    }

    let (from, to, expected, period) = match (&declared, month) {
        (MispelSettings::Abgrenzung { .. }, Some(m)) => {
            let (from, to) = calendar_month(year, m)?;
            (
                from,
                to,
                quarter_hours_between(from, to),
                format!("{year}-{m:02}"),
            )
        }
        (MispelSettings::Abgrenzung { .. }, None) => {
            return Err(MispelExportError::WrongWindow(
                "the Abgrenzungsoption settles per calendar month, so `month` is required".into(),
            ));
        }
        (MispelSettings::Pauschal { .. }, None) => {
            let (from, to) = calendar_year(year)?;
            (from, to, quarter_hours_between(from, to), year.to_string())
        }
        (MispelSettings::Pauschal { .. }, Some(_)) => {
            return Err(MispelExportError::WrongWindow(
                "the Pauschaloption settles per calendar year, so `month` must be omitted".into(),
            ));
        }
        (MispelSettings::Ausschliesslichkeit, _) => unreachable!("answered above"),
    };

    let quarters = store.quarter_hours(site, Some(from), Some(to))?;
    let complete = Completeness {
        expected,
        present: quarters.len(),
    };

    let figures = match declared {
        MispelSettings::Abgrenzung { basisfall } => {
            abgrenzung_figures(basisfall, rules, &quarters)?
        }
        MispelSettings::Pauschal {
            fall,
            solar_kwp,
            storage_kwh,
        } => {
            let plant = PauschalPlant {
                fall,
                solar_kwp: decimal(solar_kwp),
                storage_kwh: decimal(storage_kwh),
            };
            pauschal_figures(plant, rules, &quarters)?
        }
        MispelSettings::Ausschliesslichkeit => unreachable!("answered above"),
    };

    Ok(json!({
        "site": site,
        "rule_set": rules.version(),
        "effective_from": rules.effective_from().to_string(),
        "period": period,
        "from": from.unix_timestamp(),
        "to": to.unix_timestamp(),
        "settled": true,
        // The denominator, on the document. A settlement over part of a period
        // is not a settlement, and the reader has to be able to see that
        // without recomputing it.
        "quarter_hours_expected": complete.expected,
        "quarter_hours_present": complete.present,
        "complete": complete.is_complete(),
        "figures": figures,
    }))
}

/// The Abgrenzungsoption's figures, `[MiSpeL A1 (1)]`–`(33)`.
///
/// The domain type is **serialised**, not transcribed field by field. Its own
/// names are the Festlegung's — `Z1NB¼`, `Z2V¼`, the numbered intermediates —
/// and a document that re-listed them here would be a second rendering that
/// agrees with the arithmetic until one of the two is changed. That is the same
/// argument the module header makes about the other two exports.
///
/// The one addition is [`Abgrenzung::levy_relief_share`], which is a method
/// rather than a field: the share of the month's grid draw that escaped the
/// levies, and the one number a household actually wants out of all of this.
fn abgrenzung_figures(
    fall: Basisfall,
    rules: RuleSet,
    quarters: &[QuarterHour],
) -> Result<Value, MispelExportError> {
    let a = hems_grid::mispel::abgrenzung_month(fall, rules, quarters)?;
    Ok(json!({
        "option": "abgrenzung",
        "abgrenzung": serde_json::to_value(a).unwrap_or(Value::Null),
        "levy_relief_share": a.levy_relief_share().to_string(),
    }))
}

/// The Pauschaloption's figures, `[MiSpeL A2 (P1)]`–`(P15)`, serialised for the
/// same reason.
fn pauschal_figures(
    plant: PauschalPlant,
    rules: RuleSet,
    quarters: &[QuarterHour],
) -> Result<Value, MispelExportError> {
    let p = hems_grid::mispel::pauschal_year(plant, rules, quarters)?;
    Ok(json!({
        "option": "pauschal",
        "plant": serde_json::to_value(plant).unwrap_or(Value::Null),
        "pauschal": serde_json::to_value(p).unwrap_or(Value::Null),
    }))
}

/// `f64` in a configuration file to the exact decimal a settlement needs.
///
/// A nameplate is written on a price sheet with one decimal place; the
/// arithmetic it feeds is exact (P3). Going through a string rather than
/// `Decimal::from_f64_retain` is what stops `9.8` becoming
/// `9.800000000000000710542735760100185871124267578125`.
fn decimal(value: f64) -> Decimal {
    format!("{value}").parse().unwrap_or_default()
}

/// The Europe/Berlin window of one calendar month, half-open.
fn calendar_month(
    year: i32,
    month: u8,
) -> Result<(OffsetDateTime, OffsetDateTime), MispelExportError> {
    let m =
        Month::try_from(month).map_err(|_| MispelExportError::NotACalendarMonth { year, month })?;
    let first = time::Date::from_calendar_date(year, m, 1)
        .map_err(|_| MispelExportError::NotACalendarMonth { year, month })?;
    let next = if month == 12 {
        time::Date::from_calendar_date(year + 1, Month::January, 1)
    } else {
        time::Date::from_calendar_date(year, m.next(), 1)
    }
    .map_err(|_| MispelExportError::NotACalendarMonth { year, month })?;
    Ok((
        metering::calendar::day_start_utc(first),
        metering::calendar::day_start_utc(next),
    ))
}

/// The same for a whole calendar year.
fn calendar_year(year: i32) -> Result<(OffsetDateTime, OffsetDateTime), MispelExportError> {
    let first = time::Date::from_calendar_date(year, Month::January, 1)
        .map_err(|_| MispelExportError::NotACalendarMonth { year, month: 1 })?;
    let next = time::Date::from_calendar_date(year + 1, Month::January, 1)
        .map_err(|_| MispelExportError::NotACalendarMonth { year, month: 1 })?;
    Ok((
        metering::calendar::day_start_utc(first),
        metering::calendar::day_start_utc(next),
    ))
}

/// How many quarter hours a half-open window holds.
///
/// Computed from the instants rather than from `days × 96`, so the ninety-two
/// quarter hours of the short March day and the hundred of the long October one
/// come out right — which is the whole reason the boundaries come from
/// `metering::calendar`.
fn quarter_hours_between(from: OffsetDateTime, to: OffsetDateTime) -> usize {
    let seconds = (to - from).whole_seconds().max(0);
    usize::try_from(seconds / (15 * 60)).unwrap_or(0)
}
