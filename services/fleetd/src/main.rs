//! `fleetd` — adopt, configure, offer.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use fleetd::api::{Fleet, router};
use fleetd::{Registry, Settings};
use hems_service::{Health, Server, Shutdown, shutdown};
use tokio::sync::RwLock;

#[derive(Parser)]
#[command(name = "fleetd", version, about = "The hems fleet service")]
struct Cli {
    /// The configuration file. Absent, or absent from disk, means the defaults.
    #[arg(long, env = "HEMS_FLEETD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings: Settings = hems_service::load(cli.config.as_deref(), "HEMS_FLEETD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );
    tracing::info!(
        sites = settings.sites.len(),
        releases = settings.releases.len(),
        "fleet loaded"
    );

    // Resolve every `env:` and `file:` secret once, here, and let `registry`
    // stay a pure module. A reference that cannot be resolved stops the daemon:
    // a fleet that came up with a site whose enrolment secret is the literal
    // string `env:SITE_HAUS1_SECRET` is a fleet that will refuse the box its
    // installer is standing next to, and say nothing about why.
    let mut sites = settings.sites.clone();
    for (site, entry) in &mut sites {
        entry.enrolment_secret = hems_service::Secret::literal(
            entry
                .enrolment_secret
                .resolve_from_process()
                .map_err(|e| anyhow::anyhow!("site {site}: {e}"))?,
        );
    }
    let registry = Arc::new(RwLock::new(Registry::new(sites)));
    let health = Health::new();
    health.good("registry", time::OffsetDateTime::now_utc());

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    Server::new(
        hems_service::identity!(),
        settings.service.clone(),
        health,
        router(Fleet::new(registry, settings.releases.clone())),
    )
    .run_until(signal)
    .await?;
    Ok(())
}
