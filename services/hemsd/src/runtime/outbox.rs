//! Forwarding the box's own record to the fleet.
//!
//! The box records first and forwards second. `[A1 7.3]` gives a network
//! operator two years to ask about a control event, and G3 says the house is
//! never worse off when the cloud is gone — so a record that exists only once it
//! has been uploaded is an intention with a network dependency, and the day an
//! operator asks about is the day the link was down.
//!
//! What is tracked is therefore a **property of the row**: `forwarded_at` is
//! `NULL` until `histd` has taken it. A queue table beside the record could
//! disagree with the record it points at, which is two sources of truth about
//! one obligation.
//!
//! # Forwarded is not deleted
//!
//! The two years are the household's, not the fleet's. An acknowledgement moves
//! a row out of the backlog and never out of the store; pruning follows the
//! retention window alone.

use std::sync::Arc;

use hems_service::{Secret, Shutdown};
use tokio::sync::Mutex;

/// Where the fleet's copy lives, and what may write to it.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HistdSettings {
    /// `histd`'s base URL. Absent leaves the box keeping its own record and
    /// forwarding nothing, which is a household with no fleet rather than a
    /// fault.
    pub url: Option<String>,
    /// Which site the fleet knows this household as.
    pub site: Option<String>,
    /// The bearer token, or an `env:`/`file:` reference to it.
    ///
    /// A credential in a configuration file is one in an image, in a backup and
    /// eventually in a repository, and no orchestrator injects secrets that way
    /// — so what is configured is the *reference* and the value is resolved at
    /// start-up (D82).
    pub token: Option<Secret>,
    /// How often to try, seconds.
    ///
    /// Slow on purpose. What is being forwarded is a record that is already
    /// safe on the box, so the only thing urgency buys is load on a service the
    /// household does not own.
    #[serde(default = "default_every_s")]
    pub every_s: u64,
    /// How many rows to send in one round.
    ///
    /// A box that has been offline for a month has a month of backlog, and
    /// sending it in one burst is how a fleet service falls over the morning a
    /// region comes back.
    #[serde(default = "default_batch")]
    pub batch: usize,
}

fn default_every_s() -> u64 {
    300
}

fn default_batch() -> usize {
    50
}

impl HistdSettings {
    /// Whether anything is configured to forward to.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.url.is_some() && self.site.is_some()
    }
}

/// The client that drains the outbox.
pub struct Outbox {
    client: reqwest::Client,
    url: String,
    site: String,
    token: String,
    batch: usize,
}

impl Outbox {
    /// A client for a configured `histd`, or `None` where there is none.
    ///
    /// # Errors
    /// Where the credential cannot be resolved. Coming up and sending the
    /// literal string `env:HEMS_HISTD_TOKEN` as a bearer token would look
    /// exactly like a fleet that had started rejecting this box.
    pub fn new(settings: &HistdSettings) -> anyhow::Result<Option<Self>> {
        let (Some(url), Some(site)) = (&settings.url, &settings.site) else {
            return Ok(None);
        };
        let token = match &settings.token {
            Some(secret) => secret.resolve_from_process()?,
            None => {
                anyhow::bail!("a `histd` is configured with no token, and it will refuse every row")
            }
        };
        Ok(Some(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            url: url.trim_end_matches('/').to_string(),
            site: site.clone(),
            token,
            batch: settings.batch.max(1),
        }))
    }

    /// Send whatever the store has not had acknowledged, and mark what landed.
    ///
    /// Returns how many rows were acknowledged. A row that fails is simply left
    /// in the backlog: it is already safe on the box, and the next round will
    /// try again.
    ///
    /// # Errors
    /// Never for a network failure — that is a warning and a retry. Only where
    /// the store itself cannot be read or written, which is a box with a broken
    /// disk and a household that is about to lose its two years.
    pub async fn drain(
        &self,
        store: &Arc<Mutex<crate::store::Store>>,
        now: time::OffsetDateTime,
    ) -> Result<usize, crate::store::StoreError> {
        let pending = store.lock().await.pending_events(self.batch)?;
        if pending.is_empty() {
            return Ok(0);
        }
        let mut accepted = Vec::new();
        for stored in &pending {
            match self.put_event(&stored.event).await {
                Ok(()) => accepted.push(stored.id),
                Err(error) => {
                    // Warned once per round rather than once per row: a fleet
                    // that is down is down for all of them, and a box with a
                    // month of backlog would otherwise write a month of
                    // identical lines the first time it came back.
                    tracing::warn!(%error, "the fleet did not take a control event; keeping it");
                    break;
                }
            }
        }
        if accepted.is_empty() {
            return Ok(0);
        }
        store.lock().await.mark_forwarded(&accepted, &[], now)?;
        Ok(accepted.len())
    }

    /// One event, to the endpoint `histd` already serves.
    async fn put_event(&self, event: &hems_grid::evidence::ControlEvent) -> anyhow::Result<()> {
        let response = self
            .client
            .post(format!("{}/v1/sites/{}/events", self.url, self.site))
            .bearer_auth(&self.token)
            .json(event)
            .send()
            .await?;
        let status = response.status();
        anyhow::ensure!(status.is_success(), "the fleet answered {status}");
        Ok(())
    }
}

/// Drain, sleep, repeat — until the process is asked to stop.
pub async fn run(
    outbox: Outbox,
    store: Arc<Mutex<crate::store::Store>>,
    every: std::time::Duration,
    shutdown: Shutdown,
) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            _ = ticker.tick() => {}
        }
        let now = time::OffsetDateTime::now_utc();
        match outbox.drain(&store, now).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(events = n, "the fleet acknowledged part of the backlog"),
            Err(error) => tracing::error!(%error, "the box's own store could not be read"),
        }
    }
}
