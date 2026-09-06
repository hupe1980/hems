//! The fleet's database: one PostgreSQL pool, configured the same way three
//! times.
//!
//! # Why the fleet is PostgreSQL and the box is not
//!
//! `hemsd` keeps its own two years in SQLite: one process, one writer, no
//! network, and a file an installer can copy off a failed box. Every property
//! that makes that right on a gateway makes it wrong for a service holding the
//! § 14a evidence of a whole fleet (D156) — one write lock puts every
//! household's forwarded evidence behind a mutex, a file cannot be replicated so
//! the service cannot run two replicas or outlive its node, and there is no
//! exact decimal type, so a settlement quantity becomes a string to parse back.
//!
//! # What the daemons need identically, and therefore what lives here
//!
//! A connection string that is a *reference* to a credential, a pool with bounds
//! that suit a container, a statement timeout so one abandoned query cannot hold
//! a backend for ever, the migrations, and a readiness probe that says whether
//! the database is actually reachable. Written three times those thirty lines
//! diverge, and the copy that is wrong is the one whose readiness probe lies.
//!
//! The **schema** is deliberately not here: each daemon `include_str!`s its own
//! `migrations/` directory and hands the set to [`migrate`], because a shared
//! schema would be three daemons that cannot be deployed apart.
//!
//! # The shape is `mako-service`'s; the driver is not
//!
//! G4 says a mako engineer should be productive here on day one, so the shape
//! below is `mako_service::config::DatabaseConfig`'s: a `url` that may be an
//! `env:` reference, a `pool_size`, an acquire timeout that fails a request
//! rather than queueing it, an `application_name` on every connection so a
//! session is attributable in `pg_stat_activity`, a migration step that must
//! succeed before anything is served, and a **bounded** readiness ping so a dead
//! database marks a pod `NotReady` instead of hanging its probe.
//!
//! The **driver** cannot agree, and it is cargo rather than taste: `sqlx`
//! declares `sqlx-sqlite` as an optional dependency, and the `links = "sqlite3"`
//! uniqueness rule is enforced during *resolution* — over optional dependencies
//! that may never be enabled — so `sqlx` and the box's `rusqlite` conflict on
//! `libsqlite3-sys`. Measured at `sqlx =0.8.6` beside `rusqlite 0.40` with
//! `bundled`, `default-features = false` and `["postgres", "runtime-tokio"]`
//! resolves, and adding any one of `macros`, `migrate`, `json` or `any` — and so
//! `sqlx`'s own defaults — does not. What is left after those exclusions is a
//! driver with no compile-time queries, no migration runner and no JSON, which
//! is this module with extra steps. mako has no edge daemon and so no SQLite.
//! `tokio-postgres` links no native library, which leaves the safety-critical
//! edge daemon alone.
//!
//! It is the same constraint that keeps `meterstore` out of the fleet tier, and
//! for the same reason rather than a second one (`METERSTORE_FEEDBACK.md`).

use std::time::Duration;

use deadpool_postgres::{Hook, HookError, Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;

use crate::config::Secret;

/// The pool three daemons share the shape of.
pub type Db = Pool;

/// Why a daemon could not reach its database.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The configured connection string is not one.
    ///
    /// Named **without** the string: a PostgreSQL URL carries the password, and
    /// an error message is the single most likely place for a credential to be
    /// pasted into a ticket.
    #[error("the database URL is not a valid PostgreSQL connection string")]
    NotAUrl,
    /// The credential reference could not be resolved.
    #[error(transparent)]
    Secret(#[from] crate::config::ConfigError),
    /// The pool could not be built, or the server refused.
    #[error("the database could not be reached: {0}")]
    Connect(String),
    /// The schema could not be brought up.
    ///
    /// Fatal at start-up rather than logged: a daemon serving queries against a
    /// schema it did not manage to migrate answers some of them wrongly and none
    /// of them with an error, which is the failure a migration exists to
    /// prevent.
    #[error("the schema could not be applied: {0}")]
    Migrate(String),
    /// A migration file has changed since it was applied.
    ///
    /// The refusal that matters most. Two years of § 14a evidence is the last
    /// record in this workspace that should be repaired by guesswork, and a
    /// binary that ran an edited migration would write rows shaped for a schema
    /// nobody has.
    #[error(
        "migration {version} ({description}) has changed since it was applied — \
         the database was migrated by a different version of this binary"
    )]
    Tampered {
        /// Which revision.
        version: i64,
        /// Its description.
        description: &'static str,
    },
    /// The database is at a revision this build does not carry.
    ///
    /// A rolled-back deployment. Serving against it would write rows the newer
    /// schema cannot read.
    #[error("the database is at revision {found} and this build carries {understood}")]
    FromTheFuture {
        /// What the database says.
        found: i64,
        /// The newest revision this binary knows.
        understood: i64,
    },
}

/// How a daemon reaches PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DbSettings {
    /// The connection string, as a reference to a credential.
    ///
    /// `env:HEMS_HISTD_DATABASE_URL` in a deployment. A PostgreSQL URL carries a
    /// password, so it is a [`Secret`] for the reason every other credential in
    /// this workspace is: one in a configuration file is one in an image, in a
    /// backup, and eventually in a repository (D82).
    pub url: Secret,
    /// The most connections one replica opens.
    ///
    /// Ten, as in `mako_service::config::DatabaseConfig`, and the number is a
    /// ceiling on the **fleet** rather than on this process: PostgreSQL costs a
    /// backend process per connection, so `daemons × replicas × pool_size` has to
    /// fit inside the server's own `max_connections`. Three daemons at ten across
    /// three replicas is ninety, which fits the hundred a managed instance ships
    /// with and leaves room for a migration job and a `psql` session.
    pub pool_size: usize,
    /// How long a request waits for a connection before giving up, seconds.
    ///
    /// Shorter than any sensible client timeout, so a saturated pool surfaces as
    /// this daemon's own `503` rather than as a caller's timeout with nothing in
    /// the logs.
    pub acquire_timeout_s: u64,
    /// The server-side `statement_timeout`, seconds.
    ///
    /// **Set on every connection**, because it is the only bound that survives a
    /// client that has already gone away: a Data Act export over two years of a
    /// large site is a long query, and one that has lost its reader holds a
    /// backend and its locks until PostgreSQL is told otherwise. Zero disables
    /// it, which a serving replica must never have.
    ///
    /// It does **not** bound a migration, and that is handled here rather than
    /// left to an operator: [`migrate`] clears it with `SET LOCAL` inside the
    /// revision's own transaction. A migration legitimately runs longer than any
    /// request may, and one killed half way leaves a daemon refusing to start
    /// against the schema it just failed to apply.
    pub statement_timeout_s: u64,
    /// Whether the connection is made over TLS.
    ///
    /// **On by default.** What crosses this socket is which households did not
    /// respect a network operator's reduction, and a managed PostgreSQL is
    /// reached across a network somebody else operates. Turning it off is for a
    /// unix socket or a test container, it is said out loud in the log when it
    /// happens, and it is a decision an operator has to write down.
    pub tls: bool,
}

impl Default for DbSettings {
    fn default() -> Self {
        Self {
            url: Secret::literal("env:DATABASE_URL"),
            pool_size: 10,
            acquire_timeout_s: 5,
            statement_timeout_s: 30,
            tls: true,
        }
    }
}

impl DbSettings {
    /// The connection string this resolves to.
    ///
    /// # Errors
    /// [`DbError::Secret`] where an `env:` or `file:` reference cannot be read.
    pub fn url(&self) -> Result<String, DbError> {
        Ok(self.url.resolve_from_process()?)
    }
}

/// Open the pool and prove it works.
///
/// Takes one connection before returning, so a wrong password or an unreachable
/// host is a start-up failure rather than a failure on the first request. A pool
/// that connected lazily would let a daemon start, pass its readiness probe and
/// fail everything it was asked — the shape of outage that takes longest to
/// diagnose, because everything that is supposed to notice says the service is
/// fine.
///
/// # Errors
/// [`DbError::NotAUrl`], [`DbError::Secret`] or [`DbError::Connect`].
pub async fn connect(settings: &DbSettings, application_name: &str) -> Result<Db, DbError> {
    let url = settings.url()?;
    let mut config: tokio_postgres::Config = url.parse().map_err(|_| DbError::NotAUrl)?;
    // Every session says which daemon opened it, so a `pg_stat_activity` row is
    // attributable to a service rather than to "some Rust client". mako does the
    // same, and it is the difference between diagnosing a saturated server in a
    // minute and in an afternoon.
    config.application_name(application_name);
    // Not a keep-alive for its own sake: a connection idle long enough for a load
    // balancer or a managed instance to have dropped it underneath is one whose
    // next query fails. TCP keep-alive makes that visible to the pool instead of
    // to a caller.
    config.keepalives(true);
    config.keepalives_idle(Duration::from_secs(60));

    let timeout = Duration::from_secs(settings.acquire_timeout_s.max(1));
    // The two arms differ only in the connector, and the connector is a type
    // parameter rather than an enum: `MakeTlsConnect` has an associated stream,
    // an associated future and an associated error, so a hand-written enum over
    // two implementations would be three more associated types and a `Future`
    // impl to carry a boolean this function already has.
    let pool = if settings.tls {
        build(config, tls()?, settings, timeout)
    } else {
        // A container on loopback, or a unix socket. Said out loud rather than
        // inferred: a deployment that quietly fell back to plaintext because a
        // certificate would not verify is the one thing this setting exists to
        // make impossible.
        tracing::warn!(
            "connecting to PostgreSQL without TLS — anything that crosses a \
             network needs `tls = true`"
        );
        build(config, NoTls, settings, timeout)
    }?;

    // One connection taken and returned, so a wrong password or an unreachable
    // host is a start-up failure rather than a `500` on the first request.
    drop(
        pool.get()
            .await
            .map_err(|e| DbError::Connect(e.to_string()))?,
    );
    Ok(pool)
}

/// The pool, whichever connector it is reached through.
fn build<T>(
    config: tokio_postgres::Config,
    connector: T,
    settings: &DbSettings,
    timeout: Duration,
) -> Result<Db, DbError>
where
    T: tokio_postgres::tls::MakeTlsConnect<tokio_postgres::Socket> + Clone + Send + Sync + 'static,
    T::Stream: Send + Sync + 'static,
    T::TlsConnect: Send + Sync,
    <T::TlsConnect as tokio_postgres::tls::TlsConnect<tokio_postgres::Socket>>::Future: Send,
{
    let manager = Manager::from_config(
        config,
        connector,
        ManagerConfig {
            // `Fast` is a liveness check before a connection leaves the pool.
            // `Verified` would also reset the session — exactly wrong here,
            // because `statement_timeout` is a session setting and resetting it
            // would hand out connections with no bound on them.
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let statement_timeout = settings.statement_timeout_s;
    Pool::builder(manager)
        .max_size(settings.pool_size.max(1))
        .wait_timeout(Some(timeout))
        .create_timeout(Some(timeout))
        .recycle_timeout(Some(timeout))
        // **Every** connection, not the first one. A pool opens more as load
        // arrives and replaces them as they age out, so a timeout set once at
        // start-up would bound exactly one connection — and the ones that
        // actually serve a two-year export under load would be the unbounded
        // ones. `post_create` runs on each, which is what makes the setting a
        // property of the pool rather than of its first member.
        .post_create(Hook::async_fn(move |client, _metrics| {
            Box::pin(async move {
                prepare_session(client, statement_timeout)
                    .await
                    .map_err(|e| HookError::Message(e.to_string().into()))
            })
        }))
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|e| DbError::Connect(e.to_string()))
}

/// The two session settings every connection in this workspace is opened with.
///
/// # `statement_timeout`
///
/// The only bound that survives a client that has already gone away: a Data Act
/// export over two years of a large site is a long query, and one that has lost
/// its reader holds a backend and its locks until PostgreSQL is told otherwise.
///
/// # `plan_cache_mode = force_custom_plan`
///
/// This one is not a tuning preference, it is a correctness property of *these*
/// queries, and the way it fails is worth writing down.
///
/// `tokio-postgres` prepares and caches statements, and PostgreSQL switches a
/// prepared statement to a **generic** plan after five executions if the generic
/// plan does not look worse on average. A generic plan has no parameter values,
/// so it cannot compare selectivities — and every hot query here is one whose
/// right plan *depends* on the parameters. `histd`'s window query is the same
/// statement whether a caller asks for one day (96 rows) or a Data Act export
/// (two years, 70 080), and with a generic plan it chose the retention index on
/// `slot_start` and filtered the site out afterwards: a one-day question reading
/// every household's rows in that range.
///
/// Forcing a custom plan makes PostgreSQL re-plan with the actual values, which
/// turns both into an index-only scan on the primary key. It costs a planning
/// pass per execution — microseconds, against queries that read thousands of
/// rows — and this workload is settlements and exports rather than a
/// thousand-per-second point lookup.
///
/// # Errors
/// Whatever the server said.
pub async fn prepare_session(
    client: &tokio_postgres::Client,
    statement_timeout_s: u64,
) -> Result<(), tokio_postgres::Error> {
    use std::fmt::Write as _;

    let mut sql = String::from("SET plan_cache_mode = force_custom_plan");
    if statement_timeout_s > 0 {
        // Milliseconds, which is what PostgreSQL takes. Interpolated rather than
        // bound because `SET` takes no parameter — and the value is a `u64` from
        // configuration, so it cannot carry SQL.
        let _ = write!(
            sql,
            "; SET statement_timeout = {}",
            statement_timeout_s * 1000
        );
    }
    client.batch_execute(&sql).await
}

/// The TLS connector, on the workspace's own crypto provider.
fn tls() -> Result<tokio_postgres_rustls::MakeRustlsConnect, DbError> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| DbError::Connect(e.to_string()))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

/// One revision of a daemon's schema.
///
/// `include_str!`-ed rather than read from disk, so the binary carries its own
/// schema and a container needs no files beside it.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Monotonic, and never reused.
    pub version: i64,
    /// What it does, for the log line and the error.
    pub description: &'static str,
    /// The statements, applied in one transaction.
    pub sql: &'static str,
}

/// The advisory-lock key every migration in this workspace queues behind.
///
/// Arbitrary but **fixed**, and shared across the daemons on purpose: they may
/// share a database, and two of them creating `schema_migration` at the same
/// moment is a race with a real losing side.
const MIGRATION_LOCK: i64 = 4_915_141_055_260_145;

/// Bring the schema up to the newest revision the binary carries.
///
/// # What this does that a `psql -f` does not
///
/// * **An advisory lock**, so several replicas starting at once is safe: one
///   applies and the others wait and find nothing to do.
/// * **One transaction per revision** — PostgreSQL allows DDL inside one, so a
///   half-applied schema is not a state that exists.
/// * **A checksum**, so an *edited* migration is refused rather than skipped. A
///   file that has changed since it was applied means the database and the binary
///   disagree about what the schema is, and the only safe answer is to stop.
/// * **A refusal to run backwards**, for a rolled-back deployment.
///
/// # Errors
/// [`DbError::Migrate`], [`DbError::Tampered`] or [`DbError::FromTheFuture`].
pub async fn migrate(pool: &Db, migrations: &[Migration]) -> Result<(), DbError> {
    let mut client = pool
        .get()
        .await
        .map_err(|e| DbError::Migrate(e.to_string()))?;

    // Session-scoped, so it is released if this process dies mid-migration —
    // which is what makes a crashed deployment recoverable rather than a lock
    // somebody has to find and clear.
    client
        .batch_execute(&format!("SELECT pg_advisory_lock({MIGRATION_LOCK})"))
        .await
        .map_err(|e| DbError::Migrate(e.to_string()))?;

    let result = apply(&mut client, migrations).await;

    // Released whatever happened: an error that left the lock held would make the
    // next start-up hang instead of reporting the same error again.
    let _ = client
        .batch_execute(&format!("SELECT pg_advisory_unlock({MIGRATION_LOCK})"))
        .await;
    result
}

async fn apply(
    client: &mut deadpool_postgres::Client,
    migrations: &[Migration],
) -> Result<(), DbError> {
    let fail = |e: tokio_postgres::Error| DbError::Migrate(e.to_string());
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migration (
                 version     BIGINT      PRIMARY KEY,
                 description TEXT        NOT NULL,
                 checksum    TEXT        NOT NULL,
                 applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
             )",
        )
        .await
        .map_err(fail)?;

    let rows = client
        .query("SELECT version, checksum FROM schema_migration", &[])
        .await
        .map_err(fail)?;
    let applied: std::collections::BTreeMap<i64, String> = rows
        .iter()
        .map(|row| (row.get::<_, i64>(0), row.get::<_, String>(1)))
        .collect();

    let understood = migrations.iter().map(|m| m.version).max().unwrap_or(0);
    if let Some(found) = applied.keys().copied().max()
        && found > understood
    {
        return Err(DbError::FromTheFuture { found, understood });
    }

    for migration in migrations {
        let checksum = checksum(migration.sql);
        match applied.get(&migration.version) {
            Some(recorded) if *recorded == checksum => continue,
            Some(_) => {
                return Err(DbError::Tampered {
                    version: migration.version,
                    description: migration.description,
                });
            }
            None => {}
        }
        let transaction = client.transaction().await.map_err(fail)?;
        // **A migration is not a request, and the serving bound does not apply
        // to it.** Every pooled connection carries `statement_timeout` so an
        // abandoned query cannot hold a backend for ever, and this connection
        // came out of that pool — so without this line a migration that took
        // longer than a *request* may take would be killed half way, and the
        // daemon would refuse to start against a schema it had just failed to
        // apply. Adding a column to a fleet's `quarter_hour` is exactly that
        // shape of statement.
        //
        // `SET LOCAL` rather than `SET`, and that is the load-bearing half:
        // `RecyclingMethod::Fast` does not reset a session, so a plain `SET`
        // here would hand the connection back to the pool **unbounded** and the
        // next Data Act export to be given it would have no timeout at all.
        // `SET LOCAL` reverts when this transaction commits.
        transaction
            .batch_execute("SET LOCAL statement_timeout = 0")
            .await
            .map_err(fail)?;
        transaction
            .batch_execute(migration.sql)
            .await
            .map_err(fail)?;
        transaction
            .execute(
                "INSERT INTO schema_migration (version, description, checksum)
                 VALUES ($1, $2, $3)",
                &[&migration.version, &migration.description, &checksum],
            )
            .await
            .map_err(fail)?;
        transaction.commit().await.map_err(fail)?;
        tracing::info!(
            version = migration.version,
            description = migration.description,
            "schema migrated"
        );
    }
    Ok(())
}

/// The hash a migration is recognised by.
///
/// SHA-256 of the exact bytes, hex. Whitespace counts: a "harmless" reformat of
/// an applied migration is exactly the edit that makes a database and a binary
/// disagree silently, and the point of a checksum is that it has no opinion about
/// which edits are harmless.
fn checksum(sql: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(sql.as_bytes());
    hex::encode(hasher.finalize())
}

/// Whether the database is answering.
///
/// `SELECT 1` through the pool, which exercises the whole path a request takes —
/// acquiring a connection, the network, the server — rather than asking the pool
/// whether it believes it is connected. A probe that read a cached flag would
/// report a healthy daemon for exactly as long as the flag was stale, which is
/// the window an operator needs it not to.
///
/// It is **bounded**, which is the half that matters. A probe with no timeout
/// against a database that has stopped answering does not fail — it *hangs*, and
/// an orchestrator waiting on it neither restarts the pod nor takes it out of
/// rotation. Two seconds, as in `mako_service`, so a dead database is `NotReady`
/// rather than a request that never returns.
///
/// # Errors
/// A string, because what a caller does with it is log it.
pub async fn is_reachable(pool: &Db) -> Result<(), String> {
    let ping = async {
        let client = pool.get().await.map_err(|e| e.to_string())?;
        client
            .simple_query("SELECT 1")
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    match tokio::time::timeout(PROBE_TIMEOUT, ping).await {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "the database did not answer within {} s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// Keep the readiness surface honest about the database.
///
/// A loop rather than a check inside the probe handler, because
/// [`crate::Health`] is a registry a daemon *writes to* — the readiness body
/// names every dependency and when it was last good, and a probe that computed
/// its answer inline would be the one dependency with no history.
///
/// The daemon is taken **out of rotation** when the database stops answering and
/// put back when it returns: a `histd` that cannot reach PostgreSQL can serve
/// nothing at all, and one that reported itself ready would collect a fleet's
/// day reports and drop every one of them.
///
/// Spawn it through [`crate::Health::vital`], like every other background loop
/// in this workspace — `cargo xtask check-vital` fails the build otherwise, and
/// a watchdog that has silently died is worse than none (D146).
pub async fn watch(pool: Db, health: crate::Health, name: &'static str, shutdown: crate::Shutdown) {
    loop {
        match is_reachable(&pool).await {
            Ok(()) => health.good(name, crate::db::now()),
            Err(detail) => {
                tracing::error!(error = %detail, "the database is not answering");
                health.bad(name, detail);
            }
        }
        let sleep = tokio::time::sleep(PROBE_PERIOD);
        tokio::select! {
            () = sleep => {}
            () = shutdown.clone().wait() => return,
        }
    }
}

/// The clock, in the one place this module needs one.
fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}

/// How often the watcher asks.
///
/// Five seconds: often enough that a failover is noticed inside one
/// orchestrator probe interval, rare enough that the ping is not a workload.
const PROBE_PERIOD: Duration = Duration::from_secs(5);

/// How long a readiness probe waits for the database.
///
/// Short, and shorter than any orchestrator's own probe timeout, so the answer
/// is this daemon's rather than the orchestrator giving up on it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_fit_a_managed_instance() {
        let d = DbSettings::default();
        // Three daemons × three replicas has to fit inside the hundred a managed
        // PostgreSQL ships with, with room for a migration job and a psql.
        assert!(3 * 3 * d.pool_size < 100);
        assert!(d.statement_timeout_s > 0, "a serving replica needs a bound");
        assert!(
            d.tls,
            "what crosses this socket is a fleet's § 14a evidence"
        );
    }

    #[tokio::test]
    async fn a_url_that_is_not_one_is_refused_without_printing_it() {
        let settings = DbSettings {
            url: Secret::literal("postgres://user:hunter2@[not a host"),
            ..DbSettings::default()
        };
        let error = connect(&settings, "test").await.expect_err("not a URL");
        assert!(matches!(error, DbError::NotAUrl));
        // A real one has the password in it, and an error message is where a
        // credential gets pasted into a ticket.
        assert!(!error.to_string().contains("hunter2"));
    }

    #[test]
    fn an_edited_migration_has_a_different_checksum() {
        // The whole of the tamper check, and whitespace counts on purpose: a
        // reformat of an applied migration is exactly the edit that makes a
        // database and a binary disagree without anybody noticing.
        assert_ne!(
            checksum("CREATE TABLE a ()"),
            checksum("CREATE TABLE a ()\n")
        );
        assert_eq!(checksum("CREATE TABLE a ()"), checksum("CREATE TABLE a ()"));
    }
}
