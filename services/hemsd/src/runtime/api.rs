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
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::routing::{get, put};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::runtime::control::Status;
use crate::runtime::ship::Trust;
use time::OffsetDateTime;

/// What the API needs to answer.
#[derive(Clone)]
pub struct Local {
    status: Arc<Mutex<Status>>,
    site: String,
    ski: Option<String>,
    overrides: crate::runtime::overrides::Overrides,
    /// Approving a Steuerbox without restarting the box.
    ///
    /// `None` where this household has no EEBUS identity at all, which is a box
    /// with no § 14a driver and nothing to pair.
    trust: Option<crate::runtime::ship::Trust>,
    /// The credentials this box answers to, so a household can see which energy
    /// managers it has connected and withdraw one.
    access: crate::runtime::access::LocalAccess,
    /// The box's own measurement series, where it keeps one.
    ///
    /// Read-only here. The Data Act gives a user the data their product
    /// generates, and on the box that means the household's own local API rather
    /// than a fleet service — this is the one-second half of it.
    series: Option<Arc<crate::series::Series>>,
}

impl Local {
    /// The surface over this box's own status.
    #[must_use]
    pub fn new(
        status: Arc<Mutex<Status>>,
        site: String,
        ski: Option<String>,
        overrides: crate::runtime::overrides::Overrides,
        trust: Option<crate::runtime::ship::Trust>,
        access: crate::runtime::access::LocalAccess,
        series: Option<Arc<crate::series::Series>>,
    ) -> Self {
        Self {
            status,
            site,
            ski,
            overrides,
            trust,
            access,
            series,
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
    /// Controllable devices whose **consumption** the guard had to assume.
    ///
    /// The consumption side of the same honesty: a silent controllable device is
    /// taken to be drawing its nameplate power, which is the safe answer and an
    /// expensive one — every watt of it is § 14a budget spent on a device that
    /// may be doing nothing. `assumed_available` is the generation side (R20).
    pub assumed_nominal: Vec<String>,
    /// Devices that answered their last setpoint and did not act on it, with
    /// what they said about it.
    ///
    /// The one fault the box is otherwise blind to. A driver that reports a
    /// refusal is easy to see; a device that acknowledges the write, stores the
    /// setpoint and never switches it on answers exactly like one that obeyed,
    /// and the disagreement only surfaces as a meter that will not match the
    /// plan. Separate from `silent`, because a silent device is a network fault
    /// and this one is a device that has to be commanded some other way.
    pub disobedient: std::collections::BTreeMap<String, String>,
    /// Controllable devices no driver speaks for.
    ///
    /// The first thing to look at when a device is not doing what this page says
    /// it was told to: the setpoint was decided and had nowhere to go.
    pub undriven: Vec<String>,
    /// Whether the roof is still delivering what it used to.
    ///
    /// The performance ratio of IEC 61724-1 against this array's own seasonal
    /// baseline, and a verdict. It is on the status page rather than in a log
    /// because it is the one fault a household can *act* on — the others here
    /// are for an installer — and because the box is otherwise the last thing
    /// that would notice: its own forecast corrector is built to learn a lower
    /// roof and carry on planning well (D199).
    pub roof_health: Option<hems_forecast::Health>,
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
    /// How exposed this connection is to § 14a control — `null` until the box
    /// has any record to draw on.
    pub exposure: Option<ExposureBody>,
    /// Which rule is holding each asset's setpoint, where one is.
    ///
    /// The answer to *why is my car charging slowly*, which is the commonest
    /// question a household has and the one a screen of watts cannot answer. It
    /// is the same reason chain a setpoint carries (P7), summarised per asset so
    /// a page does not have to reconstruct it.
    pub overriding: std::collections::BTreeMap<String, String>,
    /// What an external Customer Energy Manager is doing over S2, where one is
    /// connected.
    ///
    /// A household that has delegated its optimising to somebody else is
    /// entitled to see that it has, and to see which of its devices that manager
    /// is actually driving — an S2 session that connected, chose a control type
    /// and then instructed nothing looks from every other screen exactly like
    /// one that is working.
    pub cem: crate::runtime::s2::CemStatus,
}

/// How often the operator reduces *this* household, and when.
///
/// The question `[A1 8.4]` cannot answer. What the operator publishes is a
/// monthly aggregate over a whole Netzbereich with no timestamps in it, so the
/// only record of when this connection was reduced is the one the box keeps for
/// `[A1 7.2]` anyway. Reported, and deliberately not planned against (D129).
#[derive(Debug, Serialize)]
pub struct ExposureBody {
    /// How many days of record the figures are drawn from.
    ///
    /// The denominator, on the page. A share whose denominator is invisible
    /// cannot be checked, which is R28.
    pub days_of_record: i64,
    /// Hours the household was reduced for, over that record.
    pub hours_reduced: f64,
    /// The quarter hour of the local day it happens in most often, as `HH:MM` —
    /// `null` where it has never happened.
    pub busiest_quarter_hour: Option<String>,
    /// How often a reduction is in force in that quarter hour, in `[0, 1]`.
    pub busiest_frequency: f64,
    /// The ceiling typically commanded there, kW.
    pub typical_ceiling_kw: Option<f64>,
}

/// The routes this daemon adds to the shared health surface.
pub fn router(local: Local) -> axum::Router {
    axum::Router::new()
        .route("/v1/status", get(status))
        .route("/v1/pairing", get(list_trusted).post(approve_peer))
        .route("/v1/pairing/{ski}", axum::routing::delete(forget_peer))
        .route("/v1/pairing/{ski}/refuse", axum::routing::post(refuse_peer))
        .route("/v1/overrides", get(list_overrides).delete(clear_overrides))
        .route("/v1/series/{point}", get(series))
        .route("/v1/managers", get(list_managers))
        .route(
            "/v1/managers/{name}",
            put(connect_manager).delete(forget_manager),
        )
        .route(
            "/v1/overrides/{asset}",
            put(set_override).delete(clear_override),
        )
        .with_state(local)
}

/// The energy managers this household has connected.
///
/// The § 14a side has had this since D102: a Steuerbox is trusted by SKI, shown
/// on a screen and forgotten from one. A Customer Energy Manager drives the same
/// house through `/s2/{asset}` and, until now, held the household's *own*
/// credential — so a household could see that something was driving its battery
/// and not what, and could withdraw one only by rotating the token every other
/// surface uses (D188).
async fn list_managers(
    State(local): State<Local>,
) -> Result<Json<Vec<crate::runtime::access::Manager>>, StatusCode> {
    local.access.managers().await.map(Json).map_err(|error| {
        tracing::error!(%error, "the manager list could not be read");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

/// Connect one, and hand back the credential it presents.
///
/// **The only time the token is shown.** It is not stored in a form this API can
/// give back, for the same reason the EEBUS private key is not: a credential an
/// endpoint can be asked for is a credential an endpoint can leak. Naming the
/// same manager again issues a new one, which is how a household rotates it.
async fn connect_manager(
    State(local): State<Local>,
    Path(name): Path<String>,
) -> Result<Json<Connected>, (StatusCode, String)> {
    if name.trim().is_empty() || name.len() > 64 {
        return Err((
            StatusCode::BAD_REQUEST,
            "a manager needs a name a household would recognise, of at most 64 characters".into(),
        ));
    }
    let token = local
        .access
        .connect_manager(&name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(Connected { name, token }))
}

/// Withdraw one. The credential stops working on the next request.
async fn forget_manager(
    State(local): State<Local>,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let existed = local
        .access
        .forget_manager(&name)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(if existed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

/// A manager that has just been connected, and the credential it presents.
#[derive(Debug, Serialize)]
pub struct Connected {
    /// What the household called it.
    pub name: String,
    /// The bearer token it presents. Shown once and never again.
    pub token: String,
}

/// The window a series is asked for.
///
/// Named for the asking, not the answer: what comes back is a
/// [`crate::series::Window`], which carries the resolution the box could answer
/// at as well as the readings.
#[derive(Debug, serde::Deserialize)]
pub struct Asked {
    /// The first instant, RFC 3339. Absent means an hour ago, which is what a
    /// screen wants and what stops an unbounded default returning a week.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub from: Option<time::OffsetDateTime>,
    /// The instant after the last, RFC 3339. Absent means now.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub to: Option<time::OffsetDateTime>,
}

/// One point of measurement over a window — `grid`, `netzwirksam`, or an asset.
///
/// The household's own history, from the box that took it. A box with no series
/// configured answers `404` rather than an empty list: *nothing was kept* and
/// *nothing happened* are different answers, and an empty array would say the
/// second.
///
/// The answer carries its own `resolution`, because the box keeps days of
/// seconds and years of quarter hours and a long window is answered from the
/// second tier. A client that ignored it would draw a year of quarter-hourly
/// means and label it one-second data.
async fn series(
    State(local): State<Local>,
    Path(point): Path<String>,
    Query(window): Query<Asked>,
) -> Result<axum::Json<crate::series::Window>, StatusCode> {
    let Some(series) = local.series.as_ref() else {
        return Err(StatusCode::NOT_FOUND);
    };
    let now = time::OffsetDateTime::now_utc();
    let to = window.to.unwrap_or(now);
    let from = window.from.unwrap_or(to - time::Duration::hours(1));
    if to <= from {
        return Err(StatusCode::BAD_REQUEST);
    }
    series
        .window(&point, from, to)
        .map(axum::Json)
        .map_err(|error| {
            tracing::warn!(%error, %point, "the measurement series could not be read");
            StatusCode::INTERNAL_SERVER_ERROR
        })
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
        assumed_nominal: held
            .assumed_nominal
            .iter()
            .map(ToString::to_string)
            .collect(),
        disobedient: held
            .disobedient
            .iter()
            .map(|(id, why)| (id.to_string(), why.clone()))
            .collect(),
        undriven: held.undriven.iter().map(ToString::to_string).collect(),
        roof_health: held.roof_health,
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
        cem: held.cem.clone(),
        overriding: held
            .overriding
            .iter()
            .map(|(id, rule)| (id.to_string(), rule.clone()))
            .collect(),
        exposure: held.exposure.as_ref().map(|e| ExposureBody {
            days_of_record: e.days_of_record,
            hours_reduced: e.hours_reduced,
            busiest_quarter_hour: e.busiest_quarter_hour.map(quarter_hour),
            busiest_frequency: e.busiest_frequency,
            typical_ceiling_kw: e.typical_ceiling_kw,
        }),
    })
}

/// A quarter-hour index of the local day as the clock face it names.
///
/// `71` is not a time anybody reads; `17:45` is.
fn quarter_hour(index: u32) -> String {
    format!("{:02}:{:02}", index / 4, (index % 4) * 15)
}

#[cfg(test)]
mod exposure_tests {
    use super::*;
    use crate::runtime::control::Exposure;

    /// The figures survive the transcription into JSON.
    ///
    /// A DTO built field by field out of a domain struct is the shape that
    /// loses one silently: the field is added to `Status`, the control loop
    /// fills it, and the handler that was written before it exists carries on
    /// serialising everything else. `hours_reduced` reading zero on a household
    /// that has been reduced for eleven hours looks like a household nobody
    /// touched.
    #[tokio::test]
    async fn what_the_box_learned_about_its_own_exposure_reaches_the_page() {
        let held = Status {
            exposure: Some(Exposure {
                days_of_record: 90,
                hours_reduced: 11.5,
                busiest_quarter_hour: Some(71),
                busiest_frequency: 0.8,
                typical_ceiling_kw: Some(4.2),
            }),
            ..Status::default()
        };
        let local = Local::new(
            Arc::new(Mutex::new(held)),
            "haus".into(),
            None,
            crate::runtime::overrides::Overrides::default(),
            None,
            crate::runtime::access::LocalAccess::for_testing("haus", "t"),
            None,
        );

        let Json(body) = status(State(local)).await;
        let seen = body.exposure.expect("an exposure that was set");

        assert_eq!(seen.days_of_record, 90);
        assert!((seen.hours_reduced - 11.5).abs() < f64::EPSILON);
        assert!((seen.busiest_frequency - 0.8).abs() < f64::EPSILON);
        assert_eq!(seen.typical_ceiling_kw, Some(4.2));
        // Quarter hour 71 of the local day is a quarter to six in the evening,
        // and 71 is not a time anybody can act on.
        assert_eq!(seen.busiest_quarter_hour.as_deref(), Some("17:45"));
    }

    /// A box that has never been reduced says so with `null` rather than zeroes.
    ///
    /// Nought hours out of nought days is not the same claim as nought hours out
    /// of ninety, and only one of them is evidence.
    #[tokio::test]
    async fn a_box_with_no_record_reports_nothing_rather_than_a_clean_sheet() {
        let local = Local::new(
            Arc::new(Mutex::new(Status::default())),
            "haus".into(),
            None,
            crate::runtime::overrides::Overrides::default(),
            None,
            crate::runtime::access::LocalAccess::for_testing("haus", "t"),
            None,
        );

        let Json(body) = status(State(local)).await;

        assert!(body.exposure.is_none());
    }
}

/// Who this box is, who it will talk to, and who is asking.
#[derive(Debug, Serialize)]
pub struct PairingBody {
    /// This box's own SKI — what the metering point operator has to be given.
    pub ski: Option<String>,
    /// The SKIs this box will exchange data with.
    pub trusted: Vec<String>,
    /// The peers whose handshake is **waiting on a decision right now**.
    ///
    /// The third side of the commissioning exchange and the one that used to be
    /// missing. An unapproved peer completes TLS — so its SKI is *proved* rather
    /// than claimed — and SHIP holds it pending precisely so a person can compare
    /// it with the label on the device in front of them. Without it the SKI had
    /// to be read off the Steuerbox instead, which is the step field reports name
    /// as the most common § 14a commissioning failure.
    pub waiting: Vec<crate::runtime::ship::Waiting>,
}

/// A SKI an installer has read off a Steuerbox.
#[derive(Debug, serde::Deserialize)]
pub struct Approval {
    /// Forty hexadecimal characters, as printed on the peer.
    pub ski: String,
}

/// Both halves of the § 14a commissioning exchange, on one page.
///
/// The two directions fail differently and an installer needs both in front of
/// them: this box's SKI is what the metering point operator has to be given, and
/// the trusted list is what this box will accept.
async fn list_trusted(State(local): State<Local>) -> Json<PairingBody> {
    Json(PairingBody {
        ski: local.ski.clone(),
        trusted: local.trust.as_ref().map(Trust::peers).unwrap_or_default(),
        waiting: local.trust.as_ref().map(Trust::waiting).unwrap_or_default(),
    })
}

/// Approve a Steuerbox, on a box that is already running.
///
/// The session it completes may already be waiting: an unapproved peer completes
/// TLS — which proves its SKI rather than taking its word — and is held in the
/// SHIP pending state, and an approval added meanwhile lets it through. So an
/// installer approves the SKI printed on the Steuerbox and the reduction path is
/// live, with no restart and no edited file.
async fn approve_peer(
    State(local): State<Local>,
    Json(approval): Json<Approval>,
) -> Result<Json<PairingBody>, (axum::http::StatusCode, String)> {
    let Some(trust) = local.trust.as_ref() else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "this box has no EEBUS identity, so there is nothing to pair with".into(),
        ));
    };
    trust
        .approve(&approval.ski, OffsetDateTime::now_utc())
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(list_trusted(State(local)).await)
}

/// Turn down a peer that is waiting.
///
/// Distinct from [`forget_peer`]: this one is about a handshake happening *now*,
/// and answering it means the peer aborts with `hello: aborted` and learns it was
/// refused instead of timing out. It says nothing about the future — the peer may
/// ask again — and nothing at all about a peer that is not currently waiting.
async fn refuse_peer(
    State(local): State<Local>,
    axum::extract::Path(ski): axum::extract::Path<String>,
) -> Result<Json<PairingBody>, (axum::http::StatusCode, String)> {
    let Some(trust) = local.trust.as_ref() else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "this box has no EEBUS identity, so there is nothing to pair with".into(),
        ));
    };
    trust
        .refuse(&ski)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(list_trusted(State(local)).await)
}

/// Withdraw approval from a peer.
///
/// Its session is not torn down: the § 14a session is how a reduction arrives,
/// and dropping it the instant somebody revokes a SKI would take the household
/// out of contact with its network operator on a keystroke. It cannot
/// reconnect, which is what revocation means.
async fn forget_peer(
    State(local): State<Local>,
    axum::extract::Path(ski): axum::extract::Path<String>,
) -> Result<Json<PairingBody>, (axum::http::StatusCode, String)> {
    let Some(trust) = local.trust.as_ref() else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "this box has no EEBUS identity, so there is nothing to pair with".into(),
        ));
    };
    trust
        .forget(&ski, OffsetDateTime::now_utc())
        .await
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(list_trusted(State(local)).await)
}

#[cfg(test)]
mod series_tests {
    use super::*;

    #[tokio::test]
    async fn a_box_that_keeps_no_series_says_so_rather_than_answering_nothing() {
        // `404`, not an empty array. *Nothing was kept* and *nothing happened*
        // are different answers, and an empty list gives the second to a
        // household asking the first.
        let local = Local::new(
            Arc::new(Mutex::new(Status::default())),
            "haus".into(),
            None,
            crate::runtime::overrides::Overrides::default(),
            None,
            crate::runtime::access::LocalAccess::for_testing("haus", "t"),
            None,
        );
        let outcome = series(
            State(local),
            Path("grid".to_owned()),
            Query(Asked {
                from: None,
                to: None,
            }),
        )
        .await;
        assert_eq!(outcome.err(), Some(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn the_household_reads_back_the_history_its_own_box_took() {
        // The Data Act half of the box: the user's own data, from the process
        // that measured it, over the local API rather than a fleet service.
        let dir = std::env::temp_dir().join(format!("hems-api-series-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = crate::series::Series::open(&dir, 7).expect("a series");
        let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
        store
            .record(
                at,
                &[(crate::series::GRID, hems_core::prelude::Power::from_kw(2.5))],
            )
            .expect("a tick");

        let local = Local::new(
            Arc::new(Mutex::new(Status::default())),
            "haus".into(),
            None,
            crate::runtime::overrides::Overrides::default(),
            None,
            crate::runtime::access::LocalAccess::for_testing("haus", "t"),
            Some(Arc::new(store)),
        );
        // No window: the default hour back is what a screen asks for, and it has
        // to be wide enough to contain a reading five minutes old.
        let Json(window) = series(
            State(local),
            Path(crate::series::GRID.to_owned()),
            Query(Asked {
                from: None,
                to: None,
            }),
        )
        .await
        .expect("a box with a series answers");
        assert_eq!(
            window.resolution,
            crate::series::Resolution::Seconds,
            "an hour back is the box's own trace, not a rollup of it"
        );
        assert_eq!(window.readings.len(), 1, "{window:?}");
        assert!((window.readings[0].watts - 2_500.0).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
