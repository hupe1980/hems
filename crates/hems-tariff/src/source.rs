//! Reading a price list somebody else published.
//!
//! Five sources cover every German household in 2026, and each publishes the
//! same day-ahead auction in a different shape:
//!
//! | Source | Format | Unit | Needs |
//! |---|---|---|---|
//! | **ENTSO-E Transparency** (A44) | XML | €/MWh | a free API token |
//! | **SMARD** (Bundesnetzagentur) | JSON | €/MWh | nothing |
//! | **aWATTar** | JSON | €/MWh | nothing |
//! | **Tibber** | GraphQL JSON | €/kWh **gross** | the customer's token |
//! | **Energy-Charts** (Fraunhofer) | JSON | g CO₂/kWh | nothing |
//!
//! The **parsing** lives here, in a crate that does no I/O: a `&str` in, a
//! series out. Fetching is `tariffd`'s job. That split is not tidiness — it is
//! what lets every one of these be tested against a captured response, on a
//! machine with no network, in a millisecond, including the two days a year that
//! break everybody's price handling.
//!
//! # Three things that are wrong in most implementations
//!
//! **The factor of ten.** Four of the five publish €/MWh and the household is
//! billed in ct/kWh. They differ by exactly 10, in the direction where a mistake
//! looks plausible — 32 ct/kWh and 32 €/MWh are both ordinary numbers for the
//! same hour. Every parser here converts once, in one place, and a test pins it.
//!
//! **The resolution.** The German day-ahead market time unit has been a quarter
//! hour since 01.10.2025, but ENTSO-E still publishes `PT60M` for some areas and
//! aWATTar's Austrian feed is hourly. An hourly price **expands** to four
//! identical quarter hours — a price is a constant over its own interval, so
//! this is exact — and a quarter-hourly one is never averaged up. Averaging four
//! quarter hours into an hour throws away exactly the structure the whole
//! workspace plans in.
//!
//! **The two days a year.** On the last Sunday in March a day has 92 quarter
//! hours and on the last in October it has 100, with one local hour occurring
//! twice. Positions are therefore resolved as **instants** — `start + (position
//! − 1) × resolution` — and never as "the *n*th quarter hour of the day", which
//! is the arithmetic that silently drops or duplicates an hour. `Slot` carries
//! the Europe/Berlin calendar from `metering`, so a slot resolved this way lands
//! in the right register on both days.
//!
//! # And one that is a decision rather than a bug
//!
//! Tibber publishes the **gross** consumer price, everything included; the other
//! four publish the **net wholesale** price. They are not interchangeable, and
//! adding a markup, network charges, levies and VAT to a Tibber figure prices a
//! kilowatt-hour at roughly twice what it costs. So a series carries the
//! [`PriceBasis`] it was published on, and [`PriceSeries::into_spot`] — the only
//! way into [`crate::tariff::EnergyPrice::Dynamic`], which expects a wholesale
//! price — **refuses** a gross one.

use std::collections::BTreeMap;

use hems_core::prelude::Slot;
use rust_decimal::Decimal;
use thiserror::Error;
use time::OffsetDateTime;

/// Why a published price list could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SourceError {
    /// The document is not the shape this parser expects.
    #[error("not a {expected} document: {detail}")]
    Shape {
        /// What was expected.
        expected: &'static str,
        /// What was wrong.
        detail: String,
    },
    /// A timestamp could not be read.
    #[error("unreadable timestamp: {0}")]
    Timestamp(String),
    /// A price could not be read.
    #[error("unreadable price: {0}")]
    Price(String),
    /// The document parsed but carried no usable points.
    #[error("the document carries no prices")]
    Empty,
    /// A series in the household's own currency was asked to act as a
    /// wholesale one.
    #[error("this series is a gross consumer price and cannot be used as a wholesale one")]
    WrongBasis,
}

/// What a published number actually means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PriceBasis {
    /// The wholesale auction price, net of everything — what
    /// [`crate::tariff::EnergyPrice::Dynamic`] wants, because the markup,
    /// network charge, levies and VAT are added on top of it by
    /// [`crate::stack`].
    Wholesale,
    /// The price the household is actually charged, everything included.
    /// Useful for a bill check; wrong as an input to the price stack.
    GrossConsumer,
}

/// Where a series came from.
///
/// Ordered so a `BTreeMap` can be keyed by it — `tariffd` holds one endpoint and
/// one schedule per source, and a map with a defined iteration order is what
/// makes a poll round the same round twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Source {
    /// ENTSO-E Transparency Platform, document A44.
    Entsoe,
    /// SMARD, the Bundesnetzagentur's data portal.
    Smard,
    /// aWATTar (DE and AT).
    Awattar,
    /// Tibber's GraphQL API.
    Tibber,
    /// Energy-Charts (Fraunhofer ISE).
    EnergyCharts,
}

/// A parsed price list.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PriceSeries {
    /// One entry per quarter hour, ct/kWh.
    #[cfg_attr(feature = "serde", serde(with = "crate::wire::decimal_map"))]
    pub points: BTreeMap<Slot, Decimal>,
    /// Where it came from.
    pub source: Source,
    /// What the numbers mean.
    pub basis: PriceBasis,
    /// The resolution the source published at, in minutes — 15 or 60. Kept
    /// because a household told "the price is negative from 13:00 to 14:00"
    /// should not be shown four separate quarter hours as though they were
    /// independently observed.
    pub published_minutes: u16,
}

impl PriceSeries {
    /// The spot map for [`crate::tariff::EnergyPrice::Dynamic`].
    ///
    /// # Errors
    /// [`SourceError::WrongBasis`] for a gross consumer series: adding a markup
    /// and the whole levy stack to a price that already contains them prices a
    /// kilowatt-hour at about twice what it costs.
    pub fn into_spot(self) -> Result<BTreeMap<Slot, Decimal>, SourceError> {
        match self.basis {
            PriceBasis::Wholesale => Ok(self.points),
            PriceBasis::GrossConsumer => Err(SourceError::WrongBasis),
        }
    }

    /// How many quarter hours the series covers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether it covers none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// The quarter hours where the wholesale price is negative — the § 51 EEG
    /// hours, and the ones worth planning around.
    #[must_use]
    pub fn negative_slots(&self) -> Vec<Slot> {
        self.points
            .iter()
            .filter(|(_, p)| **p < Decimal::ZERO)
            .map(|(s, _)| *s)
            .collect()
    }
}

/// €/MWh to ct/kWh. One place, one factor, one test.
///
/// `1 €/MWh = 100 ct / 1000 kWh = 0,1 ct/kWh`.
fn eur_per_mwh_to_ct_per_kwh(value: f64) -> Result<Decimal, SourceError> {
    Decimal::try_from(value / 10.0).map_err(|e| SourceError::Price(e.to_string()))
}

/// €/kWh to ct/kWh.
fn eur_per_kwh_to_ct_per_kwh(value: f64) -> Result<Decimal, SourceError> {
    Decimal::try_from(value * 100.0).map_err(|e| SourceError::Price(e.to_string()))
}

/// Spread one interval's price over the quarter hours it covers.
///
/// Exact for a price, which is constant over its own market time unit — unlike
/// an *energy*, which would have to be divided. Conflating the two is how an
/// hourly feed ends up quartering the price.
fn expand(
    points: &mut BTreeMap<Slot, Decimal>,
    start: OffsetDateTime,
    minutes: i64,
    price_ct: Decimal,
) {
    let quarters = (minutes / 15).max(1);
    for q in 0..quarters {
        points.insert(
            Slot::containing(start + time::Duration::minutes(q * 15)),
            price_ct,
        );
    }
}

// ── aWATTar ─────────────────────────────────────────────────────────────────

/// Parse an aWATTar `/v1/marketdata` response.
///
/// ```json
/// {"object":"list","data":[{"start_timestamp":1767222000000,
///   "end_timestamp":1767225600000,"marketprice":32.4,"unit":"Eur/MWh"}]}
/// ```
///
/// # Errors
/// [`SourceError`] when the document is not a list of market data points, or
/// carries none.
pub fn awattar(json: &str) -> Result<PriceSeries, SourceError> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(|e| SourceError::Shape {
        expected: "aWATTar",
        detail: e.to_string(),
    })?;
    let data = root
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| SourceError::Shape {
            expected: "aWATTar",
            detail: "no `data` array".into(),
        })?;

    let mut points = BTreeMap::new();
    let mut published = 60u16;
    for entry in data {
        let start_ms = entry
            .get("start_timestamp")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| SourceError::Timestamp("no `start_timestamp`".into()))?;
        let end_ms = entry
            .get("end_timestamp")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(start_ms + 3_600_000);
        let price = entry
            .get("marketprice")
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| SourceError::Price("no `marketprice`".into()))?;
        let start = OffsetDateTime::from_unix_timestamp_nanos(i128::from(start_ms) * 1_000_000)
            .map_err(|e| SourceError::Timestamp(e.to_string()))?;
        let minutes = ((end_ms - start_ms) / 60_000).clamp(15, 60);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            published = published.min(minutes as u16);
        }
        expand(
            &mut points,
            start,
            minutes,
            eur_per_mwh_to_ct_per_kwh(price)?,
        );
    }
    if points.is_empty() {
        return Err(SourceError::Empty);
    }
    Ok(PriceSeries {
        points,
        source: Source::Awattar,
        basis: PriceBasis::Wholesale,
        published_minutes: published,
    })
}

// ── SMARD ───────────────────────────────────────────────────────────────────

/// Parse a SMARD `chart_data` response.
///
/// ```json
/// {"meta":{},"series":[[1767222000000, 32.4], [1767223800000, null]]}
/// ```
///
/// SMARD publishes `null` where an auction produced no value; those quarter
/// hours are **left out** rather than filled, because the planner's fallback for
/// an unknown price is an explicit, documented one
/// ([`crate::tariff::EnergyPrice::Dynamic::fallback_ct_per_kwh`]) and quietly
/// substituting the neighbour would hide an outage behind a plausible number.
///
/// # Errors
/// [`SourceError`] when the document has no `series`, or none of it is usable.
pub fn smard(json: &str) -> Result<PriceSeries, SourceError> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(|e| SourceError::Shape {
        expected: "SMARD",
        detail: e.to_string(),
    })?;
    let series = root
        .get("series")
        .and_then(|s| s.as_array())
        .ok_or_else(|| SourceError::Shape {
            expected: "SMARD",
            detail: "no `series` array".into(),
        })?;

    // The step is read from the data rather than assumed: SMARD serves the same
    // endpoint at an hourly and a quarter-hourly resolution.
    let stamps: Vec<i64> = series
        .iter()
        .filter_map(|p| p.get(0).and_then(serde_json::Value::as_i64))
        .collect();
    let minutes = stamps
        .windows(2)
        .map(|w| (w[1] - w[0]) / 60_000)
        .filter(|m| *m > 0)
        .min()
        .unwrap_or(60)
        .clamp(15, 60);

    let mut points = BTreeMap::new();
    for entry in series {
        let Some(stamp) = entry.get(0).and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(price) = entry.get(1).and_then(serde_json::Value::as_f64) else {
            continue; // a `null`: an hour the auction did not price.
        };
        let start = OffsetDateTime::from_unix_timestamp_nanos(i128::from(stamp) * 1_000_000)
            .map_err(|e| SourceError::Timestamp(e.to_string()))?;
        expand(
            &mut points,
            start,
            minutes,
            eur_per_mwh_to_ct_per_kwh(price)?,
        );
    }
    if points.is_empty() {
        return Err(SourceError::Empty);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(PriceSeries {
        points,
        source: Source::Smard,
        basis: PriceBasis::Wholesale,
        published_minutes: minutes as u16,
    })
}

// ── Tibber ──────────────────────────────────────────────────────────────────

/// Parse a Tibber `priceInfo` GraphQL response.
///
/// ```json
/// {"data":{"viewer":{"homes":[{"currentSubscription":{"priceInfo":{
///   "today":[{"total":0.324,"startsAt":"2026-01-01T00:00:00.000+01:00"}],
///   "tomorrow":[]}}}]}}}
/// ```
///
/// `total` is the **gross consumer** price in €/kWh, so the series comes back
/// with [`PriceBasis::GrossConsumer`] and [`PriceSeries::into_spot`] refuses it.
/// Tibber's own `energy` field is closer to a wholesale price but still carries
/// the supplier's markup, so it is not one either.
///
/// # Errors
/// [`SourceError`] when the response is not a `priceInfo` document, or carries
/// no prices.
pub fn tibber(json: &str) -> Result<PriceSeries, SourceError> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(|e| SourceError::Shape {
        expected: "Tibber",
        detail: e.to_string(),
    })?;
    let info = root
        .pointer("/data/viewer/homes/0/currentSubscription/priceInfo")
        .ok_or_else(|| SourceError::Shape {
            expected: "Tibber",
            detail: "no priceInfo for the first home".into(),
        })?;

    let mut points = BTreeMap::new();
    let mut minutes = 60i64;
    let mut stamps: Vec<OffsetDateTime> = Vec::new();
    for day in ["today", "tomorrow"] {
        let Some(entries) = info.get(day).and_then(|d| d.as_array()) else {
            continue;
        };
        for entry in entries {
            let starts_at = entry
                .get("startsAt")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| SourceError::Timestamp("no `startsAt`".into()))?;
            let start =
                OffsetDateTime::parse(starts_at, &time::format_description::well_known::Rfc3339)
                    .map_err(|e| SourceError::Timestamp(e.to_string()))?;
            let total = entry
                .get("total")
                .and_then(serde_json::Value::as_f64)
                .ok_or_else(|| SourceError::Price("no `total`".into()))?;
            stamps.push(start);
            points.insert(Slot::containing(start), eur_per_kwh_to_ct_per_kwh(total)?);
        }
    }
    stamps.sort_unstable();
    if let Some(step) = stamps
        .windows(2)
        .map(|w| (w[1] - w[0]).whole_minutes())
        .filter(|m| *m > 0)
        .min()
    {
        minutes = step.clamp(15, 60);
    }
    // Expand only after the resolution is known, because Tibber publishes
    // hourly today and has said it will publish quarter hours.
    if minutes > 15 {
        let hourly: Vec<(Slot, Decimal)> = points.iter().map(|(s, p)| (*s, *p)).collect();
        for (slot, price) in hourly {
            expand(&mut points, slot.start(), minutes, price);
        }
    }
    if points.is_empty() {
        return Err(SourceError::Empty);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(PriceSeries {
        points,
        source: Source::Tibber,
        basis: PriceBasis::GrossConsumer,
        published_minutes: minutes as u16,
    })
}

// ── Energy-Charts ───────────────────────────────────────────────────────────

/// Parse a Fraunhofer Energy-Charts `co2eq` response into grams per kilowatt
/// hour per quarter hour.
///
/// ```json
/// {"unix_seconds":[1767222000,1767225600],"co2eq":[380.2,412.7]}
/// ```
///
/// Not a price: it is what [`crate::stack::SlotPrice::co2_g_per_kwh`] carries,
/// and what turns a carbon preference into a term the objective can add up.
///
/// # Errors
/// [`SourceError`] when the arrays are missing or of different lengths.
pub fn energy_charts_co2(json: &str) -> Result<BTreeMap<Slot, f64>, SourceError> {
    let root: serde_json::Value = serde_json::from_str(json).map_err(|e| SourceError::Shape {
        expected: "Energy-Charts",
        detail: e.to_string(),
    })?;
    let seconds = root
        .get("unix_seconds")
        .and_then(|v| v.as_array())
        .ok_or_else(|| SourceError::Shape {
            expected: "Energy-Charts",
            detail: "no `unix_seconds`".into(),
        })?;
    // The series is named for the quantity; take whichever of the known names
    // is present rather than insisting on one.
    let values = ["co2eq", "co2eq_forecast", "data"]
        .iter()
        .find_map(|k| root.get(*k).and_then(|v| v.as_array()))
        .ok_or_else(|| SourceError::Shape {
            expected: "Energy-Charts",
            detail: "no `co2eq` array".into(),
        })?;
    if seconds.len() != values.len() {
        return Err(SourceError::Shape {
            expected: "Energy-Charts",
            detail: format!("{} stamps against {} values", seconds.len(), values.len()),
        });
    }

    let stamps: Vec<i64> = seconds
        .iter()
        .filter_map(serde_json::Value::as_i64)
        .collect();
    let minutes = stamps
        .windows(2)
        .map(|w| (w[1] - w[0]) / 60)
        .filter(|m| *m > 0)
        .min()
        .unwrap_or(60)
        .clamp(15, 60);

    let mut out = BTreeMap::new();
    for (stamp, value) in seconds.iter().zip(values) {
        let (Some(stamp), Some(value)) = (stamp.as_i64(), value.as_f64()) else {
            continue;
        };
        let start = OffsetDateTime::from_unix_timestamp(stamp)
            .map_err(|e| SourceError::Timestamp(e.to_string()))?;
        for q in 0..(minutes / 15).max(1) {
            out.insert(
                Slot::containing(start + time::Duration::minutes(q * 15)),
                value,
            );
        }
    }
    if out.is_empty() {
        return Err(SourceError::Empty);
    }
    Ok(out)
}

// ── ENTSO-E ─────────────────────────────────────────────────────────────────

/// Parse an ENTSO-E Transparency **A44** (day-ahead prices) document.
///
/// The shape that matters:
///
/// ```xml
/// <Publication_MarketDocument>
///   <TimeSeries>
///     <Period>
///       <timeInterval><start>2026-01-01T23:00Z</start>…</timeInterval>
///       <resolution>PT15M</resolution>
///       <Point><position>1</position><price.amount>32.4</price.amount></Point>
/// ```
///
/// Positions are resolved as `start + (position − 1) × resolution` — see the
/// module note on the two days a year. A missing position is **carried forward**
/// from the previous point, which is what the specification says a gap means: a
/// curve is published sparsely and the last value stands until the next one.
/// That is the one place where filling a hole is right rather than dishonest,
/// because the publisher says so.
///
/// This is a targeted reader rather than a general XML deserialiser: A44 has one
/// shape, and a hand-written scan over its elements is both smaller and easier
/// to see the DST handling in.
///
/// # Errors
/// [`SourceError`] when the document is not an A44 publication, or carries no
/// points.
pub fn entsoe_a44(xml: &str) -> Result<PriceSeries, SourceError> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut scan = A44Scan::default();
    let mut in_period = false;
    let mut in_time_interval = false;
    let mut text = String::new();

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(SourceError::Shape {
                    expected: "ENTSO-E A44",
                    detail: e.to_string(),
                });
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                match e.local_name().as_ref() {
                    "Publication_MarketDocument" => scan.saw_document = true,
                    "timeInterval" => in_time_interval = true,
                    "Period" => {
                        in_period = true;
                        scan.begin_period();
                    }
                    "Point" => scan.begin_point(),
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Text(t)) => {
                let raw = t.xml10_content();
                text = quick_xml::escape::unescape(&raw)
                    .map_or_else(|_| raw.to_string(), std::borrow::Cow::into_owned);
            }
            Ok(Event::End(e)) => {
                match e.local_name().as_ref() {
                    // Only the *Period's* interval start matters; the document's
                    // own outer interval covers the whole publication.
                    "start" if in_time_interval && in_period => {
                        scan.period_start = Some(parse_entsoe_instant(&text)?);
                    }
                    "timeInterval" => in_time_interval = false,
                    "Period" => in_period = false,
                    "resolution" => scan.resolution_minutes = parse_iso_minutes(&text),
                    "position" => scan.position = text.trim().parse::<i64>().ok(),
                    "price.amount" => scan.amount = text.trim().parse::<f64>().ok(),
                    "Point" => scan.end_point()?,
                    _ => {}
                }
                text.clear();
            }
            Ok(_) => {}
        }
    }
    scan.finish()
}

/// The running state of a scan over one A44 document.
///
/// Split out so the event loop above stays readable: the interesting part of
/// this parser is [`A44Scan::end_point`], which is where a position becomes an
/// instant and a skipped position becomes a carried-forward price.
struct A44Scan {
    points: BTreeMap<Slot, Decimal>,
    period_start: Option<OffsetDateTime>,
    resolution_minutes: i64,
    position: Option<i64>,
    amount: Option<f64>,
    last_amount: Option<f64>,
    last_position: i64,
    finest: i64,
    saw_document: bool,
}

impl Default for A44Scan {
    fn default() -> Self {
        Self {
            points: BTreeMap::new(),
            period_start: None,
            resolution_minutes: 60,
            position: None,
            amount: None,
            last_amount: None,
            last_position: 0,
            finest: 60,
            saw_document: false,
        }
    }
}

impl A44Scan {
    fn begin_period(&mut self) {
        self.period_start = None;
        self.resolution_minutes = 60;
        self.last_amount = None;
        self.last_position = 0;
    }

    fn begin_point(&mut self) {
        self.position = None;
        self.amount = None;
    }

    /// One `<Point>` closed: place it, and fill in any positions the publisher
    /// skipped with the value that was standing.
    fn end_point(&mut self) -> Result<(), SourceError> {
        let (Some(start), Some(pos)) = (self.period_start, self.position) else {
            return Ok(());
        };
        let Some(value) = self.amount.or(self.last_amount) else {
            return Ok(());
        };
        for p in (self.last_position + 1).max(1)..=pos {
            let at = start + time::Duration::minutes((p - 1) * self.resolution_minutes);
            let price = if p == pos {
                eur_per_mwh_to_ct_per_kwh(value)?
            } else {
                eur_per_mwh_to_ct_per_kwh(self.last_amount.unwrap_or(value))?
            };
            expand(&mut self.points, at, self.resolution_minutes, price);
        }
        self.last_amount = Some(value);
        self.last_position = pos;
        self.finest = self.finest.min(self.resolution_minutes.max(15));
        Ok(())
    }

    fn finish(self) -> Result<PriceSeries, SourceError> {
        if !self.saw_document {
            return Err(SourceError::Shape {
                expected: "ENTSO-E A44",
                detail: "no Publication_MarketDocument".into(),
            });
        }
        if self.points.is_empty() {
            return Err(SourceError::Empty);
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(PriceSeries {
            points: self.points,
            source: Source::Entsoe,
            basis: PriceBasis::Wholesale,
            published_minutes: self.finest as u16,
        })
    }
}

/// `PT15M`, `PT30M`, `PT60M`, `PT1H` → minutes. Anything else is an hour, which
/// is what the platform defaults to.
fn parse_iso_minutes(text: &str) -> i64 {
    // `PT1H` and `PT60M` are both an hour, and so is anything unrecognised:
    // an hour is the platform's own default resolution, and guessing a quarter
    // for a duration nobody recognised would quadruple a curve.
    match text.trim() {
        "PT15M" => 15,
        "PT30M" => 30,
        "P1D" => 1440,
        _ => 60,
    }
}

/// ENTSO-E stamps look like `2026-01-01T23:00Z` — RFC 3339 without seconds,
/// which `time`'s RFC 3339 parser rejects.
fn parse_entsoe_instant(text: &str) -> Result<OffsetDateTime, SourceError> {
    let raw = text.trim();
    let normalised = if raw.len() == 17 && raw.ends_with('Z') {
        format!("{}:00Z", &raw[..raw.len() - 1])
    } else {
        raw.to_string()
    };
    OffsetDateTime::parse(&normalised, &time::format_description::well_known::Rfc3339)
        .map_err(|_| SourceError::Timestamp(raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use time::macros::datetime;

    #[test]
    fn awattar_converts_eur_per_mwh_to_ct_per_kwh_and_expands_the_hour() {
        // 2026-01-01T00:00+01:00 = 1767222000000 ms.
        let json = r#"{"object":"list","data":[
            {"start_timestamp":1767222000000,"end_timestamp":1767225600000,
             "marketprice":324.0,"unit":"Eur/MWh"}]}"#;
        let series = awattar(json).expect("a valid response");
        assert_eq!(series.published_minutes, 60);
        assert_eq!(series.len(), 4, "an hour is four quarter hours");
        // 324 €/MWh is 32,4 ct/kWh — not 3,24 and not 324.
        assert!(series.points.values().all(|p| *p == dec!(32.4)));
        let first = Slot::containing(datetime!(2026-01-01 00:00:00 +01:00));
        assert_eq!(series.points.get(&first), Some(&dec!(32.4)));
    }

    #[test]
    fn awattar_reads_a_quarter_hourly_feed_as_one() {
        let json = r#"{"object":"list","data":[
            {"start_timestamp":1767222000000,"end_timestamp":1767222900000,"marketprice":10.0},
            {"start_timestamp":1767222900000,"end_timestamp":1767223800000,"marketprice":-50.0}]}"#;
        let series = awattar(json).expect("a valid response");
        assert_eq!(series.published_minutes, 15);
        assert_eq!(series.len(), 2, "quarter hours are never expanded");
        assert_eq!(series.negative_slots().len(), 1);
    }

    #[test]
    fn smard_skips_the_hours_the_auction_did_not_price() {
        let json = r#"{"meta":{},"series":[
            [1767222000000, 324.0],
            [1767225600000, null],
            [1767229200000, 100.0]]}"#;
        let series = smard(json).expect("a valid response");
        // Two priced hours expand to eight quarter hours; the null one is left
        // out rather than interpolated.
        assert_eq!(series.len(), 8);
        assert!(
            !series
                .points
                .contains_key(&Slot::containing(datetime!(2026-01-01 01:00:00 +01:00))),
            "a gap stays a gap"
        );
    }

    #[test]
    fn tibber_is_a_gross_price_and_refuses_to_be_a_wholesale_one() {
        let json = r#"{"data":{"viewer":{"homes":[{"currentSubscription":{"priceInfo":{
            "today":[{"total":0.3240,"startsAt":"2026-01-01T00:00:00.000+01:00"},
                     {"total":0.2810,"startsAt":"2026-01-01T01:00:00.000+01:00"}],
            "tomorrow":[]}}}]}}}"#;
        let series = tibber(json).expect("a valid response");
        assert_eq!(series.basis, PriceBasis::GrossConsumer);
        assert_eq!(series.len(), 8);
        let first = Slot::containing(datetime!(2026-01-01 00:00:00 +01:00));
        assert_eq!(series.points.get(&first), Some(&dec!(32.40)));
        assert_eq!(series.into_spot(), Err(SourceError::WrongBasis));
    }

    #[test]
    fn entsoe_reads_a_quarter_hourly_curve() {
        let xml = r#"<?xml version="1.0"?>
        <Publication_MarketDocument xmlns="urn:iec62325.351:tc57wg16:451-3:publicationdocument:7:3">
          <TimeSeries>
            <currency_Unit.name>EUR</currency_Unit.name>
            <price_Measure_Unit.name>MWH</price_Measure_Unit.name>
            <Period>
              <timeInterval><start>2025-12-31T23:00Z</start><end>2025-12-31T23:45Z</end></timeInterval>
              <resolution>PT15M</resolution>
              <Point><position>1</position><price.amount>324.0</price.amount></Point>
              <Point><position>2</position><price.amount>-50.0</price.amount></Point>
              <Point><position>4</position><price.amount>10.0</price.amount></Point>
            </Period>
          </TimeSeries>
        </Publication_MarketDocument>"#;
        let series = entsoe_a44(xml).expect("a valid A44");
        assert_eq!(series.published_minutes, 15);
        assert_eq!(
            series.len(),
            4,
            "position 3 is carried forward, not dropped"
        );
        let at = |m: i64| {
            *series
                .points
                .get(&Slot::containing(
                    datetime!(2026-01-01 00:00:00 +01:00) + time::Duration::minutes(m),
                ))
                .expect("in series")
        };
        assert_eq!(at(0), dec!(32.4));
        assert_eq!(at(15), dec!(-5.0));
        // The publisher skipped position 3, which means "as before".
        assert_eq!(at(30), dec!(-5.0));
        assert_eq!(at(45), dec!(1.0));
        assert_eq!(series.negative_slots().len(), 2);
    }

    #[test]
    fn entsoe_expands_an_hourly_curve_without_averaging_it() {
        let xml = r"<Publication_MarketDocument>
          <TimeSeries><Period>
            <timeInterval><start>2025-12-31T23:00Z</start><end>2026-01-01T01:00Z</end></timeInterval>
            <resolution>PT60M</resolution>
            <Point><position>1</position><price.amount>100.0</price.amount></Point>
            <Point><position>2</position><price.amount>200.0</price.amount></Point>
          </Period></TimeSeries>
        </Publication_MarketDocument>";
        let series = entsoe_a44(xml).expect("a valid A44");
        assert_eq!(series.len(), 8);
        assert_eq!(series.published_minutes, 60);
        let at = |m: i64| {
            *series
                .points
                .get(&Slot::containing(
                    datetime!(2026-01-01 00:00:00 +01:00) + time::Duration::minutes(m),
                ))
                .expect("in series")
        };
        // Every quarter of the first hour carries the hour's price, undivided.
        for m in [0, 15, 30, 45] {
            assert_eq!(at(m), dec!(10.0));
        }
        assert_eq!(at(60), dec!(20.0));
    }

    #[test]
    fn the_long_october_day_has_a_hundred_quarter_hours() {
        // 2026-10-25 is the last Sunday in October: 03:00 CEST becomes 02:00
        // CET, so the local day has 25 hours. Positions are resolved as
        // instants, so both 02:00s get a slot of their own.
        let mut xml = String::from(
            r"<Publication_MarketDocument><TimeSeries><Period>
            <timeInterval><start>2026-10-24T22:00Z</start><end>2026-10-25T23:00Z</end></timeInterval>
            <resolution>PT60M</resolution>",
        );
        for position in 1..=25 {
            use std::fmt::Write;
            let _ = write!(
                xml,
                "<Point><position>{position}</position><price.amount>{}.0</price.amount></Point>",
                position * 10
            );
        }
        xml.push_str("</Period></TimeSeries></Publication_MarketDocument>");
        let series = entsoe_a44(&xml).expect("a valid A44");
        assert_eq!(
            series.len(),
            100,
            "a 25-hour day is 100 quarter hours, and each one is distinct"
        );
    }

    #[test]
    fn energy_charts_carbon_intensity_is_read_per_quarter_hour() {
        let json = r#"{"unix_seconds":[1767222000,1767225600],"co2eq":[380.2,412.7]}"#;
        let co2 = energy_charts_co2(json).expect("a valid response");
        assert_eq!(co2.len(), 8);
        let first = Slot::containing(datetime!(2026-01-01 00:00:00 +01:00));
        assert!((co2[&first] - 380.2).abs() < 1e-9);
    }

    #[test]
    fn nonsense_is_refused_rather_than_guessed_at() {
        assert!(matches!(
            awattar("not json"),
            Err(SourceError::Shape { .. })
        ));
        assert!(matches!(smard(r#"{"series":[]}"#), Err(SourceError::Empty)));
        assert!(matches!(
            entsoe_a44("<other/>"),
            Err(SourceError::Shape { .. })
        ));
        assert!(matches!(
            energy_charts_co2(r#"{"unix_seconds":[1,2],"co2eq":[1.0]}"#),
            Err(SourceError::Shape { .. })
        ));
    }
}
