//! Whether a driver is in contact with the thing it speaks for.

/// The state of the link to a device.
///
/// Reported rather than inferred, because "silent" and "connected but idle"
/// look identical from the outside and mean opposite things to the guard: a
/// controllable device nobody can hear is **assumed to be running flat out**,
/// which is the safe reading and an expensive one to apply to a device that is
/// merely doing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum LinkState {
    /// Never yet reached.
    Down,
    /// Handshaking — a SHIP pairing, a Modbus first read, a TLS session.
    Connecting,
    /// In contact and exchanging.
    Up,
    /// Was up, and has gone quiet without closing.
    ///
    /// Distinct from [`LinkState::Down`] because it is the state in which a
    /// § 14a failsafe is counting down: the peer has not said goodbye, it has
    /// stopped answering, and the difference decides whether the household is
    /// about to be restrained.
    Stale,
}

impl LinkState {
    /// Whether anything can be sent.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(self, LinkState::Up)
    }
}
