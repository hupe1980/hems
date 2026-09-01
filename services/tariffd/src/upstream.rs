//! Where a price list comes from, and the seam that keeps it out of the tests.

use std::collections::BTreeMap;

use hems_tariff::source::{PriceSeries, Source, SourceError};
use thiserror::Error;

/// Why an upstream did not produce a price list.
///
/// The field naming the source is `feed` rather than `source`, and that is not
/// a style choice: `thiserror` reads a field called `source` as the *cause* of
/// an error and requires it to be one, so an enum whose variants each name a
/// price source does not compile until the two meanings are told apart.
#[derive(Debug, Error)]
pub enum UpstreamError {
    /// The request itself failed — DNS, TLS, a refused connection, a timeout.
    #[error("{feed:?}: the request failed: {detail}")]
    Transport {
        /// Which price source.
        feed: Source,
        /// What went wrong.
        detail: String,
    },
    /// The request succeeded and the answer was not a success.
    #[error("{feed:?}: HTTP {status}")]
    Status {
        /// Which price source.
        feed: Source,
        /// The status code.
        status: u16,
    },
    /// The answer arrived and could not be parsed.
    #[error("{feed:?}: {source}")]
    Unparseable {
        /// Which price source.
        feed: Source,
        /// What the parser said.
        #[source]
        source: SourceError,
    },
}

impl UpstreamError {
    /// Which price source this is about.
    #[must_use]
    pub const fn feed(&self) -> Source {
        match self {
            Self::Transport { feed, .. }
            | Self::Status { feed, .. }
            | Self::Unparseable { feed, .. } => *feed,
        }
    }
}

/// One source's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    /// The prices.
    pub series: PriceSeries,
    /// The carbon intensity, where the source publishes one instead of a price.
    pub co2_g_per_kwh: BTreeMap<hems_core::prelude::Slot, f64>,
}

/// Something that can be asked for a day-ahead curve.
///
/// The whole point of the trait: production passes [`Http`], every test passes a
/// table of captured responses, and the daemon around it is the same code.
pub trait Upstream: Send + Sync {
    /// Ask `source` for its current publication.
    ///
    /// # Errors
    /// [`UpstreamError`] for a transport failure, a non-success status or a body
    /// the parser refuses.
    fn fetch(
        &self,
        source: Source,
    ) -> impl std::future::Future<Output = Result<Fetched, UpstreamError>> + Send;
}

/// The production upstream: `reqwest` over rustls.
#[derive(Debug, Clone)]
pub struct Http {
    client: reqwest::Client,
    endpoints: BTreeMap<Source, crate::config::Endpoint>,
}

impl Http {
    /// A client for the configured endpoints.
    ///
    /// # Errors
    /// When the HTTP client cannot be built at all — a broken TLS backend,
    /// which is a deployment fault rather than a runtime one.
    pub fn new(
        endpoints: BTreeMap<Source, crate::config::Endpoint>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                // A published API is somebody else's server, and a fleet of
                // boxes asking it questions is a fleet that can knock it over.
                // Naming ourselves is the minimum courtesy and the thing that
                // gets an operator a mail rather than a block.
                .user_agent(concat!("hems-tariffd/", env!("CARGO_PKG_VERSION")))
                .build()?,
            endpoints,
        })
    }
}

impl Upstream for Http {
    async fn fetch(&self, source: Source) -> Result<Fetched, UpstreamError> {
        let endpoint = self
            .endpoints
            .get(&source)
            .ok_or_else(|| UpstreamError::Transport {
                feed: source,
                detail: "no endpoint configured".into(),
            })?;

        let mut request = self.client.get(&endpoint.url);
        for (name, value) in &endpoint.headers {
            // Resolved at startup by `main`, so this is the token itself.
            request = request.header(name, value.expose());
        }
        let response = request.send().await.map_err(|e| UpstreamError::Transport {
            feed: source,
            detail: e.to_string(),
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(UpstreamError::Status {
                feed: source,
                status: status.as_u16(),
            });
        }
        let body = response
            .text()
            .await
            .map_err(|e| UpstreamError::Transport {
                feed: source,
                detail: e.to_string(),
            })?;
        parse(source, &body)
    }
}

/// Turn a body into a [`Fetched`], through `hems-tariff`'s own parsers.
///
/// Public because it is the whole of what a captured response has to go through
/// in a test, and a test that re-implemented the dispatch would be testing a
/// second copy of it.
///
/// # Errors
/// [`UpstreamError::Unparseable`] when the parser refuses the body.
pub fn parse(source: Source, body: &str) -> Result<Fetched, UpstreamError> {
    let unparseable = |cause| UpstreamError::Unparseable {
        feed: source,
        source: cause,
    };
    match source {
        Source::Entsoe => Ok(Fetched {
            series: hems_tariff::source::entsoe_a44(body).map_err(unparseable)?,
            co2_g_per_kwh: BTreeMap::new(),
        }),
        Source::Smard => Ok(Fetched {
            series: hems_tariff::source::smard(body).map_err(unparseable)?,
            co2_g_per_kwh: BTreeMap::new(),
        }),
        Source::Awattar => Ok(Fetched {
            series: hems_tariff::source::awattar(body).map_err(unparseable)?,
            co2_g_per_kwh: BTreeMap::new(),
        }),
        Source::Tibber => Ok(Fetched {
            series: hems_tariff::source::tibber(body).map_err(unparseable)?,
            co2_g_per_kwh: BTreeMap::new(),
        }),
        // Energy-Charts publishes carbon rather than price, so it contributes a
        // series with no points and a carbon map — which is why `Fetched` has
        // both and neither is an `Option`.
        Source::EnergyCharts => Ok(Fetched {
            series: PriceSeries {
                points: BTreeMap::new(),
                source,
                basis: hems_tariff::source::PriceBasis::Wholesale,
                published_minutes: 15,
            },
            co2_g_per_kwh: hems_tariff::source::energy_charts_co2(body).map_err(unparseable)?,
        }),
    }
}
