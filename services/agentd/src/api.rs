//! What an operator reads in the morning.
//!
//! # One route, and it returns findings rather than a score
//!
//! `GET /v1/advice` is the queue: what each specialist said, when, about which
//! window, and the run in the journal that produced it. There is no route that
//! returns a health score for the fleet, and that is deliberate — a single
//! number is what an advisory plane produces when it has stopped having anything
//! to say, and `obsd` already answers "how are we doing" honestly.
//!
//! # It reads the fleet, so it needs the fleet capability
//!
//! Every finding here is about a population, and an aggregate is not any one
//! household's data however wide that household's own reach (D112). So the route
//! asks for `hems.fleet.read` by name rather than checking a site — a box's own
//! token reaches its own household and must not reach a list of the households
//! that failed to respect a network operator's reduction.
//!
//! # Nothing here writes
//!
//! There is no `POST`. `Advice` is a leaf type nothing in this workspace
//! consumes (D118), and a route that accepted one would be the first half of the
//! path this daemon exists not to have.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use hems_service::auth::Credentials;

use crate::review::{Queue, Reviewed, SPECIALISTS};

/// What the routes read.
#[derive(Clone)]
pub struct Advisory {
    queue: Arc<Queue>,
    readers: Arc<Credentials>,
}

impl Advisory {
    /// A handle onto the queue the review loop writes.
    #[must_use]
    pub fn new(queue: Arc<Queue>, readers: Credentials) -> Self {
        Self {
            queue,
            readers: Arc::new(readers),
        }
    }

    /// The caller, if it may ask a question about every household in its scope.
    fn fleet_caller(&self, headers: &HeaderMap) -> Result<hems_service::Authority, StatusCode> {
        let authority = self
            .readers
            .authority_in(
                headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
            )
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if authority.may_read_the_fleet() {
            Ok(authority)
        } else {
            Err(StatusCode::FORBIDDEN)
        }
    }
}

/// The routes.
pub fn router(advisory: Advisory) -> Router {
    Router::new()
        .route("/v1/advice", get(advice))
        .with_state(advisory)
}

/// The advisory queue.
#[derive(Debug, serde::Serialize)]
pub struct Queued {
    /// What each specialist last said, in the order the findings are read.
    pub reviews: Vec<Reviewed>,
    /// How many findings there are in total.
    ///
    /// Counted rather than left to the reader: a queue is read by somebody
    /// deciding whether to open it, and "three findings" is that decision.
    pub findings: usize,
    /// Every specialist this plane runs and the question it answers, whether or
    /// not it has said anything yet.
    ///
    /// So an empty queue is legible. A plane that had never managed to read the
    /// fleet and one whose fleet is in good order both return no findings, and
    /// the difference is whether a specialist appears in `reviews` at all.
    pub specialists: Vec<Question>,
}

/// One specialist and what it is run to find out.
#[derive(Debug, serde::Serialize)]
pub struct Question {
    /// Its name.
    pub name: &'static str,
    /// What it answers.
    pub question: &'static str,
    /// Whether it has answered yet.
    pub has_reported: bool,
}

async fn advice(
    State(state): State<Advisory>,
    headers: HeaderMap,
) -> Result<axum::Json<Queued>, StatusCode> {
    state.fleet_caller(&headers)?;
    let reviews = state.queue.read().await;
    let findings = reviews.iter().map(|r| r.proposal.advice.len()).sum();
    let specialists = SPECIALISTS
        .iter()
        .map(|s| Question {
            name: s.name,
            question: s.question,
            has_reported: reviews.iter().any(|r| r.specialist == s.name),
        })
        .collect();
    Ok(axum::Json(Queued {
        reviews,
        findings,
        specialists,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    fn credentials() -> Credentials {
        Credentials::default().with_operator("tok-operator")
    }

    fn app(queue: Arc<Queue>) -> Router {
        router(Advisory::new(queue, credentials()))
    }

    async fn get(app: Router, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().uri("/v1/advice");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .oneshot(request.body(Body::empty()).expect("a request"))
            .await
            .expect("a response");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        let value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn an_empty_queue_still_says_which_questions_are_being_asked() {
        // A plane that has never managed to read the fleet and one whose fleet
        // is in good order both have no findings. `has_reported` is the
        // difference, and without it an operator reading `[]` cannot tell.
        let (status, body) = get(app(Arc::new(Queue::new())), Some("tok-operator")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["findings"], 0);
        assert_eq!(
            body["specialists"].as_array().expect("a list").len(),
            SPECIALISTS.len()
        );
        for specialist in body["specialists"].as_array().expect("a list") {
            assert_eq!(specialist["has_reported"], false);
        }
    }

    #[tokio::test]
    async fn the_queue_is_not_readable_without_the_fleet_capability() {
        // Every finding is about a population, and a population is not any one
        // household's data however wide that household's own reach (D112).
        let queue = Arc::new(Queue::new());
        assert_eq!(
            get(app(Arc::clone(&queue)), None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(app(queue), Some("tok-nobody")).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_boxs_own_token_does_not_read_the_advisory_queue() {
        // The sharper half: a valid credential that is not a fleet reader. A box
        // holds one, and what this queue names is other households.
        let queue = Arc::new(Queue::new());
        let readers = Credentials::default().with_site("haus-1", "tok-box");
        let app = router(Advisory::new(queue, readers));
        assert_eq!(get(app, Some("tok-box")).await.0, StatusCode::FORBIDDEN);
    }
}
