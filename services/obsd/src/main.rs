//! `obsd` — collect, aggregate, answer.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use hems_service::{Health, Server, Shutdown, shutdown};
use obsd::Settings;
use obsd::api::{Observed, router};
use obsd::fleet::Fleet;
use tokio::sync::RwLock;

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

    let fleet = Arc::new(RwLock::new(Fleet::new(settings.keep_days)));
    let health = Health::new();
    // An observability service has no upstream: it is ready the moment it can
    // accept a report. A fleet with nothing in it yet is a fleet with nothing in
    // it, not a broken service — and marking it unready would take the *only*
    // thing that can accept the first report out of rotation.
    health.good("collector", time::OffsetDateTime::now_utc());
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
    let secrets = settings
        .webhook_secrets
        .iter()
        .map(hems_service::Secret::resolve_from_process)
        .collect::<Result<Vec<_>, _>>()?;

    let readers = hems_service::Credentials::resolve(
        &std::collections::BTreeMap::new(),
        &settings.operator_tokens,
    )?;
    if readers.is_empty() {
        tracing::warn!(
            "no operator token is configured, so the fleet view will not be served; \
             set operator_tokens in the configuration file"
        );
    }

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    Server::new(
        hems_service::identity!(),
        settings.service.clone(),
        health,
        router(Observed::new(
            fleet,
            time::Duration::seconds(settings.silent_after_s.cast_signed()),
            secrets,
            time::Duration::seconds(settings.webhook_tolerance_s.cast_signed()),
            readers,
        )),
    )
    .run_until(signal)
    .await?;
    Ok(())
}
