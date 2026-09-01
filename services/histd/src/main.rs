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

    let db = histd::Db::at(&settings.database);
    // One writer, because SQLite has one; readers open their own connection.
    let store = Arc::new(std::sync::Mutex::new(db.connect()?));
    let health = Health::new();
    // A history service is ready the moment its database is open: it has no
    // upstream, and the thing it is asked for is what it already holds.
    health.good("store", time::OffsetDateTime::now_utc());
    tracing::info!(database = ?settings.database, "history open");

    // Resolved once, here, so a reference to a secret that is not there stops
    // the daemon rather than starting one that answers nothing and says why only
    // in a `401`.
    let credentials =
        hems_service::Credentials::resolve(&settings.site_tokens, &settings.operator_tokens)?;
    if credentials.is_empty() {
        tracing::warn!(
            "no site or operator tokens are configured, so every request will be refused; \
             what these routes serve is a household's whole consumption record"
        );
    }

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));
    tokio::spawn(retention_loop(
        Arc::clone(&store),
        health.clone(),
        settings.retention_sweep_s,
        signal.clone(),
    ));

    Server::new(
        hems_service::identity!(),
        settings.service.clone(),
        health,
        router(History::new(db, store, credentials)),
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
async fn retention_loop(
    store: Arc<std::sync::Mutex<Store>>,
    health: Health,
    every_s: u64,
    signal: Shutdown,
) {
    loop {
        let now = time::OffsetDateTime::now_utc();
        // Off the runtime, like every other query: a sweep over two years of
        // evidence is a `DELETE` that can take a while, and it must not be taken
        // out of a worker that a readiness probe is waiting on.
        let swept = {
            let store = Arc::clone(&store);
            tokio::task::spawn_blocking(move || {
                store
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .prune(now)
            })
            .await
        };
        match swept {
            Ok(Ok(0)) => {}
            Ok(Ok(gone)) => tracing::info!(events = gone, "evidence past its two years deleted"),
            Ok(Err(e)) => {
                tracing::error!(error = %e, "retention sweep failed");
                health.bad("store", e.to_string());
            }
            Err(e) => {
                tracing::error!(error = %e, "the retention sweep could not be run");
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
