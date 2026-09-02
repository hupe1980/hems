//! The Model Context Protocol surface of `obsd`.
//!
//! Mounted at `/mcp` on the port the daemon already binds, over the same
//! `Fleet` the REST routes read.
//!
//! # The numbers here are the ones easiest to quote wrongly
//!
//! `obsd` exists because a fleet view built out of averages hides the only
//! things anybody needs to see, and every tool carries that distinction:
//!
//! * a § 14a breach is **counted and named**, never averaged. One household in
//!   ten thousand is an incident with a site and a date; "99,99 % compliant"
//!   reads as success and is the same fact;
//! * a **saving** is a mean over the days a saving can be computed from. Days
//!   run with the weather known in advance are excluded and reported separately,
//!   because a figure that included them is an upper bound no box can reach;
//! * a **calibration** figure needs twenty independent days. Below that the
//!   coverage number is a coin toss quoted to three significant figures, and
//!   `forecast_is_calibrated` is what says so.

use std::sync::Arc;

use crate::fleet::Fleet;
use hems_service::mcp::McpAuth;
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
    /// The same fleet view the REST surface answers from.
    pub fleet: Arc<RwLock<Fleet>>,
    /// How long a site may be quiet before it is reported silent.
    pub silent_after: time::Duration,
    /// How each caller is authorised.
    ///
    /// Not one authority for the whole surface: every tool resolves the request
    /// that reached *it*, so this surface has exactly the reach of whoever is on
    /// the other end of it (D111).
    pub auth: hems_service::McpAuth,
}

/// Which site a caller is asking about.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteParams {
    /// The site identifier, as the box reports itself.
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
    /// A handler over one fleet view.
    #[must_use]
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// The whole fleet in one answer.
    #[tool(
        description = "The fleet summary: how many sites and days, the mean saving over the \
                       days a saving can be computed from, self-sufficiency, forecast \
                       calibration, and — as LISTS with a site and a date, never as rates — \
                       every § 14a breach, every ceiling below the [A1 4.5] minimum, every \
                       site that spent time without a plan, and every site that has gone \
                       quiet.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_fleet(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = self.fleet_caller(&parts)?;
        let fleet = self.state.fleet.read().await;
        let summary = fleet.summarise_within(
            caller.sites(),
            time::OffsetDateTime::now_utc(),
            self.state.silent_after,
        );
        json(&serde_json::to_value(&summary).unwrap_or_default())
    }

    /// Every day a network operator's instruction was not respected.
    #[tool(
        description = "Every day a household did not respect a network operator's § 14a \
                       reduction, with the site and the date. A LIST and never a rate: one \
                       household in ten thousand is an incident with a name, and a \
                       percentage reads as success. An empty list is the ordinary answer and \
                       means no breach was reported, not that none was possible.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_breaches(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = self.fleet_caller(&parts)?;
        let fleet = self.state.fleet.read().await;
        let summary = fleet.summarise_within(
            caller.sites(),
            time::OffsetDateTime::now_utc(),
            self.state.silent_after,
        );
        json(&serde_json::json!({
            "breached": summary.breached,
            "below_minimum": summary.below_minimum,
            "count": summary.breached.len(),
            "days_on_record": summary.days,
        }))
    }

    /// One household's own days.
    #[tool(
        description = "One site's reported days, most recent first: the saving, the \
                       self-sufficiency, whether a § 14a instruction was respected and how \
                       long it spent without a plan. A site that has never reported is \
                       absent rather than empty.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_site(
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
        let fleet = self.state.fleet.read().await;
        match fleet.site(&p.site) {
            Some(history) => {
                let days: Vec<serde_json::Value> = history
                    .days()
                    .map(|d| serde_json::to_value(d).unwrap_or_default())
                    .collect();
                json(&serde_json::json!({ "site": p.site, "days": days }))
            }
            None => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "site_not_found: no box has reported a day for '{}'.",
                p.site
            ))])),
        }
    }

    /// Whether the forecast bands are the width they claim.
    #[tool(
        description = "The fleet's forecast scores: the production and load bands' coverage \
                       and CRPS, how many INDEPENDENT days they rest on, and whether that is \
                       enough to call them calibrated. Forecast error is correlated across a \
                       day, so ninety-six quarter hours of one Tuesday are close to one \
                       draw — below twenty days a coverage figure is a coin toss quoted to \
                       three significant figures, and `is_calibrated` is false whatever it \
                       says.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_forecast_scores(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        let caller = self.fleet_caller(&parts)?;
        let fleet = self.state.fleet.read().await;
        let s = fleet.summarise_within(
            caller.sites(),
            time::OffsetDateTime::now_utc(),
            self.state.silent_after,
        );
        json(&serde_json::json!({
            "pv_coverage": s.pv_coverage,
            "pv_crps_w": s.pv_crps,
            "load_coverage": s.load_coverage,
            "load_crps_w": s.load_crps,
            "episodes": s.forecast_episodes,
            "is_calibrated": s.forecast_is_calibrated,
            "measured_days": s.measured_days,
            "foresight_days_excluded": s.foresight_days,
            "unmeasurable_days_excluded": s.unmeasurable_days,
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
        name = "review-the-fleet",
        description = "Read a fleet view without turning an incident into a percentage"
    )]
    fn review_the_fleet(&self) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(Role::User, "How is the fleet doing?"),
            PromptMessage::new_text(
                Role::Assistant,
                "1. `list_breaches` first. A § 14a breach is a compliance incident with a \
                 site and a date; report the sites, not a rate.\n\
                 2. `get_fleet` for the means. `saving_eur` is over `measured_days` only, \
                 and two kinds of day are left out for different reasons. \
                 `foresight_days` were run with the weather known in advance — an upper \
                 bound no box reaches. `unmeasurable_days` came from real hardware, which \
                 reports no baseline because a baseline is a counterfactual only a \
                 simulator can re-run. If `unmeasurable_days` is large and `measured_days` \
                 is small, the saving above rests on simulations: say so.\n\
                 3. `below_minimum` is not a fault of the box: hems applies a command below \
                 the [A1 4.5] minimum, because refusing a network operator is not a box's \
                 decision. It is the customer's entitlement and somebody has to see it.\n\
                 4. `get_forecast_scores` before quoting any coverage number. Under twenty \
                 episodes it is not a calibration figure.\n\
                 5. There is no BNetzA threshold for a saving, a self-sufficiency or a \
                 calibration figure. Any target is the operator's own — say whose.",
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
        .with_server_info(Implementation::new("obsd", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "# obsd — what the fleet of boxes is actually doing\n\
             \n\
             One signed day per box per day, aggregated. It exists because a fleet view \
             built out of averages hides the only things anybody needs to see.\n\
             \n\
             ## Counted, never averaged\n\
             - A **§ 14a breach** is a list with a site and a date. One household in ten \
             thousand is an incident with a name; `99,99 %` is the same fact and reads as \
             success.\n\
             - So is a **ceiling below the [A1 4.5] minimum**. That is not the box \
             misbehaving — hems applies such a command, because refusing a network operator \
             is not a decision a box takes — but the entitlement is the customer's.\n\
             - So is a site that spent time **without a plan**, and a site that has gone \
             **silent**.\n\
             \n\
             ## Averaged, and only over what may be averaged\n\
             - `saving_eur` is a mean over `measured_days`, and two kinds of day are \
             excluded for **different** reasons. `foresight_days` were run with the weather \
             known in advance: an upper bound no box in a house can reach. \
             `unmeasurable_days` came from a real box, which reports the energies it metered \
             and the § 14a record and **no money at all** — a baseline is what the day would \
             have cost unmanaged, and only a simulator can re-run a day. A fleet of real \
             boxes therefore reports *no* saving rather than a saving of zero, and \
             `unmeasurable_days` is what says which of the two you are looking at.\n\
             - The saving carries the battery wear, the discomfort and the service the plan \
             did not deliver, not the electricity bill alone. `bill_saving_eur` is the bill \
             alone and is the larger number.\n\
             \n\
             ## Forecast scores need days, not slots\n\
             Forecast error is correlated across a day, so ninety-six quarter hours of one \
             Tuesday are close to one draw. `forecast_is_calibrated` is false below twenty \
             independent episodes whatever the coverage says.\n\
             \n\
             ## Tools\n\
             - `get_fleet()` — the whole summary\n\
             - `list_breaches()` — § 14a incidents and below-minimum commands, named\n\
             - `get_site(site)` — one household's own days\n\
             - `get_forecast_scores()` — coverage, CRPS, episodes, and whether it counts\n\
             \n\
             **No BNetzA threshold exists** for any of these figures. Any target is the \
             operator's own; say whose it is.",
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
