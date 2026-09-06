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

use crate::drivers::{Attached, DriverId};
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
    /// Whether to announce this box on the local network as `_ship._tcp`.
    ///
    /// SHIP § 6 is how an Energy Guard *finds* a Controllable System: the
    /// household listens, so without an announcement a Steuerbox has to be
    /// given an address by hand, and a certification lab asks for the
    /// DNS-SD record set as a prerequisite. On by default, because a box nobody
    /// can discover is a box an installer has to configure twice.
    ///
    /// Off is a real configuration and not a foot-gun: a box on a segmented
    /// network, or one reached through a router, is announced by something else
    /// or not at all.
    pub announce: bool,
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
            announce: true,
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
/// Returns the node, the SKI an installer has to hand the metering point
/// operator, and the private key as it is stored — which the trust surface needs
/// in order to write the identity row back after a pairing.
///
/// # Errors
/// [`ShipError::Identity`] where the key cannot be generated, stored or read,
/// and [`ShipError::NotASki`] where a configured peer is not a SKI.
pub async fn identity(
    settings: &ShipSettings,
    store: Option<&Arc<Mutex<Store>>>,
    now: time::OffsetDateTime,
) -> Result<(Node, Ski, String), ShipError> {
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

    let key_pem = identity.key_pem();
    Ok((
        Node::new(settings.ship_id.clone(), ShipTls::new(identity), trust),
        ski,
        key_pem,
    ))
}

/// A peer whose handshake is waiting for somebody to say yes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Waiting {
    /// Forty hexadecimal characters, as printed on the peer.
    pub ski: String,
    /// The SHA-256 fingerprint of its whole certificate — what a QR code carries.
    pub fingerprint: String,
}

/// Trusting a peer, and un-trusting it, while the box is running.
///
/// § 14a commissioning is one exchange of forty hex digits in each direction,
/// and field reports make it the single most common thing that goes wrong. Until
/// this existed the household's half was a **static list in a TOML file**: an
/// installer standing in a cellar with the Steuerbox's SKI in front of them had
/// to edit a configuration file and restart the box, and a box that restarts
/// loses its § 14a session, its plan and its place in the control period.
///
/// So a SKI can be approved on a running box. It costs nothing to make safe,
/// because SHIP already did the hard part: an unapproved peer completes TLS —
/// which *proves* its SKI rather than taking its word — and is held in the
/// pending state, and an approval added meanwhile lets that same waiting
/// handshake through. The installer reads the SKI off the Steuerbox, approves
/// it, and the session that was already waiting completes.
///
/// Persisted immediately, for the reason the key is: a pairing that did not
/// survive a power cut is a household whose § 14a compliance depends on somebody
/// being at home.
#[derive(Clone)]
pub struct Trust {
    node: Arc<Node>,
    store: Option<Arc<Mutex<Store>>>,
    ship_id: String,
    /// The box's own private key, as it is stored.
    ///
    /// Carried rather than read back from the node, which has no way to give it
    /// up — and rightly: a key an API could ask for is a key an API can leak.
    /// It is here because the stored identity is one row, so writing the trust
    /// store back means writing the key with it.
    key_pem: String,
}

impl Trust {
    /// The trust surface over a running box's own identity.
    #[must_use]
    pub fn new(
        node: Arc<Node>,
        store: Option<Arc<Mutex<Store>>>,
        ship_id: String,
        key_pem: String,
    ) -> Self {
        Self {
            node,
            store,
            ship_id,
            key_pem,
        }
    }

    /// The peers whose handshake is **waiting on a decision right now**, with the
    /// SKI an installer has to compare against the label on the box.
    ///
    /// This is the half of § 14a commissioning that used to be missing. An
    /// unapproved peer completes TLS — so its SKI is *proved* rather than
    /// claimed — and SHIP holds it in the pending state precisely so a person can
    /// read it. Without this the SKI had to come off the Steuerbox instead,
    /// which works and is the step field reports name as the most common
    /// commissioning failure in the whole installation.
    ///
    /// The fingerprint is the other identity SHIP knows a node by, and the one a
    /// QR code carries as `FPH256`; a peer may be admitted on either.
    #[must_use]
    pub fn waiting(&self) -> Vec<Waiting> {
        self.node
            .pending_peers()
            .into_iter()
            .map(|peer| Waiting {
                ski: peer.ski.to_display_string(),
                fingerprint: peer.fingerprint.to_string(),
            })
            .collect()
    }

    /// Turn a waiting peer down, so it learns it was refused rather than timing
    /// out.
    ///
    /// Says nothing about the future — the peer may ask again — and nothing at
    /// all about a peer that is not currently waiting.
    ///
    /// # Errors
    /// [`ShipError::NotASki`] where the text is not one.
    pub fn refuse(&self, ski: &str) -> Result<(), ShipError> {
        let parsed: Ski = ski
            .parse()
            .map_err(|_| ShipError::NotASki(ski.to_owned()))?;
        self.node.refuse_pairing(parsed);
        tracing::info!(ski = %parsed.to_display_string(), "a waiting peer was refused");
        Ok(())
    }

    /// The SKIs this box will exchange data with.
    #[must_use]
    pub fn peers(&self) -> Vec<String> {
        self.node
            .trust_store()
            .peers()
            .into_iter()
            .map(|peer| peer.ski.to_display_string())
            .collect()
    }

    /// Approve a peer, and remember it across a reboot.
    ///
    /// # Errors
    /// [`ShipError::NotASki`] where the text is not one — forty hexadecimal
    /// characters, as printed on the peer. Refused rather than stored, because a
    /// mistyped SKI is a peer that will never connect and nothing that says why.
    pub async fn approve(&self, ski: &str, now: time::OffsetDateTime) -> Result<(), ShipError> {
        let parsed: Ski = ski
            .parse()
            .map_err(|_| ShipError::NotASki(ski.to_owned()))?;
        self.node
            .trust_store()
            .remember(TrustedPeer::new(parsed).at_time(rfc3339(now)));
        tracing::info!(ski = %parsed.to_display_string(), "a peer was approved");
        self.persist(now).await
    }

    /// Withdraw approval, and forget it across a reboot.
    ///
    /// The peer's session is **not** torn down here, and that is deliberate: the
    /// § 14a session is how a reduction arrives, and dropping it the instant
    /// somebody revokes a SKI would take the household out of contact with its
    /// network operator on a keystroke. It cannot reconnect, which is what
    /// revocation means.
    ///
    /// # Errors
    /// [`ShipError::NotASki`] where the text is not one.
    pub async fn forget(&self, ski: &str, now: time::OffsetDateTime) -> Result<(), ShipError> {
        let parsed: Ski = ski
            .parse()
            .map_err(|_| ShipError::NotASki(ski.to_owned()))?;
        self.node.trust_store().forget(&parsed);
        tracing::warn!(
            ski = %parsed.to_display_string(),
            "a peer was un-trusted and will not be able to reconnect"
        );
        self.persist(now).await
    }

    /// Write the trust store back to the box's own database.
    async fn persist(&self, now: time::OffsetDateTime) -> Result<(), ShipError> {
        let Some(store) = &self.store else {
            // A box with no store is one whose identity is new on every boot
            // anyway, which is already logged at start-up. Approving still works
            // for as long as the process lives.
            return Ok(());
        };
        let trusted = self
            .node
            .trust_store()
            .to_json()
            .map_err(|e| ShipError::Identity(e.to_string()))?;
        let keep = StoredIdentity {
            ship_id: self.ship_id.clone(),
            key_pem: self.key_pem.clone(),
            trusted,
        };
        store
            .lock()
            .await
            .put_eebus_identity(&keep, now)
            .map_err(|e| ShipError::Identity(e.to_string()))
    }
}

/// Announce this box on the local network, so a Steuerbox can find it.
///
/// SHIP § 6: `_ship._tcp` with the TXT record set — the SHIP ID, the WebSocket
/// path, the SKI, and what a person would read on a pairing screen. Without it
/// the household is reachable only by an address somebody typed, which is the
/// one thing a network operator's box does not have.
///
/// Failure is a **warning and not a refusal**. A box on a network with no
/// multicast, in a container without host networking, or one whose mDNS
/// responder is already taken by the operating system, is a box that still
/// accepts every session a Steuerbox opens to its address. Refusing to start
/// would trade a working § 14a installation for a missing convenience.
fn announce(settings: &ShipSettings, ski: &Ski, port: u16) -> Option<eebus::mdns::Mdns> {
    if !settings.announce {
        return None;
    }
    let ship_id: eebus::ship::ShipId = settings.ship_id.parse().ok()?;
    let record = eebus::ship::ShipTxtRecord::new(ship_id, *ski)
        .with_brand("hems")
        // What a pairing screen shows beside the SKI. `EnergyManagementSystem`
        // rather than a device kind: `[A1 4.4.b]` is the whole household
        // addressed through one manager, and announcing a heat pump would send
        // an Energy Guard looking for one device's limit.
        .with_device_type("EnergyManagementSystem");
    let mut mdns = match eebus::mdns::Mdns::new() {
        Ok(mdns) => mdns,
        Err(error) => {
            tracing::warn!(%error, "this box cannot announce itself on the local network");
            return None;
        }
    };
    // Every address the responder can find. A box with one interface announces
    // one; a box on a wired and a wireless network announces both, and a
    // Steuerbox reaches it on whichever it shares.
    let addresses = local_addresses();
    if let Err(error) = mdns.announce(&settings.ship_id, &record, port, &addresses) {
        tracing::warn!(%error, "this box cannot announce itself on the local network");
        return None;
    }
    tracing::info!(
        ship_id = %settings.ship_id,
        %ski,
        port,
        "announced as _ship._tcp"
    );
    Some(mdns)
}

/// The addresses this box can be reached on.
///
/// A record announcing an address a Steuerbox cannot reach is worse than no
/// record: the peer resolves the service, dials, and fails — where an absent
/// announcement at least sends an installer to type the right one. So this is
/// every address of every interface that is up, **less** the ones nothing on the
/// household network can route to.
///
/// Loopback is the exception, and only when there is nothing else. A box that
/// has not been given an address yet — DHCP still running, cable not in — would
/// otherwise announce nothing at all, and a Steuerbox simulator on the same
/// machine is exactly how this gets tested.
fn local_addresses() -> Vec<std::net::IpAddr> {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let routable: Vec<std::net::IpAddr> = interfaces
        .iter()
        .map(if_addrs::Interface::ip)
        .filter(|ip| !ip.is_loopback() && !is_link_local(*ip))
        .collect();
    if routable.is_empty() {
        vec![std::net::IpAddr::from([127, 0, 0, 1])]
    } else {
        routable
    }
}

/// Whether an address is one only this link can reach.
///
/// `169.254.0.0/16` is what a machine gives itself when DHCP did not answer, and
/// `fe80::/10` is IPv6's equivalent. Announcing either tells a peer to dial an
/// address that works only if it happens to be on the same wire *and* guesses
/// the scope, which is not something a Steuerbox does.
fn is_link_local(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Accept Steuerbox sessions and move datagrams, until the process stops.
///
/// One connection at a time is deliberate: a Controllable System has exactly one
/// Energy Guard (LPC implementation guide § 3.8), and accepting a second while
/// one is open is how two boxes end up writing limits to the same household.
///
/// The announcement follows that rule rather than sitting beside it. SHIP asks a
/// node to **stop announcing while it cannot accept another connection**, so the
/// record is withdrawn for the length of a session and put back when it ends —
/// which is also the honest thing to publish, because a second Energy Guard that
/// found this box and dialled it would be refused.
pub async fn run(
    node: Arc<Node>,
    listener: tokio::net::TcpListener,
    registry: Shared,
    on: Attached,
    settings: ShipSettings,
    ski: Ski,
    shutdown: Shutdown,
) {
    let Attached { driver, asset } = on;
    let port = listener.local_addr().map_or(0, |a| a.port());
    let mut mdns = announce(&settings, &ski, port);
    loop {
        let accepted = tokio::select! {
            biased;
            () = shutdown.clone().wait() => {
                withdraw(&mut mdns);
                return;
            }
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
        //
        // `accept_reporting` rather than `accept` so the SKI reaches the log the
        // moment the peer is held, not only `/v1/pairing`. An installer with a
        // terminal and no browser is the ordinary case in a cellar, and this is
        // the number they have to compare with the label on the Steuerbox — the
        // § 14a commissioning step field reports name as the one that most often
        // goes wrong. The callback runs on this task, so it does nothing but log.
        let connection = match node
            .accept_reporting(
                stream,
                Some(Box::new(|peer: eebus::runtime::PendingPeer| {
                    tracing::warn!(
                        ski = %peer.ski.to_display_string(),
                        fingerprint = %peer.fingerprint,
                        "a peer is waiting to be approved — compare this SKI with the \
                         label on the device, then POST it to /v1/pairing"
                    );
                })),
            )
            .await
        {
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
        // The negotiated version is worth a line, because 1.0 and 1.1 differ in
        // a way that shows up much later: only 1.1 carries `accessMethods.id`,
        // which is what a peer would be dialled back with. An installer reading
        // a log after a failed reconnect is owed the fact here rather than
        // inferring it.
        tracing::info!(
            %from,
            peer = %connection.peer(),
            ship_version = connection.ship_version().map(|v| v.to_string()).as_deref().unwrap_or("unknown"),
            "a Steuerbox completed the SHIP handshake"
        );
        // The one slot is taken, so nothing should be told this box is free.
        withdraw(&mut mdns);
        session(connection, &registry, driver, &asset, &shutdown).await;
        registry
            .lock()
            .await
            .on_link(driver, LinkState::Down, time::OffsetDateTime::now_utc());
        tracing::info!(%from, "the SHIP session ended");
        mdns = announce(&settings, &ski, port);
    }
}

/// Stop announcing, and say nothing where there was nothing to stop.
fn withdraw(mdns: &mut Option<eebus::mdns::Mdns>) {
    if let Some(mut responder) = mdns.take()
        && let Err(error) = responder.withdraw()
    {
        tracing::warn!(%error, "the mDNS announcement could not be withdrawn");
    }
}

/// Dial a device on the household's own network, and keep dialling.
///
/// The mirror of [`run`], and the whole difference is who opens the socket. § 14a
/// is a network operator reaching *this* box, so the box listens; a hot-water
/// circuit is a device on the household's own network, so the box dials. The
/// datagram pump above the handshake is the same one either way, which is what
/// keeps there being one copy of it.
///
/// It reconnects for ever with a bounded backoff, because a device on a
/// household's own network is not a request that can fail: a heat pump on a
/// switched socket, a bridge somebody unplugged and a Wi-Fi access point that
/// rebooted all come back, and a task that gave up on the third attempt would
/// leave the planner with a tank it can no longer see.
///
/// The peer's SKI is **required** and is the whole of the trust decision. TLS
/// proves a peer's SKI rather than taking its word, so a box that dialled
/// whatever answered on the address would take a tank temperature from anything
/// on the network that offered one — and the plan would then heat against it.
pub async fn dial(
    node: Arc<Node>,
    address: String,
    peer: Ski,
    registry: Shared,
    on: Attached,
    shutdown: Shutdown,
) {
    let Attached { driver, asset } = on;
    node.trust_store().trust(peer);
    let mut backoff = RECONNECT_MIN;
    loop {
        if shutdown.is_triggered() {
            return;
        }
        let connection = tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            result = node.connect(address.clone()) => result,
        };
        match connection {
            Ok(connection) => {
                backoff = RECONNECT_MIN;
                tracing::info!(
                    %address,
                    peer = %connection.peer(),
                    %asset,
                    "dialled an EEBUS device"
                );
                session(connection, &registry, driver, &asset, &shutdown).await;
                registry.lock().await.on_link(
                    driver,
                    LinkState::Down,
                    time::OffsetDateTime::now_utc(),
                );
            }
            Err(error) => {
                // At `debug`, not `warn`. A device that is off overnight is the
                // ordinary case, and an hourly warning about it is how a log
                // stops being read.
                tracing::debug!(%address, %error, "the EEBUS device could not be dialled");
                registry.lock().await.on_link(
                    driver,
                    LinkState::Down,
                    time::OffsetDateTime::now_utc(),
                );
            }
        }
        tokio::select! {
            biased;
            () = shutdown.clone().wait() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(RECONNECT_MAX);
    }
}

/// How long to wait before dialling again, at first and at most.
const RECONNECT_MIN: std::time::Duration = std::time::Duration::from_secs(2);
const RECONNECT_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// One session: datagrams in, datagrams out, until it closes.
///
/// The driver's own deadline is what wakes it when nothing arrives — that is
/// where the heartbeat timeout lives, and it is the only path to the failsafe of
/// `[LPC-911]`.
async fn session(
    mut connection: eebus::runtime::ShipConnection,
    registry: &Shared,
    driver: DriverId,
    asset: &AssetId,
    shutdown: &Shutdown,
) {
    let now = || time::OffsetDateTime::now_utc();
    registry.lock().await.on_link(driver, LinkState::Up, now());

    loop {
        // Everything the driver wants to say goes out first, so a device that
        // has just been woken by its own timer is heard without waiting for the
        // peer to speak.
        loop {
            let outgoing = registry.lock().await.poll_transmit_of(driver);
            let Some(bytes) = outgoing else { break };
            let datagram = match serde_json::from_slice(&bytes) {
                Ok(datagram) => datagram,
                Err(error) => {
                    tracing::error!(%asset, %error, "the driver produced something that is not a datagram");
                    continue;
                }
            };
            if let Err(error) = connection.send(&datagram).await {
                tracing::warn!(%asset, %error, "the SHIP session could not be written to");
                return;
            }
        }

        let deadline = registry.lock().await.deadline_of(driver);
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
                            tracing::warn!(%asset, %error, "a datagram could not be handed to the driver");
                            continue;
                        }
                    };
                    let mut guard = registry.lock().await;
                    if let Err(error) = guard.on_bytes(driver, &bytes, now()) {
                        // A datagram the driver cannot read is a peer bug, not
                        // an outage: the session survives, because dropping it
                        // would take the household's § 14a link down for one
                        // malformed message.
                        tracing::warn!(%asset, %error, "a SPINE datagram could not be read");
                    }
                }
                Err(error) => {
                    tracing::info!(%asset, %error, "the SHIP session closed");
                    return;
                }
            },
            () = sleep_for(wait) => {
                registry.lock().await.on_timeout_of(driver, now());
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

#[cfg(test)]
mod announcement_tests {
    use super::*;

    /// An announcement is only useful if a peer can dial what it says.
    ///
    /// A record naming an address nothing on the household network can route to
    /// is worse than no record: the Steuerbox resolves the service, dials, and
    /// fails — where an absent announcement at least sends an installer to type
    /// the right address. So the filter is over what a peer could reach, and it
    /// is exercised here rather than being trusted, because the failure is
    /// invisible from the box's own side.
    #[test]
    fn an_address_no_peer_could_reach_is_not_announced() {
        use std::net::IpAddr;

        // What DHCP gives a machine that got no answer.
        assert!(is_link_local("169.254.13.7".parse::<IpAddr>().unwrap()));
        assert!(is_link_local("fe80::1".parse::<IpAddr>().unwrap()));
        // …and what an ordinary household network gives one.
        assert!(!is_link_local("192.168.1.42".parse::<IpAddr>().unwrap()));
        assert!(!is_link_local("10.0.0.5".parse::<IpAddr>().unwrap()));
        assert!(!is_link_local("fd00::1".parse::<IpAddr>().unwrap()));
    }

    /// A box with no network still announces something.
    ///
    /// Never empty, and that matters: `mdns-sd` given no addresses registers a
    /// service nothing can resolve, which looks exactly like a working
    /// announcement from this side.
    #[test]
    fn a_box_with_nowhere_to_be_reached_falls_back_to_loopback() {
        let announced = local_addresses();
        assert!(
            !announced.is_empty(),
            "a service registered with no addresses is one nothing can resolve"
        );
    }

    /// Announcing is the default, because a box nobody can discover is a box an
    /// installer has to configure twice.
    #[test]
    fn discovery_is_on_unless_it_is_turned_off() {
        assert!(ShipSettings::default().announce);
        let off: ShipSettings =
            toml::from_str("announce = false").expect("a partial section fills its defaults");
        assert!(!off.announce);
        assert_eq!(off.ship_id, ShipSettings::default().ship_id);
    }
}
