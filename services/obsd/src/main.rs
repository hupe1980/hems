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

    let silent_after = time::Duration::seconds(settings.silent_after_s.cast_signed());
    // The two surfaces answer from the same fleet view and under the same
    // credentials, and each MCP call is authorised as its own caller — so a
    // token cannot reach over `/mcp` what the REST route would refuse it.
    let mut app = router(Observed::new(
        Arc::clone(&fleet),
        silent_after,
        secrets,
        time::Duration::seconds(settings.webhook_tolerance_s.cast_signed()),
        readers.clone(),
    ));
    if settings.mcp.enabled {
        let auth = hems_service::McpAuth::per_caller(&settings.mcp, &readers)?;
        app = app.merge(obsd::mcp_server::router(
            Arc::new(obsd::mcp_server::State {
                fleet,
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
