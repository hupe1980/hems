//! The Model Context Protocol surface of `histd`.
//!
//! Mounted at `/mcp` on the port the daemon already binds, over the same store
//! the REST routes read, with the same off-the-runtime discipline: `rusqlite` is
//! synchronous, so a query left on a runtime thread occupies one for the whole
//! of it.
//!
//! # What it will not serve
//!
//! There is no Data Act export here. Article 4 of Regulation (EU) 2023/2854 is a
//! right of the **user** — the household — and an agent holding a fleet token is
//! not one. The § 14a Nachweis is a different thing and is served: it is the
//! record of what the *network operator* itself commanded and what the
//! connection point drew, and it is theirs to check.
//!
//! That split is `hems_service::Authority`'s, not this module's; what this
//! module does is refuse to route around it.

use std::sync::Arc;

use crate::store::{Db, Store, StoreError};
use hems_service::mcp::{McpAuth, rfc3339, rfc3339_opt};
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

/// What the tools read.
#[derive(Clone)]
pub struct State {
    /// The same database the REST surface answers from.
    pub db: Db,
    /// How each caller is authorised.
    ///
    /// Not one authority for the whole surface: every tool resolves the request
    /// that reached *it*, so this surface has exactly the reach of whoever is on
    /// the other end of it (D111).
    pub auth: hems_service::McpAuth,
}

/// A site and an optional window.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteWindow {
    /// The site identifier.
    pub site: String,
    /// The earliest instant of interest, RFC 3339. Absent means from the start.
    pub from: Option<String>,
    /// The latest, RFC 3339. Absent means up to now.
    pub to: Option<String>,
}

impl SiteWindow {
    /// The window, parsed.
    fn range(
        &self,
    ) -> Result<(Option<time::OffsetDateTime>, Option<time::OffsetDateTime>), McpError> {
        Ok((parse(self.from.as_deref())?, parse(self.to.as_deref())?))
    }
}

/// Just a site.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SiteParams {
    /// The site identifier.
    pub site: String,
}

/// RFC 3339, refused rather than defaulted.
fn parse(text: Option<&str>) -> Result<Option<time::OffsetDateTime>, McpError> {
    text.map(|t| {
        time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339)
            .map_err(|e| McpError::invalid_params(format!("not RFC 3339: {e}"), None))
    })
    .transpose()
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
    /// A handler over one database.
    #[must_use]
    pub fn new(state: Arc<State>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// The § 14a Nachweis for a site and a window.
    #[tool(
        description = "The § 14a evidence document [A1 7.2] for one site and window: every \
                       control event with the ceiling that was commanded, the minimum the \
                       household was owed under [A1 4.5], when it was acted on, and whether \
                       the connection point stayed inside it. This is the record a network \
                       operator settles on and may read; it is not the household's own \
                       consumption history.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_nachweis(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SiteWindow>,
    ) -> Result<CallToolResult, McpError> {
        self.deny_unless_may_read(&parts, &p.site)?;
        let (from, to) = p.range()?;
        let site = p.site.clone();
        let value = self
            .read(move |store| crate::export::nachweis(store, &site, from, to))
            .await?;
        json(&value)
    }

    /// The control events themselves.
    #[tool(
        description = "Control events for one site and window, newest first: which rule (an \
                       operator's LPC limit, or the household's own failsafe — different \
                       events in the record), the first and strictest ceiling commanded, \
                       whether it went below the [A1 4.5] minimum, and how many \
                       minute-resolution compliance samples back it up. An OPEN event has no \
                       released_at: the reduction is still running.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_control_events(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SiteWindow>,
    ) -> Result<CallToolResult, McpError> {
        self.deny_unless_may_read(&parts, &p.site)?;
        let (from, to) = p.range()?;
        let site = p.site.clone();
        let events = self
            .read(move |store| store.control_events(&site, from, to))
            .await?;
        let rows: Vec<serde_json::Value> = events
            .iter()
            .map(|stored| {
                let e = &stored.event;
                serde_json::json!({
                    "id": stored.id,
                    "rule": format!("{:?}", e.rule),
                    "received_at": rfc3339(e.received_at),
                    "released_at": rfc3339_opt(e.released_at),
                    "first_ceiling_kw": e.first_ceiling().kw(),
                    "strictest_ceiling_kw": e.strictest_ceiling().kw(),
                    // The minimum belongs to the *ceiling* that was commanded,
                    // not to the event: `[A1 4.5.2]`'s figure grows with the
                    // number of controllable devices, so a reduction that
                    // outlasts a device being added is measured against two.
                    "minimum_power_kw": e.ceilings.first().map(|c| c.minimum_power.kw()),
                    "below_minimum": e.below_minimum(),
                    "fully_compliant": e.fully_compliant(),
                    "samples": e.samples.len(),
                })
            })
            .collect();
        json(&serde_json::json!({
            "site": p.site,
            "events": rows,
            "count": rows.len(),
        }))
    }

    /// The quarter-hour registers a settlement is computed from.
    #[tool(
        description = "Quarter-hour meter registers for one site and window: grid draw and \
                       feed-in, and what the storage system and the charge point drew and \
                       gave. Every quantity is an exact DECIMAL STRING in kWh — a settlement \
                       that went through a float is one nobody can reproduce. These are what \
                       MiSpeL's Abgrenzung and § 42c's allocation are computed from.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_quarter_hours(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SiteWindow>,
    ) -> Result<CallToolResult, McpError> {
        self.deny_unless_may_read(&parts, &p.site)?;
        let (from, to) = p.range()?;
        let site = p.site.clone();
        let quarters = self
            .read(move |store| store.quarter_hours(&site, from, to))
            .await?;
        let rows: Vec<serde_json::Value> = quarters
            .iter()
            .map(|q| {
                serde_json::json!({
                    "slot": rfc3339(q.slot.start()),
                    "grid_draw_kwh": q.grid_draw.to_string(),
                    "grid_feed_in_kwh": q.grid_feed_in.to_string(),
                    "device_consumption_kwh": q.device_consumption.to_string(),
                    "device_generation_kwh": q.device_generation.to_string(),
                    "anzulegender_wert_ct": q.anzulegender_wert.to_string(),
                    "spot_price_ct": q.spot_price.to_string(),
                })
            })
            .collect();
        json(&serde_json::json!({
            "site": p.site,
            "quarter_hours": rows,
            "count": rows.len(),
        }))
    }

    /// How much record there is, and how far back.
    #[tool(
        description = "How much § 14a record this service holds for one site, and the \
                       earliest event in it. [A1 7.3] asks for two years; an earliest event \
                       younger than that is a box that has not been running that long, not a \
                       record that was discarded.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_coverage(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SiteParams>,
    ) -> Result<CallToolResult, McpError> {
        self.deny_unless_may_read(&parts, &p.site)?;
        let site = p.site.clone();
        let (count, earliest) = self
            .read(move |store| {
                Ok((
                    store.control_event_count(&site)?,
                    store.earliest_event(&site)?,
                ))
            })
            .await?;
        json(&serde_json::json!({
            "site": p.site,
            "control_events": count,
            "earliest_event": rfc3339_opt(earliest),
            "retention": "two years from the day an event closed, [A1 7.3]",
        }))
    }

    /// Refuse a site this **caller** may not read.
    ///
    /// The authority comes from the request that reached this tool, so a
    /// household's own token reads its own site and an operator's reads the
    /// households in its scope — the same answer the REST route would give the
    /// same token.
    fn deny_unless_may_read(&self, parts: &Parts, site: &str) -> Result<(), McpError> {
        let caller = hems_service::mcp::caller(&self.state.auth, parts)?;
        if caller.may_read(site) {
            Ok(())
        } else {
            Err(McpError::invalid_params(
                format!("{} is not authorised for site {site:?}", caller.subject()),
                None,
            ))
        }
    }

    /// One query, off the runtime, on a connection of its own.
    ///
    /// `rusqlite` is synchronous: a query left on a runtime thread occupies one
    /// for the whole of it, and a two-year Nachweis is not a fast query.
    async fn read<T, F>(&self, work: F) -> Result<T, McpError>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    {
        let db = self.state.db.clone();
        tokio::task::spawn_blocking(move || work(&db.connect()?))
            .await
            .map_err(|e| McpError::internal_error(format!("the query panicked: {e}"), None))?
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }
}

// `#[prompt]` generates an associated function with no doc comment this crate
// can attach, and requires `&self` whether or not the body reads it.
#[allow(missing_docs, clippy::unused_self)]
#[prompt_router]
impl Handler {
    #[prompt(
        name = "answer-a-network-operator",
        description = "Produce the § 14a evidence a network operator has asked for"
    )]
    fn answer_a_network_operator(&self) -> Vec<PromptMessage> {
        vec![
            PromptMessage::new_text(
                Role::User,
                "The network operator is asking whether we respected a reduction last month.",
            ),
            PromptMessage::new_text(
                Role::Assistant,
                "1. `get_coverage(site)` first, so the answer can say what period the record \
                 actually spans.\n\
                 2. `get_nachweis(site, from, to)` for the document itself. It carries every \
                 commanded ceiling in sequence and the minute-resolution trace of what the \
                 connection point drew.\n\
                 3. Read `rule` carefully. An operator's **LPC** limit and the household's own \
                 **failsafe** look identical at the connection point and are different events: \
                 the second is the box restraining itself because it could not hear anybody, \
                 and reporting it as a control action attributes it to the operator.\n\
                 4. `below_minimum` means the commanded ceiling was under [A1 4.5]. hems \
                 applies it anyway — refusing a network operator is not a box's decision — \
                 but the entitlement is the customer's and it belongs in the answer.\n\
                 5. Do not offer the household's consumption history. That is the Data Act \
                 export, it is the household's right under Article 4, and an operator has no \
                 claim on it.",
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
        .with_server_info(Implementation::new("histd", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "# histd — the two years a network operator may ask about\n\
             \n\
             Every § 14a control event with its minute-resolution compliance trace [A1 7.2], \
             kept for the two years of [A1 7.3], and the quarter-hour registers a settlement \
             is computed from.\n\
             \n\
             ## Two records, two rights\n\
             - The **Nachweis** is what the network operator commanded and what the \
             connection point drew. It is theirs to check, and this surface serves it.\n\
             - The **Data Act export** is the household's whole consumption history, a right \
             of the *user* under Article 4 of Regulation (EU) 2023/2854. It is deliberately \
             not a tool here: an agent holding a fleet token is not a household.\n\
             \n\
             ## An operator's limit and a household's failsafe are different events\n\
             They look identical at the connection point. `rule = Lpc` is the operator \
             asking; `rule = Failsafe` is the box restraining itself because it could not \
             hear one. Reporting the second as a control action attributes it to somebody who \
             did not do it.\n\
             \n\
             ## Below the minimum is not the box misbehaving\n\
             `below_minimum` means the commanded ceiling was under what [A1 4.5.2] says the \
             household is owed. hems applies it — refusing a network operator is not a \
             decision a box takes — and records the fact, because the entitlement is the \
             customer's.\n\
             \n\
             ## Quantities are exact decimals\n\
             Every kWh and ct/kWh is a decimal **string**. A settlement that went through a \
             float is a settlement nobody can reproduce.\n\
             \n\
             ## Tools\n\
             - `get_nachweis(site, from, to)` — the § 14a document\n\
             - `list_control_events(site, from, to)` — the events, newest first\n\
             - `get_quarter_hours(site, from, to)` — the settlement registers\n\
             - `get_coverage(site)` — how much record there is, and how far back",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(credentials: &hems_service::Credentials) -> Handler {
        Handler::new(std::sync::Arc::new(State {
            db: crate::store::Db::at("/nonexistent/never-opened.sqlite"),
            auth: hems_service::McpAuth::per_caller(
                &hems_service::McpSettings {
                    enabled: true,
                    token: None,
                },
                credentials,
            )
            .expect("credentials to authorise against"),
        }))
    }

    fn parts(token: &str) -> Parts {
        http::Request::builder()
            .uri("/mcp")
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    async fn nachweis(h: &Handler, token: &str, site: &str) -> Result<CallToolResult, McpError> {
        h.get_nachweis(
            Extension(parts(token)),
            Parameters(SiteWindow {
                site: site.to_owned(),
                from: None,
                to: None,
            }),
        )
        .await
    }

    fn credentials() -> hems_service::Credentials {
        hems_service::Credentials::default()
            .with_site("haus-1", "tok-1")
            .with_operator("tok-netz")
    }

    #[tokio::test]
    async fn a_household_reads_its_own_site_and_no_other() {
        // One surface, two callers, two answers — which is the whole of the
        // correction to the withdrawn D107. The database path is deliberately
        // unopenable: a check that runs *before* the read cannot be satisfied by
        // an empty result, and one that ran after would fail here with a store
        // error rather than an authorisation error.
        let h = handler(&credentials());

        assert!(
            nachweis(&h, "tok-1", "haus-1").await.is_err(),
            "its own site gets past the gate and fails on the database"
        );
        let refused = nachweis(&h, "tok-1", "haus-2")
            .await
            .expect_err("another site is refused");
        assert!(
            refused.message.contains("not authorised"),
            "and refused by the authority rather than by the store: {}",
            refused.message
        );
    }

    #[tokio::test]
    async fn an_operator_is_not_scoped_to_one_household() {
        // The other half: an operator answering a network operator's question
        // reads any household's Nachweis in its scope, which is what the
        // credential is for. It gets past the authorisation check and fails on
        // the unopenable database, which is how this test tells the two apart.
        let h = handler(&credentials());
        let attempted = nachweis(&h, "tok-netz", "haus-2")
            .await
            .expect_err("the database is not there");
        assert!(
            !attempted.message.contains("not authorised"),
            "an operator is refused by the database, never by the authority: {}",
            attempted.message
        );
    }

    #[tokio::test]
    async fn one_tenants_operator_cannot_read_anothers_household() {
        // Two operators on one daemon. Neither is the other's.
        let h = handler(&hems_service::Credentials::default().with_operator_of(
            hems_service::SiteScope::Tenant {
                name: "nord".into(),
                sites: ["haus-1".to_owned()].into_iter().collect(),
            },
            "tok-nord",
        ));
        let refused = nachweis(&h, "tok-nord", "haus-3")
            .await
            .expect_err("out of scope");
        assert!(
            refused.message.contains("not authorised"),
            "{}",
            refused.message
        );
    }

    #[tokio::test]
    async fn a_token_this_daemon_never_issued_reaches_no_tool() {
        let h = handler(&credentials());
        assert!(nachweis(&h, "tok-invented", "haus-1").await.is_err());
    }
}
