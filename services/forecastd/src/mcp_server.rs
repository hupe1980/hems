//! The Model Context Protocol surface of `forecastd`.
//!
//! Mounted at `/mcp` on the port the daemon already binds, over the same runs
//! the REST routes read.
//!
//! # It serves the sky, and says so
//!
//! There is deliberately no `get_forecast` here. What this service holds is
//! ICON-D2's irradiance and temperature, and what a *roof* will make of it needs
//! a correction only that roof's own meter can teach — the tree shading the east
//! string, the datasheet that was optimistic, the dust. A tool called `forecast`
//! is one somebody eventually plans against uncorrected, so the two tools are
//! named for what they actually are: the **sky**, and what a named geometry
//! would **model** from it.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::poller::Run;
use hems_core::prelude::Power;
use hems_service::mcp::{McpAuth, rfc3339};
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
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
    /// The same runs the REST surface answers from.
    pub runs: Arc<RwLock<BTreeMap<String, Run>>>,
}

/// Which configured location.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LocationParams {
    /// The location name, as the operator configured it.
    pub location: String,
}

/// A location and a roof.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArrayParams {
    /// The location name, as the operator configured it.
    pub location: String,
    /// Installed direct-current power, kWp.
    pub kwp: f64,
    /// The inverter's alternating-current limit, kW. Defaults to 80 % of the
    /// array, which is what an ordinary German installation is sized at.
    pub ac_kw: Option<f64>,
    /// Tilt from horizontal, degrees. Defaults to 35.
    pub tilt: Option<f64>,
    /// Azimuth clockwise from north; 180 is due south. Defaults to 180.
    pub azimuth: Option<f64>,
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
    /// A handler over the current runs.
    #[must_use]
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// Which locations this service fetches.
    #[tool(
        description = "The locations this service fetches the sky for, with when each run was \
                       last retrieved. A location the operator has not configured cannot be \
                       asked for: the sun position a production figure is computed from has \
                       to be the one the irradiance was fetched at, so a caller may not name \
                       its own coordinates.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_locations(&self) -> Result<CallToolResult, McpError> {
        let runs = self.state.runs.read().await;
        let locations: Vec<serde_json::Value> = runs
            .iter()
            .map(|(name, run)| {
                serde_json::json!({
                    "location": name,
                    "fetched_at": rfc3339(run.fetched_at),
                    "published_minutes": run.series.published_minutes,
                    "slots": run.series.len(),
                    "latitude": run.at.latitude,
                    "longitude": run.at.longitude,
                })
            })
            .collect();
        json(&serde_json::json!({ "locations": locations }))
    }

    /// Irradiance and temperature per quarter hour.
    #[tool(
        description = "The sky over one configured location: global horizontal irradiance in \
                       W/m², air temperature in °C, and cloud cover where the model publishes \
                       one. This is NOT a production forecast — turning it into one needs a \
                       correction only a particular roof's own meter can teach.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_weather(
        &self,
        Parameters(p): Parameters<LocationParams>,
    ) -> Result<CallToolResult, McpError> {
        let runs = self.state.runs.read().await;
        let Some(run) = runs.get(&p.location) else {
            return Ok(not_found(&p.location));
        };
        let points: Vec<serde_json::Value> = run
            .series
            .slots
            .iter()
            .map(|(slot, point)| {
                serde_json::json!({
                    "slot": rfc3339(slot.start()),
                    "ghi_w_per_m2": point.ghi_w_per_m2,
                    "temperature_c": point.temperature_c,
                    "cloud_cover": point.cloud_cover,
                })
            })
            .collect();
        json(&serde_json::json!({
            "location": p.location,
            "fetched_at": rfc3339(run.fetched_at),
            "published_minutes": run.series.published_minutes,
            "points": points,
        }))
    }

    /// What a named geometry would model from that sky.
    #[tool(
        description = "What an array of a given geometry would MODEL from one location's sky, \
                       in watts per quarter hour. Modelled, not forecast: it knows the sun \
                       and the panel and nothing about the tree that shades the east string \
                       or the dust on the glass. A box applies its own roof's learned \
                       correction to this before planning against it.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_modelled_production(
        &self,
        Parameters(p): Parameters<ArrayParams>,
    ) -> Result<CallToolResult, McpError> {
        if !(p.kwp.is_finite() && p.kwp > 0.0) {
            return Err(McpError::invalid_params(
                "kwp has to be a positive number of kilowatts peak".to_string(),
                None,
            ));
        }
        let runs = self.state.runs.read().await;
        let Some(run) = runs.get(&p.location) else {
            return Ok(not_found(&p.location));
        };
        let model = hems_forecast::ArrayModel::new(
            Power::from_kw(p.kwp),
            Power::from_kw(p.ac_kw.unwrap_or(p.kwp * 0.8)),
            p.tilt.unwrap_or(35.0),
            p.azimuth.unwrap_or(180.0),
        );
        // The sun position is computed at the coordinates the **run** was
        // fetched for, never at anything a caller supplies: a household may
        // describe its own roof and may not move its own latitude, or the
        // irradiance it is given is somebody else's sky.
        let points: Vec<serde_json::Value> = run
            .series
            .modelled_production(&model, run.at)
            .into_iter()
            .map(|(slot, watts)| {
                serde_json::json!({ "slot": rfc3339(slot.start()), "watts": watts })
            })
            .collect();
        json(&serde_json::json!({
            "location": p.location,
            "fetched_at": rfc3339(run.fetched_at),
            "modelled": points,
        }))
    }
}

// `#[prompt]` generates an associated function with no doc comment this crate
// can attach, and requires `&self` whether or not the body reads it.
#[allow(missing_docs, clippy::unused_self)]
#[prompt_router]
impl Handler {
    #[prompt(
        name = "will-the-roof-produce",
        description = "Answer a production question without turning a model into a forecast"
    )]
    fn will_the_roof_produce(&self) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(Role::User, "How much will this roof make tomorrow?"),
            PromptMessage::new_text(
                Role::Assistant,
                "1. `list_locations` to find the one this household's sky is fetched for. A \
                 caller cannot name its own coordinates: the sun position has to be the one \
                 the irradiance was fetched at.\n\
                 2. `get_modelled_production(location, kwp, …)` for the geometry.\n\
                 3. Say **modelled**, not forecast. It knows the sun and the panel and \
                 nothing about the shade, the soiling or the snow, and a real roof commonly \
                 delivers around nine tenths of it. Only the box's own meter can close that \
                 gap, and only for its own roof.\n\
                 4. `fetched_at` matters: an ICON-D2 run is hours old by construction, and \
                 the further out the slot the wider the honest band around it.",
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
        .with_server_info(Implementation::new("forecastd", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "# forecastd — the sky, at quarter-hour resolution\n\
             \n\
             ICON-D2 through Open-Meteo, cached per configured location.\n\
             \n\
             ## It serves the sky, not a forecast\n\
             There is deliberately no `get_forecast` tool. What turns a geometric model into \
             a forecast of a **particular** roof is a correction only that roof's own meter \
             can teach — the tree shading the east string, the datasheet that was optimistic, \
             the dust. A route called `forecast` is one somebody eventually plans against \
             uncorrected.\n\
             \n\
             So: `get_weather` is the sky, `get_modelled_production` is what a named geometry \
             would make of it, and the word for the second is **modelled**. A real German \
             roof commonly delivers about nine tenths of it.\n\
             \n\
             ## A caller describes its roof, never its latitude\n\
             The sun position is computed at the coordinates the run was **fetched** for. A \
             household may say what its array is; it may not move its own sky.\n\
             \n\
             ## Tools\n\
             - `list_locations()` — what is configured, and when each run was fetched\n\
             - `get_weather(location)` — irradiance, temperature, cloud cover\n\
             - `get_modelled_production(location, kwp, ac_kw, tilt, azimuth)`\n\
             \n\
             `fetched_at` is not decoration: an ICON-D2 run is hours old by construction.",
        )
    }
}

/// A location nobody configured.
fn not_found(location: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(format!(
        "location_not_found: '{location}' is not one this service fetches. \
         Use list_locations to see what is."
    ))])
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
