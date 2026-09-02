//! `tariffd` — fetch, reconcile, serve.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use hems_service::{Health, Server, Shutdown, shutdown};
use hems_tariff::cache::PriceCache;
use tariffd::api::{Prices, router};
use tariffd::{Http, Poller, Settings, poller};
use tokio::sync::RwLock;

#[derive(Parser)]
#[command(name = "tariffd", version, about = "The hems price service")]
struct Cli {
    /// The configuration file. Absent, or absent from disk, means the defaults.
    #[arg(long, env = "HEMS_TARIFFD_CONFIG")]
    config: Option<PathBuf>,
}

/// Every endpoint with its credentials resolved.
///
/// Read once, at startup, so `upstream` never touches the environment or the
/// filesystem and a token cannot be re-read into a request that is already in
/// flight. A reference that cannot be resolved stops the daemon: coming up and
/// sending the literal string `env:ENTSOE_TOKEN` as a security token would look
/// exactly like a source that had started rejecting us.
fn resolved_sources(
    settings: &Settings,
) -> anyhow::Result<
    std::collections::BTreeMap<hems_tariff::source::Source, tariffd::config::Endpoint>,
> {
    settings
        .sources
        .iter()
        .map(|(source, endpoint)| {
            let headers = endpoint
                .headers
                .iter()
                .map(|(name, value)| {
                    let resolved = value
                        .resolve_from_process()
                        .map_err(|e| anyhow::anyhow!("{source:?} header {name}: {e}"))?;
                    Ok((name.clone(), hems_service::Secret::literal(resolved)))
                })
                .collect::<anyhow::Result<_>>()?;
            Ok((
                *source,
                tariffd::config::Endpoint {
                    url: endpoint.url.clone(),
                    headers,
                },
            ))
        })
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings: Settings = hems_service::load(cli.config.as_deref(), "HEMS_TARIFFD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );
    if settings.sources.is_empty() {
        // Not an error and not a silence: a `tariffd` with nothing to fetch will
        // never become ready, and saying so once at startup is cheaper than
        // working it out from a readiness probe at three in the morning.
        tracing::warn!("no price sources are configured; this service cannot become ready");
    }

    let cache = Arc::new(RwLock::new(PriceCache::new()));
    let health = Health::new();
    health.bad("prices", "no fetch has succeeded yet");

    let upstream = Http::new(
        resolved_sources(&settings)?,
        std::time::Duration::from_secs(settings.request_timeout_s),
    )?;
    let poller = Poller::new(
        upstream,
        settings.sources.keys().copied(),
        time::OffsetDateTime::now_utc(),
        time::Duration::seconds(settings.poll_interval_s as i64),
        time::Duration::seconds(settings.max_backoff_s as i64),
    );

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));
    tokio::spawn(fetch_loop(
        poller,
        Arc::clone(&cache),
        health.clone(),
        settings.ready_slots,
        signal.clone(),
    ));

    // The two surfaces answer from the same cache, so they cannot disagree. The
    // MCP one is off unless an operator switched it on: an endpoint that speaks
    // to whatever can reach it is one somebody should have to ask for.
    let mut app = router(Prices::new(cache.clone()));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::gated(&settings.mcp)?;
        app = app.merge(tariffd::mcp_server::router(
            Arc::new(tariffd::mcp_server::State { cache }),
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

/// Ask, merge, sleep, repeat — until the process is asked to stop.
///
/// The readiness it publishes is computed from what the **cache covers**, not
/// from whether the last request returned. A daemon whose upstream is down but
/// which still holds tomorrow can serve every plan the box will make today, and
/// taking it out of rotation for that would be taking a working service away.
async fn fetch_loop(
    mut poller: Poller<Http>,
    cache: Arc<RwLock<PriceCache>>,
    health: Health,
    ready_slots: usize,
    signal: Shutdown,
) {
    loop {
        let now = time::OffsetDateTime::now_utc();
        let outcome = {
            let mut guard = cache.write().await;
            poller.poll(&mut guard, now).await
        };
        for (source, why) in &outcome.failed {
            tracing::warn!(?source, why, "a price source did not answer");
        }
        if !outcome.stale.is_empty() {
            tracing::info!(sources = ?outcome.stale, "answered with nothing new");
        }

        {
            let guard = cache.read().await;
            if poller::is_ready(&guard, now, ready_slots) {
                health.good("prices", now);
            } else {
                health.bad(
                    "prices",
                    format!(
                        "only {:.0} % of the next {ready_slots} quarter hours are priced",
                        poller::coverage(&guard, now, ready_slots) * 100.0
                    ),
                );
            }
        }

        // Sleep until the next source is due, or a minute if none is scheduled
        // — a service with nothing configured should still notice a shutdown.
        let wait = poller
            .next_due()
            .map_or(time::Duration::minutes(1), |due| due - now)
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
