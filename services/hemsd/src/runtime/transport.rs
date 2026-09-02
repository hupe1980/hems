//! One socket, one driver, for as long as the process lives.
//!
//! The drivers are sans-I/O: bytes and a clock in, events and bytes out. This is
//! the other half — the task that owns a socket, gives its driver the bytes that
//! arrived, wakes it when nothing did, writes back whatever it produced, and
//! reconnects when the connection goes.
//!
//! # The loop, and the two things that are easy to get wrong
//!
//! ```text
//! loop {
//!     socket = connect().await   // backing off, for ever
//!     registry.on_link(asset, Up, now)
//!     loop {
//!         flush(registry.poll_transmit_of(asset))
//!         select {
//!             n = socket.read()             => registry.on_bytes(asset, .., now)
//!             _ = sleep_until(deadline)     => registry.on_timeout_of(asset, now)
//!         }
//!     }
//!     registry.on_link(asset, Down, now)
//! }
//! ```
//!
//! **A reconnect is told to the driver.** Nothing in a stream of bytes says the
//! socket underneath it is a new one, and the first bytes of a fresh connection
//! look exactly like the continuation of the old one — so a half-frame left over
//! from before the drop makes the whole stream decode at an offset. That is what
//! [`hems_drv::Driver::on_link`] is for, and it is why the transport calls it on
//! both edges rather than only on failure.
//!
//! **A deadline that has already passed is not a sleep.** `poll_deadline` is a
//! wall-clock instant, and a driver whose deadline is in the past wants waking
//! *now*. Computing `deadline − now` as an unsigned duration and sleeping for it
//! is how a timeout becomes a seventy-year wait; this saturates at zero instead.
//!
//! # Why it never gives up
//!
//! A household gateway box is not a request that can fail. An inverter that is
//! off overnight, a wallbox on a switched socket, a Wi-Fi bridge somebody
//! unplugged — all of them come back, and a driver task that exited on the third
//! attempt would leave the guard assuming a nameplate for the rest of the year.
//! So the backoff is bounded and the loop is not.

use std::sync::Arc;

use hems_core::prelude::AssetId;
use hems_drv::LinkState;
use hems_service::Shutdown;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::drivers::Registry;

/// The shared driver set.
pub type Shared = Arc<Mutex<Registry>>;

/// How long to wait before the first retry.
const BACKOFF_MIN: std::time::Duration = std::time::Duration::from_secs(1);

/// And the longest.
///
/// A minute rather than an hour: the thing on the other end is on the same
/// household network, and a box that waited an hour to notice an inverter had
/// come back would spend that hour planning around a device it could not hear.
const BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// How much is read from a socket at a time.
///
/// Modbus TCP frames are at most 260 bytes and SPINE datagrams are a few
/// kilobytes; the driver reassembles whatever arrives, so this is a buffer size
/// rather than a protocol limit.
const READ_BUFFER: usize = 8 * 1024;

/// Connect to `address`, run `asset`'s driver against it, and keep doing so.
///
/// Returns when `shutdown` is triggered, and not before — there is no error
/// path, and that is the design rather than an omission: see the module note on
/// why a household gateway box is not a request that can fail.
pub async fn tcp(registry: Shared, asset: AssetId, address: String, shutdown: Shutdown) {
    let mut backoff = BACKOFF_MIN;
    loop {
        if shutdown.is_triggered() {
            return;
        }
        let stream = tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            connected = TcpStream::connect(&address) => connected,
        };
        let stream = match stream {
            Ok(stream) => {
                // Nagle's algorithm holds a small write back waiting for a
                // second one. Every frame here is small and every one of them is
                // a request whose answer the driver is timing, so the delay
                // lands directly on the control loop's latency.
                let _ = stream.set_nodelay(true);
                tracing::info!(%asset, %address, "connected");
                backoff = BACKOFF_MIN;
                stream
            }
            Err(error) => {
                tracing::warn!(%asset, %address, %error, retry_in_s = backoff.as_secs(), "could not connect");
                tokio::select! {
                    () = shutdown.clone().wait() => return,
                    () = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };

        session(&registry, &asset, stream, &shutdown).await;
        registry
            .lock()
            .await
            .on_link(&asset, LinkState::Down, now());
        if shutdown.is_triggered() {
            return;
        }
        tracing::warn!(%asset, %address, "the link went down");
        tokio::select! {
            () = shutdown.clone().wait() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Give a driver its clock, and nothing else.
///
/// For a driver this build has no transport for — today that is the EEBUS
/// Controllable System, whose SHIP session is blocked upstream. It would be
/// tempting to leave such a driver alone until there is a socket for it, and
/// that is exactly what makes its state a lie.
///
/// The LPC machine has five states and every transition out of the first one is
/// a **timer**. A Controllable System nobody ticks stays in `Init` for ever —
/// where `effective_limit` is the failsafe value — so a box would hold the
/// household at its `[A1 4.5.2]` minimum indefinitely on the strength of an
/// implementation accident, and no rule asks for that. Given its clock, the same
/// machine does what the specification says: with no Energy Guard ever in
/// contact it goes `UnlimitedAutonomous` after the heartbeat window
/// (`[LPC-922]` — a limit nobody is maintaining does not last for ever), and it
/// **reports** that, which is what the evidence record and the screen need.
///
/// It is not a substitute for the transport, and nothing here pretends it is:
/// the readiness probe says the box cannot hear a reduction. What it buys is
/// that the state the box reports is the state the machine is actually in.
pub async fn clock_only(registry: Shared, asset: AssetId, shutdown: Shutdown) {
    loop {
        let deadline = registry.lock().await.deadline_of(&asset);
        let wait = deadline.map_or(
            // No deadline at all: nothing to wake for, so wait for the shutdown.
            // A driver in this state is inert by its own account.
            std::time::Duration::from_secs(60),
            |at| std::time::Duration::try_from(at - now()).unwrap_or(std::time::Duration::ZERO),
        );
        tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            () = tokio::time::sleep(wait) => {
                registry.lock().await.on_timeout_of(&asset, now());
            }
        }
    }
}

/// One connection, from the handshake to whatever ends it.
async fn session(registry: &Shared, asset: &AssetId, mut stream: TcpStream, shutdown: &Shutdown) {
    registry.lock().await.on_link(asset, LinkState::Up, now());
    let mut buffer = vec![0_u8; READ_BUFFER];

    loop {
        // Everything the driver wants to say goes out first, so a driver that
        // has just been woken does not have to wait for the next read to be
        // heard. A write that fails ends the session: the socket is gone and the
        // bytes the driver produced are lost, which the reconnect will notice
        // because the driver is told the link dropped.
        loop {
            let outgoing = registry.lock().await.poll_transmit_of(asset);
            let Some(bytes) = outgoing else { break };
            if let Err(error) = stream.write_all(&bytes).await {
                tracing::warn!(%asset, %error, "the write failed");
                return;
            }
        }

        let deadline = registry.lock().await.deadline_of(asset);
        let wait = deadline.map(|at| {
            // Saturating: a deadline already in the past means "wake now", and
            // an unsigned conversion of a negative duration is a seventy-year
            // sleep with no symptom but a device that is never polled again.
            std::time::Duration::try_from(at - now()).unwrap_or(std::time::Duration::ZERO)
        });

        tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            read = stream.read(&mut buffer) => match read {
                Ok(0) => {
                    tracing::info!(%asset, "the peer closed the connection");
                    return;
                }
                Ok(n) => {
                    let mut guard = registry.lock().await;
                    if let Err(error) = guard.on_bytes(asset, &buffer[..n], now()) {
                        // A malformed frame is a device, not an outage: the
                        // driver has already resynchronised as best it can, and
                        // dropping the connection for one would turn a noisy
                        // gateway into a device that is never read at all.
                        tracing::warn!(%asset, %error, "a frame could not be read");
                    }
                }
                Err(error) => {
                    tracing::warn!(%asset, %error, "the read failed");
                    return;
                }
            },
            () = sleep_for(wait) => {
                registry.lock().await.on_timeout_of(asset, now());
            }
        }
    }
}

/// Sleep for `wait`, or for ever where a driver has no deadline.
///
/// `None` means "only when bytes arrive", which is a legitimate answer for a
/// device that is purely reactive — and pending for ever is exactly right inside
/// a `select!` that has a read and a shutdown in its other arms.
async fn sleep_for(wait: Option<std::time::Duration>) {
    match wait {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// The wall clock, in one place.
///
/// The whole reason the drivers and the control planes take time as a parameter
/// is that this call exists in as few places as possible; `just purity` fails
/// the build if one of them reaches for it.
fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}
