//! The box's two outbound questions: what does electricity cost, and what will
//! the sky do.
//!
//! `tariffd` serves a reconciled day-ahead curve and says how much of a horizon
//! it covers; `forecastd` serves ICON-D2 irradiance and temperature per quarter
//! hour. The reference days supply their own from `hems-sim`, which is right for
//! a *test* and is why this is the only caller.
//!
//! # Neither is a trust anchor, and the design says so twice
//!
//! Prices and weather only ever make a plan **better** (G3). A box that cannot
//! reach either keeps the house safe and lawful — the guard and the arbiter need
//! nothing but measurements — and loses the plan, which is a cost in euros
//! rather than in compliance. So:
//!
//! * every fetch is allowed to fail, and a failure is a `None` with a log line
//!   rather than an error that stops a loop;
//! * what came back last is **kept**, because a day-ahead curve fetched an hour
//!   ago is still tomorrow's auction result, and refusing to plan because the
//!   network is down would throw away a perfectly good answer.
//!
//! # What is deliberately *not* asked for
//!
//! `forecastd` serves `/v1/production` — what a named roof would make of the
//! sky. This module asks for `/v1/weather` instead and models the roof locally,
//! because the correction that turns a *model* into a *forecast* is a property
//! of **this** roof and only the box's own meter can teach it
//! (`hems_forecast::residual`). A route called `/forecast` is one somebody
//! eventually plans against uncorrected; asking for the sky makes that mistake
//! unavailable.

use std::collections::BTreeMap;

use hems_core::prelude::{Horizon, Slot};
use rust_decimal::Decimal;

/// What a fetch could not do.
///
/// Every variant is recoverable and none of them stops the box: see the module
/// note on why a missing price is a plan that is worse rather than a house that
/// is unsafe.
#[derive(Debug, thiserror::Error)]
pub enum FleetError {
    /// The request did not complete.
    #[error("{what} could not be fetched from {url}: {source}")]
    Unreachable {
        /// `"prices"` or `"weather"`.
        what: &'static str,
        /// Where it was asked.
        url: String,
        /// Why not.
        source: reqwest::Error,
    },
    /// It completed and the answer was not one this box understands.
    #[error("{what} from {url} was not readable: {detail}")]
    Unreadable {
        /// `"prices"` or `"weather"`.
        what: &'static str,
        /// Where it was asked.
        url: String,
        /// What went wrong.
        detail: String,
    },
}

/// One quarter hour of sky, as `forecastd` publishes it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct SkyPoint {
    /// The quarter hour's start.
    #[serde(with = "time::serde::rfc3339")]
    pub slot: time::OffsetDateTime,
    /// Global horizontal irradiance, W/m².
    pub ghi_w_per_m2: f64,
    /// Air temperature, °C.
    pub temperature_c: f64,
    /// Cloud cover in `[0, 1]`, where the model publishes one.
    #[serde(default)]
    pub cloud_cover: Option<f64>,
}

/// A location's current run.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Sky {
    /// When the run was fetched — by `forecastd`, not by this box.
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: time::OffsetDateTime,
    /// The resolution the model published at.
    pub published_minutes: u16,
    /// The points.
    pub points: Vec<SkyPoint>,
}

impl Sky {
    /// Irradiance and temperature indexed by slot, for the local roof model.
    #[must_use]
    pub fn by_slot(&self) -> BTreeMap<Slot, SkyPoint> {
        self.points
            .iter()
            .map(|p| (Slot::containing(p.slot), *p))
            .collect()
    }
}

/// One priced quarter hour, as `tariffd` publishes it.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct PricePoint {
    /// The quarter hour's start.
    #[serde(with = "time::serde::rfc3339")]
    pub slot: time::OffsetDateTime,
    /// The wholesale price, ct/kWh, as an exact decimal **string**.
    ///
    /// A string on the wire and a `Decimal` here, and the round trip is the
    /// point (P3): a price that had been through an `f64` is one nobody can
    /// reproduce a bill from, and the impl a `Decimal` inherits would accept a
    /// JSON number that had already lost digits.
    pub price_ct: String,
    /// Which of the five sources it came from.
    pub source: hems_tariff::source::Source,
}

/// The answer to a price question.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Prices {
    /// The points that are known, in order. A slot `tariffd` cannot price is
    /// **absent** rather than guessed.
    pub points: Vec<PricePoint>,
    /// How much of the window is priced, in `[0, 1]`.
    pub coverage: f64,
    /// The last quarter hour reachable from the start without a gap.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub contiguous_until: Option<time::OffsetDateTime>,
}

impl Prices {
    /// The spot curve, as `hems_tariff::EnergyPrice::Dynamic` wants it.
    ///
    /// A point whose price is not an exact decimal is **dropped** rather than
    /// rounded into one: the tariff already knows what to do with a slot it has
    /// no price for (the flat fallback), and inventing a number for one it does
    /// is how a bill stops reconciling.
    #[must_use]
    pub fn spot(&self) -> BTreeMap<Slot, Decimal> {
        self.points
            .iter()
            .filter_map(|p| {
                p.price_ct
                    .parse::<Decimal>()
                    .ok()
                    .map(|ct| (Slot::containing(p.slot), ct))
            })
            .collect()
    }
}

/// The box's client for the two fleet services.
#[derive(Debug, Clone)]
pub struct Fleet {
    client: reqwest::Client,
    tariffd: Option<String>,
    forecastd: Option<String>,
    location: Option<String>,
}

impl Fleet {
    /// A client for whatever is configured. Both halves are optional.
    ///
    /// # Errors
    /// Where the HTTP client itself cannot be built, which is a TLS backend
    /// problem rather than a configuration one.
    pub fn new(settings: &crate::config::FleetSettings) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(
                    settings.request_timeout_s.max(1),
                ))
                // A box on a household connection, asking two questions every
                // five minutes. Keeping the connection open costs one socket and
                // saves a TLS handshake on a link that is often slow.
                .pool_max_idle_per_host(2)
                .build()?,
            tariffd: settings.tariffd_url.clone(),
            forecastd: settings.forecastd_url.clone(),
            location: settings.location.clone(),
        })
    }

    /// Whether anything is configured at all.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.tariffd.is_some() || self.forecastd.is_some()
    }

    /// Whether prices are available to be asked for.
    #[must_use]
    pub fn has_prices(&self) -> bool {
        self.tariffd.is_some()
    }

    /// Whether the sky is.
    #[must_use]
    pub fn has_weather(&self) -> bool {
        self.forecastd.is_some() && self.location.is_some()
    }

    /// Ask `tariffd` for the day-ahead curve over `horizon`.
    ///
    /// # Errors
    /// [`FleetError`] where the request fails or the answer is not readable.
    /// Returns `Ok(None)` where no `tariffd` is configured, which is a household
    /// on a fixed tariff rather than a fault.
    pub async fn prices(&self, horizon: Horizon) -> Result<Option<Prices>, FleetError> {
        let Some(base) = &self.tariffd else {
            return Ok(None);
        };
        let first = horizon.first.start();
        let url = format!(
            "{}/v1/prices?from={}&slots={}",
            base.trim_end_matches('/'),
            urlencode(&rfc3339(first)),
            horizon.len,
        );
        self.get("prices", &url).await.map(Some)
    }

    /// Ask `forecastd` for this household's sky.
    ///
    /// # Errors
    /// [`FleetError`]. `Ok(None)` where no `forecastd` or no location is
    /// configured.
    pub async fn sky(&self) -> Result<Option<Sky>, FleetError> {
        let (Some(base), Some(location)) = (&self.forecastd, &self.location) else {
            return Ok(None);
        };
        let url = format!(
            "{}/v1/weather/{}",
            base.trim_end_matches('/'),
            urlencode(location),
        );
        self.get("weather", &url).await.map(Some)
    }

    /// One GET, decoded.
    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        what: &'static str,
        url: &str,
    ) -> Result<T, FleetError> {
        let response =
            self.client
                .get(url)
                .send()
                .await
                .map_err(|source| FleetError::Unreachable {
                    what,
                    url: url.to_string(),
                    source,
                })?;
        let status = response.status();
        if !status.is_success() {
            return Err(FleetError::Unreadable {
                what,
                url: url.to_string(),
                detail: format!("the service answered {status}"),
            });
        }
        // Read the body once and decode from the bytes, so a decoding failure
        // can say what arrived. A `reqwest::Response::json` that fails reports
        // only serde's complaint, and "expected a string at line 1 column 84" is
        // not something an installer can act on.
        let body = response
            .bytes()
            .await
            .map_err(|source| FleetError::Unreachable {
                what,
                url: url.to_string(),
                source,
            })?;
        serde_json::from_slice(&body).map_err(|e| FleetError::Unreadable {
            what,
            url: url.to_string(),
            detail: format!(
                "{e} — the first bytes were {:?}",
                String::from_utf8_lossy(&body[..body.len().min(120)])
            ),
        })
    }
}

/// RFC 3339, which is what both services parse.
fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Percent-encode the characters a query string cannot carry literally.
///
/// Hand-written rather than a dependency, because exactly two values are ever
/// encoded here — an RFC 3339 instant and a location name — and the alternative
/// is a crate in the edge image for eleven lines.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            other => {
                // Two hex digits, without reaching for `format!` on every byte
                // of a string this is called on twice per re-plan.
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(char::from(HEX[usize::from(other >> 4)]));
                out.push(char::from(HEX[usize::from(other & 0x0f)]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured `tariffd` answer, exactly as the daemon serves one.
    const PRICES: &str = r#"{
        "points": [
            {"slot":"2026-01-15T00:00:00Z","price_ct":"8.42","source":"entsoe"},
            {"slot":"2026-01-15T00:15:00Z","price_ct":"-1.5","source":"entsoe"}
        ],
        "coverage": 0.5,
        "contiguous_until": "2026-01-15T00:15:00Z"
    }"#;

    /// And a captured `forecastd` one.
    const SKY: &str = r#"{
        "fetched_at": "2026-01-15T00:02:00Z",
        "published_minutes": 15,
        "points": [
            {"slot":"2026-01-15T11:00:00Z","ghi_w_per_m2":420.0,"temperature_c":3.5,"cloud_cover":0.2},
            {"slot":"2026-01-15T11:15:00Z","ghi_w_per_m2":380.0,"temperature_c":3.7}
        ]
    }"#;

    #[test]
    fn a_price_curve_survives_the_wire_as_an_exact_decimal() {
        // The whole reason the wire carries a string. A price that had been
        // through an `f64` is one nobody can reproduce a bill from, and a
        // negative quarter hour is exactly where the rounding would show up.
        let prices: Prices = serde_json::from_str(PRICES).expect("a captured answer");
        let spot = prices.spot();
        assert_eq!(spot.len(), 2);
        let values: Vec<String> = spot.values().map(ToString::to_string).collect();
        assert_eq!(values, vec!["8.42".to_string(), "-1.5".to_string()]);
    }

    #[test]
    fn a_partial_answer_is_read_as_partial_rather_than_as_zero() {
        // `tariffd` answers `200` with a coverage below one when it knows part
        // of the window: the caller asked what the service has. Reading the
        // absent slots as free electricity is the mistake
        // `SolveError::ForecastTooShort` exists to refuse one layer up, and it
        // starts here.
        let prices: Prices = serde_json::from_str(PRICES).expect("a captured answer");
        assert!((prices.coverage - 0.5).abs() < 1e-9);
        assert_eq!(prices.points.len(), 2);
        assert!(prices.contiguous_until.is_some());
    }

    #[test]
    fn the_sky_is_read_with_and_without_a_cloud_cover() {
        // ICON-D2 publishes cloud cover and some models do not, so the field is
        // optional on the wire. A missing one must not read as a clear sky.
        let sky: Sky = serde_json::from_str(SKY).expect("a captured answer");
        let by_slot = sky.by_slot();
        assert_eq!(by_slot.len(), 2);
        let first = by_slot.values().next().expect("a point");
        assert_eq!(first.cloud_cover, Some(0.2));
        let second = by_slot.values().nth(1).expect("a second point");
        assert_eq!(second.cloud_cover, None);
    }

    #[test]
    fn a_box_with_nothing_configured_asks_nobody() {
        // The offline case, and it has to be a `None` rather than an error: a
        // household on a fixed tariff has no day-ahead curve to ask for, and a
        // box with no WAN still keeps the house safe and lawful.
        let fleet = Fleet::new(&crate::config::FleetSettings::default()).expect("a client");
        assert!(!fleet.is_configured());
        assert!(!fleet.has_prices());
        assert!(!fleet.has_weather());
    }

    #[test]
    fn an_instant_and_a_location_are_encoded_for_a_query_string() {
        assert_eq!(
            urlencode("2026-01-15T00:00:00Z"),
            "2026-01-15T00%3A00%3A00Z"
        );
        assert_eq!(urlencode("berlin-mitte"), "berlin-mitte");
        assert_eq!(urlencode("Köln/Süd"), "K%C3%B6ln%2FS%C3%BCd");
    }
}
