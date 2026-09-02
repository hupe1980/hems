//! Telling the fleet how the day went.
//!
//! One signed CloudEvent, and the three rules around it: it goes inside TLS
//! unless it is going nowhere, it does not hang, and a failure that could
//! succeed later is not a lost day.
//!
//! What crosses this link is a household's day — what it consumed, when the car
//! charged, when nobody was in. That is personal data under the GDPR and
//! data-in-transit under the CRA (Annex I Part I (2)(e)), and the Standard
//! Webhooks signature does not stand in for TLS: a signature buys integrity and
//! says nothing about who is reading.
//!
//! # Transient and permanent are different failures
//!
//! A `5xx`, a `429` and a refused connection are the fleet being unavailable,
//! and the day is worth keeping until it is not. A `4xx` that is not a rate
//! limit is `obsd` having **read** the document and refused it — a signature it
//! cannot verify, a body it cannot parse, a site it does not know — and no
//! number of retries changes any of those. Treating the two alike gives either
//! a box that discards a day because a service restarted, or a box that asks
//! the same rejected question every five minutes for ever.
//!
//! # The signature is made at the attempt, never stored
//!
//! Standard Webhooks signs `id . timestamp . body`, and a receiver refuses a
//! timestamp outside five minutes. So a signature made when a day was queued is
//! worthless by the time a box back from an overnight outage sends it. What is
//! kept is the **body**; each attempt signs it again. The `webhook-id` stays the
//! same across every attempt, because it is derived from the site and the date
//! and is what the receiver deduplicates on.

/// Why one delivery attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// The fleet is unavailable: a `5xx`, a `429`, or no answer at all.
    ///
    /// Worth keeping the document for.
    #[error("the fleet did not take it, and might later: {0}")]
    Transient(String),
    /// The fleet read the document and refused it.
    ///
    /// A `4xx` that is not a rate limit. Retrying asks the same rejected
    /// question again, so the caller should stop rather than back off.
    #[error("the fleet refused it, and will refuse it again: {0}")]
    Permanent(String),
}

impl DeliveryError {
    /// Whether trying again could ever work.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }

    /// Classify what a receiver answered.
    ///
    /// `429` is transient although it is a `4xx`: it is the one status that
    /// asks to be asked again.
    fn from_status(status: u16) -> Self {
        if status == 429 || (500..600).contains(&status) {
            Self::Transient(format!("HTTP {status}"))
        } else {
            Self::Permanent(format!("HTTP {status}"))
        }
    }
}

/// `POST` one signed CloudEvent to the fleet.
///
/// `async`, and that is not a style choice. `hemsd`'s `main` is `#[tokio::main]`,
/// so every command already runs inside a runtime — and `reqwest::blocking`
/// builds a runtime of its own, which **panics on drop** inside an asynchronous
/// context. This was that panic: `hemsd simulate --report-to` reported the day
/// and then died before the process could exit cleanly, and `just fleet-demo`
/// printed a summary with no sites in it (D115).
///
/// # Errors
/// Anything `reqwest` could not do: a name that does not resolve, a refused
/// connection, a timeout, a certificate the box does not trust.
pub async fn post_event(
    endpoint: &str,
    body: Vec<u8>,
    headers: &[(&'static str, String)],
) -> anyhow::Result<u16> {
    let mut request = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("hemsd/", env!("CARGO_PKG_VERSION")))
        .build()?
        .post(endpoint)
        // CloudEvents structured mode: the whole event is the body, so the media
        // type is the event's rather than the payload's.
        .header("content-type", "application/cloudevents+json");
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    Ok(request.body(body).send().await?.status().as_u16())
}

/// Whether an endpoint keeps a household's day confidential in transit.
///
/// `https` anywhere, plain `http` only to a loopback address. Anything else is
/// refused rather than warned about: a warning on a box nobody is watching is a
/// warning nobody reads.
///
/// # Errors
/// When `endpoint` is not a URL, or would send the day across a network in the
/// clear.
pub fn is_confidential(endpoint: &str) -> anyhow::Result<()> {
    let url: reqwest::Url = endpoint.parse()?;
    if url.scheme() == "https" {
        return Ok(());
    }
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        loopback,
        "{endpoint} would send this household's day across a network in the clear; \
         use https, or report to a loopback address"
    );
    Ok(())
}

/// Sign `body` and `POST` it, once, classifying whatever comes back.
///
/// `now` is a parameter, so "a signature this receiver will refuse as stale" is
/// a test rather than a wait.
///
/// # Errors
/// [`DeliveryError`], classified as above.
pub async fn deliver(
    client: &reqwest::Client,
    endpoint: &str,
    secret: &[u8],
    event_id: &str,
    body: &[u8],
    now: time::OffsetDateTime,
) -> Result<(), DeliveryError> {
    let signature = hems_events::webhook::sign(secret, event_id, now, body);
    let mut request = client
        .post(endpoint)
        // CloudEvents structured mode: the whole event is the body, so the media
        // type is the event's rather than the payload's.
        .header("content-type", "application/cloudevents+json");
    for (name, value) in signature.headers() {
        request = request.header(name, value);
    }
    let response = request
        .body(body.to_vec())
        .send()
        .await
        // No answer at all: a name that does not resolve, a refused connection,
        // a timeout. Every one of them is the fleet being unreachable rather
        // than the document being wrong.
        .map_err(|e| DeliveryError::Transient(e.to_string()))?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(DeliveryError::from_status(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_limit_is_the_one_client_error_worth_retrying() {
        // Everything else in the 4xx range means the receiver read the document
        // and will not have it, and a box that retried those would ask the same
        // rejected question every five minutes for the life of the household.
        assert!(DeliveryError::from_status(429).is_transient());
        assert!(DeliveryError::from_status(503).is_transient());
        assert!(DeliveryError::from_status(500).is_transient());

        assert!(!DeliveryError::from_status(400).is_transient());
        assert!(!DeliveryError::from_status(401).is_transient());
        assert!(!DeliveryError::from_status(404).is_transient());
        assert!(!DeliveryError::from_status(422).is_transient());
    }

    #[test]
    fn a_day_never_leaves_a_box_in_the_clear() {
        assert!(is_confidential("https://fleet.example/v1/days").is_ok());
        assert!(is_confidential("http://127.0.0.1:8080/v1/days").is_ok());
        assert!(is_confidential("http://localhost:8080/v1/days").is_ok());
        assert!(is_confidential("http://[::1]:8080/v1/days").is_ok());

        assert!(
            is_confidential("http://fleet.example/v1/days").is_err(),
            "a household's day is personal data, and plain http across a \
             network publishes it"
        );
    }
}
