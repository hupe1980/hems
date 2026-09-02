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
    // Opened before anything is served: a daemon that came up and answered
    // enrolments it could not write down would hand out credentials nobody
    // will recognise after the next restart.
    let store = fleetd::store::Store::open(&settings.store_path)
        .map_err(|e| anyhow::anyhow!("{}: {e}", settings.store_path.display()))?;
    let enrolments = store.enrolments()?;
    let reports = store.reports()?;
    tracing::info!(
        enrolled = enrolments.len(),
        reported = reports.len(),
        store = %settings.store_path.display(),
        "fleet state restored"
    );
    let registry = Arc::new(RwLock::new(Registry::restore(sites, enrolments, reports)));
    let store = Arc::new(std::sync::Mutex::new(store));
    let health = Health::new();
    health.good("registry", time::OffsetDateTime::now_utc());

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    // The roster is every household this fleet has adopted. An operator's
    // credential reads it; a box's own enrolment credential does not.
    let operators = hems_service::Credentials::resolve(
        &std::collections::BTreeMap::new(),
        &settings.tenants,
        &settings.operators,
    )?;
    if operators.is_empty() {
        tracing::warn!(
            "no operator token is configured, so the fleet roster will not be served; \
             set [[operators]] in the configuration file"
        );
    }

    // Read-only, and here that is load-bearing: enrolling a box and moving a
    // configuration version are writes, and an agent may do neither.
    let mut app = router(Fleet::new(
        Arc::clone(&registry),
        settings.releases.clone(),
        operators.clone(),
        Arc::clone(&store),
    ));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::per_caller(&settings.mcp, &operators)?;
        app = app.merge(fleetd::mcp_server::router(
            Arc::new(fleetd::mcp_server::State {
                registry,
                silent_after: time::Duration::seconds(settings.silent_after_s.cast_signed()),
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
