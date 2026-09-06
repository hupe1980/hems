//! `histd` — store it, keep it for two years, hand it back.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use hems_service::{Health, Server, Shutdown, shutdown};
use histd::api::{History, router};
use histd::{Settings, Store};

#[derive(Parser)]
#[command(name = "histd", version, about = "The hems history service")]
struct Cli {
    /// The configuration file. Absent, or absent from disk, means the defaults.
    #[arg(long, env = "HEMS_HISTD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings: Settings = hems_service::load(cli.config.as_deref(), "HEMS_HISTD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );

    // Eagerly, so a wrong password or an unreachable database stops the daemon
    // here rather than answering every request with a `500`. The migrations run
    // under an advisory lock, so several replicas starting together is safe.
    let db = hems_service::db::connect(&settings.database, hems_service::identity!().name).await?;
    hems_service::db::migrate(&db, histd::store::MIGRATIONS).await?;
    let store = Store::new(db);
    let health = Health::new();
    tracing::info!("history open");

    // Resolved once, here, so a reference to a secret that is not there stops
    // the daemon rather than starting one that answers nothing and says why only
    // in a `401`.
    let credentials = hems_service::Credentials::resolve(
        &settings.site_tokens,
        &settings.tenants,
        &settings.operators,
    )?;
    if credentials.is_empty() {
        tracing::warn!(
            "no site or operator tokens are configured, so every request will be refused; \
             what these routes serve is a household's whole consumption record"
        );
    }

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    // The database *is* this daemon's readiness: it has no other upstream, and
    // what it is asked for is what the database holds. Vital, so a watcher that
    // died cannot leave a green probe over an unreachable store (D146).
    health.vital(
        "store",
        signal.clone(),
        hems_service::db::watch(store.db().clone(), health.clone(), "store", signal.clone()),
    );
    // The series a probe cannot answer: a saturated pool serves `503`s while
    // `/livez` and `/readyz` both stay green, because the process is alive and
    // the database is reachable and there is simply no connection to be had.
    hems_service::metrics::publish_pool(store.db(), hems_service::identity!().name);
    // **Vital**, and the quietest of the three: a retention loop that has died
    // sweeps nothing, so the two years of `[A1 7.3]` evidence grow without bound
    // and the only symptom is a disk filling up months later. `/livez` used to
    // stay green through all of it (D132).
    health.vital(
        "retention",
        signal.clone(),
        retention_loop(
            store.clone(),
            health.clone(),
            settings.retention_sweep_s,
            signal.clone(),
        ),
    );

    // The two surfaces answer from the same store and from the same credentials,
    // and each MCP call is authorised as its own caller — so a token cannot
    // reach a site over `/mcp` that the REST route would refuse it.
    let mut app =
        router(History::new(store.clone(), credentials.clone()).settling(settings.mispel.clone()));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::per_caller(&settings.mcp, &credentials)?;
        app = app.merge(histd::mcp_server::router(
            Arc::new(histd::mcp_server::State {
                store: store.clone(),
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

/// Delete what `[A1 7.3]`'s two years have released, once a day.
///
/// A failure here takes the service **out of rotation** rather than down: a
/// history that cannot prune is still a history that can answer, and the thing
/// that has gone wrong is a disk rather than the record.
async fn retention_loop(store: Store, health: Health, every_s: u64, signal: Shutdown) {
    loop {
        let now = time::OffsetDateTime::now_utc();
        // A `DELETE` over two years of evidence can take a while, and on
        // PostgreSQL it takes it on a backend rather than on a runtime worker —
        // so the sweep no longer has to be pushed off the runtime to keep a
        // readiness probe answering.
        match store.prune(now).await {
            Ok(0) => {}
            Ok(gone) => tracing::info!(events = gone, "evidence past its two years deleted"),
            Err(e) => {
                tracing::error!(error = %e, "retention sweep failed");
                health.bad("store", e.to_string());
            }
        }
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(every_s.max(60)));
        tokio::select! {
            () = sleep => {}
            () = signal.clone().wait() => {
                tracing::info!("retention loop stopping");
                return;
            }
        }
    }
}
