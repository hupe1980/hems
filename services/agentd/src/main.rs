//! `agentd` — the advisory plane an operator reads in the morning.
//!
//! Sockets, a journal, a principal and a cadence. What the specialists decide is
//! in [`agentd`], *what they may decide* is bounded in `agentd::advice`, and
//! *when they are asked* is `agentd::review` — none of it here.

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

    // Where the days come from. A missing credential stops the daemon: a plane
    // whose every review is refused has an empty queue, and an empty queue is
    // what a fleet in good order looks like.
    let token = settings
        .obsd_token
        .as_ref()
        .context(
            "obsd_token is not set; a plane that cannot read the fleet has an empty queue, \
             and an empty queue is what a fleet in good order looks like",
        )?
        .resolve_from_process()?;
    let upstream: Arc<dyn agentd::Upstream> = Arc::new(agentd::Obsd::new(
        &settings.obsd,
        token,
        &settings.service.http,
        std::time::Duration::from_secs(settings.obsd_timeout_s.max(1)),
    )?);

    // Who may read the queue. It names households that did not respect a network
    // operator's reduction, so it is the fleet credential and not a site's.
    let readers = hems_service::Credentials::resolve(
        &std::collections::BTreeMap::new(),
        &settings.tenants,
        &settings.operators,
    )?;
    if readers.is_empty() {
        tracing::warn!(
            "no operator credential is configured, so nothing can read the advisory queue; \
             the reviews still run and are still journaled"
        );
    }

    let queue = Arc::new(agentd::Queue::new());
    // Unready until the first review lands. A queue that has never been filled
    // reads exactly like a fleet in good order, so a plane reported ready before
    // it has read anything is a plane that answers "nothing to report" about a
    // fleet it has not seen.
    health.set(agentd::review::UPSTREAM, agentd::review::not_reviewed_yet());
    let (signal, trigger) = Shutdown::channel();
    tokio::spawn(shutdown::on_signal(trigger));

    // **Vital**: a plane whose review loop has died goes on serving whatever it
    // found last, dated, to an operator with no way to tell (D132, D146).
    health.vital(
        "review",
        signal.clone(),
        agentd::review_loop(
            Arc::clone(&plane),
            Arc::clone(&upstream),
            Arc::clone(&queue),
            health.clone(),
            std::time::Duration::from_secs(settings.review_every_s.max(60)),
            signal.clone(),
        ),
    );

    tracing::info!(
        specialists = ?agentd::SPECIALISTS.iter().map(|s| s.name).collect::<Vec<_>>(),
        journal = %settings.journal.display(),
        obsd = %settings.obsd,
        every_s = settings.review_every_s,
        "reviewing"
    );

    // The two surfaces answer from the same queue, so they cannot disagree.
    let mut app = agentd::router(agentd::Advisory::new(Arc::clone(&queue), readers.clone()));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::per_caller(&settings.mcp, &readers)?;
        app = app.merge(agentd::mcp_server::router(
            Arc::new(agentd::mcp_server::State {
                queue,
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
