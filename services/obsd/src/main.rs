//! `obsd` — collect, aggregate, answer.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use hems_service::{Health, Server, Shutdown, shutdown};
use obsd::Settings;
use obsd::api::{Observed, router};

#[derive(Parser)]
#[command(name = "obsd", version, about = "The hems observability service")]
struct Cli {
    /// The configuration file. Absent, or absent from disk, means the defaults.
    #[arg(long, env = "HEMS_OBSD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings: Settings = hems_service::load(cli.config.as_deref(), "HEMS_OBSD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );

    // Eagerly, so a database this service cannot reach stops it here rather than
    // letting it accept days it silently drops.
    let db = hems_service::db::connect(&settings.database, hems_service::identity!().name).await?;
    hems_service::db::migrate(&db, obsd::store::MIGRATIONS).await?;
    let store = obsd::store::Store::new(db);
    let health = Health::new();
    // Not a refusal to start: a fleet view that cannot yet accept a report can
    // still answer every question about the days it already holds, and a service
    // that exits on a missing environment variable is one an operator restarts
    // without ever reading why. It is a `warn` on every boot instead.
    if settings.webhook_secrets.is_empty() {
        tracing::warn!(
            "no webhook secret is configured, so every reported day will be refused; \
             set webhook_secrets in the configuration file"
        );
    }
    // Resolved once, at startup, and it is a **hard** failure: a reference to a
    // secret that is not there is a deployment somebody thought they had
    // configured, and coming up with an empty list would look exactly like one
    // nobody configured at all.
    // Flattened to (site, secret) pairs: `verify` reports *which* key signed a
    // report, and the site beside it is how `obsd` learns who sent it rather
    // than believing what the payload claims (D114).
    let mut secrets: Vec<(String, String)> = Vec::new();
    for (site, keys) in &settings.webhook_secrets {
        for key in keys {
            secrets.push((site.clone(), key.resolve_from_process()?));
        }
    }

    // Two households holding one key make "who signed this" unanswerable, and
    // the shape that answer takes — pick one and carry on — is the defect this
    // whole mechanism exists to close (D114). A configuration error, refused
    // here, rather than a report attributed to whichever site the fold happened
    // to see last.
    for (i, (site, key)) in secrets.iter().enumerate() {
        if let Some((other, _)) = secrets[..i].iter().find(|(_, k)| k == key) {
            anyhow::bail!(
                "sites {other:?} and {site:?} are configured with the same webhook secret, \
                 so a report signed with it names no household in particular; give each \
                 box a key of its own"
            );
        }
    }

    let readers = hems_service::Credentials::resolve(
        &std::collections::BTreeMap::new(),
        &settings.tenants,
        &settings.operators,
    )?;
    if readers.is_empty() {
        tracing::warn!(
            "no operator token is configured, so the fleet view will not be served; \
             set [[operators]] in the configuration file"
        );
    }

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    // A fleet with nothing in it yet is a fleet with nothing in it, not a broken
    // service — so readiness is about the *database* rather than about the
    // number of sites. What must never happen is a replica reporting itself
    // ready while it cannot store a day: a box that was told its report was
    // accepted keeps no second copy.
    health.vital(
        "collector",
        signal.clone(),
        hems_service::db::watch(
            store.db().clone(),
            health.clone(),
            "collector",
            signal.clone(),
        ),
    );
    // The series a probe cannot answer: a saturated pool serves `503`s while
    // `/livez` and `/readyz` both stay green, because the process is alive and
    // the database is reachable and there is simply no connection to be had.
    hems_service::metrics::publish_pool(store.db());

    // **Vital**: a retention sweep that has died lets the fleet's days grow
    // without bound, and the only symptom is a disk filling up months later
    // (D132).
    health.vital(
        "retention",
        signal.clone(),
        retention_loop(
            store.clone(),
            settings.keep_days,
            health.clone(),
            signal.clone(),
        ),
    );

    let silent_after = time::Duration::seconds(settings.silent_after_s.cast_signed());
    // The two surfaces answer from the same fleet view and under the same
    // credentials, and each MCP call is authorised as its own caller — so a
    // token cannot reach over `/mcp` what the REST route would refuse it.
    let mut app = router(Observed::new(
        store.clone(),
        settings.keep_days,
        silent_after,
        secrets,
        time::Duration::seconds(settings.webhook_tolerance_s.cast_signed()),
        readers.clone(),
    ));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::per_caller(&settings.mcp, &readers)?;
        app = app.merge(obsd::mcp_server::router(
            Arc::new(obsd::mcp_server::State {
                store: store.clone(),
                keep_days: settings.keep_days,
                silent_after,
                auth: auth.clone(),
            }),
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

/// Delete what has fallen out of the retention window, once a day.
///
/// A failure here takes the service **out of rotation** rather than down: a
/// fleet view that cannot prune is still one that can answer, and what has gone
/// wrong is a disk rather than the record.
async fn retention_loop(
    store: obsd::store::Store,
    keep_days: usize,
    health: Health,
    signal: Shutdown,
) {
    loop {
        let before = obsd::store::window_start(keep_days, time::OffsetDateTime::now_utc().date());
        match store.prune(before).await {
            Ok(0) => {}
            Ok(gone) => {
                tracing::info!(days = gone, before = %before, "days past the window deleted")
            }
            Err(e) => {
                tracing::error!(error = %e, "retention sweep failed");
                health.bad("collector", e.to_string());
            }
        }
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60));
        tokio::select! {
            () = sleep => {}
            () = signal.clone().wait() => {
                tracing::info!("retention loop stopping");
                return;
            }
        }
    }
}
