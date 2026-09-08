//! The one place an outbound HTTP client is built.
//!
//! A daemon that reaches out has to decide **who it trusts** and **whether the
//! link is confidential**. Both live here, so a daemon gets them by calling
//! [`client`] rather than by remembering — the same rule the health surface and
//! the MCP mount already follow.
//!
//! # Confidentiality
//!
//! `https` anywhere, plain `http` only to a loopback address, **refused** rather
//! than warned about (D85): a warning on a box nobody is watching is a warning
//! nobody reads. [`confidential`] is called on every configured endpoint at
//! start-up, so a deployment that would send a household's day — or a credential
//! that reads every household's — across a network in the clear does not come up.
//!
//! # Trust anchors
//!
//! `reqwest` 0.13 verifies with `rustls-platform-verifier` and offers no way to
//! install a compiled-in root list, so where the anchors come from is a
//! deployment's decision and [`TlsRoots`] is where it is written down (D171).
//! [`TlsRoots::Platform`] suits a daemon calling the open web and makes the
//! image's trust store a packaging requirement; [`TlsRoots::Pinned`] suits a box
//! that talks only to its own fleet, and *replaces* the platform roots rather
//! than adding to them.

use std::path::PathBuf;
use std::time::Duration;

/// Why an outbound client could not be built, or a URL could not be used.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The endpoint is not a URL at all.
    #[error("{endpoint:?} is not a URL: {detail}")]
    NotAUrl {
        /// What was configured.
        endpoint: String,
        /// What the parser said.
        detail: String,
    },
    /// The endpoint would carry a household's data, or a credential, in the
    /// clear.
    ///
    /// Refused rather than warned about: a warning on a box nobody is watching
    /// is a warning nobody reads (D85).
    #[error(
        "{endpoint:?} is not confidential — `https` anywhere, plain `http` only to a \
         loopback address, and this is neither"
    )]
    NotConfidential {
        /// What was configured.
        endpoint: String,
    },
    /// The pinned bundle could not be read.
    #[error("the trust anchors at {path} could not be read: {detail}")]
    NoTrustAnchors {
        /// Where they were expected.
        path: String,
        /// What went wrong.
        detail: String,
    },
    /// The client itself could not be built.
    #[error("the HTTP client could not be built: {0}")]
    Client(String),
}

/// Where an outbound client's trust anchors come from.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "from")]
pub enum TlsRoots {
    /// The platform's own trust store, which is what `reqwest` does.
    ///
    /// Right for a daemon calling the open web, where the public root store is
    /// the question being asked and updates with the operating system rather
    /// than with the firmware. **The image has to carry one**: a minimal
    /// container with no `/etc/ssl/certs` gives the verifier nothing, and every
    /// request fails at the handshake — which CI, on a host that has a store,
    /// cannot reproduce.
    #[default]
    Platform,
    /// Only the certificates in a PEM bundle.
    ///
    /// `reqwest`'s `tls_certs_only`, which **replaces** the platform roots
    /// rather than adding to them — the distinction between a control and a
    /// decoration. Right for a box that talks only to its own fleet: a private
    /// PKI relationship verified against every public CA is weaker than anybody
    /// means by it.
    Pinned {
        /// The PEM bundle, as a path on the box.
        bundle: PathBuf,
    },
}

/// How a daemon's outbound calls are made.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpSettings {
    /// How long to wait for the connection itself, seconds.
    pub connect_timeout_s: u64,
    /// How long to wait for the whole request, seconds.
    pub timeout_s: u64,
    /// How many idle connections to keep per host.
    ///
    /// Two. A box on a household connection asks `tariffd` and `forecastd` a
    /// question every five minutes, so keeping the connection open costs one
    /// socket each and saves a TLS handshake on a link that is often slow. Zero
    /// turns pooling off, which is what a daemon making one call an hour wants.
    pub pool_max_idle_per_host: usize,
    /// Where the trust anchors come from. See the module note.
    pub tls_roots: TlsRoots,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            connect_timeout_s: 10,
            timeout_s: 30,
            pool_max_idle_per_host: 2,
            tls_roots: TlsRoots::Platform,
        }
    }
}

impl HttpSettings {
    /// The same, with a different overall timeout.
    ///
    /// A convenience for a caller whose one call is slower than the shell's
    /// default — a fleet-sized read, a settlement export — so that overriding
    /// one number does not mean restating the trust decision beside it.
    #[must_use]
    pub const fn with_timeout_s(mut self, seconds: u64) -> Self {
        self.timeout_s = seconds;
        self
    }
}

/// Build the outbound client for `identity`.
///
/// # Errors
/// [`HttpError::NoTrustAnchors`] where a pinned bundle cannot be read or holds
/// no certificate, and [`HttpError::Client`] where `reqwest` refuses to build.
///
/// A bundle that exists and parses to **nothing** is an error rather than an
/// empty root store, because an empty root store is a client that refuses every
/// endpoint — which looks exactly like the far side being down.
pub fn client(
    identity: crate::Identity,
    settings: &HttpSettings,
) -> Result<reqwest::Client, HttpError> {
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(settings.connect_timeout_s.max(1)))
        .timeout(Duration::from_secs(settings.timeout_s.max(1)))
        .pool_max_idle_per_host(settings.pool_max_idle_per_host)
        // A published API is somebody else's server, and a fleet of boxes asking
        // it questions is a fleet that can knock it over. Naming ourselves is the
        // minimum courtesy and the thing that gets an operator a mail rather than
        // a block.
        .user_agent(format!("{}/{}", identity.name, identity.version));

    let builder = match &settings.tls_roots {
        TlsRoots::Platform => {
            tracing::debug!(
                "outbound TLS verifies against the platform trust store; the image has to \
                 carry one"
            );
            builder
        }
        TlsRoots::Pinned { bundle } => {
            let path = bundle.display().to_string();
            let pem = std::fs::read(bundle).map_err(|e| HttpError::NoTrustAnchors {
                path: path.clone(),
                detail: e.to_string(),
            })?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
                HttpError::NoTrustAnchors {
                    path: path.clone(),
                    detail: e.to_string(),
                }
            })?;
            if certs.is_empty() {
                return Err(HttpError::NoTrustAnchors {
                    path,
                    detail: "the bundle holds no certificate; an empty root store refuses \
                             every endpoint, which looks exactly like the far side being down"
                        .into(),
                });
            }
            tracing::info!(
                anchors = certs.len(),
                path,
                "outbound TLS verifies against a pinned bundle and nothing else"
            );
            // `tls_certs_only` rather than `add_root_certificate`: the second
            // *adds* to the platform roots, so a pinned deployment would still
            // accept anything a public CA vouched for and the pinning would be
            // decoration.
            builder.tls_certs_only(certs)
        }
    };

    builder
        .build()
        .map_err(|e| HttpError::Client(e.to_string()))
}

/// Whether an endpoint keeps what is sent to it confidential in transit.
///
/// `https` anywhere, plain `http` only to a loopback address. Anything else is
/// **refused** rather than warned about (D85): a warning on a box nobody is
/// watching is a warning nobody reads.
///
/// Called on every configured endpoint at start-up rather than on every request,
/// so a deployment that would send a household's day — or a credential that
/// reads every household's — across a network in the clear fails to start.
///
/// # Errors
/// [`HttpError::NotAUrl`] and [`HttpError::NotConfidential`].
pub fn confidential(endpoint: &str) -> Result<(), HttpError> {
    let url: reqwest::Url = endpoint
        .parse()
        .map_err(|e: url::ParseError| HttpError::NotAUrl {
            endpoint: endpoint.to_owned(),
            detail: e.to_string(),
        })?;
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
    if url.scheme() == "http" && loopback {
        return Ok(());
    }
    Err(HttpError::NotConfidential {
        endpoint: endpoint.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_is_confidential_and_plain_http_across_a_network_is_not() {
        for endpoint in [
            "https://obsd.example/v1/fleet",
            "https://127.0.0.1:8443/v1/fleet",
            "http://localhost:7780/v1/fleet",
            "http://127.0.0.1:7780/v1/fleet",
            "http://[::1]:7780/v1/fleet",
        ] {
            confidential(endpoint).unwrap_or_else(|e| panic!("{endpoint} should pass: {e}"));
        }
        for endpoint in [
            "http://obsd.example/v1/fleet",
            "http://10.0.0.4:7780/v1/fleet",
            "ftp://obsd.example/v1/fleet",
        ] {
            assert!(
                matches!(
                    confidential(endpoint),
                    Err(HttpError::NotConfidential { .. })
                ),
                "{endpoint} should be refused"
            );
        }
        assert!(matches!(
            confidential("not a url at all"),
            Err(HttpError::NotAUrl { .. })
        ));
    }

    #[test]
    fn a_client_is_built_against_the_platform_store_by_default() {
        let settings = HttpSettings::default();
        assert_eq!(settings.tls_roots, TlsRoots::Platform);
        client(crate::identity!(), &settings).expect("a client");
    }

    #[test]
    fn a_pinned_bundle_that_is_not_there_stops_the_daemon() {
        // Rather than falling back to the platform store, which would be a
        // deployment that believes it is pinned and is not.
        let settings = HttpSettings {
            tls_roots: TlsRoots::Pinned {
                bundle: PathBuf::from("/nonexistent/fleet-ca.pem"),
            },
            ..HttpSettings::default()
        };
        assert!(matches!(
            client(crate::identity!(), &settings),
            Err(HttpError::NoTrustAnchors { .. })
        ));
    }

    #[test]
    fn a_bundle_with_no_certificate_in_it_is_an_error_and_not_an_empty_root_store() {
        // An empty root store refuses every endpoint, which from the far end
        // looks exactly like the service being down — so the diagnosis has to
        // happen here, once, at start-up.
        let path =
            std::env::temp_dir().join(format!("hems-empty-bundle-{}.pem", std::process::id()));
        std::fs::write(&path, b"# a comment and nothing else\n").expect("a file");
        let settings = HttpSettings {
            tls_roots: TlsRoots::Pinned {
                bundle: path.clone(),
            },
            ..HttpSettings::default()
        };
        let error = client(crate::identity!(), &settings).expect_err("no anchors");
        assert!(
            error.to_string().contains("no certificate"),
            "it says what is wrong: {error}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_settings_round_trip_through_the_shape_an_operator_writes() {
        let pinned: HttpSettings = toml::from_str(
            r#"
            timeout_s = 45
            [tls_roots]
            from = "pinned"
            bundle = "/etc/hems/fleet-ca.pem"
            "#,
        )
        .expect("the pinned shape parses");
        assert_eq!(pinned.timeout_s, 45);
        assert_eq!(
            pinned.tls_roots,
            TlsRoots::Pinned {
                bundle: PathBuf::from("/etc/hems/fleet-ca.pem")
            }
        );

        let platform: HttpSettings = toml::from_str(
            r#"
            [tls_roots]
            from = "platform"
            "#,
        )
        .expect("the platform shape parses");
        assert_eq!(platform.tls_roots, TlsRoots::Platform);
        // …and the default is the platform store with the shell's own timeouts.
        assert_eq!(
            toml::from_str::<HttpSettings>("").expect("an empty table"),
            HttpSettings::default()
        );
    }
}
