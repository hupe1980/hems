//! What a driver is, and what it may not be.
//!
//! A driver is the only part of this workspace that knows a protocol. It is
//! also the part most likely to be written by somebody who has never read the
//! rest of it, against a device that behaves badly, in a hurry. So the contract
//! is deliberately narrow: **bytes and a clock in, events and bytes out**.
//!
//! # Sans-I/O, and why that is not a style preference
//!
//! No socket, no thread, no clock. A driver is handed the bytes that arrived and
//! the time it is now, and it answers with what it would like to send and when
//! it would like to be woken. `hemsd` owns the socket and the runtime.
//!
//! Three things follow, and the third is the one that matters.
//!
//! *A whole day is a unit test.* The § 14a failsafe is a sixty-second heartbeat
//! and a two-hour minimum: a driver that read a clock could only be tested by
//! waiting. Passing time as a parameter makes "the Steuerbox goes quiet at
//! 17:04 and comes back at 19:11" an ordinary assertion, and it is the same
//! property `hems-core`, `hems-grid` and `hems-realtime` are built on — the
//! `just purity` gate fails the build if any of them reaches for a clock.
//!
//! *A device that misbehaves is reproducible.* The interesting failures are
//! partial frames, a register that stops updating, a peer that answers late.
//! All of them are a byte slice and a timestamp.
//!
//! *And the guard cannot be lied to by accident.* A driver reports what it
//! **read**; it does not decide what the site may do. That decision belongs to
//! `hems-realtime::Guard`, which sees every asset at once. A driver that
//! computed its own limit would be a second control plane nobody audited.
//!
//! # Two kinds of driver, one trait
//!
//! A **device** driver speaks to something the household owns: an inverter, a
//! wallbox, a meter. It reports [`DriverEvent::Measured`] and accepts
//! [`hems_core::setpoint::Command`].
//!
//! A **grid** driver speaks to something the network operator owns: a Steuerbox
//! over EEBUS, a ripple-control receiver. It reports
//! [`DriverEvent::GridLimit`] and accepts nothing — a household does not
//! command its own reduction.
//!
//! They are one trait because `hemsd` runs one loop, and because the difference
//! is in what a driver *emits* rather than in how it is driven. A driver
//! declares which of the two it is through [`DriverCapabilities`], and a
//! mismatch is caught at registration rather than by a limit that never arrives.
//!
//! # One crate, protocols behind features
//!
//! The protocols live here — [`eebus`] and [`modbus`] — rather than in a crate
//! each, and that was measured against this workspace's own sizing rather than
//! chosen by taste. All of it together is about two thousand lines; `hems-grid`
//! alone is five thousand and `hems-device` is eight hundred as a *single*
//! crate. Three crates for two thousand lines of pure code is ceremony, and this
//! project's standing rule is that machinery has to be earned: the *trait* is,
//! by two implementors of genuinely different shapes, and a crate each is not.
//!
//! The isolation a crate each would buy is bought by `optional = true` instead.
//! A box that speaks only Modbus never compiles, audits or ships the EEBUS
//! stack, because `--features modbus` does not pull it in. What one crate adds
//! is that the whole feature matrix is in one manifest rather than spread across
//! four.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]
#![allow(
    clippy::module_name_repetitions,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::cast_precision_loss
)]

mod capability;
mod event;
mod link;

#[cfg(feature = "eebus")]
pub mod eebus;
#[cfg(feature = "modbus")]
pub mod modbus;

pub use capability::DriverCapabilities;
pub use event::{CommandOutcome, DriverEvent, GridLimit, LimitDirection, LimitSource};
pub use link::LinkState;

use hems_core::prelude::AssetId;
use hems_core::setpoint::Command;
use time::OffsetDateTime;

/// Why a driver could not do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    /// The bytes were not a frame this protocol recognises.
    ///
    /// Recoverable: `hemsd` logs it and keeps the link. A device that emits one
    /// malformed frame an hour is a device, not an outage.
    #[error("malformed frame: {0}")]
    Malformed(String),
    /// The device answered, and the answer was a refusal.
    #[error("the device refused: {0}")]
    Refused(String),
    /// The command is one this driver cannot express on this protocol.
    ///
    /// A *programming* error rather than a runtime one: the arbiter should not
    /// have produced it, because [`DriverCapabilities`] said the device could
    /// not take it. Worth an error rather than a silent drop, because a command
    /// nobody sends and nobody reports is how a device quietly stops being
    /// managed.
    #[error("this driver cannot express {0}")]
    Unsupported(String),
    /// The link is not in a state where anything can be sent.
    #[error("not connected")]
    NotConnected,
    /// Nothing speaks for this asset at all.
    ///
    /// Distinct from [`DriverError::Unsupported`], and the distinction is what
    /// keeps a log readable: "this device cannot do that" is a fault to look
    /// into, and "this device has no driver" is a **configuration** fact that is
    /// equally true on every tick. Reported as the same error, a partially
    /// commissioned box writes one warning per asset per second for ever, and
    /// the real fault is somewhere in the middle of it.
    #[error("no driver speaks for {0}")]
    NoDriver(String),
}

/// The contract every driver keeps.
///
/// # The order `hemsd` calls these in
///
/// ```text
/// socket = connect()?;                       driver.on_link(Up, now);
/// loop {
///     // whichever comes first
///     bytes = read(socket, until: driver.poll_deadline())
///     match bytes {
///         Some(b)  => driver.on_bytes(&b, now)?,
///         None     => driver.on_timeout(now),
///         Closed   => { driver.on_link(Down, now); break }
///     }
///     while let Some(event) = driver.poll_event() { … }
///     while let Some(out)   = driver.poll_transmit() { write(socket, out) }
/// }
/// ```
///
/// Nothing here blocks and nothing here allocates a runtime. A driver that
/// needs to wait says so with [`Driver::poll_deadline`] and is called back.
pub trait Driver {
    /// Which asset this driver speaks for.
    ///
    /// One driver, one asset. A device that presents several — a hybrid
    /// inverter with a battery behind it — is several drivers over one
    /// transport, because the guard bounds assets and not boxes.
    fn asset(&self) -> &AssetId;

    /// What this driver can report and what it can be told.
    ///
    /// Read once at registration. `hemsd` refuses a site whose configuration
    /// asks a driver for something it has not declared, which is the cheap half
    /// of the bug where a command is sent for a year and silently ignored.
    fn capabilities(&self) -> DriverCapabilities;

    /// The transport under this driver opened or closed.
    ///
    /// # Why a byte-oriented contract still needs this
    ///
    /// A driver holds state that belongs to a *session* and not to a device:
    /// half a Modbus frame that has arrived and is not yet whole, a request that
    /// went out and is waiting for its answer, a SPINE peer it has discovered.
    /// A reconnect invalidates every one of them, and nothing in a stream of
    /// bytes says so — the first bytes of the new socket look exactly like the
    /// continuation of the old one, and a partial frame left over from before
    /// the drop makes the whole stream decode at an offset.
    ///
    /// So the layer that owns the socket says. It is the one fact only that
    /// layer knows, and leaving it out is how a box comes back from a
    /// twenty-second outage reading rubbish and reporting it as a measurement.
    ///
    /// The default is to do nothing, because a stateless driver genuinely has
    /// nothing to forget.
    fn on_link(&mut self, state: LinkState, now: OffsetDateTime) {
        let _ = (state, now);
    }

    /// Bytes arrived from the device.
    ///
    /// # Errors
    /// [`DriverError::Malformed`] when they are not a frame. The link survives;
    /// it is the caller's decision whether a rate of them is an outage.
    fn on_bytes(&mut self, bytes: &[u8], now: OffsetDateTime) -> Result<(), DriverError>;

    /// The deadline passed and nothing arrived.
    ///
    /// This is where a heartbeat is missed and a failsafe is entered, so it is
    /// **not** optional and it is not an error path: for a grid driver it is the
    /// most important call in the trait.
    fn on_timeout(&mut self, now: OffsetDateTime);

    /// Ask the device to do something.
    ///
    /// # Errors
    /// [`DriverError::Unsupported`] where [`Driver::capabilities`] said so, and
    /// [`DriverError::NotConnected`] where the link is down. Neither is a reason
    /// to drop the command silently.
    fn command(&mut self, command: &Command, now: OffsetDateTime) -> Result<(), DriverError>;

    /// The next thing that happened, if anything has.
    ///
    /// Drained to empty each turn. Events are in the order they occurred,
    /// because a limit and the acknowledgement of it are a sequence and a
    /// consumer that reordered them would write the wrong Nachweis.
    fn poll_event(&mut self) -> Option<DriverEvent>;

    /// The next bytes to put on the wire, if any.
    fn poll_transmit(&mut self) -> Option<Vec<u8>>;

    /// When to call [`Driver::on_timeout`] if nothing arrives before then.
    ///
    /// `None` means "only when bytes arrive". A driver with a heartbeat should
    /// never return `None`: that is what makes the failsafe reachable.
    fn poll_deadline(&self) -> Option<OffsetDateTime>;
}
