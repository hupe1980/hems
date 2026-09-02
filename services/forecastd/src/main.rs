//! `forecastd` — fetch the sky, serve it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use forecastd::api::{Weather, router};
use forecastd::poller::{Run, is_ready};
use forecastd::{Http, Poller, Settings};
use hems_service::{Health, Server, Shutdown, shutdown};
use tokio::sync::RwLock;

#[derive(Parser)]
#[command(name = "forecastd", version, about = "The hems weather service")]
struct Cli {
    /// The configuration file. Absent, or absent from disk, means the defaults.
    #[arg(long, env = "HEMS_FORECASTD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings: Settings = hems_service::load(cli.config.as_deref(), "HEMS_FORECASTD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );
    if settings.locations.is_empty() {
        tracing::warn!("no locations are configured; this service cannot become ready");
    }

    let runs: Arc<RwLock<BTreeMap<String, Run>>> = Arc::new(RwLock::new(BTreeMap::new()));
    let health = Health::new();
    health.bad("weather", "no run has been fetched yet");

    let poller = Poller::new(
        Http::new(
            settings.endpoint.clone(),
            std::time::Duration::from_secs(settings.request_timeout_s),
        )?,
        settings.locations.clone(),
        time::OffsetDateTime::now_utc(),
        time::Duration::seconds(settings.poll_interval_s as i64),
        time::Duration::seconds(settings.max_backoff_s as i64),
    );

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));
    tokio::spawn(fetch_loop(
        poller,
        Arc::clone(&runs),
        health.clone(),
        settings.clone(),
        signal.clone(),
    ));

    // The two surfaces answer from the same runs, so they cannot disagree.
    let mut app = router(Weather::new(runs.clone()));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::gated(&settings.mcp)?;
        app = app.merge(forecastd::mcp_server::router(
            Arc::new(forecastd::mcp_server::State { runs }),
            auth,
            hems_service::mcp::cancel_on(&signal),
        ));
        tracing::info!("the Model Context Protocol surface is mounted at /mcp");
    }

    Server::new(
        hems_service::identity!(),
        settings.service.clone(),
        health,
        app,
    )
    .run_until(signal)
    .await?;
    Ok(())
}

async fn fetch_loop(
    mut poller: Poller<Http>,
    runs: Arc<RwLock<BTreeMap<String, Run>>>,
    health: Health,
    settings: Settings,
    signal: Shutdown,
) {
    loop {
        let now = time::OffsetDateTime::now_utc();
        let outcome = {
            let mut guard = runs.write().await;
            poller.poll(&mut guard, now).await
        };
        for (location, why) in &outcome.failed {
            tracing::warn!(location, why, "no weather run");
        }
        if !outcome.refreshed.is_empty() {
            tracing::info!(locations = ?outcome.refreshed, "weather refreshed");
        }

        {
            let guard = runs.read().await;
            if is_ready(&guard, &settings.locations, now, settings.ready_slots) {
                health.good("weather", now);
            } else {
                let missing: Vec<&str> = settings
                    .locations
                    .iter()
                    .filter(|l| {
                        guard
                            .get(&l.id)
                            .is_none_or(|run| run.contiguous_from(now) < settings.ready_slots)
                    })
                    .map(|l| l.id.as_str())
                    .collect();
                health.bad("weather", format!("no usable run for {missing:?}"));
            }
        }

        let wait = poller
            .next_due()
            .map_or(time::Duration::minutes(5), |due| due - now)
            .max(time::Duration::seconds(1));
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(
            wait.whole_seconds().max(1).unsigned_abs(),
        ));
        tokio::select! {
            () = sleep => {}
            () = signal.clone().wait() => {
                tracing::info!("fetch loop stopping");
                return;
            }
        }
    }
}
