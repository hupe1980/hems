//! A real PostgreSQL for a test.
//!
//! # Why a container is a prerequisite
//!
//! The fleet daemons deploy on PostgreSQL, so their queries are tested against
//! it: `NUMERIC` against `TEXT`, `TIMESTAMPTZ` against an integer, `UNNEST`
//! against a statement in a loop, an advisory lock against nothing at all, an
//! `Index Cond` against a `Filter` — every one of those is a place two engines
//! answer differently, and every one is in this workspace's schema. A service
//! checked on a different engine is a service whose SQL is checked by nothing.
//!
//! # One server, a database per test
//!
//! One container for the whole workspace, started by the first test that asks
//! and torn down by `just db-stop`. Each fixture then does its own
//! `CREATE DATABASE`, because tests sharing one would break each other in an
//! order that depends on the scheduler.
//!
//! `cargo test` runs test binaries in parallel, so a database name carries the
//! **process id** as well as a counter, and a stale one from a crashed run is
//! dropped rather than collided with.
//!
//! # Why the container is managed by hand
//!
//! `docker run --rm`, by name, on a fixed port, and this module checks the
//! container is *usable* before handing it out. Two traps it exists to avoid:
//! a handle held in a `static` is never dropped, so a library that stops its
//! container on `Drop` leaks one per test binary per run; and attaching to a
//! container *by name* attaches to whatever bears it, including one Docker has
//! left in the `dead` state, which publishes no ports. `--rm` means a stopped
//! container is gone rather than left to be attached to.
//!
//! # The escape hatch, and why it is not a skip
//!
//! `HEMS_TEST_POSTGRES` points at a server that is already running, which is how
//! CI runs it. There is deliberately no way to *skip*: a test that passes when
//! it could not reach a database reports green for a query nobody ran.

use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::OnceCell;

use crate::db::{Db, DbSettings, Migration};

/// The server every fixture in this process shares, as a `postgres://` base.
///
/// A `Result` rather than a panic inside the initialiser, so a broken
/// environment is diagnosed **once**: `OnceCell` does not remember a panicking
/// init, so every test in the binary would otherwise pay the readiness timeout
/// again — four failing tests, four minutes, and the same message four times.
static SERVER: OnceCell<Result<String, String>> = OnceCell::const_new();

/// Counts fixtures within one test binary. Not enough on its own — see
/// [`fresh_name`].
static NEXT: AtomicU32 = AtomicU32::new(1);

/// The one container the whole workspace shares, across binaries and runs.
///
/// Named so it can be found again — and so `just db-stop` can remove it, which
/// is the teardown.
const CONTAINER: &str = "hems-test-postgres";

/// The port it publishes on the host.
///
/// Deliberately **not** 5432: a developer with their own PostgreSQL should not
/// have to stop it to run this suite, and a fixed port keeps the connection
/// string guessable for `psql`.
const PORT: u16 = 55_432;

/// Pinned rather than `latest`: a suite whose database version moves under it is
/// a suite that can fail on a Tuesday for a reason nobody changed.
const IMAGE: &str = "postgres:18-alpine";

/// How long to wait for a freshly started server to accept connections.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A PostgreSQL database a test owns outright.
pub struct Postgres {
    /// The pool, ready to use.
    pub db: Db,
    /// Which database on the shared server, for a message.
    pub name: String,
}

impl Postgres {
    /// A fresh database with `migrations` applied.
    ///
    /// # Panics
    /// When no server can be reached — with a message naming both ways to give
    /// it one. A skip would be worse: a test that passes without having run its
    /// queries reports green for code nobody exercised.
    pub async fn start(migrations: &[Migration]) -> Self {
        Self::start_with(migrations, 4).await
    }

    /// The same, with a pool of a stated size.
    ///
    /// For a test whose subject *is* contention: the pool is where a
    /// PostgreSQL-backed service queues, so a test about what happens under load
    /// has to be able to say how many connections there are.
    ///
    /// # Panics
    /// As [`Postgres::start`].
    pub async fn start_with(migrations: &[Migration], pool_size: usize) -> Self {
        let fixture = Self::empty_with(pool_size).await;
        crate::db::migrate(&fixture.db, migrations)
            .await
            .expect("the schema applies to an empty database");
        fixture
    }

    /// A fresh database with nothing on it, for a test about migrating.
    ///
    /// # Panics
    /// As [`Postgres::start`].
    pub async fn empty() -> Self {
        Self::empty_with(4).await
    }

    /// The same, with a pool of a stated size.
    ///
    /// # Panics
    /// As [`Postgres::start`].
    pub async fn empty_with(pool_size: usize) -> Self {
        let base = match SERVER.get_or_init(server).await {
            Ok(base) => base,
            Err(why) => panic!("{why}"),
        };
        let name = fresh_name();

        let admin = connect_to(&format!("{base}/postgres"), 2).await;
        {
            let client = admin.get().await.expect("the server accepts a connection");
            // Dropped first, because a shared server outlives the suite: a
            // process id is unique among live processes and not across runs, so
            // a leftover from a crashed one would otherwise collide. `FORCE`
            // because that leftover may still have a connection on it.
            //
            // The name is built from a pid and a counter, so it carries no input
            // — but it is still an identifier being interpolated, and
            // `CREATE DATABASE` takes no parameter, so both are quoted.
            client
                .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                .await
                .expect("a stale database can be dropped");
            client
                .batch_execute(&format!("CREATE DATABASE \"{name}\""))
                .await
                .expect("a database can be created");
        }
        // Closed before the fixture is handed over: a pool held open on
        // `postgres` for the length of a test binary is a connection per fixture
        // against a server's own `max_connections`.
        admin.close();

        let db = connect_to(&format!("{base}/{name}"), pool_size).await;
        Self { db, name }
    }
}

/// A database name no other fixture can be using.
///
/// The counter alone is not enough and the way it failed is worth keeping. With
/// a server per test *binary* it was safe, because each binary had one to
/// itself. Against a **shared** server — which is what this is, and what
/// `HEMS_TEST_POSTGRES` gives in CI — `cargo test` runs the binaries in parallel
/// and every one of them asked for `hems_test_1`. The process id is what makes
/// the name unique across binaries; the counter is what makes it unique within
/// one.
fn fresh_name() -> String {
    format!(
        "hems_test_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

async fn connect_to(url: &str, pool_size: usize) -> Db {
    let settings = DbSettings {
        url: crate::Secret::literal(url.to_owned()),
        // Loopback. The warning this prints is the point: nothing that crosses a
        // network should be here.
        tls: false,
        // Small by default: a test binary runs several fixtures at once against
        // a server whose own `max_connections` is a hundred.
        pool_size,
        // Long enough that a laptop under a full `cargo test` fails on the
        // assertion rather than on the pool.
        acquire_timeout_s: 30,
        ..DbSettings::default()
    };
    crate::db::connect(&settings, "hems-test")
        .await
        .expect("the test database accepts connections")
}

/// The server this run will use, as a `postgres://…` base with no database on
/// it.
async fn server() -> Result<String, String> {
    if let Ok(url) = std::env::var("HEMS_TEST_POSTGRES") {
        // Given as a full URL with a database on it, or as a base. Taking
        // everything before the last `/` after the host is what a base *is*;
        // trimming a trailing segment blindly would guess.
        let base = url.trim_end_matches('/');
        let cut = base.rfind('/').filter(|c| *c > "postgres://".len());
        return Ok(cut.map_or_else(|| base.to_owned(), |c| base[..c].to_owned()));
    }

    let base = format!("postgres://postgres:postgres@127.0.0.1:{PORT}");
    ensure_container()?;
    wait_until_ready(&base).await?;
    Ok(base)
}

/// Start the shared container, or leave the one that is already there.
///
/// Several test binaries reach this at once, so the losers of the `docker run`
/// race find the name taken and fall through to the readiness wait — which is
/// the correct outcome and not an error.
fn ensure_container() -> Result<(), String> {
    // `.State.Status` rather than `.State.Running`, because the two disagree in
    // exactly the case this check exists for: a container Docker has left in the
    // `dead` state reports a status of `dead`, publishes no ports, and attaching
    // to it is how the suite used to fail with `PortNotExposed`.
    match Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", CONTAINER])
        .output()
    {
        Ok(out) if is_running(&out.stdout) => return Ok(()),
        // Anything else — absent, exited, `dead` — is removed and replaced.
        // `--rm` means a *stopped* container is already gone, so this is the
        // crashed and the wedged case.
        Ok(_) => {
            let _ = Command::new("docker")
                .args(["rm", "-f", CONTAINER])
                .output();
        }
        Err(e) => {
            return Err(format!(
                "`docker` could not be run ({e}). The fleet daemons deploy on \
                 PostgreSQL and their queries are tested against it, so this \
                 suite needs either a container runtime (Docker, Podman or \
                 Colima) on the PATH, or HEMS_TEST_POSTGRES pointing at a server \
                 it may create databases on"
            ));
        }
    }

    let started = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            &format!("{PORT}:5432"),
            "-e",
            "POSTGRES_USER=postgres",
            "-e",
            "POSTGRES_PASSWORD=postgres",
            IMAGE,
        ])
        .output()
        .expect("docker is on the PATH");
    if !started.status.success() {
        let stderr = String::from_utf8_lossy(&started.stderr);
        // Another test binary won the race. That is the normal case rather than
        // a failure: the wait below is what both sides do next.
        // **"Already in use" is two different things**, and telling them apart
        // is the whole of what this branch is for. Either another test binary
        // won the race — the normal case, and the readiness wait is what both
        // sides do next — or the name is held by a container that could not be
        // removed. The second happens when Docker's own metadata store has gone
        // read-only (a full VM disk), which leaves a `dead` container that
        // `docker rm -f` refuses to delete and that nothing can connect to. Both
        // arrive here with the same message, so the status is what separates
        // them.
        if stderr.contains("already in use") && !container_is_running() {
            return Err(format!(
                "the name `{CONTAINER}` is held by a container that is not \
                 running and could not be removed. Docker itself is wedged — \
                 most often a full VM disk, which turns its metadata store \
                 read-only. Free it (`docker system prune`, or reset the \
                 runtime), or set HEMS_TEST_POSTGRES to a server this suite may \
                 create databases on.\ndocker said: {}",
                stderr.trim()
            ));
        }
        if !stderr.contains("already in use") {
            // Carried through rather than swallowed. What the runtime actually
            // said — "no space left on device", "read-only file system", "Cannot
            // connect to the Docker daemon" — is the whole diagnosis, and a
            // suite that reported only "did not accept a connection" would send
            // somebody looking at the database.
            return Err(format!(
                "the test PostgreSQL could not be started: {}\nEither free the \
                 container runtime, or set HEMS_TEST_POSTGRES to a server this \
                 suite may create databases on. `just db-stop` removes a stale \
                 container.",
                stderr.trim()
            ));
        }
    }
    Ok(())
}

/// Whether the shared container is up right now.
fn container_is_running() -> bool {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", CONTAINER])
        .output()
        .is_ok_and(|out| is_running(&out.stdout))
}

/// `docker inspect` said `running`.
///
/// The status rather than `.State.Running`, because the two disagree in exactly
/// the case this exists for: a `dead` container reports `Running=false` and a
/// status of `dead`, and a *stopped* one is already gone under `--rm`.
fn is_running(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).trim() == "running"
}

/// Wait until the server actually answers, or say what to do about it.
async fn wait_until_ready(base: &str) -> Result<(), String> {
    let settings = DbSettings {
        url: crate::Secret::literal(format!("{base}/postgres")),
        tls: false,
        pool_size: 1,
        acquire_timeout_s: 2,
        ..DbSettings::default()
    };
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        match crate::db::connect(&settings, "hems-test").await {
            Ok(pool) => {
                pool.close();
                return Ok(());
            }
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!(
        "the test PostgreSQL on port {PORT} did not accept a connection within \
         {}s: {last}\nRun `just db-stop` to remove a stale container, or set \
         HEMS_TEST_POSTGRES to a server this suite may create databases on.",
        READY_TIMEOUT.as_secs()
    ))
}
