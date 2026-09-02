//! What the box will tell you about itself.
//!
//! Small on purpose. The fleet's view of a household is `obsd`'s and the
//! household's own history is `histd`'s; what a box needs locally is the answer
//! to "what is it doing, and why" — which is the one question neither of those
//! can answer while the WAN is down, and the one an installer standing next to
//! the box is actually asking.
//!
//! # One write, and it is a *desire* rather than a setpoint
//!
//! No endpoint here commands a device. A setpoint that did not come through the
//! arbiter would not have been through the guard, which is the one property this
//! whole workspace is built to keep: `[BK6-22-300 A1 4.6 S. 3]` makes a network
//! operator's reduction win over market-driven control, and an HTTP handler that
//! could write a value straight to a driver would be a second control plane
//! nobody audited.
//!
//! `/v1/overrides` is therefore the only write, and it is safe for exactly that
//! reason: what it changes is what the arbiter *wants*, which the guard then
//! narrows like anything else. A household in the middle of a § 14a reduction
//! that presses boost gets as much as the reduction allows and not a watt more.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::routing::{get, put};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::runtime::control::Status;

/// What the API needs to answer.
#[derive(Clone)]
pub struct Local {
    status: Arc<Mutex<Status>>,
    site: String,
    ski: Option<String>,
    overrides: crate::runtime::overrides::Overrides,
}

impl Local {
    /// The surface over this box's own status.
    #[must_use]
    pub fn new(
        status: Arc<Mutex<Status>>,
        site: String,
        ski: Option<String>,
        overrides: crate::runtime::overrides::Overrides,
    ) -> Self {
        Self {
            status,
            site,
            ski,
            overrides,
        }
    }
}

/// One tick, as JSON.
#[derive(Debug, Serialize)]
pub struct StatusBody {
    /// Which household.
    pub site: String,
    /// The box's EEBUS Subject Key Identifier, where it has one.
    ///
    /// What an installer gives the metering point operator so a Steuerbox can be
    /// told to trust this box. Field reports make that exchange the single most
    /// common § 14a commissioning failure there is, which is why it is on the
    /// first page a box will show rather than in a log.
    pub ski: Option<String>,
    /// When the last tick ran, RFC 3339.
    pub at: Option<String>,
    /// What every asset was last told, in watts.
    pub commanded_w: std::collections::BTreeMap<String, f64>,
    /// The assets no driver has been heard from.
    pub silent: Vec<String>,
    /// Devices whose available power is a nameplate rather than a reading.
    ///
    /// Worth a field of its own: a curtailed inverter that cannot say what it
    /// *could* produce is one whose curtailment lifts on an assumption, and a
    /// household is entitled to know which of its devices are in that position.
    pub assumed_available: Vec<String>,
    /// Controllable devices no driver speaks for.
    ///
    /// The first thing to look at when a device is not doing what this page says
    /// it was told to: the setpoint was decided and had nowhere to go.
    pub undriven: Vec<String>,
    /// The § 14a ceiling in force, kW.
    pub steuve_ceiling_kw: Option<f64>,
    /// What the controllable devices may draw in total, surplus included, kW.
    pub steuve_budget_kw: Option<f64>,
    /// The netzwirksamer Leistungsbezug right now, kW — `[A1 2.3]`.
    pub netzwirksam_kw: Option<f64>,
    /// How far the grid meter is from the sum of the assets, W.
    pub balance_residual_w: Option<f64>,
    /// How old the plan in force is, minutes — `null` where the box has never
    /// published one.
    ///
    /// The seam a box is most likely to be quietly broken at: one that never
    /// plans looks exactly like one that plans badly, and the difference is a
    /// whole tariff's worth of money.
    pub minutes_without_a_plan: Option<i64>,
    /// What the plan in force expects its horizon to cost, euros — wear,
    /// discomfort and undelivered service included.
    pub plan_expected_eur: Option<f64>,
    /// What the same horizon would cost with no energy manager, euros.
    ///
    /// The comparison delivers the **same service**: the car still reaches its
    /// target and the house is still warm, and the baseline household lives
    /// under the same § 14a and § 9 EEG limits. A saving measured against a
    /// household nobody is allowed to be is an advertisement.
    pub plan_baseline_eur: Option<f64>,
    /// How many control ticks took longer than a control period.
    pub overruns: u64,
}

/// The routes this daemon adds to the shared health surface.
pub fn router(local: Local) -> axum::Router {
    axum::Router::new()
        .route("/v1/status", get(status))
        .route("/v1/overrides", get(list_overrides).delete(clear_overrides))
        .route(
            "/v1/overrides/{asset}",
            put(set_override).delete(clear_override),
        )
        .with_state(local)
}

/// What the household is asking for, and until when.
#[derive(Debug, serde::Deserialize)]
pub struct OverrideBody {
    /// `boost`, `pause` or `away`.
    pub what: hems_core::setpoint::UserOverride,
    /// How long, in minutes. Absent takes the default; anything over a day is
    /// clamped, because longer than that is a statement about the house rather
    /// than about this afternoon.
    #[serde(default)]
    pub minutes: Option<i64>,
}

/// One override in force.
#[derive(Debug, serde::Serialize)]
pub struct OverrideView {
    /// Which asset.
    pub asset: String,
    /// What was asked for.
    pub what: hems_core::setpoint::UserOverride,
    /// When it stops applying, RFC 3339.
    pub until: String,
}

/// Ask for one.
async fn set_override(
    State(local): State<Local>,
    axum::extract::Path(asset): axum::extract::Path<String>,
    Json(body): Json<OverrideBody>,
) -> Result<Json<OverrideView>, axum::http::StatusCode> {
    let asset = hems_core::prelude::AssetId::new(&asset)
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    let now = time::OffsetDateTime::now_utc();
    let held = local
        .overrides
        .set(
            asset.clone(),
            body.what,
            body.minutes.map(time::Duration::minutes),
            now,
        )
        .await;
    // Logged, because an override is a *decision somebody made* and the reason
    // chain a household is shown has to be able to say so.
    tracing::info!(%asset, what = ?held.what, until = %rfc3339(held.until), "a household override");
    Ok(Json(OverrideView {
        asset: asset.to_string(),
        what: held.what,
        until: rfc3339(held.until),
    }))
}

/// Withdraw one.
async fn clear_override(
    State(local): State<Local>,
    axum::extract::Path(asset): axum::extract::Path<String>,
) -> axum::http::StatusCode {
    let Ok(asset) = hems_core::prelude::AssetId::new(&asset) else {
        return axum::http::StatusCode::BAD_REQUEST;
    };
    if local.overrides.clear(&asset).await {
        tracing::info!(%asset, "a household override was withdrawn");
        axum::http::StatusCode::NO_CONTENT
    } else {
        axum::http::StatusCode::NOT_FOUND
    }
}

/// Withdraw all of them — the "back to normal" button.
async fn clear_overrides(State(local): State<Local>) -> axum::http::StatusCode {
    let n = local.overrides.clear_all().await;
    tracing::info!(withdrawn = n, "every household override was withdrawn");
    axum::http::StatusCode::NO_CONTENT
}

/// What is in force.
async fn list_overrides(State(local): State<Local>) -> Json<Vec<OverrideView>> {
    let now = time::OffsetDateTime::now_utc();
    Json(
        local
            .overrides
            .all(now)
            .await
            .into_iter()
            .map(|(asset, held)| OverrideView {
                asset: asset.to_string(),
                what: held.what,
                until: rfc3339(held.until),
            })
            .collect(),
    )
}

/// RFC 3339, or an empty string for an instant that cannot be formatted.
fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// What the box is doing.
async fn status(State(local): State<Local>) -> Json<StatusBody> {
    let held = local.status.lock().await;
    Json(StatusBody {
        site: local.site.clone(),
        ski: local.ski.clone(),
        at: held.at.map(rfc3339),
        commanded_w: held
            .commanded
            .iter()
            .map(|(id, p)| (id.to_string(), p.get()))
            .collect(),
        silent: held.silent.iter().map(ToString::to_string).collect(),
        assumed_available: held
            .assumed_available
            .iter()
            .map(ToString::to_string)
            .collect(),
        undriven: held.undriven.iter().map(ToString::to_string).collect(),
        steuve_ceiling_kw: held.steuve_ceiling.map(hems_core::prelude::Power::kw),
        steuve_budget_kw: held.steuve_budget.map(hems_core::prelude::Power::kw),
        netzwirksam_kw: held.netzwirksam.map(hems_core::prelude::Power::kw),
        balance_residual_w: held.balance_residual.map(hems_core::prelude::Power::get),
        // `i64::MAX` is the sentinel for "never planned", and it must not reach
        // a screen as a number: nine quintillion minutes is not a duration
        // anybody can act on, and `null` is the honest JSON for "there has not
        // been one".
        minutes_without_a_plan: (held.minutes_without_a_plan != i64::MAX)
            .then_some(held.minutes_without_a_plan),
        plan_expected_eur: held.plan_expected_eur,
        plan_baseline_eur: held.plan_baseline_eur,
        overruns: held.overruns,
    })
}
