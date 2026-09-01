//! Where the weather comes from, and the seam that keeps it out of the tests.

use hems_forecast::{WeatherSeries, weather::WeatherError};
use thiserror::Error;

/// Why a weather run did not arrive.
#[derive(Debug, Error)]
pub enum UpstreamError {
    /// The request failed — DNS, TLS, a refused connection, a timeout.
    #[error("{location}: the request failed: {detail}")]
    Transport {
        /// Which location was asked about.
        location: String,
        /// What went wrong.
        detail: String,
    },
    /// The request succeeded and the answer was not a success.
    #[error("{location}: HTTP {status}")]
    Status {
        /// Which location.
        location: String,
        /// The status code.
        status: u16,
    },
    /// The body arrived and could not be read.
    #[error("{location}: {source}")]
    Unreadable {
        /// Which location.
        location: String,
        /// What the parser said.
        #[source]
        source: WeatherError,
    },
}

/// Something that can be asked for a weather run.
pub trait Upstream: Send + Sync {
    /// Fetch the run for `location`.
    ///
    /// # Errors
    /// [`UpstreamError`] for a transport failure, a non-success status or a body
    /// the parser refuses.
    fn fetch(
        &self,
        location: &crate::Location,
    ) -> impl std::future::Future<Output = Result<WeatherSeries, UpstreamError>> + Send;
}

/// The production upstream: `reqwest` over rustls.
#[derive(Debug, Clone)]
pub struct Http {
    client: reqwest::Client,
    endpoint: String,
}

impl Http {
    /// A client for `endpoint`.
    ///
    /// # Errors
    /// When the HTTP client cannot be built at all.
    pub fn new(endpoint: String, timeout: std::time::Duration) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .user_agent(concat!("hems-forecastd/", env!("CARGO_PKG_VERSION")))
                .build()?,
            endpoint,
        })
    }

    /// The URL for one location.
    ///
    /// The coordinates are appended rather than templated in, because the
    /// endpoint already carries the variables and the time format and an
    /// operator pointing this at their own mirror should not have to know the
    /// order of the query string.
    #[must_use]
    pub fn url_for(&self, location: &crate::Location) -> String {
        let separator = if self.endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        format!(
            "{}{separator}latitude={}&longitude={}",
            self.endpoint, location.latitude, location.longitude
        )
    }
}

impl Upstream for Http {
    async fn fetch(&self, location: &crate::Location) -> Result<WeatherSeries, UpstreamError> {
        let response = self
            .client
            .get(self.url_for(location))
            .send()
            .await
            .map_err(|e| UpstreamError::Transport {
                location: location.id.clone(),
                detail: e.to_string(),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(UpstreamError::Status {
                location: location.id.clone(),
                status: status.as_u16(),
            });
        }
        let body = response
            .text()
            .await
            .map_err(|e| UpstreamError::Transport {
                location: location.id.clone(),
                detail: e.to_string(),
            })?;
        hems_forecast::open_meteo(&body).map_err(|source| UpstreamError::Unreadable {
            location: location.id.clone(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_coordinates_are_appended_to_whatever_endpoint_was_configured() {
        let http = Http::new(
            crate::config::DEFAULT_ENDPOINT.into(),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let url = http.url_for(&crate::Location {
            id: "berlin".into(),
            latitude: 52.5,
            longitude: 13.4,
            altitude_m: 34.0,
        });
        assert!(url.contains("timeformat=unixtime"), "{url}");
        assert!(url.contains("latitude=52.5"), "{url}");
        assert!(url.ends_with("&longitude=13.4"), "{url}");
    }

    #[test]
    fn a_mirror_with_no_query_string_gets_a_question_mark() {
        let http = Http::new(
            "https://weather.local/v1".into(),
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let url = http.url_for(&crate::Location {
            id: "berlin".into(),
            latitude: 52.5,
            longitude: 13.4,
            altitude_m: 0.0,
        });
        assert_eq!(url, "https://weather.local/v1?latitude=52.5&longitude=13.4");
    }
}
