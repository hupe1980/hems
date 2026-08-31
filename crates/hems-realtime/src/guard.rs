//! The guard plane: limits that nothing else may argue with.
//!
//! `[BK6-22-300 A1 4.6 S. 3]` requires that where a network operator's reduction
//! conflicts with market-driven control — a cheap hour, a full battery, a
//! customer in a hurry — **the reduction wins**. An optimiser cannot be trusted
//! with that, because an optimiser's job is to trade things off, and this is not
//! tradeable. So it lives here, applied last, as an intersection of intervals
//! that the layers above can only narrow further.
//!
//! # Two kinds of limit
//!
//! **Per-asset** limits bind one device on its own, and intersecting them is all
//! there is to it:
//!
//! | Bound | Source |
//! |---|---|
//! | What the hardware can do, asymmetrically | the datasheet |
//! | State of charge, and the backup reserve | the device, and the household |
//! | Every fuse on the path from the asset to the connection | VDE-AR-N 4100 |
//! | Feed-in, for anything that can produce | § 9 EEG, EEBUS LPP, `[MGCP-011]` |
//!
//! **Shared** limits bind a *set* of devices together, and each is spent by a
//! different set — which is the distinction that makes them worth separating:
//!
//! | Bound | Spent by | Source |
//! |---|---|---|
//! | The § 14a ceiling | the controllable devices only | `[A1 4.4.b]`, EEBUS LPC |
//! | The grid connection | everything behind it | the connection agreement |
//! | Each sub-distribution board | everything below it | VDE-AR-N 4100 |
//! | Schieflast, 4,6 kVA | the single-phase devices on one conductor | VDE-AR-N 4100 |
//!
//! Each is shared out by [`mod@crate::allocate`] and the results are
//! **intersected**. Because a per-asset minimum of two allocations can only be
//! smaller than either, respecting several shared limits at once needs no case
//! analysis. Bounding each asset by a shared limit *individually* is the mistake
//! that looks right and is not: 11 kW of wallbox, 8 kW of heat pump and 5 kW of
//! battery each fit under a 24 kW connection, and burn it together.
//!
//! # The § 14a budget is not the § 14a limit
//!
//! The limit applies to the **netzwirksamer Leistungsbezug** (`[A1 2.3]`) — the
//! part of the grid draw the controllable devices cause. While the photovoltaic
//! system covers the rest of the house, its surplus can drive them on top of the
//! limit without any of it being netzwirksam. [`Guard::verdict`] computes that
//! budget with [`hems_grid::steuve_budget`] and then shares it out.
//!
//! # A missing measurement is not a zero
//!
//! A controllable device the drivers cannot hear from is assumed to be drawing
//! its **nameplate** power; a silent inverter is assumed to be producing
//! **nothing**. The asymmetry is the point: guessing high for load and low for
//! generation is the only pair of guesses that cannot overstate a budget.
//! [`GuardVerdict::assumed_nominal`] names every asset it had to guess for, so a
//! compliance figure computed from an assumption never looks like one computed
//! from a meter.

use std::collections::BTreeMap;

use hems_core::prelude::*;
use hems_grid::para14a::{
    ControlMode, SteuVe, Verursachungsregel, minimum_power, netzwirksamer_leistungsbezug_by,
    steuve_budget,
};
use time::{Duration, OffsetDateTime};

use crate::allocate::{Claim, allocate_indivisible};

/// The VDE-AR-N 4100 Abschnitt 5.5.2 ceiling on the Unsymmetrieleistung of a
/// customer installation.
///
/// The watt view of `metering::power_quality::UNSYMMETRIE_LIMIT_KVA`, which
/// carries the derivation: EN 50160 caps the voltage unbalance at 2 %, and at
/// 20 A per Außenleiter that is 4,6 kVA — the same limit expressed twice. It is
/// a *default* rather than a constant, which is why
/// [`GuardConfig::unbalance_limit`] exists.
pub const UNBALANCE_LIMIT: ApparentPower = ApparentPower::new_const(4_600.0);

/// What the grid is currently asking of this site.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GridLimits {
    /// The § 14a ceiling on the netzwirksamer Leistungsbezug of all controllable
    /// devices behind the energy management system, from an active LPC session
    /// or a relay input.
    #[cfg_attr(feature = "serde", serde(default))]
    pub steuve_ceiling: Option<Power>,
    /// When that ceiling started to apply, for the reason chain and the record.
    #[cfg_attr(
        feature = "serde",
        serde(default, with = "time::serde::rfc3339::option")
    )]
    pub steuve_since: Option<OffsetDateTime>,
    /// The ceiling on feed-in at the connection point, as a non-negative
    /// magnitude — § 9 EEG, an LPP session, or the EEBUS feed-in factor.
    #[cfg_attr(feature = "serde", serde(default))]
    pub feed_in_ceiling: Option<Power>,
    /// Which rule produced [`GridLimits::feed_in_ceiling`].
    #[cfg_attr(feature = "serde", serde(default))]
    pub feed_in_rule: Option<GuardRule>,
    /// Whether the § 14a ceiling arrived through the EEBUS failsafe rather than
    /// as a live command — the same number, a different story to tell.
    #[cfg_attr(feature = "serde", serde(default))]
    pub in_failsafe: bool,
}

/// What the site is doing right now, as far as the drivers can tell.
#[derive(Debug, Clone, Default)]
pub struct SiteState {
    /// Active power at the connection point, load convention.
    pub grid: Option<Measurement>,
    /// Per-asset active power.
    pub assets: BTreeMap<AssetId, Measurement>,
    /// Which conductors each switchable asset is actually using.
    ///
    /// Reported by the driver, not inferred: the guard has to bound a device by
    /// what it *is* doing. An entry is only needed for an asset that can switch;
    /// everything else is decided by its wiring.
    pub phases: BTreeMap<AssetId, PhaseMode>,
}

impl SiteState {
    /// The mode `asset` is in, falling back to what its wiring implies.
    #[must_use]
    pub fn phase_mode(&self, asset: &Asset) -> PhaseMode {
        self.phases.get(asset.id()).copied().map_or_else(
            || asset.meta().phases.default_mode(),
            |m| asset.meta().phases.clamp_mode(m),
        )
    }
}

impl SiteState {
    /// The measurement for one asset.
    #[must_use]
    pub fn asset(&self, id: &AssetId) -> Option<&Measurement> {
        self.assets.get(id)
    }

    /// Fresh active power for one asset.
    #[must_use]
    pub fn power_of(&self, id: &AssetId, now: OffsetDateTime, max_age: Duration) -> Option<Power> {
        self.assets.get(id)?.fresh_power(now, max_age)
    }

    /// Fresh state of charge for one asset.
    ///
    /// The guard needs it for the two bounds a planner alone cannot keep: a full
    /// battery must not be told to charge, and one at its backup reserve must
    /// not be told to discharge. Both happen between re-plans, which is why they
    /// belong here and not only in the optimiser.
    #[must_use]
    pub fn soc_of(&self, id: &AssetId, now: OffsetDateTime, max_age: Duration) -> Option<Soc> {
        let m = self.assets.get(id)?;
        (m.age(now) <= max_age).then_some(m.soc).flatten()
    }
}

/// How the guard is configured for one site.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GuardConfig {
    /// How the network operator addresses this site, `[A1 4.4]`.
    pub control_mode: ControlMode,
    /// Which share of the grid draw the controllable devices are taken to have
    /// caused while a roof is producing, `[A1 2.3]`.
    ///
    /// The Festlegung defines the quantity and does not say how to split it, so
    /// this is a choice rather than a constant — and the default is the
    /// conservative one, which can never understate the controllable share. A
    /// household whose network operator has agreed the pro-rata reading in its
    /// Technische Mindestanforderungen may say so here; nobody else should.
    #[cfg_attr(feature = "serde", serde(default))]
    pub verursachungsregel: Verursachungsregel,
    /// The largest unbalance the installation may present.
    pub unbalance_limit: ApparentPower,
    /// How old a measurement may be before it counts as absent.
    #[cfg_attr(
        feature = "serde",
        serde(default = "default_max_age", with = "duration_secs")
    )]
    pub max_measurement_age: Duration,
    /// How long a commanded value will be held before the guard runs again.
    ///
    /// The guard needs it to turn a bound on a *state* into a bound on a
    /// *rate*. "Stop discharging once the reserve is reached" is a promise the
    /// battery has already broken by the time it can be checked: at 5 kW from a
    /// 10 kWh pack, one minute is 0,8 % of capacity, and the household's reserve
    /// ends up 0,8 % short. Bounding the power so a whole period cannot cross the
    /// floor makes it a promise instead of an aspiration.
    #[cfg_attr(
        feature = "serde",
        serde(default = "default_tick_period", with = "duration_secs")
    )]
    pub tick_period: Duration,
}

fn default_max_age() -> Duration {
    Duration::seconds(30)
}

fn default_tick_period() -> Duration {
    Duration::seconds(1)
}

#[cfg(feature = "serde")]
pub(crate) mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(d.whole_seconds())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        i64::deserialize(d).map(Duration::seconds)
    }
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            control_mode: ControlMode::Ems,
            verursachungsregel: Verursachungsregel::default(),
            unbalance_limit: UNBALANCE_LIMIT,
            max_measurement_age: default_max_age(),
            tick_period: default_tick_period(),
        }
    }
}

/// Which rule set each end of an envelope.
///
/// Both ends are tracked, because they are set by different rules and a single
/// "binding rule" would be wrong half the time: a § 9 EEG cap tightens the
/// *floor* (how much may be fed in) while a § 14a limit tightens the *ceiling*
/// (how much may be drawn). Which one a user should be told about depends on
/// which way the asset is actually being pushed — [`Binding::at`] decides that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Binding {
    /// The rule that set the upper bound.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ceiling: Option<GuardRule>,
    /// The rule that set the lower bound.
    #[cfg_attr(feature = "serde", serde(default))]
    pub floor: Option<GuardRule>,
}

impl Binding {
    /// The rule to name for a setpoint that landed on `value` inside `envelope`.
    ///
    /// The end the value is sitting *at* is the one that explains it. Falling
    /// back to the other end tells a household that a feed-in limit stopped
    /// their inverter when in fact it was simply not producing.
    #[must_use]
    pub fn at(&self, envelope: Envelope, value: Power) -> Option<GuardRule> {
        const EPS: Power = Power::new_const(1e-6);
        match (
            value >= envelope.ceiling - EPS,
            value <= envelope.floor + EPS,
        ) {
            // Pinned to a single value: either end is a true explanation.
            (true, true) => self.ceiling.or(self.floor),
            (true, false) => self.ceiling,
            (false, true) => self.floor,
            (false, false) => None,
        }
    }

    /// Whether any rule is in play at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ceiling.is_none() && self.floor.is_none()
    }
}

/// What the guard decided, and why.
#[derive(Debug, Clone, Default)]
pub struct GuardVerdict {
    /// The interval each asset must stay inside.
    pub envelopes: BTreeMap<AssetId, Envelope>,
    /// Which rule set each end of each envelope.
    pub binding: BTreeMap<AssetId, Binding>,
    /// The controllable devices the § 14a rules apply to.
    pub steuve: Vec<SteuVe>,
    /// The minimum power the customer is owed, `[A1 4.5]`.
    pub minimum_power: Power,
    /// How much the controllable devices may draw in total, surplus included.
    pub steuve_budget: Option<Power>,
    /// The netzwirksamer Leistungsbezug measured right now, `[A1 2.3]`.
    ///
    /// Every local generator counts against it, a discharging battery included:
    /// `[A1 2.3]` measures what the controllable devices draw *from the grid*,
    /// and energy that came out of a store never crossed the connection point.
    /// This is the figure the evidence record and the compliance check use, and
    /// it is deliberately not the same question as
    /// [`GuardVerdict::lent_generation`] — that one asks how much of it the
    /// guard is willing to *plan* on a second from now.
    pub netzwirksam: Power,
    /// Controllable generation the guard counted towards the § 14a budget.
    ///
    /// Measured, about to be commanded in the same direction, and sustainable
    /// for a whole control period — see the note on `Guard::lend_generation`. The assets
    /// behind it have their ceiling pinned at zero for the tick in exchange.
    pub lent_generation: Power,
    /// The Schieflast the installation is presenting, VDE-AR-N 4100.
    pub unbalance: ApparentPower,
    /// Controllable devices whose consumption had to be assumed from the
    /// nameplate because no fresh measurement reached the guard.
    ///
    /// Everything downstream — the evidence record, the user interface, the
    /// fleet's health view — should say so, because a compliance figure computed
    /// from an assumption is a different kind of number from one computed from a
    /// meter.
    pub assumed_nominal: Vec<AssetId>,
    /// Each asset's ceiling before the mode-dependent rules were applied.
    ///
    /// What a switchable charge point could use *if it were in the right mode* —
    /// which is exactly the question its phase policy has to answer, and exactly
    /// the one [`GuardVerdict::envelopes`] cannot, because the Schieflast bound
    /// on a single-phase session is a consequence of the mode rather than a
    /// constraint on choosing it.
    pub phase_headroom: BTreeMap<AssetId, Power>,
    /// The binding rules at the same moment, so [`Guard::apply_modes`] can
    /// re-derive the mode-dependent bounds rather than narrow them twice.
    pub phase_binding: BTreeMap<AssetId, Binding>,
    /// `true` when the commanded ceiling is below the minimum of `[A1 4.5]`.
    ///
    /// hems still applies it — a box does not overrule a network operator, and
    /// `[A1 4.6 S. 2]` even requires going to the next possible lower value —
    /// but the fact is recorded, because the entitlement is the customer's.
    pub below_minimum: bool,
}

impl GuardVerdict {
    /// The envelope for one asset, or an unbounded one when the guard has
    /// nothing to say about it.
    #[must_use]
    pub fn envelope(&self, asset: &AssetId) -> Envelope {
        self.envelopes
            .get(asset)
            .copied()
            .unwrap_or(Envelope::UNBOUNDED)
    }

    /// How much this asset could use in the best mode available to it.
    #[must_use]
    pub fn phase_headroom(&self, asset: &AssetId) -> Power {
        self.phase_headroom
            .get(asset)
            .copied()
            .unwrap_or_else(|| self.envelope(asset).ceiling)
    }

    /// Which rules bound one asset.
    #[must_use]
    pub fn binding(&self, asset: &AssetId) -> Binding {
        self.binding.get(asset).copied().unwrap_or_default()
    }

    /// The rule to name for a setpoint of `value` on `asset`, if it is being
    /// held by one.
    #[must_use]
    pub fn binding_at(&self, asset: &AssetId, value: Power) -> Option<GuardRule> {
        self.binding(asset).at(self.envelope(asset), value)
    }

    /// Whether this asset's consumption was assumed rather than measured.
    #[must_use]
    pub fn is_assumed(&self, asset: &AssetId) -> bool {
        self.assumed_nominal.contains(asset)
    }
}
/// The guard plane.
#[derive(Debug, Clone)]
pub struct Guard {
    config: GuardConfig,
}

impl Guard {
    /// A guard with this configuration.
    #[must_use]
    pub fn new(config: GuardConfig) -> Self {
        Self { config }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &GuardConfig {
        &self.config
    }

    /// Split what the site is doing into the quantities the rules need.
    ///
    /// Two conservative readings are built in, and both matter more than they
    /// look:
    ///
    /// * **Only generation the arbiter is not about to re-command counts as
    ///   surplus.** A discharging battery is a controllable device: this same
    ///   tick may tell it to charge instead, and a budget built on its discharge
    ///   would evaporate as the command took effect.
    /// * **A controllable device whose driver has gone quiet is assumed to be
    ///   drawing its nameplate power.** Assuming zero is the tempting default and
    ///   it is exactly wrong: the guard would hand the rest of the § 14a budget
    ///   to the other devices while the silent one keeps charging, and the site
    ///   would exceed a network operator's limit with nothing in the log.
    fn flows(
        &self,
        site: &Site,
        state: &SiteState,
        steuve_ids: &[&AssetId],
        now: OffsetDateTime,
    ) -> Flows {
        let mut flows = Flows::default();
        for asset in &site.assets {
            if matches!(asset, Asset::Meter(_)) {
                continue;
            }
            let is_steuve = steuve_ids.contains(&asset.id());
            let Some(p) = state.power_of(asset.id(), now, self.config.max_measurement_age) else {
                // Nothing is heard from this device. Assuming it draws nothing is
                // the tempting default and it is exactly wrong: the guard would
                // then hand its share of the budget to somebody else while it
                // kept drawing, and the site would pass a network operator's
                // limit with nothing in the log to say why.
                if is_steuve {
                    let assumed = asset.steuve_power().max(Power::ZERO);
                    flows.steuve_consumption += assumed;
                    flows.assumed_consumption += assumed;
                    if !is_controllable(asset) {
                        flows.uncommandable_steuve += assumed;
                    }
                    flows.assumed.push(asset.id().clone());
                } else if !is_controllable(asset) && !produces(asset) {
                    let assumed = asset.meta().connection_power.max(Power::ZERO);
                    flows.other_consumption += assumed;
                    flows.assumed_consumption += assumed;
                    flows.assumed.push(asset.id().clone());
                }
                continue;
            };
            if p < Power::ZERO {
                // A controllable device that is *generating* — a discharging
                // battery, a car feeding the house — is deliberately kept out of
                // the surplus: this same tick may tell it to charge instead, and
                // a budget built on its discharge would evaporate as the command
                // took effect. It still has to be counted somewhere, though, or
                // the grid meter below stops closing the balance.
                if is_steuve {
                    flows.steuve_generation += p.outflow();
                } else {
                    flows.generation += p.outflow();
                    if !is_controllable(asset) {
                        flows.uncontrollable_generation += p.outflow();
                    }
                }
            } else if is_steuve {
                flows.steuve_consumption += p;
                if !is_controllable(asset) {
                    flows.uncommandable_steuve += p;
                }
            } else {
                flows.other_consumption += p;
            }
        }

        // The grid meter closes the balance. Where it is fresh it is the better
        // witness for "everything else the house is doing", because it also sees
        // the loads nobody instrumented — and under the load convention
        // `grid == Σ assets` makes the arithmetic exact rather than approximate.
        if let Some(grid) = state
            .grid
            .as_ref()
            .and_then(|m| m.fresh_power(now, self.config.max_measurement_age))
        {
            // Under the load convention `grid = steuve − generation + other`,
            // counting *every* generating asset, so the inversion has to as
            // well. Leaving the controllable generation out understates the rest
            // of the house by exactly the battery's discharge, which overstates
            // the surplus and therefore the § 14a budget — the one direction a
            // guard may never be wrong in.
            let derived =
                grid - flows.steuve_consumption + flows.generation + flows.steuve_generation;
            flows.other_consumption = flows.other_consumption.max(derived).max(Power::ZERO);
        }
        flows
    }

    /// Bound every asset of `site`, given the grid's demands and what the assets
    /// would like to do.
    ///
    /// `wants` is the power each controllable asset would use if nothing limited
    /// it, and the optional weight is its priority — the planner passes the
    /// marginal value of energy, so a reduction takes power from where it is
    /// worth least.
    pub fn verdict(
        &self,
        site: &Site,
        limits: &GridLimits,
        state: &SiteState,
        wants: &BTreeMap<AssetId, (Power, f64)>,
        now: OffsetDateTime,
    ) -> GuardVerdict {
        let steuve = hems_grid::classify_at(&site.assets, now);
        let minimum = minimum_power(&steuve, self.config.control_mode);

        let mut verdict = GuardVerdict {
            steuve: steuve.clone(),
            minimum_power: minimum,
            ..GuardVerdict::default()
        };

        // ── What the site is doing ─────────────────────────────────────────
        let steuve_ids: Vec<&AssetId> = steuve.iter().flat_map(|s| s.assets.iter()).collect();
        let flows = self.flows(site, state, &steuve_ids, now);
        // Every local generator, the controllable ones included: this is what
        // the connection point actually saw, and it is what the Nachweis of
        // `[A1 7]` has to be able to reproduce. Whether the guard is willing to
        // *rely* on a discharge for the next second is a different question, and
        // it is answered by `lend_generation` below.
        verdict.netzwirksam = netzwirksamer_leistungsbezug_by(
            flows.steuve_consumption,
            flows.other_consumption,
            flows.generation + flows.steuve_generation,
            self.config.verursachungsregel,
        );
        verdict.assumed_nominal.clone_from(&flows.assumed);

        // ── Each asset's own bounds, before anything is shared ─────────────
        for asset in &site.assets {
            self.narrow_asset(asset, site, state, now, &mut verdict);
        }

        let controllable: Vec<AssetId> = site
            .assets
            .iter()
            .filter(|a| is_controllable(a))
            .map(|a| a.id().clone())
            .collect();
        Self::bound_feed_in(site, limits, &flows, &controllable, wants, &mut verdict);

        // ── § 14a: the budget for the controllable devices, then its sharing ─
        //
        // The limit is on the netzwirksamer Leistungsbezug `[A1 2.3]`, so the
        // surplus that already covers the rest of the house raises it. The
        // household may then spend the total as it likes `[A1 4.5.2 S. 6]`.
        if let Some(ceiling) = limits.steuve_ceiling {
            verdict.below_minimum = ceiling < minimum;
            let lent = self.lend_generation(site, state, wants, now, &mut verdict);
            verdict.lent_generation = lent;
            // A controllable device the guard cannot *command* — a relay-only
            // heat pump with no control path, an asset whose driver offers only
            // measurement — still spends the § 14a budget. Its consumption comes
            // off the top before the rest is shared, because the only lever left
            // is to give the others less. It must not then *also* take a share
            // of what is left: subtracting its draw and then letting it bid for
            // the remainder spends the same kilowatts twice, and the devices the
            // guard can actually move are the ones that pay for it.
            let budget = (steuve_budget(ceiling, flows.other_consumption, flows.generation + lent)
                - flows.uncommandable_steuve)
                .max(Power::ZERO);
            verdict.steuve_budget = Some(budget);
            let rule = if limits.in_failsafe {
                GuardRule::Failsafe
            } else {
                GuardRule::Lpc
            };
            let ids: Vec<AssetId> = steuve_ids
                .iter()
                .filter(|id| site.asset(id).is_some_and(is_controllable))
                .map(|id| (*id).clone())
                .collect();
            Self::share(&ids, budget, site, wants, rule, &mut verdict);
        }

        // ── The physical limits everything shares: the connection, the fuses ─
        //
        // Unlike the § 14a ceiling these bound *everything* behind them, so the
        // uncontrollable load spends the same capacity. Each is shared out on
        // its own and the results intersect; because a per-asset minimum of two
        // allocations can only be smaller than either, respecting both sums is
        // automatic.
        let uncontrolled_net = flows.other_consumption - flows.generation;
        Self::share(
            &controllable,
            physical_headroom(site.grid.import_ceiling(), uncontrolled_net),
            site,
            wants,
            GuardRule::ContractLimit,
            &mut verdict,
        );

        for circuit in site.circuits.all() {
            let Some(limit) = circuit.symmetric_power_limit(NOMINAL_VOLTAGE) else {
                continue;
            };
            let below = site.circuits.assets_below(&circuit.id, &site.assets);
            // A circuit that carries the whole site adds nothing the connection
            // has not already said.
            if below.len() == site.assets.len() && limit >= site.grid.import_ceiling() {
                continue;
            }
            let (controlled, uncontrolled) = self.split_below(site, state, &below, now);
            if controlled.is_empty() {
                continue;
            }
            Self::share(
                &controlled,
                physical_headroom(limit, uncontrolled),
                site,
                wants,
                GuardRule::CircuitLimit,
                &mut verdict,
            );
        }

        // ── Everything above is mode-independent ───────────────────────────
        //
        // Snapshot the ceilings here, because the rule that follows is not. A
        // single-phase charge point is bounded by the Schieflast it presents, so
        // using the ceiling *after* that rule to decide whether it should stop
        // being single-phase is circular — and the circle closes: the 4,6 kVA
        // limit sits just below the 4,64 kW a three-phase session needs to start,
        // so a charge point that switched down could never find a reason to
        // switch back up. It spent a June day charging at a third of the power
        // the roof was giving it.
        verdict.phase_headroom = verdict
            .envelopes
            .iter()
            .map(|(id, e)| (id.clone(), e.ceiling))
            .collect();
        verdict.phase_binding.clone_from(&verdict.binding);

        self.apply_modes(site, state, now, &BTreeMap::new(), &mut verdict);
        verdict
    }

    /// Apply the bounds that depend on the conductor count each charge point is
    /// in, for the modes in `modes` — falling back to what the drivers report.
    ///
    /// # Why this is a second pass and not part of the first
    ///
    /// The arbiter decides a charge point's conductor count *after* the guard
    /// has spoken, because the question it answers is "how much could this
    /// device use if it were in the right mode", and only the guard can say. But
    /// the bound that then applies is the **new** mode's, and the first pass
    /// could only know the old one. So the tick that switched a wallbox up
    /// carried a ceiling of 3,68 kW — the single-phase maximum — into a
    /// three-phase decision, where anything below 4,14 kW is not a small
    /// current but no current at all. The arbiter duly commanded **zero**, the
    /// measurement went to zero, the surplus jumped, and the policy switched it
    /// back. Fifty-one contactor operations in a June day, and seven
    /// kilowatt-hours that should have gone into a car.
    ///
    /// Re-deriving is safe rather than merely convenient:
    /// [`GuardVerdict::phase_headroom`] is by construction the ceiling before
    /// any mode rule was applied, so this can be run again with a different
    /// answer without ever widening past a limit that is not about conductors.
    pub fn apply_modes(
        &self,
        site: &Site,
        state: &SiteState,
        now: OffsetDateTime,
        modes: &BTreeMap<AssetId, PhaseMode>,
        verdict: &mut GuardVerdict,
    ) {
        let mode_of = |asset: &Asset| {
            modes.get(asset.id()).copied().map_or_else(
                || state.phase_mode(asset),
                |m| asset.meta().phases.clamp_mode(m),
            )
        };

        // Back to the mode-independent bound, then narrow again for the mode
        // that is about to be commanded.
        for asset in &site.assets {
            let id = asset.id();
            if let Some(ceiling) = verdict.phase_headroom.get(id).copied() {
                let mut envelope = verdict.envelope(id);
                envelope.ceiling = ceiling;
                verdict.envelopes.insert(id.clone(), envelope);
            }
            match verdict.phase_binding.get(id) {
                Some(b) => {
                    verdict.binding.insert(id.clone(), *b);
                }
                None => {
                    verdict.binding.remove(id);
                }
            }
        }

        verdict.unbalance = self.unbalance_now(site, state, now, &mode_of);
        Self::narrow_by_mode(site, &mode_of, verdict);
        self.narrow_unbalance(site, state, now, &mode_of, verdict);

        for envelope in verdict.envelopes.values_mut() {
            *envelope = envelope.resolve();
        }
    }

    /// Bound what may leave the connection point: the fuse, and the rules on
    /// feeding in.
    ///
    /// Both are measured at the **connection point**, not at any one inverter:
    /// the fuse carries current in either direction, and § 9 EEG, an LPP session
    /// and the EEBUS `MGCP` factor all cap the Einspeisung. So the house's own
    /// consumption is headroom — a household using 3 kW may produce 3 kW above
    /// the cap and still feed in exactly the cap.
    ///
    /// *All* measured consumption counts, the controllable devices included,
    /// which is deliberately a different reading from the § 14a budget. § 14a is
    /// a control instruction with a five-minute response presumption
    /// `[A1 4.2]`, so the guard may never be over it even for a tick; the 60 %
    /// cap is a **settlement** limit read off quarter-hour registers, so the
    /// quantity to control is the average over the quarter hour.
    ///
    /// Runs before the § 14a block because it narrows *floors* while that one
    /// narrows *ceilings*, so [`Guard::lend_generation`] sees a discharge bound
    /// nothing later takes away.
    fn bound_feed_in(
        site: &Site,
        limits: &GridLimits,
        flows: &Flows,
        controllable: &[AssetId],
        wants: &BTreeMap<AssetId, (Power, f64)>,
        verdict: &mut GuardVerdict,
    ) {
        Self::share_export(
            controllable,
            physical_headroom(
                site.grid.export_ceiling(),
                flows.uncontrollable_generation - flows.metered_consumption(),
            ),
            wants,
            GuardRule::ContractLimit,
            verdict,
        );
        if let Some(ceiling) = limits.feed_in_ceiling {
            Self::share_export(
                controllable,
                physical_headroom(
                    ceiling,
                    flows.uncontrollable_generation - flows.metered_consumption(),
                ),
                wants,
                limits.feed_in_rule.unwrap_or(GuardRule::Lpp),
                verdict,
            );
        }
    }

    /// Share a limit on **export** between the assets that can produce.
    ///
    /// The mirror image of [`Guard::share`], and it needs its own function
    /// rather than a sign flip inside that one, because the two are not
    /// symmetric where it matters: there is no indivisible floor on production.
    /// An inverter curtails continuously, a battery modulates its discharge, and
    /// nothing on a household site has a "6 A or nothing" rule for feeding in —
    /// so the water filling is over magnitudes and no device ever has to be
    /// switched off to make room for another.
    ///
    /// The leftover is spread the same way and for the same reason: the floor
    /// has to be a **true** bound, or the arbiter's ramp carries a value back
    /// through it one tick after the limit arrives.
    fn share_export(
        ids: &[AssetId],
        budget: Power,
        wants: &BTreeMap<AssetId, (Power, f64)>,
        rule: GuardRule,
        verdict: &mut GuardVerdict,
    ) {
        if ids.is_empty() {
            return;
        }
        let claims: Vec<Claim> = ids
            .iter()
            .map(|id| {
                let (want, weight) = wants.get(id).copied().unwrap_or((Power::ZERO, 1.0));
                let room = verdict.envelope(id).floor.outflow();
                Claim::new(id.clone(), want.outflow().min(room)).with_weight(weight)
            })
            .collect();
        let grants = crate::allocate::allocate(budget, &claims);

        let granted: Power = grants.iter().map(|g| g.power).sum();
        let leftover = (budget - granted).max(Power::ZERO);
        let headroom: Vec<Power> = grants
            .iter()
            .map(|g| (verdict.envelope(&g.asset).floor.outflow() - g.power).max(Power::ZERO))
            .collect();
        let total_headroom: Power = headroom.iter().copied().sum();
        if total_headroom <= leftover {
            return;
        }
        let share_of_leftover = leftover / total_headroom;

        for (grant, room) in grants.into_iter().zip(headroom) {
            let before = verdict.envelope(&grant.asset);
            let floor = -(grant.power + room * share_of_leftover);
            let after = before.intersect(Envelope::at_least(floor));
            verdict.envelopes.insert(grant.asset.clone(), after);
            if after.floor > before.floor {
                verdict.binding.entry(grant.asset).or_default().floor = Some(rule);
            }
        }
    }

    /// How much of the site's *controllable* generation may be counted against
    /// the § 14a ceiling, and the price of counting it.
    ///
    /// `[A1 2.3]` measures the netzwirksamer Leistungsbezug at the connection
    /// point, so a battery discharging into the wallbox reduces it exactly like
    /// a roof does. Ignoring that curtails a household harder than the
    /// Festlegung asks for: with a full battery and a 4,2 kW ceiling the car
    /// waits while the store sits there.
    ///
    /// A discharge is not a roof, though: **the same tick may tell the battery
    /// to charge instead**, and a budget built on a discharge that then reverses
    /// evaporates at the worst possible moment. So the guard lends only what
    /// survives three tests, and takes something in return:
    ///
    /// * it is **measured** — a driver that has gone quiet lends nothing;
    /// * this tick is **about to ask for it too**, so the lender cannot be
    ///   re-commanded into a load by the same decision that spent its loan;
    /// * the asset can **sustain it for a whole control period** — which is the
    ///   bound [`Guard::narrow_asset`] has already derived from the usable
    ///   energy above the operating floor and the backup reserve, so a store
    ///   three minutes from empty lends three minutes' worth and no more;
    ///
    /// and in exchange the lender's ceiling is pinned at zero for the tick. It
    /// may go on discharging, it may stop — what it may **not** do is turn into
    /// a load while the others are spending the headroom it created.
    fn lend_generation(
        &self,
        site: &Site,
        state: &SiteState,
        wants: &BTreeMap<AssetId, (Power, f64)>,
        now: OffsetDateTime,
        verdict: &mut GuardVerdict,
    ) -> Power {
        let mut lent = Power::ZERO;
        for asset in &site.assets {
            if !is_controllable(asset) || produces(asset) {
                continue;
            }
            let id = asset.id();
            // Measured, wanted, and sustainable — the smallest of the three.
            let Some(measured) = state
                .power_of(id, now, self.config.max_measurement_age)
                .filter(|p| *p < Power::ZERO)
                .map(Power::outflow)
            else {
                continue;
            };
            let Some(wanted) = wants
                .get(id)
                .map(|(w, _)| *w)
                .filter(|w| *w < Power::ZERO)
                .map(Power::outflow)
            else {
                continue;
            };
            let sustainable = verdict.envelope(id).floor.outflow();
            let share = measured.min(wanted).min(sustainable);
            if share <= Power::ZERO {
                continue;
            }
            lent += share;
            let before = verdict.envelope(id);
            let after = before.intersect(Envelope::at_most(Power::ZERO));
            verdict.envelopes.insert(id.clone(), after);
            if after.ceiling < before.ceiling {
                verdict.binding.entry(id.clone()).or_default().ceiling = Some(GuardRule::Lpc);
            }
        }
        lent
    }

    /// Split the assets below a node into the controllable ones and the net
    /// power the rest of them are taking through it.
    fn split_below(
        &self,
        site: &Site,
        state: &SiteState,
        below: &[&AssetId],
        now: OffsetDateTime,
    ) -> (Vec<AssetId>, Power) {
        let mut controlled = Vec::new();
        let mut uncontrolled = Power::ZERO;
        for id in below {
            let Some(asset) = site.asset(id) else {
                continue;
            };
            if matches!(asset, Asset::Meter(_)) {
                continue;
            }
            if is_controllable(asset) {
                controlled.push((*id).clone());
            } else {
                // Same conservatism as `flows`: a load nobody can hear from is
                // assumed to be taking its nameplate power out of the fuse.
                uncontrolled += state
                    .power_of(id, now, self.config.max_measurement_age)
                    .unwrap_or_else(|| {
                        if produces(asset) {
                            Power::ZERO
                        } else {
                            asset.meta().connection_power.max(Power::ZERO)
                        }
                    });
            }
        }
        (controlled, uncontrolled)
    }

    /// Share one budget between `ids` and intersect the result into the verdict.
    fn share(
        ids: &[AssetId],
        budget: Power,
        site: &Site,
        wants: &BTreeMap<AssetId, (Power, f64)>,
        rule: GuardRule,
        verdict: &mut GuardVerdict,
    ) {
        if ids.is_empty() {
            return;
        }
        let claims: Vec<Claim> = ids
            .iter()
            .map(|id| {
                let (want, weight) = wants.get(id).copied().unwrap_or((Power::ZERO, 1.0));
                let ceiling = verdict.envelope(id).ceiling;
                let floor = site.asset(id).map_or(Power::ZERO, minimum_useful_power);
                Claim::new(
                    id.clone(),
                    want.max(Power::ZERO).min(ceiling.max(Power::ZERO)),
                )
                .with_floor(floor)
                .with_weight(weight)
            })
            .collect();

        let grants = allocate_indivisible(budget, &claims);

        // What the demands did not take is spread over the room each device has
        // left, in proportion to that room. Two reasons, and both are load
        // bearing:
        //
        // * the envelope has to be a **true** bound, because the arbiter's ramp
        //   and deadband move a value inside it afterwards. Leaving an idle
        //   device unbounded lets a ramp carry it over the network operator's
        //   limit one tick after the limit arrives — which is exactly the case
        //   `ramping_never_carries_a_value_back_over_a_grid_limit` pins.
        // * a device that asked for nothing this second must not be reported as
        //   forbidden to run. It is not: it has a share, it simply is not using
        //   it.
        //
        // When the room left over fits inside the leftover budget the sharing is
        // not binding at all and nothing is narrowed.
        let granted: Power = grants.iter().map(|g| g.power).sum();
        let leftover = (budget - granted).max(Power::ZERO);
        let headroom: Vec<Power> = grants
            .iter()
            .map(|g| (verdict.envelope(&g.asset).ceiling - g.power).max(Power::ZERO))
            .collect();
        let total_headroom: Power = headroom.iter().copied().sum();
        if total_headroom <= leftover {
            return;
        }
        let share_of_leftover = leftover / total_headroom;

        for (grant, room) in grants.into_iter().zip(headroom) {
            let before = verdict.envelope(&grant.asset);
            let ceiling = grant.power + room * share_of_leftover;
            let after = before.intersect(Envelope::at_most(ceiling));
            verdict.envelopes.insert(grant.asset.clone(), after);
            if after.ceiling < before.ceiling {
                verdict.binding.entry(grant.asset).or_default().ceiling = Some(rule);
            }
        }
    }

    /// Intersect one asset's envelope with everything that binds it alone: its
    /// own ratings, its state of charge, every fuse on its path, and the rules
    /// on feeding in.
    fn narrow_asset(
        &self,
        asset: &Asset,
        site: &Site,
        state: &SiteState,
        now: OffsetDateTime,
        verdict: &mut GuardVerdict,
    ) {
        let id = asset.id().clone();
        let mut envelope = verdict.envelope(&id);
        let mut binding = verdict.binding(&id);

        let narrow =
            |limit: Envelope, rule: GuardRule, envelope: &mut Envelope, binding: &mut Binding| {
                let before = *envelope;
                *envelope = envelope.intersect(limit);
                if envelope.ceiling < before.ceiling {
                    binding.ceiling = Some(rule);
                }
                if envelope.floor > before.floor {
                    binding.floor = Some(rule);
                }
            };

        // What the hardware can do at all. Asymmetric, because hardware is: an
        // inverter cannot consume and a one-way charge point cannot export.
        //
        // The *floor* is never named: an inverter sitting at zero watts of
        // consumption is not being held there by anything, and reporting "device
        // limit" for every idle asset buries the one message that matters — that
        // a network operator reduced this house. A positive ceiling is named,
        // because "you asked for more than this device has" is worth saying.
        let ratings = asset.ratings();
        envelope = envelope.intersect(ratings);
        if ratings.ceiling > Power::ZERO {
            binding.ceiling = Some(GuardRule::DeviceLimit);
        }

        // A store may not charge past full, discharge past empty, or eat into
        // the energy the household asked to keep for a power cut.
        //
        // As a bound on **power**, not on state. "Stop once the reserve is
        // reached" is checked after the fact, and by then the tick that reached
        // it has already spent 0,8 % of a 10 kWh pack. What the household was
        // promised is that the reserve is still there, so the guard bounds the
        // power such that a whole control period cannot cross it — which at the
        // floor comes out at exactly zero, and just above it lets the last few
        // watt-hours out at the rate that spends them and no faster.
        if let Asset::Battery(battery) = asset
            && let Some(soc) = state.soc_of(&id, now, self.config.max_measurement_age)
        {
            let hours = self.config.tick_period.as_seconds_f64() / 3600.0;
            let stored = soc.energy_in(battery.capacity);

            let room = (battery.soc_max.energy_in(battery.capacity) - stored).max(Energy::ZERO);
            let charge_ceiling = if hours > 0.0 && battery.efficiency_charge > 0.0 {
                Power::new(room.get() / hours / battery.efficiency_charge)
            } else {
                Power::ZERO
            };
            narrow(
                Envelope::at_most(charge_ceiling),
                GuardRule::DeviceLimit,
                &mut envelope,
                &mut binding,
            );

            let floor = battery.discharge_floor();
            let usable = (stored - floor.energy_in(battery.capacity)).max(Energy::ZERO);
            let discharge_floor = if hours > 0.0 {
                Power::new(-usable.get() * battery.efficiency_discharge / hours)
            } else {
                Power::ZERO
            };
            let rule = if floor == battery.reserve_soc && battery.reserve_soc > battery.soc_min {
                GuardRule::BackupReserve
            } else {
                GuardRule::DeviceLimit
            };
            narrow(
                Envelope::at_least(discharge_floor),
                rule,
                &mut envelope,
                &mut binding,
            );
        }

        // Every fuse between the asset and the connection, as a bound on this
        // asset alone. The *shared* form of the same limit is applied in
        // `verdict`, because a fuse is spent by everything behind it.
        for circuit in site.circuits.path_for_asset(asset) {
            if let Some(limit) = circuit.symmetric_power_limit(NOMINAL_VOLTAGE) {
                narrow(
                    Envelope::new(-limit, limit),
                    GuardRule::CircuitLimit,
                    &mut envelope,
                    &mut binding,
                );
            }
        }

        // The connection itself.
        narrow(
            Envelope::at_most(site.grid.import_ceiling()),
            GuardRule::ContractLimit,
            &mut envelope,
            &mut binding,
        );

        // The feed-in rules are deliberately *not* here either. § 9 EEG, an LPP
        // session and the EEBUS `MGCP` factor all limit what leaves the
        // **connection point**, not what any one inverter produces, so they are
        // shared out in `verdict` like every other limit spent by more than one
        // device. Applied per asset — which is what this did until the fourth
        // audit — a house consuming 3 kW had its roof curtailed as though it
        // consumed nothing, and threw away exactly those 3 kW every sunny hour.
        //
        // The unbalance rule is deliberately *not* here. It depends on the mode
        // the device is in right now, and every mode-dependent bound is applied
        // together at the end of `verdict` — see the snapshot taken there.

        verdict.envelopes.insert(id.clone(), envelope);
        if !binding.is_empty() {
            verdict.binding.insert(id, binding);
        }
    }

    /// The unbalance the site is presenting right now.
    fn unbalance_now(
        &self,
        site: &Site,
        state: &SiteState,
        now: OffsetDateTime,
        mode_of: &impl Fn(&Asset) -> PhaseMode,
    ) -> ApparentPower {
        self.per_phase_now(site, state, now, mode_of).unbalance()
    }

    /// Active power per outer conductor for the devices VDE-AR-N 4100's
    /// symmetry requirement applies to.
    ///
    /// **Not every asset**, which is the reading that looks obvious and is
    /// wrong. The VDE FNN Hinweis is explicit that Abschnitt 5.5.2 covers only
    /// equipment that can feed in or store — generation, storage, charge points
    /// — so a heat pump, a hot-water tank and the household's own single-phase
    /// load are outside it. Summing them made the installation look more
    /// unbalanced than the rule says it is, and the difference was spent on the
    /// one device the manager could still move: a charge point held below what
    /// it was entitled to draw because a kettle was on L1.
    ///
    /// A meter that reports per-phase values is used as it stands; anything else
    /// is spread over the conductors its `PhaseConnection` says it uses. The
    /// second is an assumption, but it is the same assumption an installer makes
    /// with a clamp meter, and it is the only one available from a device that
    /// reports one number.
    fn per_phase_now(
        &self,
        site: &Site,
        state: &SiteState,
        now: OffsetDateTime,
        mode_of: &impl Fn(&Asset) -> PhaseMode,
    ) -> PerPhase<Power> {
        let mut total = PerPhase::ZERO;
        for asset in &site.assets {
            if !asset.symmetry_relevant() {
                continue;
            }
            total = total + self.asset_per_phase(asset, state, now, mode_of);
        }
        total
    }

    fn asset_per_phase(
        &self,
        asset: &Asset,
        state: &SiteState,
        now: OffsetDateTime,
        mode_of: &impl Fn(&Asset) -> PhaseMode,
    ) -> PerPhase<Power> {
        let fresh = state
            .asset(asset.id())
            .filter(|m| m.age(now) <= self.config.max_measurement_age);
        if let Some(per_phase) = fresh.and_then(|m| m.power_per_phase) {
            return per_phase;
        }
        let mode = mode_of(asset);
        // The same asymmetry as `flows`, for the same reason. A silent
        // single-phase load read as zero is a Schieflast the guard cannot see;
        // the only asset it can still move is the one it is about to bound, and
        // it would bound it too generously.
        let power = fresh.and_then(|m| m.power).unwrap_or_else(|| {
            if produces(asset) {
                Power::ZERO
            } else {
                asset.meta().connection_power.max(Power::ZERO)
            }
        });
        asset.meta().phases.distribute(power, mode)
    }

    /// Bound a charge point by what it can draw in the mode it is actually in.
    ///
    /// A switchable wallbox rated 11 kW draws at most 3,7 kW on one conductor.
    /// Bounding it by the three-phase rating in single-phase mode produces a
    /// command the hardware silently clips — and a clipped command is
    /// indistinguishable in the log from a driver fault, while the energy
    /// accounting quietly believes the larger number.
    fn narrow_by_mode(
        site: &Site,
        mode_of: &impl Fn(&Asset) -> PhaseMode,
        verdict: &mut GuardVerdict,
    ) {
        for asset in &site.assets {
            let Asset::Evse(evse) = asset else {
                continue;
            };
            if !evse.meta.phases.is_switchable() {
                continue;
            }
            let ceiling = evse.max_power(mode_of(asset));
            let id = asset.id().clone();
            let before = verdict.envelope(&id);
            let after = before.intersect(Envelope::at_most(ceiling));
            verdict.envelopes.insert(id.clone(), after);
            if after.ceiling < before.ceiling {
                verdict.binding.entry(id).or_default().ceiling = Some(GuardRule::DeviceLimit);
            }
        }
    }

    /// Keep the installation's Schieflast inside VDE-AR-N 4100's 4,6 kVA.
    ///
    /// Only a single-phase asset can make the unbalance worse, so only those are
    /// narrowed. For one of them on conductor `p`, with `rest` the per-phase
    /// power of everything else, the spread stays inside the limit exactly while
    ///
    /// ```text
    /// rest[p] + x  ≤  min(rest[q], q ≠ p) + limit
    /// ```
    ///
    /// which is the ceiling this hands the asset. Nothing here can widen an
    /// envelope; when the rest of the house is already unbalanced past the limit
    /// the ceiling comes out at zero, which is the correct answer — the single
    /// -phase device is the only part of it the guard can still move.
    fn narrow_unbalance(
        &self,
        site: &Site,
        state: &SiteState,
        now: OffsetDateTime,
        mode_of: &impl Fn(&Asset) -> PhaseMode,
        verdict: &mut GuardVerdict,
    ) {
        let limit = Power::new(self.config.unbalance_limit.get());
        let total = self.per_phase_now(site, state, now, mode_of);
        for asset in &site.assets {
            // The same scope as the measurement: a device the rule does not
            // reach must not be bounded by it either.
            if !is_controllable(asset) || !asset.symmetry_relevant() {
                continue;
            }
            // Only a device actually on one conductor can make the spread
            // worse. A switchable charge point counts here exactly while it is
            // switched down, which is the case the rule was written for.
            if mode_of(asset) != PhaseMode::Single {
                continue;
            }
            let Some(phase) = asset.meta().phases.single_phase_conductor() else {
                continue;
            };
            let rest = total - self.asset_per_phase(asset, state, now, mode_of);
            let others_min = Phase::ALL
                .into_iter()
                .filter(|q| *q != phase)
                .map(|q| rest.get(q))
                .reduce(Power::min)
                .unwrap_or(Power::ZERO);
            // Two bounds, and the tighter wins: the spread against the rest of
            // the house, and the flat 4,6 kVA a single device may present on its
            // own (VDE-AR-N 4100 § 6.2.2).
            let ceiling = (others_min + limit - rest.get(phase))
                .max(Power::ZERO)
                .min(limit);
            let id = asset.id().clone();
            let before = verdict.envelope(&id);
            let after = before.intersect(Envelope::at_most(ceiling));
            verdict.envelopes.insert(id.clone(), after);
            if after.ceiling < before.ceiling {
                verdict.binding.entry(id).or_default().ceiling = Some(GuardRule::Unbalance);
            }
        }
    }
}

/// How much of a physical limit is left for the devices the guard can move.
///
/// A fuse is spent by everything behind it, so the uncontrollable load counts
/// against it — which is exactly what makes it different from a § 14a ceiling,
/// where only the controllable devices do.
#[must_use]
pub fn physical_headroom(limit: Power, uncontrolled_net: Power) -> Power {
    (limit - uncontrolled_net).max(Power::ZERO)
}

/// Whether this asset is a generator, so an absent measurement means "nothing"
/// rather than "the worst case".
///
/// Assuming a silent inverter is producing its nameplate power would *raise*
/// every budget on the site, which is the one direction a guard may never guess
/// in.
#[must_use]
pub fn produces(asset: &Asset) -> bool {
    matches!(asset, Asset::Pv(_))
}

/// Whether the arbiter can move this asset at all.
#[must_use]
pub fn is_controllable(asset: &Asset) -> bool {
    let caps = asset.capabilities();
    caps.contains(Capabilities::LIMIT_CONSUMPTION)
        || caps.contains(Capabilities::SET_POWER)
        || caps.contains(Capabilities::LIMIT_PRODUCTION)
        || caps.contains(Capabilities::SET_MODE)
}

/// The measured power flows a § 14a decision needs.
#[derive(Debug, Clone, Default)]
struct Flows {
    steuve_consumption: Power,
    /// What the controllable devices are *producing*. Kept out of the surplus
    /// on purpose (this tick may re-command them) but needed to close the
    /// balance against the grid meter.
    steuve_generation: Power,
    /// The part of it drawn by controllable devices the guard cannot command,
    /// and which therefore has to come out of the budget before the rest is
    /// shared.
    uncommandable_steuve: Power,
    other_consumption: Power,
    generation: Power,
    /// The part of it the guard cannot curtail, which is what the connection's
    /// export capacity is spent by before anything is shared out.
    uncontrollable_generation: Power,
    /// Consumption that came from a nameplate rather than from a meter.
    ///
    /// The nameplate assumption is safe in exactly **one** direction. Guessing
    /// that a silent device is drawing its rated power keeps the § 14a budget
    /// small, which is the right way to be wrong. Reusing the same guess as
    /// *headroom for feeding in* turns it inside out: a house with three quiet
    /// devices would be told it may produce nineteen kilowatts above the § 9 EEG
    /// cap because it is probably using them. So the export side subtracts this
    /// back out and counts only what a meter actually saw.
    assumed_consumption: Power,
    /// Devices whose consumption had to be assumed rather than measured.
    assumed: Vec<AssetId>,
}

impl Flows {
    /// Consumption a meter actually saw — the only kind that may be counted as
    /// headroom for feeding in.
    ///
    /// Where the grid meter closed the balance, the uninstrumented load it
    /// revealed is measured too and is included; the nameplate guesses are
    /// subtracted back out. Under-counting is the safe direction here, and this
    /// under-counts.
    fn metered_consumption(&self) -> Power {
        (self.other_consumption + self.steuve_consumption - self.assumed_consumption)
            .max(Power::ZERO)
    }
}

/// The least power at which an asset still does something useful.
///
/// A charge point cannot go below the 6 A its standard mandates, so anything
/// less is the same as switching it off — and the allocator needs to know that
/// in order not to spread a shortage into uselessness across three devices.
///
/// For a charge point that can switch conductors this is the minimum in the
/// **lowest** mode it is wired for, not the one it happens to be in. Refusing a
/// device 2 kW because it cannot use them three-phase, when it could use them on
/// one conductor, wastes the 2 kW and the contactor the hardware came with; the
/// arbiter's phase policy is what closes the gap, and it needs a little time to
/// do it.
#[must_use]
pub fn minimum_useful_power(asset: &Asset) -> Power {
    match asset {
        Asset::Evse(e) => e.lowest_useful_power(),
        _ => Power::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{
        AssetMeta, Battery, Capabilities, Chemistry, Evse, FlexibleLoad, LoadKind, PvArray,
    };
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-01-15 17:05:00 UTC);

    fn cid() -> CircuitId {
        CircuitId::new("main").unwrap()
    }

    fn meta(id: &str, kw: f64) -> AssetMeta {
        AssetMeta::new(
            AssetId::new(id).unwrap(),
            cid(),
            PhaseConnection::Three,
            Power::from_kw(kw),
        )
        .with_capabilities(Capabilities::MEASURE | Capabilities::LIMIT_CONSUMPTION)
        .commissioned(time::macros::date!(2025 - 03 - 01))
    }

    fn site() -> Site {
        Site::new(
            SiteId::new(),
            GeoPoint {
                latitude: 52.5,
                longitude: 13.4,
                altitude_m: 34.0,
            },
            GridConnection::new(Current::new(35.0)),
            Circuits::new(vec![Circuit::new(cid(), None, Current::new(35.0))]).unwrap(),
            vec![
                Asset::Pv(PvArray {
                    meta: meta("pv", 9.8),
                    kwp_dc: Power::from_kw(9.8),
                    ac_nominal: Power::from_kw(8.0),
                    tilt_deg: 35.0,
                    azimuth_deg: 180.0,
                    para9: Para9Status::default(),
                }),
                Asset::Evse(Evse {
                    meta: meta("wallbox", 11.0),
                    min_current: Current::new(6.0),
                    max_current: Current::new(16.0),
                    bidirectional: false,
                    public: false,
                    charge_limit: None,
                }),
                Asset::Battery(Battery {
                    meta: meta("battery", 5.0),
                    capacity: Energy::from_kwh(10.0),
                    max_charge: Power::from_kw(5.0),
                    max_discharge: Power::from_kw(5.0),
                    efficiency_charge: 0.95,
                    efficiency_discharge: 0.95,
                    soc_min: Soc::new(0.05).unwrap(),
                    soc_max: Soc::FULL,
                    reserve_soc: Soc::new(0.1).unwrap(),
                    chemistry: Chemistry::Lfp,
                    grid_charging_allowed: true,
                }),
                Asset::Load(FlexibleLoad {
                    meta: meta("haushalt", 3.0),
                    nominal: Power::from_kw(0.5),
                    kind: LoadKind::Fixed,
                }),
            ],
        )
        .unwrap()
    }

    fn state(entries: &[(&str, f64)]) -> SiteState {
        let mut s = SiteState::default();
        for (id, kw) in entries {
            s.assets.insert(
                AssetId::new(id).unwrap(),
                Measurement::power(NOW, Power::from_kw(*kw)),
            );
        }
        s
    }

    fn wants(entries: &[(&str, f64)]) -> BTreeMap<AssetId, (Power, f64)> {
        entries
            .iter()
            .map(|(id, kw)| (AssetId::new(id).unwrap(), (Power::from_kw(*kw), 1.0)))
            .collect()
    }

    #[test]
    fn without_a_grid_limit_only_the_hardware_binds() {
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(&site, &GridLimits::default(), &state(&[]), &wants(&[]), NOW);
        let wallbox = AssetId::new("wallbox").unwrap();
        // 35 A × 3 × 230 V = 24,15 kW at the connection, but the wallbox is
        // rated 11 kW, so its own nameplate is what binds.
        assert_eq!(v.envelope(&wallbox).ceiling, Power::from_kw(11.0));
        assert_eq!(v.binding(&wallbox).ceiling, Some(GuardRule::DeviceLimit));
    }

    /// `wants`, with a per-asset weight — the marginal value of a kilowatt-hour
    /// to that device, as the planner now prices it.
    fn weighted(entries: &[(&str, f64, f64)]) -> BTreeMap<AssetId, (Power, f64)> {
        entries
            .iter()
            .map(|(id, kw, w)| (AssetId::new(id).unwrap(), (Power::from_kw(*kw), *w)))
            .collect()
    }

    #[test]
    fn a_reduction_takes_power_from_where_it_is_worth_least() {
        // The property §7 and D12 claim and the build could not deliver until
        // the planner priced assets separately: with one marginal value per
        // *slot*, every device was handed the same weight and the weighted
        // max-min allocator degenerated to plain max-min.
        //
        // A car three hours from a departure it needs 30 kWh for against a store
        // that would merely rather be fuller: 16 kW of demand under a 12 kW
        // ceiling, and the difference between the two runs is only what the
        // planner says a kilowatt-hour is worth to each of them.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(12.0)),
            steuve_since: Some(NOW),
            ..GridLimits::default()
        };
        let ceiling_of =
            |v: &GuardVerdict, id: &str| v.envelope(&AssetId::new(id).unwrap()).ceiling;

        let uniform = guard.verdict(
            &site,
            &limits,
            &state(&[("haushalt", 0.5)]),
            &weighted(&[("wallbox", 11.0, 1.0), ("battery", 5.0, 1.0)]),
            NOW,
        );
        let priced = guard.verdict(
            &site,
            &limits,
            &state(&[("haushalt", 0.5)]),
            &weighted(&[("wallbox", 11.0, 2.0), ("battery", 5.0, 0.5)]),
            NOW,
        );

        assert!(
            ceiling_of(&priced, "wallbox") > ceiling_of(&uniform, "wallbox") + Power::from_kw(1.0),
            "the dearer device has to gain: {} against {}",
            ceiling_of(&priced, "wallbox"),
            ceiling_of(&uniform, "wallbox")
        );
        assert!(
            ceiling_of(&priced, "battery") < ceiling_of(&uniform, "battery"),
            "and the cheaper one has to give it up"
        );
        // And neither run may exceed the ceiling, which is the point of doing
        // any of this inside the guard.
        for v in [&uniform, &priced] {
            let total = ceiling_of(v, "wallbox") + ceiling_of(v, "battery");
            assert!(total <= Power::from_kw(12.0) + Power::new(1e-6), "{total}");
        }
        // Nobody is starved: a max-min allocator rations, it does not pick a
        // winner, and the compression in `arbiter::target_weight` is what keeps
        // it that way when a shortfall price is fifty times a tariff.
        assert!(ceiling_of(&priced, "battery") > Power::from_kw(0.5));
    }

    #[test]
    fn a_fourteen_a_limit_is_shared_between_the_controllable_devices() {
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(7.56)),
            steuve_since: Some(NOW),
            ..GridLimits::default()
        };
        // Nothing generating, household drawing 0,5 kW.
        let v = guard.verdict(
            &site,
            &limits,
            &state(&[("haushalt", 0.5)]),
            &wants(&[("wallbox", 11.0), ("battery", 5.0)]),
            NOW,
        );
        let total: Power = ["wallbox", "battery"]
            .iter()
            .map(|id| v.envelope(&AssetId::new(id).unwrap()).ceiling)
            .sum();
        assert!(
            total <= Power::from_kw(7.56) + Power::new(1e-6),
            "shared out to {total}"
        );
        assert!(
            !v.below_minimum,
            "7,56 kW is exactly the minimum for two devices"
        );
    }

    #[test]
    fn the_connection_bounds_export_as_well_as_import() {
        // A fuse does not care which way the current goes. 16 A is 11,04 kW in
        // either direction; the roof alone can do 8 kW and the battery another
        // 5, so their sum has to be shared out just as their consumption is.
        let mut site = site();
        site.grid = GridConnection::new(Current::new(16.0));
        site.circuits = Circuits::new(vec![Circuit::new(cid(), None, Current::new(16.0))]).unwrap();
        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(
            &site,
            &GridLimits::default(),
            &state(&[]),
            &wants(&[("pv", -8.0), ("battery", -5.0)]),
            NOW,
        );
        let exported: Power = ["pv", "battery"]
            .iter()
            .map(|id| v.envelope(&AssetId::new(id).unwrap()).floor.outflow())
            .sum();
        assert!(
            exported <= Power::from_kw(11.04) + Power::new(1e-6),
            "the fuse is spent by both of them together, got {exported}"
        );
    }

    #[test]
    fn a_discharging_battery_lifts_the_ceiling_for_the_devices_it_is_feeding() {
        // `[A1 2.3]` measures what the controllable devices draw *from the
        // grid*. A battery discharging 4 kW into the wallbox means 4 kW that
        // never crossed the connection point, so the ceiling for the rest is
        // 4,2 + 4. Ignoring it leaves a household with a full battery watching
        // its car wait through a teatime reduction.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        let mut st = state(&[("haushalt", 0.5), ("battery", -4.0)]);
        if let Some(m) = st.assets.get_mut(&AssetId::new("battery").unwrap()) {
            m.soc = Some(Soc::new(0.8).unwrap());
        }
        let v = guard.verdict(
            &site,
            &limits,
            &st,
            &wants(&[("wallbox", 11.0), ("battery", -4.0)]),
            NOW,
        );
        assert_eq!(v.lent_generation, Power::from_kw(4.0));
        let wallbox = v.envelope(&AssetId::new("wallbox").unwrap()).ceiling;
        // 4,2 kW of ceiling plus 4 kW lent, less the 0,5 kW the household is
        // taking out of the loan before the controllable devices see it.
        assert!(
            (wallbox.kw() - 7.7).abs() < 1e-6,
            "the wallbox should get 7,7 kW, got {wallbox}"
        );
    }

    #[test]
    fn a_lending_battery_may_not_become_a_load_in_the_same_tick() {
        // The price of the loan, and the answer to the objection that killed
        // the idea the first time: the tick that spends a discharge cannot also
        // reverse it.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        let mut st = state(&[("haushalt", 0.5), ("battery", -4.0)]);
        if let Some(m) = st.assets.get_mut(&AssetId::new("battery").unwrap()) {
            m.soc = Some(Soc::new(0.8).unwrap());
        }
        let v = guard.verdict(
            &site,
            &limits,
            &st,
            &wants(&[("wallbox", 11.0), ("battery", -4.0)]),
            NOW,
        );
        assert_eq!(
            v.envelope(&AssetId::new("battery").unwrap()).ceiling,
            Power::ZERO
        );
    }

    #[test]
    fn a_battery_this_tick_is_about_to_charge_lends_nothing() {
        // It is discharging now and the same decision is about to reverse it, so
        // a budget built on the discharge would evaporate as the command took
        // effect.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        let mut st = state(&[("haushalt", 0.5), ("battery", -4.0)]);
        if let Some(m) = st.assets.get_mut(&AssetId::new("battery").unwrap()) {
            m.soc = Some(Soc::new(0.8).unwrap());
        }
        let v = guard.verdict(
            &site,
            &limits,
            &st,
            &wants(&[("wallbox", 11.0), ("battery", 5.0)]),
            NOW,
        );
        assert_eq!(v.lent_generation, Power::ZERO);
    }

    #[test]
    fn a_nearly_empty_battery_lends_only_what_it_can_hold_for_a_whole_period() {
        // The same bound D24 turned the backup reserve into: a store three
        // minutes from its floor lends three minutes of power, not five
        // kilowatts. Without it the loan runs out mid-period and the wallbox
        // spends the rest of it over the ceiling.
        let site = site();
        let guard = Guard::new(GuardConfig {
            tick_period: Duration::minutes(15),
            ..GuardConfig::default()
        });
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        let mut st = state(&[("haushalt", 0.5), ("battery", -4.0)]);
        if let Some(m) = st.assets.get_mut(&AssetId::new("battery").unwrap()) {
            // 10,5 % of 10 kWh, against a reserve of 10 %: 50 Wh usable, which
            // over a quarter of an hour is 190 W at the 0,95 discharge path.
            m.soc = Some(Soc::new(0.105).unwrap());
        }
        let v = guard.verdict(
            &site,
            &limits,
            &st,
            &wants(&[("wallbox", 11.0), ("battery", -4.0)]),
            NOW,
        );
        assert!(
            v.lent_generation < Power::from_kw(0.25),
            "lent {} from 50 Wh",
            v.lent_generation
        );
    }

    #[test]
    fn a_device_the_guard_cannot_command_does_not_spend_the_budget_twice() {
        // A relay-only heat pump is a SteuVE and takes its 3 kW off the top,
        // because the only lever left is to give the others less. Letting it
        // then *also* bid for what remains spends the same kilowatts twice, and
        // the wallbox is the one that pays.
        let mut site = site();
        site.assets
            .push(Asset::HeatPump(hems_core::asset::HeatPump {
                meta: AssetMeta::new(
                    AssetId::new("relaiswp").unwrap(),
                    cid(),
                    PhaseConnection::Three,
                    Power::from_kw(6.0),
                )
                .with_capabilities(Capabilities::MEASURE)
                .commissioned(time::macros::date!(2025 - 03 - 01)),
                electrical_nominal: Power::from_kw(6.0),
                heating_rod: None,
                control: hems_core::asset::HeatPumpControl::SgReady,
                modulating: true,
            }));
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(10.0)),
            ..GridLimits::default()
        };
        let v = guard.verdict(
            &site,
            &limits,
            &state(&[("haushalt", 0.5), ("relaiswp", 3.0)]),
            &wants(&[("wallbox", 11.0)]),
            NOW,
        );
        assert_eq!(v.steuve_budget, Some(Power::from_kw(7.0)));
        let wallbox = v.envelope(&AssetId::new("wallbox").unwrap()).ceiling;
        assert!(
            (wallbox.kw() - 7.0).abs() < 1e-6,
            "the wallbox should get all 7 kW that are left, got {wallbox}"
        );
    }

    #[test]
    fn photovoltaic_surplus_lets_the_devices_run_above_the_limit_lawfully() {
        // The economic case for an energy manager under § 14a, in one test.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        // 8 kW of production, 0,5 kW household: 7,5 kW of surplus.
        let v = guard.verdict(
            &site,
            &limits,
            &state(&[("pv", -8.0), ("haushalt", 0.5)]),
            &wants(&[("wallbox", 11.0)]),
            NOW,
        );
        assert_eq!(
            v.steuve_budget,
            Some(Power::from_kw(11.7)),
            "4,2 kW limit + 7,5 kW surplus"
        );
        assert_eq!(
            v.envelope(&AssetId::new("wallbox").unwrap()).ceiling,
            Power::from_kw(11.0)
        );
    }

    #[test]
    fn a_limit_below_the_minimum_is_applied_and_flagged() {
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(2.0)),
            ..GridLimits::default()
        };
        let v = guard.verdict(
            &site,
            &limits,
            &state(&[]),
            &wants(&[("wallbox", 11.0)]),
            NOW,
        );
        assert!(v.below_minimum, "the customer was owed more than 2 kW");
        assert!(v.envelope(&AssetId::new("wallbox").unwrap()).ceiling <= Power::from_kw(2.0));
    }

    // ── The limits everything shares ──────────────────────────────────────

    #[test]
    fn the_connection_is_shared_rather_than_handed_to_every_asset_in_full() {
        // The hole this closes: bounding each asset by the connection *on its
        // own* lets the sum sail past the main fuse. 11 kW of wallbox, 8 kW of
        // heat pump and 5 kW of battery all fit under a 24 kW connection
        // individually, and burn it together.
        let mut site = site();
        site.grid = GridConnection::new(Current::new(20.0)); // 13,8 kW
        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(
            &site,
            &GridLimits::default(),
            &state(&[("haushalt", 1.0)]),
            &wants(&[("wallbox", 11.0), ("battery", 5.0)]),
            NOW,
        );
        let total: Power = ["wallbox", "battery"]
            .iter()
            .map(|id| v.envelope(&AssetId::new(id).unwrap()).ceiling)
            .sum();
        // 13,8 kW of fuse less the kilowatt the house is already taking.
        assert!(
            total <= Power::from_kw(12.8) + Power::new(1.0),
            "the sum broke the fuse: {total}"
        );
        assert_eq!(
            v.binding(&AssetId::new("wallbox").unwrap()).ceiling,
            Some(GuardRule::ContractLimit)
        );
    }

    #[test]
    fn a_sub_circuit_is_shared_between_what_hangs_off_it() {
        // Two charge points in a garage on a 20 A board: 22 kW of wallbox behind
        // 13,8 kW of cable.
        let garage = CircuitId::new("garage").unwrap();
        let mut assets = site().assets;
        for name in ["wb-left", "wb-right"] {
            let mut m = meta(name, 11.0);
            m.circuit = garage.clone();
            assets.push(Asset::Evse(Evse {
                meta: m,
                min_current: Current::new(6.0),
                max_current: Current::new(16.0),
                bidirectional: false,
                public: false,
                charge_limit: None,
            }));
        }
        let site = Site::new(
            SiteId::new(),
            GeoPoint {
                latitude: 52.5,
                longitude: 13.4,
                altitude_m: 34.0,
            },
            GridConnection::new(Current::new(63.0)),
            Circuits::new(vec![
                Circuit::new(cid(), None, Current::new(63.0)),
                Circuit::new(garage.clone(), Some(cid()), Current::new(20.0)),
            ])
            .unwrap(),
            assets,
        )
        .unwrap();

        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(
            &site,
            &GridLimits::default(),
            &state(&[]),
            &wants(&[("wb-left", 11.0), ("wb-right", 11.0)]),
            NOW,
        );
        let total: Power = ["wb-left", "wb-right"]
            .iter()
            .map(|id| v.envelope(&AssetId::new(id).unwrap()).ceiling)
            .sum();
        assert!(
            total <= Power::from_kw(13.8) + Power::new(1.0),
            "the garage board was oversubscribed: {total}"
        );
    }

    // ── Measurements the guard did not get ────────────────────────────────

    #[test]
    fn a_controllable_device_nobody_can_hear_is_assumed_to_be_running_flat_out() {
        // Assuming zero is the tempting default and it is exactly wrong: the
        // guard would hand the silent device's share to everybody else while it
        // kept drawing, and the site would pass the network operator's limit.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        // The wallbox says nothing; everything else reports zero.
        let v = guard.verdict(
            &site,
            &limits,
            &state(&[("battery", 0.0), ("haushalt", 0.0), ("pv", 0.0)]),
            &wants(&[("battery", 5.0)]),
            NOW,
        );
        assert!(
            v.is_assumed(&AssetId::new("wallbox").unwrap()),
            "the silent charge point should have been assumed at its nameplate"
        );
        // 11 kW assumed, no generation: that is the netzwirksamer Leistungsbezug
        // the evidence record has to carry, not zero.
        assert_eq!(v.netzwirksam, Power::from_kw(11.0));
    }

    // ── Storage ───────────────────────────────────────────────────────────

    #[test]
    fn a_full_battery_is_not_told_to_charge_and_an_empty_one_is_not_told_to_discharge() {
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let battery = AssetId::new("battery").unwrap();

        let mut full = SiteState::default();
        full.assets.insert(battery.clone(), {
            let mut m = Measurement::power(NOW, Power::ZERO);
            m.soc = Some(Soc::FULL);
            m
        });
        let v = guard.verdict(&site, &GridLimits::default(), &full, &wants(&[]), NOW);
        assert_eq!(v.envelope(&battery).ceiling, Power::ZERO);

        let mut empty = SiteState::default();
        empty.assets.insert(battery.clone(), {
            let mut m = Measurement::power(NOW, Power::ZERO);
            m.soc = Some(Soc::new(0.01).unwrap());
            m
        });
        let v = guard.verdict(&site, &GridLimits::default(), &empty, &wants(&[]), NOW);
        assert_eq!(v.envelope(&battery).floor, Power::ZERO);
    }

    #[test]
    fn the_backup_reserve_stops_a_discharge_and_says_so() {
        // §24.13 of the concept, and the reason it belongs in the guard rather
        // than only in the planner: the reserve has to hold between re-plans.
        let mut site = site();
        for asset in &mut site.assets {
            if let Asset::Battery(b) = asset {
                b.reserve_soc = Soc::new(0.30).unwrap();
            }
        }
        let battery = AssetId::new("battery").unwrap();
        let mut state = SiteState::default();
        state.assets.insert(battery.clone(), {
            let mut m = Measurement::power(NOW, Power::from_kw(-3.0));
            m.soc = Some(Soc::new(0.25).unwrap());
            m
        });
        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(&site, &GridLimits::default(), &state, &wants(&[]), NOW);
        assert_eq!(v.envelope(&battery).floor, Power::ZERO);
        assert_eq!(
            v.binding(&battery).floor,
            Some(GuardRule::BackupReserve),
            "a household promise deserves its own name in the reason chain"
        );
    }

    // ── Unbalanced load, VDE-AR-N 4100 ────────────────────────────────────

    /// One single-phase charge point on L1, and another already drawing 3 kW on
    /// the same conductor. Together they are 7,6 kVA of spread; 4,6 kVA is the
    /// limit, so only 1,6 kW more fits.
    fn two_single_phase_charge_points() -> Site {
        let mut assets = site().assets;
        for (id, kw) in [("garage", 4.6), ("carport", 3.0)] {
            let mut m = AssetMeta::new(
                AssetId::new(id).unwrap(),
                cid(),
                PhaseConnection::Single { phase: Phase::L1 },
                Power::from_kw(kw),
            )
            .with_capabilities(Capabilities::MEASURE | Capabilities::LIMIT_CONSUMPTION);
            m.commissioned_at = Some(time::macros::date!(2025 - 03 - 01));
            assets.push(Asset::Evse(Evse {
                meta: m,
                min_current: Current::new(6.0),
                max_current: Current::new(20.0),
                bidirectional: false,
                public: false,
                charge_limit: None,
            }));
        }
        Site::new(
            SiteId::new(),
            GeoPoint {
                latitude: 52.5,
                longitude: 13.4,
                altitude_m: 34.0,
            },
            GridConnection::new(Current::new(63.0)),
            Circuits::new(vec![Circuit::new(cid(), None, Current::new(63.0))]).unwrap(),
            assets,
        )
        .unwrap()
    }

    #[test]
    fn a_single_phase_asset_is_held_back_so_the_site_stays_inside_its_schieflast() {
        let site = two_single_phase_charge_points();
        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(
            &site,
            &GridLimits::default(),
            &state(&[("carport", 3.0)]),
            &wants(&[("garage", 4.6)]),
            NOW,
        );
        let garage = AssetId::new("garage").unwrap();
        // L1 already carries 3 kW and the other conductors carry nothing, so
        // only 1,6 kW more fits inside 4,6 kVA of spread.
        assert!(
            (v.envelope(&garage).ceiling.kw() - 1.6).abs() < 1e-6,
            "got {}",
            v.envelope(&garage).ceiling
        );
        assert_eq!(v.binding(&garage).ceiling, Some(GuardRule::Unbalance));
    }

    #[test]
    fn an_ordinary_household_load_is_outside_the_symmetry_rule() {
        // VDE-AR-N 4100 Abschnitt 5.5.2 reaches only equipment that can feed in
        // or store — *"Erzeugungsanlagen, Speicher, Ladeeinrichtungen für
        // Elektrofahrzeuge"*. hems counted every asset, so a kettle on L1 spent
        // the budget of the one device the manager could actually move. This
        // **loosens** the guard, which is unusual and is the point: a limit
        // applied more widely than the rule is still a limit applied wrongly,
        // and the household pays for it in charging power.
        let mut site = two_single_phase_charge_points();
        site.assets.push(Asset::Load(FlexibleLoad {
            meta: AssetMeta::new(
                AssetId::new("altbau").unwrap(),
                cid(),
                PhaseConnection::Single { phase: Phase::L1 },
                Power::from_kw(3.0),
            ),
            nominal: Power::from_kw(3.0),
            kind: LoadKind::Fixed,
        }));
        let guard = Guard::new(GuardConfig::default());
        let v = guard.verdict(
            &site,
            &GridLimits::default(),
            &state(&[("carport", 3.0), ("altbau", 3.0)]),
            &wants(&[("garage", 4.6)]),
            NOW,
        );
        let garage = AssetId::new("garage").unwrap();
        assert!(
            (v.envelope(&garage).ceiling.kw() - 1.6).abs() < 1e-6,
            "the kettle must not cost the charge point anything: got {}",
            v.envelope(&garage).ceiling
        );
    }

    #[test]
    fn the_feed_in_cap_is_shared_at_the_connection_point() {
        // § 9 EEG caps the Einspeisung *at the connection point*. Applied to
        // each generator on its own — which is what this did until the fourth
        // audit — a roof allowed 5,88 kW and a battery allowed 5,88 kW put
        // 11,76 kW through a limit of 5,88.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            feed_in_ceiling: Some(Power::from_kw(5.88)),
            feed_in_rule: Some(GuardRule::Para9Cap),
            ..GridLimits::default()
        };
        let v = guard.verdict(&site, &limits, &state(&[]), &wants(&[]), NOW);
        let exported: Power = ["pv", "battery"]
            .iter()
            .map(|id| v.envelope(&AssetId::new(id).unwrap()).floor.outflow())
            .sum();
        assert!(
            exported <= Power::from_kw(5.88) + Power::new(1e-6),
            "the cap is spent by both of them together, got {exported}"
        );
        let pv_id = AssetId::new("pv").unwrap();
        let pv = v.envelope(&pv_id);
        assert!(pv.floor < Power::ZERO, "the roof still gets a share");
        // Feeding in at the cap names the § 9 rule; drawing does not.
        assert_eq!(v.binding_at(&pv_id, pv.floor), Some(GuardRule::Para9Cap));
        assert_eq!(v.binding_at(&pv_id, Power::ZERO), None);
        // A household load is not a generator and is never handed a share of a
        // feed-in limit it could not use.
        assert_eq!(
            v.envelope(&AssetId::new("haushalt").unwrap()).floor,
            Power::ZERO,
            "a load consumes"
        );
    }

    #[test]
    fn what_the_house_is_using_is_headroom_under_the_feed_in_cap() {
        // The correction that pays for itself every sunny hour: the statute caps
        // what *leaves* the connection point, so a house drawing 4 kW may
        // produce 4 kW above the cap. Curtailing the roof as though the car were
        // not charging throws those kilowatt-hours away.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            feed_in_ceiling: Some(Power::from_kw(5.88)),
            feed_in_rule: Some(GuardRule::Para9Cap),
            ..GridLimits::default()
        };
        let quiet = guard.verdict(
            &site,
            &limits,
            &state(&[
                ("haushalt", 0.0),
                ("wallbox", 0.0),
                ("battery", 0.0),
                ("pv", -8.0),
            ]),
            &wants(&[("pv", -8.0)]),
            NOW,
        );
        let busy = guard.verdict(
            &site,
            &limits,
            &state(&[
                ("haushalt", 0.5),
                ("wallbox", 4.0),
                ("battery", 0.0),
                ("pv", -8.0),
            ]),
            &wants(&[("pv", -8.0)]),
            NOW,
        );
        let roof = |v: &GuardVerdict| v.envelope(&AssetId::new("pv").unwrap()).floor.outflow();
        assert!(
            (roof(&quiet).kw() - 5.88).abs() < 1e-6,
            "with the house idle the roof gets the cap, got {}",
            roof(&quiet)
        );
        assert!(
            (roof(&busy).kw() - 8.0).abs() < 1e-6,
            "4,5 kW of measured draw lifts it past the inverter's own rating, got {}",
            roof(&busy)
        );
    }

    #[test]
    fn the_failsafe_is_reported_as_its_own_rule() {
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(1.0)),
            in_failsafe: true,
            ..GridLimits::default()
        };
        let v = guard.verdict(
            &site,
            &limits,
            &state(&[]),
            &wants(&[("wallbox", 11.0)]),
            NOW,
        );
        assert_eq!(
            v.binding(&AssetId::new("wallbox").unwrap()).ceiling,
            Some(GuardRule::Failsafe)
        );
    }

    #[test]
    fn a_stale_measurement_is_treated_as_absent() {
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let mut s = SiteState::default();
        s.assets.insert(
            AssetId::new("pv").unwrap(),
            Measurement::power(NOW - Duration::minutes(10), Power::from_kw(-8.0)),
        );
        let limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(4.2)),
            ..GridLimits::default()
        };
        let v = guard.verdict(&site, &limits, &s, &wants(&[("wallbox", 11.0)]), NOW);
        // The stale production does not become surplus, so no headroom is invented.
        assert_eq!(v.steuve_budget, Some(Power::from_kw(4.2)));
    }
    #[test]
    fn a_discharging_battery_does_not_inflate_the_budget_through_the_grid_meter() {
        // The grid meter closes the balance for load nobody instrumented. Under
        // the load convention that inversion has to count *every* generating
        // asset — including a controllable one. Leaving the battery's discharge
        // out understates the rest of the house by exactly that discharge, which
        // overstates the surplus and hands the § 14a budget power it does not
        // have.
        let site = site();
        let guard = Guard::new(GuardConfig::default());
        let mut st = state(&[
            ("pv", -6.0),
            ("haushalt", 1.0),
            ("battery", -2.0),
            ("wallbox", 0.0),
        ]);
        // 1 kW of load nobody metered, so the meter is the only witness for it:
        // −6 (roof) + 1 (metered) + 1 (not) − 2 (battery) = −6 kW at the meter.
        st.grid = Some(Measurement::power(NOW, Power::from_kw(-6.0)));

        let limits = GridLimits {
            steuve_ceiling: Some(Power::ZERO),
            ..GridLimits::default()
        };
        let v = guard.verdict(&site, &limits, &st, &wants(&[("wallbox", 11.0)]), NOW);

        // Generation 6 kW, house 2 kW (1 metered + 1 not), so 4 kW of surplus and
        // a budget of 4 kW — not the 5 kW an unclosed balance would report.
        let budget = v.steuve_budget.expect("a ceiling was given");
        assert!(
            (budget.kw() - 4.0).abs() < 1e-6,
            "budget should be 4 kW, got {budget}"
        );
    }

    #[test]
    fn a_silent_single_phase_device_still_counts_towards_the_schieflast() {
        // `flows` assumes a silent device is at its nameplate, and the
        // per-phase view has to read the same silence the same way. A guard that
        // cannot see an unbalance it is causing hands the one device it *can*
        // move a ceiling that keeps the spread inside 4,6 kVA only on paper.
        let site = two_single_phase_charge_points();
        let guard = Guard::new(GuardConfig::default());
        // Nobody is measuring the carport, which is rated 3 kW.
        let v = guard.verdict(
            &site,
            &GridLimits::default(),
            &state(&[]),
            &wants(&[("garage", 4.6)]),
            NOW,
        );
        let garage = AssetId::new("garage").unwrap();
        assert!(
            v.envelope(&garage).ceiling <= Power::from_kw(1.7),
            "silence is not zero on a conductor either: got {}",
            v.envelope(&garage).ceiling
        );
        assert_eq!(v.binding(&garage).ceiling, Some(GuardRule::Unbalance));
    }
}
