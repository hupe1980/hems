//! Standard Webhooks: whether the fleet should believe a box.
//!
//! # What it is protecting
//!
//! A fleet view is not a dashboard nobody acts on — it is the list of households
//! that did **not** respect a network operator's reduction. An unauthenticated
//! write to it can make a compliant site look like a breach, or a breach look
//! like nothing.
//!
//! A bearer token proves who is asking, for as long as nobody on the path keeps
//! a copy. A signature proves something a token cannot: that **this document**
//! is the one the box sent. The fleet uses both.
//!
//! # The three things that are signed
//!
//! ```text
//! signed content = <message id> . <unix timestamp> . <the exact body bytes>
//! webhook-signature: v1,<base64 of HMAC-SHA256(secret, signed content)>
//! ```
//!
//! The **id** so a replay cannot be re-attributed, the **timestamp** so a replay
//! is bounded, the **body bytes** so the document cannot be edited. Signing the
//! body alone leaves a captured request valid for ever, which makes re-sending
//! yesterday's breach every hour a supported operation.
//!
//! `v1` is a version prefix and a header may carry several space-separated
//! signatures — that is the rotation story. [`verify`] takes a slice of secrets
//! and tries **all** of them rather than stopping at the first match, so how
//! long verification takes says nothing about which one was live.
//!
//! # Sans-I/O
//!
//! `now` is a parameter, so an expired signature, one from the future and a good
//! one are three unit tests rather than three waits.

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use time::{Duration, OffsetDateTime};

/// The header carrying the message id — also the CloudEvent's `id`.
pub const ID_HEADER: &str = "webhook-id";
/// The header carrying the signing timestamp, Unix seconds.
pub const TIMESTAMP_HEADER: &str = "webhook-timestamp";
/// The header carrying the signature or signatures.
pub const SIGNATURE_HEADER: &str = "webhook-signature";

/// The scheme prefix of a signature this build produces and accepts.
pub const SCHEME: &str = "v1";

/// How far a signing timestamp may be from the receiver's clock.
///
/// Five minutes each way, which is Standard Webhooks' own recommendation and
/// comfortably more than two machines that both run NTP will ever differ by. It
/// is a **replay** bound rather than a clock-skew allowance: the narrower it is,
/// the shorter a captured request stays useful.
pub const DEFAULT_TOLERANCE: Duration = Duration::minutes(5);

/// Why a signature was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// A required header is absent.
    #[error("the request carries no {0}")]
    MissingHeader(&'static str),
    /// The timestamp header is not Unix seconds.
    #[error("{0:?} is not a Unix timestamp")]
    MalformedTimestamp(String),
    /// The timestamp is outside [`DEFAULT_TOLERANCE`] of the receiver's clock.
    ///
    /// Both directions are this one error. A request from the future and a
    /// request from an hour ago are the same decision — do not accept it — and
    /// an error that distinguishes them tells a caller how far off it has to
    /// move to get in.
    #[error("the signature is timestamped {skew_s} s from now, and the window is ±{window_s} s")]
    OutsideWindow {
        /// How far off, signed: negative is in the past.
        skew_s: i64,
        /// The tolerance, seconds.
        window_s: i64,
    },
    /// No secret was configured, so nothing could be verified.
    ///
    /// An error rather than an acceptance. A receiver with no secret is a
    /// receiver that is not checking, and the failure mode of "accept when
    /// unconfigured" is a fleet that has been open since the day somebody
    /// deployed it without the environment variable.
    #[error("this service has no webhook secret, so it cannot accept a signed report")]
    NoSecret,
    /// The signature header holds nothing this build can check.
    #[error("the signature header carries no {SCHEME} signature")]
    NoSupportedScheme,
    /// The signature does not match under any configured secret.
    #[error("the signature does not match")]
    NotOurs,
}

/// The headers a signed request carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The message id, which is the CloudEvent's `id`.
    pub id: String,
    /// The signing timestamp, Unix seconds.
    pub timestamp: i64,
    /// `v1,<base64>`.
    pub signature: String,
}

impl Signed {
    /// The three headers, ready to be written onto a request.
    #[must_use]
    pub fn headers(&self) -> [(&'static str, String); 3] {
        [
            (ID_HEADER, self.id.clone()),
            (TIMESTAMP_HEADER, self.timestamp.to_string()),
            (SIGNATURE_HEADER, self.signature.clone()),
        ]
    }
}

/// Sign `body` as message `id` at `at`.
///
/// `id` should be the CloudEvent's own `id`, so the thing a receiver
/// de-duplicates on is the thing the signature covers.
#[must_use]
pub fn sign(secret: &[u8], id: &str, at: OffsetDateTime, body: &[u8]) -> Signed {
    let timestamp = at.unix_timestamp();
    let mac = tag(secret, id, timestamp, body).finalize().into_bytes();
    Signed {
        id: id.to_owned(),
        timestamp,
        signature: format!(
            "{SCHEME},{}",
            base64::engine::general_purpose::STANDARD.encode(mac)
        ),
    }
}

/// Which of `secrets` signed `body`, if any signed it recently enough.
///
/// Returns the **index** into `secrets`, and the caller is expected to use it.
/// A signature proves the bytes were not edited by somebody without the key; it
/// proves nothing about *who* sent them unless each sender has a key of its own.
/// A receiver that holds one fleet-wide secret and takes the sender's identity
/// from the payload will believe whatever any holder of that secret writes —
/// which, for `obsd`, is the named list of households that failed to respect a
/// network operator's reduction.
///
/// So the index comes back, the caller maps it to an identity, and a report
/// whose claimed site is not the site whose key signed it is refused (D114).
///
/// # Errors
/// [`WebhookError`], one variant per way it can fail, because "no secret
/// configured", "too old" and "wrong signature" are three different operational
/// problems.
pub fn verify(
    secrets: &[impl AsRef<[u8]>],
    id: &str,
    timestamp_header: &str,
    signature_header: &str,
    body: &[u8],
    now: OffsetDateTime,
    tolerance: Duration,
) -> Result<usize, WebhookError> {
    if secrets.is_empty() {
        return Err(WebhookError::NoSecret);
    }
    let timestamp: i64 = timestamp_header
        .trim()
        .parse()
        .map_err(|_| WebhookError::MalformedTimestamp(timestamp_header.to_owned()))?;
    let skew = now.unix_timestamp() - timestamp;
    let window = tolerance.whole_seconds().abs();
    if skew.abs() > window {
        return Err(WebhookError::OutsideWindow {
            skew_s: -skew,
            window_s: window,
        });
    }

    // Every `v1,` signature in the header, and every configured secret. Both
    // loops run to the end rather than returning on the first match: a check
    // that short-circuits tells an attacker, in the time it took, which half of
    // a rotation pair was the live one.
    let offered: Vec<&str> = signature_header
        .split_whitespace()
        .filter_map(|part| part.strip_prefix(SCHEME).and_then(|r| r.strip_prefix(',')))
        .collect();
    if offered.is_empty() {
        return Err(WebhookError::NoSupportedScheme);
    }

    // The **last** secret that verified, not the first, so the fold runs to the
    // end whatever matches. `Option` rather than a `bool` because the caller
    // needs to know *which* one: a shared secret authenticates the bytes and
    // says nothing about the sender, and only the caller can map a key back to
    // an identity.
    let mut matched = None;
    for (index, secret) in secrets.iter().enumerate() {
        for candidate in &offered {
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(candidate.trim())
            else {
                continue;
            };
            // `verify_slice` is the comparison, and it is `hmac`'s rather than
            // one written here: a signature check compared with `==` leaks where
            // the two first differ, and that is a whole class of bug this
            // workspace should not be re-deriving.
            if tag(secret.as_ref(), id, timestamp, body)
                .verify_slice(&bytes)
                .is_ok()
            {
                matched = Some(index);
            }
        }
    }
    matched.ok_or(WebhookError::NotOurs)
}

/// `HMAC-SHA256(secret, "<id>.<timestamp>.<body>")`, not yet finalised.
fn tag(secret: &[u8], id: &str, timestamp: i64, body: &[u8]) -> Hmac<Sha256> {
    // `new_from_slice` only fails for a key length the algorithm cannot take,
    // and HMAC takes a key of any length — `hmac`'s own implementation of the
    // trait says exactly that.
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    mac
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-16 00:05:00 UTC);
    const SECRET: &[u8] = b"whsec_a-secret-the-fleet-and-the-box-share";
    const BODY: &[u8] = br#"{"specversion":"1.0","data":{"saved_eur":2.09}}"#;

    fn signed() -> Signed {
        sign(SECRET, "msg-1", NOW, BODY)
    }

    fn check(s: &Signed, body: &[u8], now: OffsetDateTime) -> Result<usize, WebhookError> {
        verify(
            &[SECRET],
            &s.id,
            &s.timestamp.to_string(),
            &s.signature,
            body,
            now,
            DEFAULT_TOLERANCE,
        )
    }

    #[test]
    fn what_the_box_signed_is_what_the_fleet_accepts() {
        assert!(check(&signed(), BODY, NOW).is_ok());
    }

    #[test]
    fn the_signature_is_the_scheme_and_base64_and_nothing_else() {
        let s = signed();
        let rest = s.signature.strip_prefix("v1,").expect("the v1 prefix");
        let raw = base64::engine::general_purpose::STANDARD
            .decode(rest)
            .expect("base64");
        assert_eq!(raw.len(), 32, "HMAC-SHA256 is 32 bytes");
    }

    #[test]
    fn a_body_edited_after_signing_is_refused() {
        // The whole point: a proxy, a fleet server or anything on the path that
        // rewrites the day the household actually had.
        let edited = br#"{"specversion":"1.0","data":{"saved_eur":9.99}}"#;
        assert_eq!(check(&signed(), edited, NOW), Err(WebhookError::NotOurs));
    }

    #[test]
    fn another_secret_does_not_verify() {
        let s = signed();
        assert_eq!(
            verify(
                &[b"whsec_somebody-elses".as_slice()],
                &s.id,
                &s.timestamp.to_string(),
                &s.signature,
                BODY,
                NOW,
                DEFAULT_TOLERANCE,
            ),
            Err(WebhookError::NotOurs)
        );
    }

    #[test]
    fn a_captured_request_stops_working() {
        // Replay: the bytes and the signature are exactly what the box sent, and
        // six minutes later they are no longer good enough. Without the
        // timestamp in the signed content this test cannot be written at all.
        let s = signed();
        assert!(check(&s, BODY, NOW + Duration::minutes(4)).is_ok());
        assert!(matches!(
            check(&s, BODY, NOW + Duration::minutes(6)),
            Err(WebhookError::OutsideWindow { .. })
        ));
    }

    #[test]
    fn a_signature_from_the_future_is_the_same_answer() {
        let s = signed();
        assert!(matches!(
            check(&s, BODY, NOW - Duration::minutes(6)),
            Err(WebhookError::OutsideWindow { skew_s, .. }) if skew_s > 0
        ));
    }

    #[test]
    fn the_id_is_covered_so_a_replay_cannot_be_re_attributed() {
        let s = signed();
        assert_eq!(
            verify(
                &[SECRET],
                "msg-2",
                &s.timestamp.to_string(),
                &s.signature,
                BODY,
                NOW,
                DEFAULT_TOLERANCE,
            ),
            Err(WebhookError::NotOurs)
        );
    }

    #[test]
    fn both_secrets_of_a_rotation_are_accepted() {
        let old = b"whsec_the-old-one".as_slice();
        let new = b"whsec_the-new-one".as_slice();
        let by_old = sign(old, "msg-1", NOW, BODY);
        let by_new = sign(new, "msg-1", NOW, BODY);
        for s in [&by_old, &by_new] {
            assert!(
                verify(
                    &[old, new],
                    &s.id,
                    &s.timestamp.to_string(),
                    &s.signature,
                    BODY,
                    NOW,
                    DEFAULT_TOLERANCE,
                )
                .is_ok(),
                "a signature made during a rotation window has to keep working"
            );
        }
    }

    #[test]
    fn a_receiver_with_no_secret_refuses_rather_than_accepts() {
        // The failure this module exists to remove, stated as a test: an
        // unconfigured receiver must not be an open one.
        let s = signed();
        assert_eq!(
            verify(
                &[] as &[&[u8]],
                &s.id,
                &s.timestamp.to_string(),
                &s.signature,
                BODY,
                NOW,
                DEFAULT_TOLERANCE,
            ),
            Err(WebhookError::NoSecret)
        );
    }

    #[test]
    fn a_header_this_build_cannot_read_says_so() {
        let s = signed();
        assert_eq!(
            verify(
                &[SECRET],
                &s.id,
                &s.timestamp.to_string(),
                "v2,ZGVmaW5pdGVseS1ub3QtaXQ=",
                BODY,
                NOW,
                DEFAULT_TOLERANCE,
            ),
            Err(WebhookError::NoSupportedScheme)
        );
    }

    #[test]
    fn a_timestamp_that_is_not_a_number_is_not_a_signature_failure() {
        let s = signed();
        assert!(matches!(
            verify(
                &[SECRET],
                &s.id,
                "yesterday",
                &s.signature,
                BODY,
                NOW,
                DEFAULT_TOLERANCE,
            ),
            Err(WebhookError::MalformedTimestamp(_))
        ));
    }

    #[test]
    fn the_headers_are_the_three_the_specification_names() {
        let names: Vec<&str> = signed().headers().iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            ["webhook-id", "webhook-timestamp", "webhook-signature"]
        );
    }

    /// RFC 4231 test case 2, so the construction underneath is the one every
    /// other implementation of Standard Webhooks computes.
    ///
    /// It is a property of `hmac` rather than of this module, and it is here
    /// because "our signature verifies against our own verifier" is a test that
    /// passes for two wrong implementations as readily as for one right one.
    #[test]
    fn the_underlying_mac_is_hmac_sha256() {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            hex_of(&mac.finalize().into_bytes()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod which_key_signed_it {
    use super::*;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 12:00:00 UTC);

    #[test]
    fn verification_says_which_secret_it_was() {
        // The property `obsd` needs and could not ask for: a receiver holding one
        // key per sender learns *who* signed, not merely that somebody with a
        // key did. Without it, a fleet-wide secret lets any holder attribute a
        // report to any household.
        let secrets = [b"whsec-haus-1".as_slice(), b"whsec-haus-2".as_slice()];
        let signed = sign(secrets[1], "haus-2:2026-01-15", NOW, b"{}");

        let who = verify(
            &secrets,
            &signed.id,
            &signed.timestamp.to_string(),
            &signed.signature,
            b"{}",
            NOW,
            DEFAULT_TOLERANCE,
        )
        .expect("one of them signed it");
        assert_eq!(who, 1, "and it was the second");
    }

    #[test]
    fn a_key_nobody_holds_verifies_nothing() {
        let signed = sign(b"whsec-outsider", "haus-1:2026-01-15", NOW, b"{}");
        assert!(matches!(
            verify(
                &[b"whsec-haus-1".as_slice()],
                &signed.id,
                &signed.timestamp.to_string(),
                &signed.signature,
                b"{}",
                NOW,
                DEFAULT_TOLERANCE,
            ),
            Err(WebhookError::NotOurs)
        ));
    }
}
