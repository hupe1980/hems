//! Where the days come from.
//!
//! # The specialists read `obsd`'s summary, and they read it over the wire
//!
//! `obsd` holds the fleet's days and is the only thing that does. `agentd`
//! could have read the same database — two daemons on one schema — and that is
//! the shape this deliberately does not take: the scope predicate that keeps one
//! tenant's rows out of another tenant's answer lives in `obsd`'s store (D112),
//! and a second reader with its own `SELECT` is a second place for that rule to
//! be got wrong. So the boundary is `obsd`'s own `GET /v1/fleet`, authorised as
//! whatever credential `agentd` presents — which holds `hems.fleet.read` and
//! nothing that writes.
//!
//! # It reads the **summary**, not the days, and that is a bound
//!
//! [`hems_core::report::Summary`] is bounded by **findings**: every compliance
//! answer in it is already a list with a site and a date, because `obsd`'s whole
//! design is *lists, never rates*, and the rest are counts. So a fleet in good
//! order is a handful of numbers whatever its size. The rows would be
//! `keep_days × sites` documents — sixty days of ten thousand households is six
//! hundred thousand — with no bound on the body at all.
//!
//! The correlation stays here. `obsd` says *which* households breached and
//! *which* spent time on the fallback; whether those are the same `(site, date)`
//! pairs is the question no exact answer contains, and it is the agent's.

use hems_core::report::Summary;

/// What a specialist read, and where it came from.
///
/// Carried with the summary rather than assumed. A specialist's answer is about
/// a window of a fleet, and an operator reading a finding in March needs to know
/// which service said so and when — the run's journal records this envelope as
/// the input, so the question is answered by a replay rather than by an
/// argument.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Window {
    /// The service that answered, as a URL with no credential in it.
    pub source: String,
    /// When it was read.
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: time::OffsetDateTime,
    /// What it said.
    pub fleet: Summary,
}

/// Why a window could not be read.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The service could not be reached, or did not answer in time.
    #[error("{source_url} could not be reached: {detail}")]
    Unreachable {
        /// Which service.
        source_url: String,
        /// What the client said.
        detail: String,
    },
    /// It answered, and the answer was a refusal.
    ///
    /// Named apart from [`Self::Unreachable`] because the two send an operator
    /// to different places: a refusal is a credential this deployment has to fix
    /// and no amount of retrying changes it.
    #[error("{source_url} refused the read: {status}")]
    Refused {
        /// Which service.
        source_url: String,
        /// What it said.
        status: u16,
    },
    /// It answered with something this build cannot read.
    #[error("{source_url} answered with a body this build cannot read: {detail}")]
    Unreadable {
        /// Which service.
        source_url: String,
        /// What `serde` said.
        detail: String,
    },
}

impl UpstreamError {
    /// Whether coming back later could plausibly give a different answer.
    ///
    /// A refusal cannot: it is the same credential asking the same rule the same
    /// question. Everything else can, so the review loop retries on its own
    /// cadence rather than escalating a restarted `obsd` into a fault.
    #[must_use]
    pub const fn is_worth_retrying(&self) -> bool {
        !matches!(self, Self::Refused { .. })
    }
}

/// The fleet's days, from wherever they come from.
#[async_trait::async_trait]
pub trait Upstream: Send + Sync + 'static {
    /// Read the fleet as the upstream currently summarises it.
    ///
    /// The **window is the upstream's**, not this daemon's. `obsd`'s
    /// `keep_days` is what its retention sweep deletes outside and what its
    /// summary is computed over, so a consumer naming its own window could ask
    /// about days that were deleted and quote a denominator nothing stands
    /// behind.
    ///
    /// # Errors
    /// [`UpstreamError`].
    async fn window(&self) -> Result<Window, UpstreamError>;
}

/// `obsd` over HTTP.
pub struct Obsd {
    client: reqwest::Client,
    endpoint: String,
    token: String,
}

impl Obsd {
    /// A client for the `obsd` at `endpoint`, presenting `token`.
    ///
    /// # Errors
    /// [`hems_service::HttpError`] where the client cannot be built, or where
    /// `endpoint` would carry this daemon's **credential** across a network in
    /// the clear. That second check is the one this daemon was missing: the
    /// token it presents holds `hems.fleet.read`, which reads every household in
    /// the tenant, and it was being sent to whatever scheme was configured (D85).
    pub fn new(
        endpoint: impl Into<String>,
        token: impl Into<String>,
        http: &hems_service::HttpSettings,
        timeout: std::time::Duration,
    ) -> Result<Self, hems_service::HttpError> {
        let endpoint = endpoint.into().trim_end_matches('/').to_owned();
        hems_service::http::confidential(&endpoint)?;
        Ok(Self {
            client: hems_service::http::client(
                hems_service::identity!(),
                &http.clone().with_timeout_s(timeout.as_secs().max(1)),
            )?,
            endpoint,
            token: token.into(),
        })
    }

    /// The URL the summary is read from — no credential in it, so it is safe to
    /// journal and safe to log.
    #[must_use]
    pub fn url(&self) -> String {
        format!("{}/v1/fleet", self.endpoint)
    }
}

#[async_trait::async_trait]
impl Upstream for Obsd {
    async fn window(&self) -> Result<Window, UpstreamError> {
        let url = self.url();
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| UpstreamError::Unreachable {
                source_url: url.clone(),
                detail: e.to_string(),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(UpstreamError::Refused {
                source_url: url,
                status: status.as_u16(),
            });
        }
        // `Summary` is `hems-core`'s, not a mirror written here: the two ends
        // share the type rather than two people remembering the same field
        // names, which is the whole reason it moved out of `obsd`.
        let fleet: Summary = response
            .json()
            .await
            .map_err(|e| UpstreamError::Unreadable {
                source_url: url.clone(),
                detail: e.to_string(),
            })?;
        Ok(Window {
            source: url,
            // The instant this daemon read it, which is the only one it can
            // vouch for. `obsd` does not date its answer and should not: the
            // window is what it keeps, not when it was asked.
            fetched_at: time::OffsetDateTime::now_utc(),
            fleet,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_carries_no_credential() {
        // It is journaled as the source of every finding and logged on every
        // failure, so a token in it would be a token in the plan of record.
        let obsd = Obsd::new(
            "https://obsd.example/",
            "tok-secret",
            &hems_service::HttpSettings::default(),
            std::time::Duration::from_secs(5),
        )
        .expect("a client");
        assert_eq!(obsd.url(), "https://obsd.example/v1/fleet");
        assert!(!obsd.url().contains("tok-secret"));
    }

    #[test]
    fn a_plaintext_obsd_stops_the_daemon_rather_than_sending_the_credential() {
        // The token this daemon presents holds `hems.fleet.read` — every
        // household in the tenant. Sending it in the clear once is sending it
        // for ever, so the endpoint is checked at start-up and the daemon does
        // not come up (D85).
        assert!(matches!(
            Obsd::new(
                "http://obsd.example/",
                "tok-secret",
                &hems_service::HttpSettings::default(),
                std::time::Duration::from_secs(5),
            ),
            Err(hems_service::HttpError::NotConfidential { .. })
        ));
        // …and loopback is the exception, because that is a socket on the same
        // host and there is no network to read it off.
        assert!(
            Obsd::new(
                "http://127.0.0.1:7780",
                "tok-secret",
                &hems_service::HttpSettings::default(),
                std::time::Duration::from_secs(5),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_summary_with_fields_this_build_does_not_know_still_reads() {
        // `obsd` and `agentd` deploy independently, so one of them is newer. The
        // shared type is what stops a rename from being silent; what stops an
        // *addition* from being fatal is `serde(default)` on `Summary`, and this
        // is the assertion that says so — a fleet daemon that refused to read a
        // summary carrying one field it had not heard of could not be upgraded
        // in either order.
        let document = r#"{"sites":3,"days":90,"breached":[
            {"site":"haus-1","date":"2026-01-15","detail":"over a commanded ceiling"}
        ],"something_obsd_learned_later":42}"#;
        let summary: Summary = serde_json::from_str(document).expect("a summary");
        assert_eq!(summary.sites, 3);
        assert_eq!(summary.days, 90);
        assert_eq!(summary.breached.len(), 1);
        assert_eq!(
            summary.breached[0].date,
            time::macros::date!(2026 - 01 - 15)
        );
        assert!(
            summary.below_minimum.is_empty(),
            "a list the document did not carry is empty, not absent"
        );
    }

    #[test]
    fn a_refusal_is_not_worth_retrying_and_everything_else_is() {
        // The distinction the review loop acts on: a refused credential is a
        // deployment to fix, and a restarted `obsd` is a minute to wait.
        assert!(
            !UpstreamError::Refused {
                source_url: "u".into(),
                status: 403
            }
            .is_worth_retrying()
        );
        assert!(
            UpstreamError::Unreachable {
                source_url: "u".into(),
                detail: "connection refused".into()
            }
            .is_worth_retrying()
        );
    }
}
