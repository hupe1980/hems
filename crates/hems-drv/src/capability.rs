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
    reason = "a capability set is a handful of independent yes/no facts about one \
              driver; folding them into a state machine would relate things that \
              are not related"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DriverCapabilities {
    /// It reports what the device is doing, and is **expected to**.
    ///
    /// The second half is what the flag is for. It is what "not being heard
    /// from" is judged by: a driver that declares it measures and then goes
    /// quiet is a fault, and one that never claimed to is not. The § 14a grid
    /// driver is the case that makes the distinction matter — `[A1 4.6]` is an
    /// *instruction*, so it carries limits and link state and owes no reading,
    /// and judging it by the age of one counted a perfectly connected Steuerbox
    /// as a device nobody could hear on every box that has one.
    ///
    /// It stays `false` for that driver even though it *can* report the
    /// connection point's power, because MGCP scenarios 1 and 2 are Optional and
    /// Recommended on the peer and a § 14a household is lawful with neither. A
    /// measurement that does arrive is used; its absence is not a fault.
    pub measures: bool,
    /// It can be told a power setpoint or ceiling.
    pub accepts_commands: bool,
    /// It can be told which way a reversible thermal device should run.
    ///
    /// A power is not an instruction for a machine with two directions. A
    /// reversible heat pump handed "draw four kilowatts" in July with no
    /// direction beside it runs whichever way its own thermostat last chose, and
    /// no meter can tell afterwards which that was — so a planner that decided
    /// to pre-cool a house into a cheap afternoon gets pre-*heating* and reports
    /// a saving it never made.
    ///
    /// Declared rather than assumed for the reason the rest of this struct is:
    /// a household whose unit is reversible and whose driver cannot turn it
    /// round is refused at start-up (`RegistryError::CannotSetThermalMode`)
    /// instead of running the wrong way for a summer. EEBUS has no cooling
    /// process to match `OHPCF`, so today this is a vendor register map or
    /// nothing, and saying which is the driver's job.
    pub sets_thermal_mode: bool,
    /// It carries limits from the network operator.
    ///
    /// The distinguishing mark of a **grid** driver. A site that declares § 14a
    /// participation and has no driver with this set is a site whose reductions
    /// can only ever arrive from the simulator, and `hemsd` says so at startup
    /// rather than after the first control event is missed.
    pub reports_grid_limits: bool,
    /// Its values arrive when they **change**, not on a cadence.
    ///
    /// The difference decides whether the age of the last reading says anything
    /// about the device. A driver that polls reads every second, so a reading
    /// older than a few of those means the device stopped answering. One whose
    /// values are notified on change — EEBUS MDT, MRT, MOT — is silent exactly
    /// while nothing is happening, and a hot-water tank holding 52 °C or a room
    /// holding 21 °C is silent for hours. That is the protocol working.
    ///
    /// Notified-versus-polled is *not* the distinction, which is worth saying
    /// because it is the one to reach for. Every EEBUS scenario here is
    /// subscription-driven — each UC TS §3.4.n.1 asks an actor to subscribe, and
    /// §3.3.4 names polling only as the fallback for a subscription that was
    /// refused. What separates them is whether the notification comes on a
    /// **clock**: `eebus`'s `Delivery::Periodic` marks the ones that do, and in
    /// all fifty-seven descriptors those are the heartbeats and nothing else.
    ///
    /// Judging one by the age of its last reading drops it from the site's state
    /// seconds after every reading, so the tank and the building are in the plan
    /// only in the moments just after they change temperature — which is the
    /// opposite of when a plan needs them. It is the same carve-out
    /// [`DriverCapabilities::measures`] already makes for the § 14a driver, one
    /// step further in: what a subscribed driver owes is a **link**, and it says
    /// so on its own initiative.
    ///
    /// What this gives up is the device that keeps its socket open and stops
    /// updating. Only a timestamp *from the peer* can tell that apart from a
    /// value that is simply still true, and where one arrives the driver stamps
    /// the measurement with it and the age is meaningful again.
    ///
    /// Held to the specification rather than to judgement:
    /// `the_only_thing_this_box_may_time_out_on_is_a_heartbeat` checks each
    /// driver's answer here against `UseCaseDescriptor::periodic_functions` for
    /// the use cases it plays, so a driver that gains one with a clock on it
    /// fails the build rather than a household.
    pub reports_on_change: bool,
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
    /// The same, and able to turn a reversible thermal device round.
    ///
    /// A builder rather than a sixth constructor: the direction is orthogonal to
    /// every other question here — a unit can be measured or not, polled or
    /// notified, and still reversible — and a constructor per combination is the
    /// matrix this struct exists to avoid.
    #[must_use]
    pub const fn setting_thermal_mode(mut self) -> Self {
        self.sets_thermal_mode = true;
        self
    }

    /// A driver that reports a device and takes commands.
    #[must_use]
    pub const fn device() -> Self {
        Self {
            measures: true,
            accepts_commands: true,
            reports_grid_limits: false,
            reports_on_change: false,
            reports_available_power: false,
            sets_thermal_mode: false,
        }
    }

    /// A driver that only listens — a meter.
    #[must_use]
    pub const fn meter() -> Self {
        Self {
            measures: true,
            accepts_commands: false,
            reports_grid_limits: false,
            reports_on_change: false,
            reports_available_power: false,
            sets_thermal_mode: false,
        }
    }

    /// A driver that takes commands and measures nothing.
    ///
    /// The shape of an EEBUS use case that *drives* a device whose consumption
    /// somebody else meters — a compressor over OHPCF beside the site's own
    /// meter. Claiming `measures` here would have two drivers reporting one
    /// asset, which the registry refuses at start-up and is right to.
    #[must_use]
    pub const fn commanding() -> Self {
        Self {
            measures: false,
            accepts_commands: true,
            reports_grid_limits: false,
            reports_on_change: false,
            reports_available_power: false,
            sets_thermal_mode: false,
        }
    }

    /// A driver that watches a device without metering or driving it.
    ///
    /// The shape of a use case that reports a *session fact* rather than a
    /// quantity: EEBUS EVCC and EVSOC say whether there is a car on the cable
    /// and how full it is, and never what the wallbox is drawing — the wallbox's
    /// own Modbus driver does that, and commands it.
    ///
    /// `measures` is false and that is the point rather than modesty. The flag
    /// is what **silence is judged by**, so a driver that claimed it and then
    /// only ever reported a car arriving would be counted as a device nobody can
    /// hear for the whole of every day the car is away.
    #[must_use]
    pub const fn observing() -> Self {
        Self {
            measures: false,
            accepts_commands: false,
            reports_grid_limits: false,
            reports_on_change: false,
            reports_available_power: false,
            sets_thermal_mode: false,
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
            reports_on_change: false,
            reports_available_power: false,
            sets_thermal_mode: false,
        }
    }

    /// The same, and its values arrive on change rather than on a cadence.
    ///
    /// For a driver none of whose use cases put a function on a clock — EEBUS's
    /// measurement family — where silence is the protocol working rather than a
    /// device that has gone away.
    #[must_use]
    pub const fn on_change(mut self) -> Self {
        self.reports_on_change = true;
        self
    }

    /// The same, and it publishes available power.
    #[must_use]
    pub const fn with_available_power(mut self) -> Self {
        self.reports_available_power = true;
        self
    }
}
