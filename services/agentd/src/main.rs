//! `agentd` — the advisory plane an operator reads in the morning.
//!
//! Sockets, a journal and a principal. What the specialists decide is in
//! [`agentd`], and *what they may decide* is bounded in `agentd::advice` rather
//! than here.

use std::path::PathBuf;
use std::sync::Arc;

use agentplane::prelude::{JournalStore, RedbStore};
use anyhow::Context as _;
use clap::Parser;
use hems_service::{Authority, Health, Server, Shutdown, shutdown};

#[derive(Parser)]
#[command(name = "agentd", version, about = "The hems advisory plane")]
struct Cli {
    /// The configuration file. Absent, or absent from disk, means the defaults.
    #[arg(long, env = "HEMS_AGENTD_CONFIG")]
    config: Option<PathBuf>,
}

/// What the daemon waits for before it takes traffic.
const JOURNAL: &str = "journal";
const PRINCIPAL: &str = "principal";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings: agentd::Settings = hems_service::load(cli.config.as_deref(), "HEMS_AGENTD")?;
    hems_service::init_tracing(
        hems_service::identity!(),
        &settings.service.log_filter,
        settings.service.log_json,
    );

    let health = Health::new();
    let now = time::OffsetDateTime::now_utc();

    // The journal is the plan of record, so a daemon that cannot open one has
    // nothing to be trusted about later. A hard failure rather than a warning.
    let store: Arc<dyn JournalStore> =
        Arc::new(RedbStore::open(&settings.journal).context("opening the journal")?);
    let plane = agentd::runtime(Arc::clone(&store));
    health.good(JOURNAL, now);

    // The operator this daemon acts for, and the attenuated authority every
    // specialist runs under. `advisory` is the only constructor and it cannot
    // widen, so "nothing an agent says moves a watt" is settled here (D118).
    let scope = if settings.tenant == hems_service::auth::EVERY_TENANT {
        hems_service::SiteScope::Every
    } else {
        hems_service::SiteScope::Tenant {
            sites: settings
                .tenants
                .get(&settings.tenant)
                .cloned()
                .with_context(|| {
                    format!(
                        "tenant {:?} is not defined; a daemon scoped to nothing reads no \
                         household and looks, at the other end, like a permissions problem",
                        settings.tenant
                    )
                })?,
            name: settings.tenant.clone(),
        }
    };
    let operator = Authority::operator(scope);
    let agent = agentd::advisory(&operator)
        .context("the operator does not hold every advisory capability")?;
    tracing::info!(
        subject = agent.subject(),
        sites = agent.sites().name(),
        capabilities = ?agent.capabilities().patterns().collect::<Vec<_>>(),
        "specialists run under an advisory authority — it reads, and it cannot write"
    );
    health.good(PRINCIPAL, now);

    tracing::info!(
        specialists = ?agentd::registered_specialists(),
        journal = %settings.journal.display(),
        "registered"
    );

    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    // Held for the life of the process. Nothing carries an event to it yet —
    // the subscription table says which type wakes which specialist, and the
    // hop from `obsd`'s collector is the piece that is missing.
    let _plane = plane;

    Server::new(
        hems_service::identity!(),
        settings.service.clone(),
        health,
        axum::Router::new(),
    )
    .run_until(signal)
    .await?;
    Ok(())
}
