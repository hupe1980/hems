//! The Model Context Protocol surface of `agentd`.
//!
//! Mounted at `/mcp` on the port the daemon already binds, over the same queue
//! the REST route reads — so the two surfaces cannot come to different
//! conclusions about what the specialists said.
//!
//! # Read-only, and here that is structural rather than a hint
//!
//! Every fleet daemon's `/mcp` is read-only. On this one there is nothing that
//! could be otherwise: the only state `agentd` holds is what its own specialists
//! concluded, `Advice` is a leaf type nothing in this workspace consumes, and no
//! authority derivable here holds a capability that writes (D118). An agent on
//! the other end of this surface reads the same queue an operator does.
//!
//! # And it says what a finding is *not*
//!
//! Every tool description states that these are proposals about a population and
//! that the exact answers are `obsd`'s and `histd`'s. That is not politeness: a
//! model reading "four of five breaches were unplanned" without being told it is
//! a correlation over a window will quote it as a fact about a household, and the
//! household it names will be one of the five that happened to be listed as
//! evidence.

use std::sync::Arc;

use hems_service::mcp::McpAuth;
use http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, InitializeResult, ServerCapabilities, ServerInfo,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::review::{Queue, SPECIALISTS};

/// What the tools read.
#[derive(Clone)]
pub struct State {
    /// The same queue the REST surface answers from.
    pub queue: Arc<Queue>,
    /// How each caller is authorised.
    ///
    /// Not one authority for the whole surface: every tool resolves the request
    /// that reached *it*, so this surface has exactly the reach of whoever is on
    /// the other end of it (D111).
    pub auth: McpAuth,
}

/// The handler.
#[derive(Clone)]
pub struct Handler {
    state: Arc<State>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Handler>,
}

#[tool_router]
impl Handler {
    /// A handler over one advisory queue.
    #[must_use]
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// The queue.
    #[tool(
        description = "The advisory queue: what each specialist last concluded about the \
                       fleet, with the window it read, when it ran, and the journal run \
                       that produced it. These are PROPOSALS about a POPULATION over a \
                       window of days — a correlation across many exact answers, not a \
                       fact about any one household. The exact answers are obsd's (which \
                       household breached, on which date) and histd's (the § 14a evidence \
                       and the settlement registers); quote those when the question is \
                       about a household. An empty queue is the ordinary answer and means \
                       the specialists found nothing worth an operator's morning — check \
                       `has_reported` to tell that apart from a plane that has not managed \
                       to read the fleet. Nothing here can be acted on by a machine: no \
                       advice this plane produces reaches a device, by construction.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_advice(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.fleet_caller(&parts)?;
        let reviews = self.state.queue.read().await;
        let findings: usize = reviews.iter().map(|r| r.proposal.advice.len()).sum();
        Ok(json(&serde_json::json!({
            "reviews": reviews,
            "findings": findings,
            "specialists": SPECIALISTS.iter().map(|s| serde_json::json!({
                "name": s.name,
                "question": s.question,
                "has_reported": reviews.iter().any(|r| r.specialist == s.name),
            })).collect::<Vec<_>>(),
        })))
    }

    /// What is being asked at all.
    #[tool(
        description = "Every specialist this plane runs and the question it is run to \
                       answer, whether or not it has said anything yet. Read this before \
                       list_advice when the queue is empty: it is the difference between \
                       'the fleet is in good order' and 'nobody asked'.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_specialists(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        self.fleet_caller(&parts)?;
        let reviews = self.state.queue.read().await;
        Ok(json(&serde_json::json!({
            "specialists": SPECIALISTS.iter().map(|s| serde_json::json!({
                "name": s.name,
                "question": s.question,
                "last_run": reviews.iter().find(|r| r.specialist == s.name).map(|r| &r.run),
                "last_reviewed_at": reviews.iter().find(|r| r.specialist == s.name)
                    .and_then(|r| r.at.format(&time::format_description::well_known::Rfc3339).ok()),
            })).collect::<Vec<_>>(),
            "advisory_only": "every specialist runs under an authority attenuated from the \
                              operator's, which cannot widen and holds no capability that \
                              writes; Advice is a leaf type nothing in this workspace consumes",
        })))
    }

    /// The caller, if it may ask a question about every household in its scope.
    ///
    /// An aggregate is not any one household's data however wide that
    /// household's own reach, so this asks for the fleet capability by name
    /// rather than inferring it from a site check (D112).
    fn fleet_caller(&self, parts: &Parts) -> Result<hems_service::Authority, McpError> {
        let caller = hems_service::mcp::caller(&self.state.auth, parts)?;
        if caller.may_read_the_fleet() {
            Ok(caller)
        } else {
            Err(McpError::invalid_request(
                format!(
                    "{} may not read a question about every household; the advisory queue \
                     names households other than its own",
                    caller.subject()
                ),
                None,
            ))
        }
    }
}

#[tool_handler]
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentd", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "# agentd — the hems advisory plane\n\
                 \n\
                 Every answer here is a **proposal about a population** of households over a \
                 window of days: a correlation across many exact answers, produced by a \
                 specialist whose run is in an append-only hash-chained journal — so any \
                 finding can be replayed to the days it was drawn from.\n\
                 \n\
                 ## What this is not\n\
                 - **Not a fact about one household.** Ask `obsd` for what a household did \
                 and `histd` for its § 14a evidence and its settlement registers. The sites \
                 in a finding's `evidence` are examples of a pattern, not a verdict on each \
                 of them.\n\
                 - **Not something a machine can act on.** No advice this plane produces \
                 reaches a device: `Advice` is a leaf type nothing in this workspace \
                 consumes, and a specialist's authority is derived by attenuation, which \
                 refuses to widen and holds no capability that writes.\n\
                 \n\
                 ## The quantities are the workspace's own\n\
                 Households are **counted, never averaged** — one household in ten thousand \
                 that failed to respect a network operator's reduction is an incident with a \
                 name, and the same fact as a percentage reads as success. The other two are \
                 minutes on the fallback and days of record. Two findings of different kinds \
                 are **not** ranked against each other: there is no exchange rate between a \
                 household and a minute that this plane is entitled to invent.\n\
                 \n\
                 ## An empty queue\n\
                 The ordinary answer, and it means the specialists found nothing worth an \
                 operator's morning. `has_reported` is what tells that apart from a plane \
                 that has not managed to read the fleet at all.",
            )
    }
}

/// Mount the surface at `/mcp`.
pub fn router(
    state: Arc<State>,
    auth: McpAuth,
    shutdown: tokio_util::sync::CancellationToken,
) -> axum::Router {
    hems_service::mcp::router(auth, shutdown, move || Handler::new(Arc::clone(&state)))
}

/// One JSON document as a tool result.
///
/// Pretty-printed: what reads this is a model or a person looking at a queue,
/// and neither is helped by one long line.
fn json(value: &serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    )])
}
