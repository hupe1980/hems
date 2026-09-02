//! The Model Context Protocol surface of `fleetd`.
//!
//! Mounted at `/mcp` on the port the daemon already binds, over the same
//! registry the REST routes read.
//!
//! # Read-only, and here that is load-bearing
//!
//! `fleetd` is where a box is adopted, told what configuration it should be on,
//! and offered an update. Every one of those is a *write*, and none of them is a
//! tool. An agent that could enrol a box could adopt somebody else's household;
//! one that could move a version could roll a fleet of gateway boxes onto a
//! build nobody approved.
//!
//! What an agent can usefully answer is the question a rollout is actually
//! about — **which boxes are on the configuration the fleet wants**, and which
//! have not said anything for a while — and that is what these tools are.

use std::sync::Arc;

use crate::registry::Registry;
use hems_service::mcp::{McpAuth, rfc3339_opt};
use http::request::Parts;
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, InitializeResult, PromptMessage, Role,
    ServerCapabilities, ServerInfo,
};
use rmcp::{
    ErrorData as McpError, ServerHandler, prompt, prompt_handler, prompt_router, schemars, tool,
    tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::RwLock;

/// What the tools read.
#[derive(Clone)]
pub struct State {
    /// The same registry the REST surface answers from.
    pub registry: Arc<RwLock<Registry>>,
    /// How long a box may be quiet before it is worth naming.
    pub silent_after: time::Duration,
    /// How each caller is authorised. See `hems_service::mcp` and D111.
    pub auth: hems_service::McpAuth,
}

/// Just a site.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteParams {
    /// The site identifier.
    pub site: String,
}

/// The handler.
#[derive(Clone)]
pub struct Handler {
    state: Arc<State>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Handler>,
    #[allow(dead_code)]
    prompt_router: PromptRouter<Handler>,
}

#[tool_router]
impl Handler {
    /// A handler over one registry.
    #[must_use]
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// Every enrolled box and what it is on.
    #[tool(
        description = "Every enrolled box: the configuration version the fleet wants it on, \
                       the version it last SAID it is running, when it last said anything, \
                       and whether the two agree. `converged` is the question a rollout is \
                       actually about — a fleet that can only push cannot answer it, which \
                       is why a box reports back rather than being assumed to have complied. \
                       A box that has never reported has a null running_version and is not \
                       converged.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_boxes(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = self.fleet_caller(&parts)?;
        let registry = self.state.registry.read().await;
        let now = time::OffsetDateTime::now_utc();
        let rows: Vec<serde_json::Value> = registry
            .states()
            .into_iter()
            // Scoped, so a shared deployment does not answer one tenant with
            // another's households (D112).
            .filter(|b| caller.sites().covers(&b.site))
            .map(|b| {
                let silent = b
                    .last_seen
                    .is_none_or(|seen| now - seen > self.state.silent_after);
                serde_json::json!({
                    "site": b.site,
                    "wanted_version": b.wanted_version,
                    "running_version": b.running_version,
                    "last_seen": rfc3339_opt(b.last_seen),
                    "converged": b.converged,
                    "silent": silent,
                })
            })
            .collect();
        let converged = rows
            .iter()
            .filter(|r| r["converged"].as_bool().unwrap_or(false))
            .count();
        json(&serde_json::json!({
            "boxes": rows,
            "enrolled": registry.enrolled(),
            "converged": converged,
        }))
    }

    /// One box.
    #[tool(
        description = "One box's enrolment state: what the fleet wants it on, what it last \
                       said it is running, and when. A site that has never enrolled is \
                       absent rather than empty.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_box(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SiteParams>,
    ) -> Result<CallToolResult, McpError> {
        let caller = hems_service::mcp::caller(&self.state.auth, &parts)?;
        if !caller.may_read(&p.site) {
            return Err(McpError::invalid_params(
                format!(
                    "{} is not authorised for site {:?}",
                    caller.subject(),
                    p.site
                ),
                None,
            ));
        }
        let registry = self.state.registry.read().await;
        match registry
            .states()
            .into_iter()
            .find(|b| b.site == p.site && caller.sites().covers(&b.site))
        {
            Some(b) => json(&serde_json::json!({
                "site": b.site,
                "wanted_version": b.wanted_version,
                "running_version": b.running_version,
                "last_seen": rfc3339_opt(b.last_seen),
                "converged": b.converged,
            })),
            None => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "site_not_found: '{}' is not an enrolled box.",
                p.site
            ))])),
        }
    }

    /// The boxes that have gone quiet.
    #[tool(
        description = "Boxes that have not reported within the fleet's silence window, and \
                       boxes that are not on the configuration the fleet wants. Two different \
                       problems: a silent box may be perfectly compliant and unreachable, and \
                       an unconverged one may be talking and refusing.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_stragglers(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = self.fleet_caller(&parts)?;
        let registry = self.state.registry.read().await;
        let now = time::OffsetDateTime::now_utc();
        let states: Vec<_> = registry
            .states()
            .into_iter()
            .filter(|b| caller.sites().covers(&b.site))
            .collect();
        let silent: Vec<&str> = states
            .iter()
            .filter(|b| {
                b.last_seen
                    .is_none_or(|seen| now - seen > self.state.silent_after)
            })
            .map(|b| b.site.as_str())
            .collect();
        let unconverged: Vec<serde_json::Value> = states
            .iter()
            .filter(|b| !b.converged)
            .map(|b| {
                serde_json::json!({
                    "site": b.site,
                    "wanted_version": b.wanted_version,
                    "running_version": b.running_version,
                })
            })
            .collect();
        json(&serde_json::json!({
            "silent": silent,
            "unconverged": unconverged,
            "enrolled": registry.enrolled(),
        }))
    }

    /// The caller, if it may ask a question about every household in its scope.
    ///
    /// An aggregate is not any one household's data however wide that
    /// household's own reach, so this asks for the fleet capability by name
    /// rather than inferring it from a site check that a `None` site would pass
    /// (D112).
    fn fleet_caller(&self, parts: &Parts) -> Result<hems_service::Authority, McpError> {
        let caller = hems_service::mcp::caller(&self.state.auth, parts)?;
        if caller.may_read_the_fleet() {
            Ok(caller)
        } else {
            Err(McpError::invalid_params(
                format!(
                    "{} may not read an answer about every household",
                    caller.subject()
                ),
                None,
            ))
        }
    }
}

// `#[prompt]` generates an associated function with no doc comment this crate
// can attach, and requires `&self` whether or not the body reads it.
#[allow(missing_docs, clippy::unused_self)]
#[prompt_router]
impl Handler {
    #[prompt(
        name = "check-a-rollout",
        description = "Tell a stalled rollout from an unreachable box"
    )]
    fn check_a_rollout(&self) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(Role::User, "Did the configuration change land?"),
            PromptMessage::new_text(
                Role::Assistant,
                "1. `list_stragglers`. It separates the two failures, which have the same \
                 symptom on a dashboard and different causes.\n\
                 2. **Silent** is a box nobody has heard from. It may be perfectly compliant \
                 and simply unreachable — a household router, a power cut, a WAN outage — and \
                 the § 14a guard on it does not need this service to keep working.\n\
                 3. **Unconverged** is a box that is talking and is on a different version \
                 from the one the fleet wants. That is a real rollout problem.\n\
                 4. `converged` comes from what the box *reported*, never from what was \
                 pushed. A fleet that assumed compliance would report a rollout as finished \
                 the moment it was started.\n\
                 5. Nothing here can change any of it. Enrolment, configuration and releases \
                 are writes and deliberately not tools.",
            ),
        ]
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("fleetd", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "# fleetd — which boxes are on what\n\
             \n\
             Single-use enrolment, versioned configuration a box reports back on, and \
             Ed25519-signed release manifests whose trust anchor is a key the box was built \
             with rather than this server.\n\
             \n\
             ## Read-only, and here that matters\n\
             Enrolling a box, moving a configuration version and publishing a release are all \
             writes, and none of them is a tool. An agent that could enrol could adopt \
             somebody else's household; one that could move a version could roll a fleet of \
             gateway boxes onto a build nobody approved.\n\
             \n\
             ## `converged` is reported, never assumed\n\
             It compares the version the fleet **wants** with the version the box last **said** \
             it is running. A fleet that could only push would report a rollout as finished \
             the moment it started.\n\
             \n\
             ## Silent and unconverged are different problems\n\
             A **silent** box may be perfectly compliant and unreachable: the § 14a guard runs \
             on the box and needs nothing from here. An **unconverged** box is talking and is \
             on the wrong version. Only the second is a rollout problem.\n\
             \n\
             ## Tools\n\
             - `list_boxes()` — every enrolled box and what it is on\n\
             - `get_box(site)` — one of them\n\
             - `list_stragglers()` — the silent and the unconverged, separately",
        )
    }
}

/// One JSON block, or an internal error.
fn json(value: &serde_json::Value) -> Result<CallToolResult, McpError> {
    ContentBlock::json(value)
        .map(|block| CallToolResult::success(vec![block]))
        .map_err(|e| McpError::internal_error(e.message, None))
}

/// The `/mcp` router for this daemon.
pub fn router(
    state: Arc<State>,
    auth: McpAuth,
    shutdown: tokio_util::sync::CancellationToken,
) -> axum::Router {
    hems_service::mcp::router(auth, shutdown, move || Handler::new(Arc::clone(&state)))
}
