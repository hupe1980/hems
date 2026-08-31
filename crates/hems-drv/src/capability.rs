//! What a driver has declared it can do.

/// What a driver can report and what it can be told.
///
/// Read once, at registration. It exists so that a mismatch between a site's
/// configuration and the hardware behind it is caught **then**, rather than by a
/// command that is sent for a year and silently ignored — which is the failure
/// this workspace keeps finding in itself, and which no property test catches
/// because a property is a statement about code that runs.
#[expect(
    clippy::struct_excessive_bools,
    reason = "a capability set is four independent yes/no facts about one driver; \
              folding them into a state machine would relate things that are not related"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DriverCapabilities {
    /// It reports what the device is doing.
    pub measures: bool,
    /// It can be told a power setpoint or ceiling.
    pub accepts_commands: bool,
    /// It carries limits from the network operator.
    ///
    /// The distinguishing mark of a **grid** driver. A site that declares § 14a
    /// participation and has no driver with this set is a site whose reductions
    /// can only ever arrive from the simulator, and `hemsd` says so at startup
    /// rather than after the first control event is missed.
    pub reports_grid_limits: bool,
    /// It publishes what a generator *could* produce, not only what it is
    /// producing.
    ///
    /// The one capability that cannot be worked around, and the reason it is a
    /// flag rather than an assumption. A curtailed inverter asked what it is
    /// producing answers with what the manager already commanded, so a
    /// controller reading that alone never lifts its own curtailment. SunSpec
    /// model 701 and EEBUS `MOI` publish the figure; a cheap inverter behind a
    /// vendor HTTP API does not.
    ///
    /// Where it is `false`, the fallback is the inverter's **nameplate** —
    /// optimistic, and self-correcting on the next tick, which is the right way
    /// round for a quantity that only ever *relaxes* a bound. Where it is
    /// `true`, nothing is guessed. A household is entitled to know which of the
    /// two its box is running on, so it is reported rather than inferred.
    pub reports_available_power: bool,
}

impl DriverCapabilities {
    /// A driver that reports a device and takes commands.
    #[must_use]
    pub const fn device() -> Self {
        Self {
            measures: true,
            accepts_commands: true,
            reports_grid_limits: false,
            reports_available_power: false,
        }
    }

    /// A driver that only listens — a meter.
    #[must_use]
    pub const fn meter() -> Self {
        Self {
            measures: true,
            accepts_commands: false,
            reports_grid_limits: false,
            reports_available_power: false,
        }
    }

    /// A driver that carries the network operator's limits and nothing else.
    ///
    /// It accepts no commands, and that is the regulation rather than an
    /// omission: a household does not command its own reduction.
    #[must_use]
    pub const fn grid() -> Self {
        Self {
            measures: false,
            accepts_commands: false,
            reports_grid_limits: true,
            reports_available_power: false,
        }
    }

    /// The same, and it publishes available power.
    #[must_use]
    pub const fn with_available_power(mut self) -> Self {
        self.reports_available_power = true;
        self
    }

    /// The same, and it also measures.
    #[must_use]
    pub const fn with_measurements(mut self) -> Self {
        self.measures = true;
        self
    }
}
