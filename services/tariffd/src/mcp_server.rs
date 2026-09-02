//! The Model Context Protocol surface of `tariffd`.
//!
//! Mounted at `/mcp` on the port the daemon already binds. Every tool reads the
//! same `PriceCache` the REST routes read, so the two surfaces cannot answer
//! differently — a second query path is a second thing to keep in step.
//!
//! # What an agent needs told that a REST caller does not
//!
//! A day-ahead curve is easy to read and easy to misread, and every tool here
//! carries the difference:
//!
//! * a slot that is **absent** is a slot nobody has published a price for. It is
//!   not free electricity, and a horizon that runs past tomorrow's auction is
//!   the ordinary case rather than a fault;
//! * `coverage` is a fraction of the window asked for, so a plan built on a
//!   partial answer has to know which part;
//! * a **negative** quarter hour is not a discount on consumption. It is the
//!   wholesale price, and what it changes for a household is the *feed-in* side
//!   (§ 51 EEG), which depends on facts about the plant that this service does
//!   not hold.
//!
//! Prices travel as **decimal strings**. A price that has been through an `f64`
//! is one nobody can reproduce a bill from, and an agent asked to add up a day
//! should be adding up the numbers the invoice will.

use std::sync::Arc;

use hems_core::prelude::Horizon;
use hems_service::mcp::{McpAuth, rfc3339};
use hems_tariff::cache::PriceCache;
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
    /// The same cache the REST surface answers from.
    pub cache: Arc<RwLock<PriceCache>>,
}

/// The window a caller is asking about.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Window {
    /// The first instant of interest, RFC 3339. Absent means now.
    pub from: Option<String>,
    /// How many quarter hours. Absent means 96 — one day.
    pub slots: Option<u32>,
}

impl Window {
    /// The horizon this names.
    ///
    /// An unparseable `from` is an error rather than a silent fallback to now: a
    /// caller that asked about tomorrow and was answered about today would have
    /// no way to tell.
    fn horizon(&self) -> Result<Horizon, McpError> {
        let from = match &self.from {
            Some(text) => {
                time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                    .map_err(|e| {
                        McpError::invalid_params(format!("`from` is not RFC 3339: {e}"), None)
                    })?
            }
            None => time::OffsetDateTime::now_utc(),
        };
        // Capped at four days. A horizon longer than the auction publishes is
        // answered with mostly-absent slots, which is a slow way to be told
        // nothing.
        Ok(Horizon::new(
            from,
            self.slots.unwrap_or(96).min(384) as usize,
        ))
    }
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
    /// A handler over one cache.
    #[must_use]
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// The reconciled day-ahead curve.
    #[tool(
        description = "The reconciled day-ahead price curve over a window, one entry per \
                       quarter hour. Prices are net wholesale ct/kWh as exact decimal \
                       strings. A slot the service cannot price is ABSENT rather than zero — \
                       absent means nobody has published one, never free electricity.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_prices(
        &self,
        Parameters(window): Parameters<Window>,
    ) -> Result<CallToolResult, McpError> {
        let horizon = window.horizon()?;
        let cache = self.state.cache.read().await;
        let points: Vec<serde_json::Value> = horizon
            .slots()
            .filter_map(|slot| {
                cache.at(slot).map(|observed| {
                    serde_json::json!({
                        "slot": rfc3339(slot.start()),
                        "price_ct": observed.price_ct.to_string(),
                        "source": observed.source,
                    })
                })
            })
            .collect();
        json(&serde_json::json!({
            "points": points,
            "asked_for": horizon.len,
            "coverage": cache.coverage(horizon),
            "contiguous_until": horizon
                .get(0)
                .and_then(|first| cache.contiguous_until(first))
                .and_then(|slot| rfc3339(slot.start())),
        }))
    }

    /// How much of a window the service can price.
    #[tool(
        description = "How much of a window this service can price, as a fraction in [0,1], \
                       and the last quarter hour reachable from the start without a gap. Ask \
                       this before planning against a horizon: a partial answer is normal \
                       past tomorrow's auction and is not a fault.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_coverage(
        &self,
        Parameters(window): Parameters<Window>,
    ) -> Result<CallToolResult, McpError> {
        let horizon = window.horizon()?;
        let cache = self.state.cache.read().await;
        json(&serde_json::json!({
            "coverage": cache.coverage(horizon),
            "asked_for": horizon.len,
            "cached_slots": cache.len(),
            "contiguous_until": horizon
                .get(0)
                .and_then(|first| cache.contiguous_until(first))
                .and_then(|slot| rfc3339(slot.start())),
        }))
    }

    /// The quarter hours where the wholesale price is below zero.
    #[tool(
        description = "Quarter hours in the window whose wholesale price is negative. What \
                       this changes for a household is the FEED-IN side: § 51 EEG sets the \
                       anzulegender Wert to zero in such a quarter hour, but only once it \
                       reaches that plant — which turns on when its intelligent metering \
                       system was fitted and is a fact about the plant, not about the price. \
                       It is not a discount on consumption.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn negative_quarter_hours(
        &self,
        Parameters(window): Parameters<Window>,
    ) -> Result<CallToolResult, McpError> {
        let horizon = window.horizon()?;
        let cache = self.state.cache.read().await;
        let hours: Vec<serde_json::Value> = horizon
            .slots()
            .filter_map(|slot| cache.at(slot).map(|o| (slot, o)))
            .filter(|(_, o)| o.price_ct.is_sign_negative())
            .map(|(slot, o)| {
                serde_json::json!({
                    "slot": rfc3339(slot.start()),
                    "price_ct": o.price_ct.to_string(),
                })
            })
            .collect();
        json(&serde_json::json!({
            "negative": hours,
            "count": hours.len(),
            "asked_for": horizon.len,
            "coverage": cache.coverage(horizon),
        }))
    }

    /// Which sources the cache is currently answering from.
    #[tool(
        description = "Which of the five published sources the cached prices actually came \
                       from, and how many quarter hours each contributed. The sources are \
                       reconciled under a written trust order, so a slot has one price and \
                       one provenance.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_sources(&self) -> Result<CallToolResult, McpError> {
        let cache = self.state.cache.read().await;
        let mut counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        // The cache has no iterator over its own entries, so this walks the two
        // days it is allowed to hold. Cheap, and it means this tool cannot
        // disagree with `get_prices` about what is in there.
        let now = time::OffsetDateTime::now_utc();
        let span = Horizon::new(now - time::Duration::days(2), 96 * 4);
        for slot in span.slots() {
            if let Some(observed) = cache.at(slot) {
                *counts
                    .entry(format!("{:?}", observed.source).to_lowercase())
                    .or_default() += 1;
            }
        }
        json(&serde_json::json!({
            "sources": counts,
            "cached_slots": cache.len(),
        }))
    }
}

// `#[prompt]` generates an associated function with no doc comment this crate
// can attach, and requires `&self` whether or not the body reads it.
#[allow(missing_docs, clippy::unused_self)]
#[prompt_router]
impl Handler {
    #[prompt(
        name = "read-a-price-curve",
        description = "Read a day-ahead curve without mistaking an absent slot for a free one"
    )]
    fn read_a_price_curve(&self) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(
                Role::User,
                "I want to know when electricity is cheapest tomorrow.",
            ),
            PromptMessage::new_text(
                Role::Assistant,
                "1. `get_coverage` first. If coverage is below 1, part of the window has no \
                 published price and any ranking over it is a ranking over a guess.\n\
                 2. `get_prices` for the covered part. Prices are net **wholesale** ct/kWh: a \
                 household pays that plus the supplier's markup, the network charge, the \
                 levies and VAT, so the *spread* is meaningful and the level is not.\n\
                 3. An absent slot is absent. Do not read it as zero, and do not average over \
                 it.\n\
                 4. A negative quarter hour is a wholesale price. It does not make consumption \
                 free, and what it changes on the feed-in side depends on the plant.",
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
        .with_server_info(Implementation::new("tariffd", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "# tariffd — day-ahead prices for a German household\n\
             \n\
             Five published sources fetched, reconciled under a written trust order and \
             cached two days each way. Every price is **net wholesale** ct/kWh as an exact \
             decimal string.\n\
             \n\
             ## What the numbers are, and are not\n\
             - A **wholesale** price is not what a household pays. Add the supplier's markup, \
             the network charge (§ 14a Modul 1/2/3), the levies and VAT. The *spread* between \
             quarter hours is what a plan can act on; the level is not.\n\
             - An **absent** slot is one nobody has published a price for — past tomorrow's \
             auction, or after an outage. It is not zero and not free.\n\
             - `coverage` is a fraction of the window that was asked for. Below 1 means part \
             of the answer is missing, and which part matters.\n\
             - A **negative** quarter hour is the wholesale price going below zero. It does \
             not make consumption free. What it changes is the feed-in side: § 51 EEG zeroes \
             the anzulegender Wert there, but only once it reaches that plant, which turns on \
             when the plant's intelligent metering system was fitted.\n\
             \n\
             ## Tools\n\
             - `get_prices(from, slots)` — the curve, with coverage\n\
             - `get_coverage(from, slots)` — how much of a window is priced\n\
             - `negative_quarter_hours(from, slots)` — where the wholesale price is below zero\n\
             - `list_sources()` — which sources the cache is answering from\n\
             \n\
             Since 01.10.2025 the market time unit is a quarter hour, which is why everything \
             here is quarter-hourly.",
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
