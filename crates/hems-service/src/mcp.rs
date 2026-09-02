//! Mounting an MCP server on a daemon that already has an HTTP surface.
//!
//! Every fleet service answers the same two audiences. An operator reads JSON
//! over REST; an agent reads the same numbers over the Model Context Protocol,
//! and gets told what they *mean* — which clock a deadline is on, what a null
//! rate is and is not, whose target a threshold is. The second audience is the
//! reason a tool carries prose a REST route does not need.
//!
//! # One port, one path, one credential model
//!
//! The MCP transport is mounted at `/mcp` on the socket the daemon already
//! binds. A second port is a second thing to firewall, a second thing to
//! certificate and a second thing an operator forgets; `Router::merge` costs
//! nothing.
//!
//! Authentication is **this workspace's own** rather than `mako`'s.
//! `mako-service` carries OIDC and a Cedar schema built on market roles a
//! household energy manager does not have (D8), so what gates this surface is
//! the same bearer credential every other route on these daemons already takes —
//! [`crate::Credentials`] — resolved to the same [`Authority`].
//!
//! # Every call is authorised as its own caller
//!
//! `rmcp`'s streamable-HTTP transport injects the request's
//! [`http::request::Parts`] into the extensions a tool handler receives, so a
//! tool takes `Extension<Parts>` and reads the `Authorization` header that
//! reached *it*. [`caller`] is that step, in one place, so a tool cannot invent
//! its own.
//!
//! The alternative — one authority fixed for the whole server at start-up — is
//! worth naming because it looks reasonable and is not. A surface holding an
//! operator's credential would answer **every** caller as that operator, so
//! anyone who could reach the port would read every household. The
//! authorisation model would be sound and the transport would have thrown the
//! caller away before it was consulted (D111).
//!
//! # Read-only unless a daemon says otherwise
//!
//! Every tool declared here sets `read_only_hint`. The one thing an agent must
//! never be able to do through this surface is move a household's energy: that
//! decision belongs to the arbiter, behind the guard, on the box.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::auth::{Authority, Credentials, bearer};

/// How a daemon's MCP surface is configured.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct McpSettings {
    /// Whether to mount it at all.
    ///
    /// Off by default. An endpoint that speaks a household's data to whatever
    /// can reach it is one an operator should have to switch on, and a daemon
    /// that grew one in a minor release should not start answering on it.
    pub enabled: bool,
    /// The credential the surface accepts, or an `env:`/`file:` reference to it.
    ///
    /// The same shape as every other credential on these daemons (D82): what is
    /// configured is the *reference*, and a value that cannot be resolved stops
    /// the daemon rather than being sent as the literal string `env:…`.
    ///
    /// `None` on a service whose REST surface is open — a day-ahead auction
    /// result and public weather are not a household's data — and required on
    /// one whose is not.
    pub token: Option<crate::config::Secret>,
}

/// What an MCP surface accepts, and what each caller is then allowed to do.
///
/// Two shapes, because the daemons come in two kinds. See the module note on why
/// this is not one fixed authority for the whole server.
#[derive(Debug, Clone)]
pub enum McpAuth {
    /// Anybody may call, and every tool answers the same.
    ///
    /// For a daemon whose REST routes are open on purpose — `tariffd` and
    /// `forecastd`. A day-ahead auction result and public weather are not a
    /// household's data, so there is no household to authorise for. An optional
    /// token is a plain gate an operator may put in front of their own upstream
    /// quota; it carries no authority.
    Public {
        /// The gate, where one is configured.
        token: Option<String>,
    },
    /// Every call is authorised as whoever made it.
    ///
    /// For `histd`, `obsd` and `fleetd`, over the same [`Credentials`] the REST
    /// routes read — so the two surfaces cannot come to different conclusions
    /// about the same token.
    PerCaller(Arc<Credentials>),
}

impl McpAuth {
    /// A surface anybody may call.
    #[must_use]
    pub const fn open() -> Self {
        Self::Public { token: None }
    }

    /// A surface gated by one token, answering every caller the same.
    #[must_use]
    pub fn gate(token: impl Into<String>) -> Self {
        Self::Public {
            token: Some(token.into()),
        }
    }

    /// Build one for a daemon that has **no** credential model.
    ///
    /// `tariffd` and `forecastd`. What a token costs here is the operator's own
    /// upstream quota, which is a rate-limiting question rather than an
    /// authorisation one.
    ///
    /// # Errors
    /// [`crate::ConfigError::UnresolvedSecret`] where the reference cannot be
    /// resolved.
    pub fn gated(settings: &McpSettings) -> Result<Self, crate::ConfigError> {
        match &settings.token {
            None => Ok(Self::open()),
            Some(secret) => Ok(Self::gate(secret.resolve_from_process()?)),
        }
    }

    /// Build one for a daemon that holds a household's data.
    ///
    /// `histd`, `obsd` and `fleetd`. Every call is authorised as its own caller
    /// against `credentials`, so the surface has exactly the reach of whoever is
    /// on the other end of it and no more.
    ///
    /// A daemon with **no** credentials configured is refused: an MCP surface
    /// over a store that accepts nothing would answer every call with the same
    /// denial, which looks like a caller's mistake for as long as nobody looks.
    ///
    /// `settings.token` must be unset. A token here would be a second
    /// authorisation model beside the real one, and the shape it invites — one
    /// operator credential answering every caller — is what per-caller
    /// authorisation exists to prevent.
    ///
    /// # Errors
    /// [`crate::ConfigError::BadEnvironment`] for a configured token, or for a
    /// daemon with no credentials at all.
    pub fn per_caller(
        settings: &McpSettings,
        credentials: &Credentials,
    ) -> Result<Self, crate::ConfigError> {
        if settings.token.is_some() {
            return Err(crate::ConfigError::BadEnvironment {
                variable: "mcp.token".into(),
                value: "<redacted>".into(),
                expected: "no value: this surface authorises each caller against the \
                           daemon's own credentials, and one shared token here would \
                           answer every caller as the same principal",
            });
        }
        if credentials.is_empty() {
            return Err(crate::ConfigError::BadEnvironment {
                variable: "mcp.enabled".into(),
                value: "true".into(),
                expected: "at least one credential: a surface over a store that accepts \
                           nothing refuses every call identically",
            });
        }
        Ok(Self::PerCaller(Arc::new(credentials.clone())))
    }

    /// Whether `presented` may reach a tool at all.
    ///
    /// The cheap check at the door. What the caller may then *do* is
    /// [`caller`]'s question, asked per tool call.
    #[must_use]
    pub fn accepts(&self, presented: Option<&str>) -> bool {
        match self {
            Self::Public { token: None } => true,
            // Constant time, because a token compared byte by byte with an early
            // return is a token that can be guessed one character at a time.
            Self::Public {
                token: Some(expected),
            } => presented.is_some_and(|got| {
                crate::auth::constant_time_eq(got.as_bytes(), expected.as_bytes())
            }),
            Self::PerCaller(credentials) => {
                presented.is_some_and(|got| credentials.authority_of(got).is_some())
            }
        }
    }

    /// The authority behind one call, from the HTTP request that carried it.
    ///
    /// # Errors
    /// Where the surface authorises per caller and the request carries no
    /// credential this daemon issued. A [`McpAuth::Public`] surface has no
    /// household to authorise for and returns `None` rather than an error.
    pub fn authority_of(
        &self,
        parts: &http::request::Parts,
    ) -> Result<Option<Authority>, rmcp::ErrorData> {
        match self {
            Self::Public { .. } => Ok(None),
            Self::PerCaller(credentials) => credentials
                .authority_in(
                    parts
                        .headers
                        .get(http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok()),
                )
                .map(Some)
                .ok_or_else(|| {
                    rmcp::ErrorData::invalid_request(
                        "this call carries no credential this service issued".to_string(),
                        None,
                    )
                }),
        }
    }
}

/// The authority behind one MCP tool call.
///
/// The step every tool on a household-data daemon begins with. `rmcp`'s
/// streamable-HTTP transport injects the request's [`http::request::Parts`] into
/// the handler's extensions, so the tool takes `Extension<Parts>` and this turns
/// it into the same [`Authority`] the REST routes would have reached.
///
/// # Errors
/// Where the request carries no credential this daemon issued.
pub fn caller(auth: &McpAuth, parts: &http::request::Parts) -> Result<Authority, rmcp::ErrorData> {
    auth.authority_of(parts)?.ok_or_else(|| {
        rmcp::ErrorData::internal_error(
            "this surface has no credential model and cannot authorise a household".to_string(),
            None,
        )
    })
}

/// Mount `handler` at `/mcp`, behind `auth`.
///
/// `shutdown` is the daemon's own signal: an MCP session is a long-lived stream,
/// and one that outlived the shutdown would hold the grace period open until the
/// orchestrator lost patience.
pub fn router<H, F>(
    auth: McpAuth,
    shutdown: tokio_util::sync::CancellationToken,
    handler: F,
) -> Router
where
    H: rmcp::ServerHandler + Send + Sync + 'static,
    F: Fn() -> H + Send + Sync + 'static,
{
    let config = StreamableHttpServerConfig::default()
        .disable_allowed_hosts()
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(30)))
        .with_cancellation_token(shutdown);

    let service = StreamableHttpService::new(
        move || Ok(handler()),
        Arc::new(LocalSessionManager::default()),
        config,
    );

    Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(Arc::new(auth), gate))
}

/// A cancellation token that fires when the daemon is asked to stop.
///
/// `rmcp` cancels its sessions on a `CancellationToken` and this workspace stops
/// on a [`crate::Shutdown`]; bridging them is three lines, and five daemons
/// writing those three lines is four chances to forget one. A session that
/// outlived the shutdown would hold the grace period open until the
/// orchestrator lost patience and sent `SIGKILL` — which is the outcome the
/// graceful path exists to avoid, arrived at slowly.
#[must_use]
pub fn cancel_on(signal: &crate::Shutdown) -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    let follows = token.clone();
    let signal = signal.clone();
    tokio::spawn(async move {
        signal.wait().await;
        follows.cancel();
    });
    token
}

/// Refuse a caller that did not present the token.
async fn gate(State(auth): State<Arc<McpAuth>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| bearer(Some(v)));
    if auth.accepts(presented) {
        next.run(request).await
    } else {
        // The same answer for a missing token and a wrong one. Telling the two
        // apart tells somebody probing which half they got right.
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Turn an instant into RFC 3339, or `null`.
///
/// `time::OffsetDateTime`'s **derived** `Serialize` produces a nine-element
/// array in `time`'s internal component order — documented nowhere a consumer
/// would look, and silently correct-looking inside a `json!`. On this surface
/// that is the worst place for it: an agent asked whether a § 14a reduction
/// lapsed would be handed an undocumented integer array and expected to do
/// arithmetic on it.
///
/// A formatting failure yields `null` rather than a fabricated instant. A
/// timestamp a consumer cannot read must not look like one it can.
#[must_use]
pub fn rfc3339(at: time::OffsetDateTime) -> Option<String> {
    at.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// [`rfc3339`] for an optional instant.
#[must_use]
pub fn rfc3339_opt(at: Option<time::OffsetDateTime>) -> Option<String> {
    at.and_then(rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::SiteScope;

    fn parts_with(header: Option<&str>) -> http::request::Parts {
        let mut builder = http::Request::builder().uri("/mcp");
        if let Some(value) = header {
            builder = builder.header(http::header::AUTHORIZATION, value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    #[test]
    fn an_open_surface_takes_anybody_and_a_gated_one_does_not() {
        let open = McpAuth::open();
        assert!(open.accepts(None));
        assert!(open.accepts(Some("anything")));

        let gated = McpAuth::gate("tok-secret");
        assert!(gated.accepts(Some("tok-secret")));
        assert!(!gated.accepts(Some("tok-secre")), "not a prefix");
        assert!(!gated.accepts(Some("tok-secrets")), "not an extension");
        assert!(!gated.accepts(None), "and not nothing");
    }

    #[test]
    fn each_call_is_authorised_as_the_caller_that_made_it() {
        // The correction to the withdrawn D107. `rmcp`'s streamable-HTTP
        // transport injects the request's `Parts` into the handler's
        // extensions, so a tool reads the credential that reached *it* — and
        // two callers on one surface get two answers rather than one.
        let credentials = Credentials::default()
            .with_site("haus-1", "tok-1")
            .with_operator("tok-operator");
        let settings = McpSettings {
            enabled: true,
            token: None,
        };
        let auth = McpAuth::per_caller(&settings, &credentials).expect("credentials to authorise");

        let household = caller(&auth, &parts_with(Some("Bearer tok-1"))).expect("a known token");
        assert!(household.may_read("haus-1"));
        assert!(!household.may_read("haus-2"));
        assert!(
            !household.may_read_the_fleet(),
            "and a household's token does not become an operator's by arriving \
             over MCP instead of over REST"
        );

        let operator = caller(&auth, &parts_with(Some("Bearer tok-operator"))).expect("likewise");
        assert!(operator.may_read("haus-2") && operator.may_read_the_fleet());

        assert!(caller(&auth, &parts_with(Some("Bearer tok-invented"))).is_err());
        assert!(caller(&auth, &parts_with(None)).is_err());
    }

    #[test]
    fn a_shared_token_beside_the_credential_model_is_refused() {
        // One token here would answer every caller as the same principal, which
        // is exactly the shape the withdrawn D107 produced: a surface holding an
        // operator's token served every household to whoever reached the port.
        let credentials = Credentials::default().with_operator("tok-operator");
        let with_token = McpSettings {
            enabled: true,
            token: Some(crate::config::Secret::literal("tok-operator")),
        };
        assert!(McpAuth::per_caller(&with_token, &credentials).is_err());
    }

    #[test]
    fn a_daemon_holding_household_data_will_not_serve_it_openly() {
        // The asymmetry between the two constructors, and the reason there are
        // two. `tariffd` may be open because a published auction result is not
        // anybody's data; `histd` may not, because two years of a household's
        // control history is.
        let open = McpSettings {
            enabled: true,
            token: None,
        };
        assert!(McpAuth::gated(&open).is_ok());
        assert!(
            McpAuth::per_caller(&open, &Credentials::default()).is_err(),
            "a surface over a store that accepts nothing refuses every call \
             identically, which looks like the caller's mistake"
        );
    }

    #[test]
    fn a_public_surface_has_no_household_to_authorise_for() {
        // `tariffd` and `forecastd`. `authority_of` is `None` rather than an
        // error, and `caller` is an error rather than a fabricated principal —
        // a public surface asked to authorise a household is a bug in the
        // daemon, not in the request.
        let public = McpAuth::gate("tok-quota");
        assert_eq!(public.authority_of(&parts_with(None)).unwrap(), None);
        assert!(caller(&public, &parts_with(Some("Bearer tok-quota"))).is_err());
    }

    #[test]
    fn an_instant_reaches_an_agent_as_a_string_it_can_read() {
        let at = time::macros::datetime!(2026-01-15 17:00:00 UTC);
        assert_eq!(rfc3339(at).as_deref(), Some("2026-01-15T17:00:00Z"));
        assert_eq!(rfc3339_opt(None), None);
    }

    #[test]
    fn an_unscoped_operator_still_reaches_every_household() {
        let credentials = Credentials::default().with_operator_of(SiteScope::Every, "tok-all");
        let auth = McpAuth::per_caller(
            &McpSettings {
                enabled: true,
                token: None,
            },
            &credentials,
        )
        .unwrap();
        let all = caller(&auth, &parts_with(Some("Bearer tok-all"))).unwrap();
        assert!(all.may_read("anything") && all.may_read_the_fleet());
    }
}
