//! Telling the fleet how the day went.
//!
//! One `POST` of one signed CloudEvent, and the two rules around it: it goes
//! inside TLS unless it is going nowhere, and it does not hang.
//!
//! What crosses this link is a household's day — what it consumed, when the car
//! charged, when nobody was in. That is personal data under the GDPR and
//! data-in-transit under the CRA (Annex I Part I (2)(e)), and the Standard
//! Webhooks signature does not stand in for TLS: a signature buys integrity and
//! says nothing about who is reading.

/// `POST` one signed CloudEvent to the fleet.
///
/// `reqwest` rather than a hand-written `POST`, so the box and the fleet share
/// one client and one TLS provider and cannot disagree about which cryptography
/// they trust (D84, D85).
///
/// # Errors
/// Anything `reqwest` could not do: a name that does not resolve, a refused
/// connection, a timeout, a certificate the box does not trust.
pub fn post_event(
    endpoint: &str,
    body: Vec<u8>,
    headers: &[(&'static str, String)],
) -> anyhow::Result<u16> {
    let mut request = reqwest::blocking::Client::builder()
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
    Ok(request.body(body).send()?.status().as_u16())
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
