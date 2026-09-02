//! The session a § 14a limit actually arrives over.
//!
//! `hems-drv/eebus` is the Controllable System: it runs the LPC state machine,
//! owns a SPINE engine, and takes and gives SPINE datagrams as bytes — which is
//! exactly what a SHIP data frame carries. This is the layer underneath: TCP,
//! TLS 1.2 with mutual authentication, the WebSocket upgrade, the SHIP
//! handshake, and the framing.
//!
//! Nothing about the protocol is decided here. What crosses the seam is a
//! datagram, so a limit that arrives in `crates/hems-drv/tests/eebus_spine.rs`
//! arrives on a real box, and there is exactly one copy of the § 14a state
//! machine in the product.
//!
//! # The household listens; the Steuerbox dials
//!
//! The Energy Guard is the network operator's box and it is the side that
//! connects — it browses for Controllable Systems and opens a session to the
//! ones it has been told to trust. So this binds a listener and accepts, rather
//! than dialling out to an address a household would have to be told.
//!
//! # The identity has to survive a reboot
//!
//! SHIP's whole trust model is a list of SKIs, and a SKI follows the *key*. An
//! installer reads this box's SKI off a screen and gives it to the metering
//! point operator; field reports make that exchange the most common § 14a
//! commissioning failure there is. A box that generated a fresh key on every
//! boot would make it fail again on every boot, so the key is kept in the box's
//! own store and the certificate is re-issued from it.
//!
//! The trust store is kept with it, for the mirror-image reason: a household
//! that had to re-pair its Steuerbox after a power cut is a household whose §
//! 14a compliance depends on somebody being at home.

use std::sync::Arc;

use eebus::cert::{self, CertParams};
use eebus::runtime::{Node, TrustStore, TrustedPeer};
use eebus::ship::Ski;
use eebus::tls::ShipTls;
use hems_core::prelude::AssetId;
use hems_drv::LinkState;
use hems_service::Shutdown;
use tokio::sync::Mutex;

use crate::runtime::transport::Shared;
use crate::store::{Store, StoredIdentity};

/// How the box presents itself on the EEBUS network.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShipSettings {
    /// Where to accept a Steuerbox, `host:port`.
    ///
    /// `None` leaves the Controllable System running its clock and hearing
    /// nothing, which is a household whose § 14a driver is configured and whose
    /// session is not.
    pub listen: Option<String>,
    /// The SHIP ID this node announces, `<IANA PEN>_<vendor product id>`.
    ///
    /// It is the certificate's common name and what a peer sees before it has
    /// anything else to go on. It is **not** the identity — that is the SKI, and
    /// the SKI follows the key.
    pub ship_id: String,
    /// The SKIs this box will exchange data with.
    ///
    /// A peer that is not here may still connect and complete TLS — it has to,
    /// so that its SKI can be shown to a user — and is held short of the data
    /// phase. Adding one is what "an installer approved this Steuerbox" means.
    ///
    /// Merged with whatever the store already trusts, so a pairing done through
    /// a screen is not undone by a deployment that did not know about it.
    pub trust: Vec<String>,
}

impl Default for ShipSettings {
    fn default() -> Self {
        Self {
            listen: None,
            // hems has no IANA Private Enterprise Number, and inventing one
            // would be claiming somebody else's. The `n:` form of a SPINE
            // address is the honest equivalent and this is its SHIP counterpart.
            ship_id: "hems_hems-1".into(),
            trust: Vec::new(),
        }
    }
}

/// Why a SHIP session could not be started.
#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    /// The identity could not be created or read back.
    #[error("the box's EEBUS identity is not usable: {0}")]
    Identity(String),
    /// A configured SKI is not one.
    #[error("`{0}` is not a SKI: forty hexadecimal characters, as printed on the peer")]
    NotASki(String),
    /// The listener could not be bound.
    #[error("the SHIP listener could not bind to {address}: {source}")]
    Listen {
        /// Where.
        address: String,
        /// Why not.
        source: std::io::Error,
    },
}

/// The box's own SHIP identity, created once and then read back.
///
/// Returns the node and the SKI an installer has to hand the metering point
/// operator.
///
/// # Errors
/// [`ShipError::Identity`] where the key cannot be generated, stored or read,
/// and [`ShipError::NotASki`] where a configured peer is not a SKI.
pub async fn identity(
    settings: &ShipSettings,
    store: Option<&Arc<Mutex<Store>>>,
    now: time::OffsetDateTime,
) -> Result<(Node, Ski), ShipError> {
    let stored = match store {
        Some(store) => store
            .lock()
            .await
            .eebus_identity()
            .map_err(|e| ShipError::Identity(e.to_string()))?,
        None => None,
    };

    // A key that already exists is re-used and its certificate re-issued: the
    // SKI follows the key, so every trust relationship this box has established
    // survives a longer validity or a corrected name.
    let params = CertParams::new(settings.ship_id.clone());
    let identity = match &stored {
        Some(kept) => {
            let key = cert::key_from_pem(&kept.key_pem)
                .map_err(|e| ShipError::Identity(format!("the stored key: {e}")))?;
            cert::self_signed_with(params, key)
        }
        None => cert::self_signed(params),
    }
    .map_err(|e| ShipError::Identity(e.to_string()))?;
    let ski = identity.ski;

    // What the store already trusts, plus what the configuration names. The
    // union rather than either alone: a pairing done through a screen must not
    // be undone by a deployment that did not know about it, and a Steuerbox
    // named in the file must not need a screen.
    let trust = match &stored {
        Some(kept) => TrustStore::from_json(&kept.trusted).unwrap_or_default(),
        None => TrustStore::new(),
    };
    for configured in &settings.trust {
        let ski: Ski = configured
            .parse()
            .map_err(|_| ShipError::NotASki(configured.clone()))?;
        trust.remember(TrustedPeer::new(ski).at_time(rfc3339(now)));
    }

    if let Some(store) = store {
        let keep = StoredIdentity {
            ship_id: settings.ship_id.clone(),
            key_pem: identity.key_pem(),
            trusted: trust.to_json().unwrap_or_else(|_| "[]".into()),
        };
        store
            .lock()
            .await
            .put_eebus_identity(&keep, now)
            .map_err(|e| ShipError::Identity(e.to_string()))?;
    } else {
        // A box with no store gets a fresh SKI on every boot, which means
        // re-pairing on every boot. Safe, useless, and silent unless said.
        tracing::warn!(
            "no store is configured, so this box's EEBUS identity is new on every \
             start and its Steuerbox will have to be paired again each time"
        );
    }

    Ok((
        Node::new(settings.ship_id.clone(), ShipTls::new(identity), trust),
        ski,
    ))
}

/// Accept Steuerbox sessions and move datagrams, until the process stops.
///
/// One connection at a time is deliberate: a Controllable System has exactly one
/// Energy Guard (LPC implementation guide § 3.8), and accepting a second while
/// one is open is how two boxes end up writing limits to the same household.
pub async fn run(
    node: Node,
    listener: tokio::net::TcpListener,
    registry: Shared,
    asset: AssetId,
    shutdown: Shutdown,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, from) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, "a SHIP connection could not be accepted");
                continue;
            }
        };
        // The handshake, which is where each end learns the other's SKI and
        // where an untrusted peer is held short of the data phase.
        let connection = match node.accept(stream).await {
            Ok(connection) => connection,
            Err(error) => {
                // Worth a line rather than silence: the ordinary cause is a
                // Steuerbox whose SKI nobody has approved yet, and that is the
                // commissioning step this workspace names as the one that most
                // often goes wrong.
                tracing::warn!(%from, %error, "the SHIP handshake did not complete");
                continue;
            }
        };
        tracing::info!(
            %from,
            peer = %connection.peer(),
            "a Steuerbox completed the SHIP handshake"
        );
        session(connection, &registry, &asset, &shutdown).await;
        registry
            .lock()
            .await
            .on_link(&asset, LinkState::Down, time::OffsetDateTime::now_utc());
        tracing::info!(%from, "the SHIP session ended");
    }
}

/// One session: datagrams in, datagrams out, until it closes.
///
/// The driver's own deadline is what wakes it when nothing arrives — that is
/// where the heartbeat timeout lives, and it is the only path to the failsafe of
/// `[LPC-911]`.
async fn session(
    mut connection: eebus::runtime::ShipConnection,
    registry: &Shared,
    asset: &AssetId,
    shutdown: &Shutdown,
) {
    let now = || time::OffsetDateTime::now_utc();
    registry.lock().await.on_link(asset, LinkState::Up, now());

    loop {
        // Everything the driver wants to say goes out first, so a device that
        // has just been woken by its own timer is heard without waiting for the
        // peer to speak.
        loop {
            let outgoing = registry.lock().await.poll_transmit_of(asset);
            let Some(bytes) = outgoing else { break };
            let datagram = match serde_json::from_slice(&bytes) {
                Ok(datagram) => datagram,
                Err(error) => {
                    tracing::error!(%error, "the driver produced something that is not a datagram");
                    continue;
                }
            };
            if let Err(error) = connection.send(&datagram).await {
                tracing::warn!(%error, "the SHIP session could not be written to");
                return;
            }
        }

        let deadline = registry.lock().await.deadline_of(asset);
        let wait = deadline.map(|at| {
            // Saturating: a deadline already in the past means "wake now". An
            // unsigned conversion of a negative duration is a seventy-year
            // sleep, and here it is a failsafe that never engages.
            std::time::Duration::try_from(at - now()).unwrap_or(std::time::Duration::ZERO)
        });

        tokio::select! {
            biased;
            () = shutdown.clone().wait() => {
                let _ = connection
                    .close(eebus::ship::ConnectionCloseReason::Unspecific,
                           std::time::Duration::from_secs(2))
                    .await;
                return;
            }
            received = connection.recv() => match received {
                Ok(datagram) => {
                    let bytes = match serde_json::to_vec(&datagram) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            tracing::warn!(%error, "a datagram could not be handed to the driver");
                            continue;
                        }
                    };
                    let mut guard = registry.lock().await;
                    if let Err(error) = guard.on_bytes(asset, &bytes, now()) {
                        // A datagram the driver cannot read is a peer bug, not
                        // an outage: the session survives, because dropping it
                        // would take the household's § 14a link down for one
                        // malformed message.
                        tracing::warn!(%error, "a SPINE datagram could not be read");
                    }
                }
                Err(error) => {
                    tracing::info!(%error, "the SHIP session closed");
                    return;
                }
            },
            () = sleep_for(wait) => {
                registry.lock().await.on_timeout_of(asset, now());
            }
        }
    }
}

/// Sleep for `wait`, or for ever where the driver has no deadline.
async fn sleep_for(wait: Option<std::time::Duration>) {
    match wait {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

/// An instant in the form `TrustedPeer` records.
fn rfc3339(at: time::OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
