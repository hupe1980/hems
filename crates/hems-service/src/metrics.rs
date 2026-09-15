//! `GET /metrics`.
//!
//! `/livez` and `/readyz` answer *is it up* and *may it serve*. Neither answers
//! *is it healthy*, and a pool-backed service fails by **saturating its pool** —
//! which serves `503`s while both probes stay green, because the process is
//! alive and the database is reachable and there is simply no connection to be
//! had. So the pool's own numbers are published beside the HTTP counter and
//! latency histogram (D163).
//!
//! # The route label is the matched route, not the path
//!
//! A hems site is called something like `reference-household`, which no
//! heuristic path-normaliser recognises as an identifier — so a path label would
//! put every household into an endpoint that is scraped, stored for months and
//! read by everybody, and give it a cardinality proportional to the fleet.
//! [`MatchedPath`] is the exact answer instead. A request that matched no route
//! is labelled `unmatched`, because an unrouted URI is attacker-controlled and is
//! the one string that must never become a label.

use std::sync::OnceLock;
use std::time::Instant;

use axum::Router;
use axum::extract::MatchedPath;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use prometheus::{CounterVec, Encoder, HistogramVec, TextEncoder};

static REQUESTS: OnceLock<CounterVec> = OnceLock::new();
static DURATION: OnceLock<HistogramVec> = OnceLock::new();

/// Register the HTTP series on the default registry.
///
/// Idempotent, so a test that builds several servers in one process does not
/// fail on the second.
fn http_metrics() -> (&'static CounterVec, &'static HistogramVec) {
    let requests = REQUESTS.get_or_init(|| {
        prometheus::register_counter_vec!(
            "hems_http_requests_total",
            "HTTP requests handled by this daemon",
            &["method", "route", "status"]
        )
        .expect("hems_http_requests_total registers once")
    });
    let duration = DURATION.get_or_init(|| {
        prometheus::register_histogram_vec!(
            "hems_http_request_duration_seconds",
            "HTTP request latency",
            &["method", "route"],
            // Out to thirty seconds, because the long tail here is real work
            // rather than a fault: a Data Act export over two years of a large
            // household is seconds, and a histogram that topped out at five
            // would report every one of them as "over the last bucket" and say
            // nothing about how far over.
            vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0
            ]
        )
        .expect("hems_http_request_duration_seconds registers once")
    });
    (requests, duration)
}

/// Publish a daemon's connection pool.
///
/// The series an operator needs after the fleet moved to PostgreSQL, and the one
/// no probe can answer: a saturated pool serves `503`s while `/livez` and
/// `/readyz` both stay green, because the process is alive and the database is
/// reachable — there is simply no connection to be had.
///
/// Pulled at scrape time rather than pushed, because the pool already keeps the
/// numbers and a second copy updated on every acquire is a second thing that can
/// disagree with it.
///
/// Idempotent: registering twice in one process is ignored rather than fatal, so
/// a test that builds two daemons does not die on the second.
#[cfg(feature = "postgres")]
pub fn publish_pool(pool: &crate::db::Db) {
    // The daemon's name is **not** in the metric name and not in a label, and
    // that is the correct answer rather than a missing feature: a deployment
    // scrapes one daemon per target, so the series a dashboard groups by is the
    // *job*, which the scraper attaches. Putting the daemon in the name would
    // give every service a metric of its own and make `sum by (job)` impossible
    // to write.
    //
    // This function used to take the name and drop it on the floor, under a
    // comment claiming it was in the metric name — a parameter three daemons
    // passed and nothing read, describing behaviour the code did not have
    // (D196). The name is gone rather than used, because the metric was already
    // right.
    let gauge = |suffix: &str, help: &str, read: Box<dyn Fn() -> f64 + Send + Sync>| {
        if let Ok(g) = prometheus::PullingGauge::new(format!("hems_db_pool_{suffix}"), help, read) {
            // Ignored rather than fatal: a process that builds two daemons —
            // which a test does — registers the same name twice, and a duplicate
            // registration is not a reason to refuse to start.
            let _ = prometheus::register(Box::new(g));
        }
    };

    let status = pool.clone();
    gauge(
        "connections",
        "Connections the pool currently holds",
        Box::new(move || f64::from(u32::try_from(status.status().size).unwrap_or(u32::MAX))),
    );
    let status = pool.clone();
    gauge(
        "available",
        "Connections that could be handed out right now — zero for any length of \
         time is a daemon serving 503s while every health probe stays green",
        Box::new(move || f64::from(i32::try_from(status.status().available).unwrap_or(i32::MAX))),
    );
    let status = pool.clone();
    gauge(
        "waiting",
        "Requests queued for a connection",
        Box::new(move || f64::from(u32::try_from(status.status().waiting).unwrap_or(u32::MAX))),
    );
}

/// The `/metrics` route, and the middleware that feeds it.
///
/// Merged by [`crate::Server::new`] beside `/livez` and `/readyz`, on the port
/// the daemon already binds — the same argument the MCP surface is mounted
/// under: a second port is a second thing to secure and a second thing to
/// forget.
pub(crate) fn routes() -> Router {
    // Registered here rather than on the first request, so the *collectors*
    // exist from start-up.
    //
    // That is not the same as the **series** existing, and the distinction is
    // worth being exact about because it decides what an alert can be written
    // on. A labelled vector has no series until some combination of its labels
    // has been observed — there is no list of routes and statuses to
    // pre-populate it with, and inventing one would publish a
    // `status="500"` of zero for every route in the daemon. So
    // `hems_http_requests_total` is absent until the first request, and an
    // alert on *silence* has to be written against something that is always
    // there: the scrape itself (`up`), and the pool gauges below, which are
    // unlabelled and are published the moment a daemon opens its pool.
    let _ = http_metrics();
    Router::new().route("/metrics", get(render))
}

async fn render() -> impl axum::response::IntoResponse {
    let mut buffer = Vec::with_capacity(4096);
    let _ = TextEncoder::new().encode(&prometheus::gather(), &mut buffer);
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        buffer,
    )
}

/// Record one request against the route it matched.
pub(crate) async fn record(request: Request, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    // The **matched route**, so a label is `/v1/sites/{site}/export` however
    // many households there are. A request that matched nothing is `unmatched`:
    // its URI is attacker-controlled, and an unbounded label is a way to fill a
    // monitoring system from outside.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |m| m.as_str().to_owned());

    let started = Instant::now();
    let response = next.run(request).await;
    let elapsed = started.elapsed().as_secs_f64();

    let status = response.status().as_str().to_owned();
    let (requests, duration) = http_metrics();
    requests
        .with_label_values(&[method.as_str(), route.as_str(), status.as_str()])
        .inc();
    duration
        .with_label_values(&[method.as_str(), route.as_str()])
        .observe(elapsed);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_encodes_before_anything_has_been_served() {
        // The **collectors** are registered and the exposition renders. The
        // *series* for `hems_http_requests_total` does not exist yet, because a
        // labelled vector has no members until a label combination has been
        // observed — which is why an alert on silence is written against the
        // scrape and the pool gauges rather than against a request counter.
        let _ = routes();
        let mut buffer = Vec::new();
        Encoder::encode(&TextEncoder::new(), &prometheus::gather(), &mut buffer)
            .expect("the registry encodes");
    }

    #[test]
    fn observing_a_request_creates_the_series() {
        // The other half: once a route has been seen it is there, with the
        // route and the status as labels. `serve.rs` proves the same thing over
        // a socket and proves the label is the *matched route*; this proves the
        // recording itself, without a server.
        let (requests, duration) = http_metrics();
        requests
            .with_label_values(&["GET", "/v1/probe", "200"])
            .inc();
        duration
            .with_label_values(&["GET", "/v1/probe"])
            .observe(0.01);

        let mut buffer = Vec::new();
        Encoder::encode(&TextEncoder::new(), &prometheus::gather(), &mut buffer)
            .expect("the registry encodes");
        let text = String::from_utf8_lossy(&buffer);
        assert!(text.contains("hems_http_requests_total"), "{text}");
        assert!(text.contains(r#"route="/v1/probe""#), "{text}");
    }
}
