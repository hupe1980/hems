//! The arbiter: plan plus reality, once a second.
//!
//! Three planes decide what a house does, and they run at different speeds:
//!
//! | Plane | Cadence | Authority |
//! |---|---|---|
//! | Guard ([`crate::guard`]) | every tick | absolute — `[A1 4.6 S. 3]` |
//! | Arbiter (here) | ~1 s | inside the guard's bounds |
//! | Planner (`hems-optimizer`) | every 5 min, 15-minute slots | advisory |
//!
//! The arbiter exists because a plan made at 12:00 for the quarter hour starting
//! at 12:15 cannot know that a cloud will pass at 12:19. It follows the plan's
//! *intent* while tracking what is actually measured, and it is the last thing
//! to touch a number before it becomes a command.
//!
//! # The order, and why it is that order
//!
//! 1. **Desire** — what each asset would do: a user's explicit wish, else the
//!    plan, else photovoltaic surplus.
//! 2. **Guard** — narrow every desire into the interval the grid, the fuses and
//!    the hardware leave open.
//! 3. **Smooth** — ramp and deadband, so a device is not asked to chase noise.
//! 4. **Explain** — attach the reason, which is the guard's rule whenever the
//!    guard is what is holding the value.
//!
//! Because step 2 is an intersection and step 3 only moves a value *towards* the
//! previous one inside that interval, no later step can undo an earlier bound.
//! The test `no_input_can_make_the_arbiter_exceed_a_grid_limit` is that
//! argument as a property test.

use std::collections::BTreeMap;

use hems_core::prelude::*;
use time::{Duration, OffsetDateTime};

use crate::allocate::{Claim, allocate_indivisible};
use crate::guard::{GridLimits, Guard, GuardConfig, GuardVerdict, SiteState, minimum_useful_power};
use crate::phases::{PhaseState, PhaseSwitchConfig};

/// How the arbiter behaves between plans.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ArbiterConfig {
    /// The guard's configuration.
    pub guard: GuardConfig,
    /// How eagerly a switchable charge point changes its conductors.
    #[cfg_attr(feature = "serde", serde(default))]
    pub phase_switch: PhaseSwitchConfig,
    /// How old a plan may be before it is ignored.
    ///
    /// A stale plan is worse than none: it was computed against prices and
    /// forecasts that have moved on, and following it looks deliberate while
    /// being nothing of the kind.
    #[cfg_attr(
        feature = "serde",
        serde(default = "default_plan_age", with = "crate::guard::duration_secs")
    )]
    pub max_plan_age: Duration,
    /// The largest change to a setpoint in one tick, if the operator wants
    /// ramping. `None` lets values move freely.
    #[cfg_attr(feature = "serde", serde(default))]
    pub ramp_per_tick: Option<Power>,
    /// Changes smaller than this are not sent at all — a device that is asked to
    /// chase 20 W of measurement noise wears out its relays for nothing.
    #[cfg_attr(feature = "serde", serde(default = "default_deadband"))]
    pub deadband: Power,
    /// The import the arbiter aims to leave at the connection point when it is
    /// tracking surplus. A small positive value keeps a household from
    /// oscillating around zero export.
    #[cfg_attr(feature = "serde", serde(default))]
    pub residual_power: Power,
}

fn default_plan_age() -> Duration {
    Duration::minutes(20)
}

fn default_deadband() -> Power {
    Power::new_const(50.0)
}

impl Default for ArbiterConfig {
    fn default() -> Self {
        Self {
            guard: GuardConfig::default(),
            phase_switch: PhaseSwitchConfig::default(),
            max_plan_age: default_plan_age(),
            ramp_per_tick: None,
            deadband: default_deadband(),
            residual_power: Power::ZERO,
        }
    }
}

/// Everything the arbiter needs for one tick.
#[derive(Debug, Clone, Copy)]
pub struct Tick<'a> {
    /// The instant this tick represents.
    pub now: OffsetDateTime,
    /// The installation.
    pub site: &'a Site,
    /// What the drivers report.
    pub state: &'a SiteState,
    /// What the grid is asking.
    pub limits: &'a GridLimits,
    /// The current plan, if there is one.
    pub plan: Option<&'a Plan>,
    /// Explicit wishes from the household.
    pub overrides: &'a BTreeMap<AssetId, UserOverride>,
    /// What was commanded last tick, for ramping and the deadband.
    pub previous: &'a BTreeMap<AssetId, Power>,
    /// Energy already moved by each asset **since the start of the current
    /// quarter hour**, load convention.
    ///
    /// This is what turns a plan into a commitment rather than a suggestion. A
    /// plan says "put 2,4 kWh into the battery during this slot"; a cloud passes
    /// at 12:19 and the literal setpoint stops being the right one, but the
    /// energy still is. The arbiter divides what is left by the time left and
    /// tracks *that*, inside the envelope the plan gave it.
    ///
    /// The caller accumulates it and resets it on every slot boundary. An empty
    /// map means "nothing delivered yet", which is exactly right at the start of
    /// a slot and merely conservative in the middle of one.
    pub delivered: &'a BTreeMap<AssetId, Energy>,
    /// What the arbiter decided last tick about each switchable charge point's
    /// conductors — see [`crate::phases`].
    ///
    /// An absent entry starts the charge point in the mode its wiring implies.
    pub phases: &'a BTreeMap<AssetId, PhaseState>,
}

/// The weight a boost carries. Large enough to win any ordinary allocation,
/// small enough to stay a finite number the allocator can divide by.
const BOOST_WEIGHT: f64 = 1_000.0;

/// Where a desire came from, before the guard had its say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desire {
    User(UserOverride),
    Plan,
    Realtime(RealtimeCause),
    Idle(FallbackCause),
}

/// Step 1's output, kept together so the later steps can read it.
struct Desires<'a> {
    wants: BTreeMap<AssetId, (Power, f64)>,
    /// What each asset wants **averaged over the slot**, rather than at this
    /// instant.
    ///
    /// The two differ because the arbiter tracks the plan's *energy*: nine
    /// minutes into a quarter hour, "what is left divided by the time left" can
    /// be anywhere between zero and the device's rating even though the plan
    /// asked for one steady figure. Decisions that should hold for minutes — the
    /// conductor count, and nothing else so far — read this one; a contactor
    /// driven from the instantaneous figure chases the tracking loop and switches
    /// dozens of times a day.
    steady: BTreeMap<AssetId, Power>,
    sources: BTreeMap<AssetId, Desire>,
    plan: Option<&'a Plan>,
    slot_plan: Option<&'a SlotPlan>,
}

/// What one tick produced.
#[derive(Debug, Clone)]
pub struct Decision {
    /// The commands to send. Only assets whose value moved by more than the
    /// deadband appear.
    pub setpoints: Vec<Setpoint>,
    /// What every asset was told, including the unchanged ones — the state the
    /// next tick compares against.
    pub commanded: BTreeMap<AssetId, Power>,
    /// The guard's bounds and reasons for this tick.
    pub verdict: GuardVerdict,
    /// The phase policy for the next tick. Feed it back through
    /// [`Tick::phases`]; where a mode differs from the one before, the setpoints
    /// already carry the [`Command::PhaseCount`] that changes it.
    pub phases: BTreeMap<AssetId, PhaseState>,
    /// How far the measured grid power is from the sum of the measured assets.
    /// A residual that is not near zero means a meter is missing or mis-signed.
    pub balance_residual: Option<Power>,
}

/// The one-second control loop.
#[derive(Debug, Clone)]
pub struct Arbiter {
    config: ArbiterConfig,
    guard: Guard,
}

impl Arbiter {
    /// An arbiter with this configuration.
    #[must_use]
    pub fn new(config: ArbiterConfig) -> Self {
        let guard = Guard::new(config.guard.clone());
        Self { config, guard }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &ArbiterConfig {
        &self.config
    }

    /// Decide what every asset should do now.
    #[must_use]
    pub fn tick(&self, tick: Tick<'_>) -> Decision {
        let max_age = self.config.guard.max_measurement_age;
        let desires = self.desires(&tick, max_age);

        // ── 2. Guard ───────────────────────────────────────────────────────
        let mut verdict =
            self.guard
                .verdict(tick.site, tick.limits, tick.state, &desires.wants, tick.now);

        // ── 3. Conductors ──────────────────────────────────────────────────
        //
        // Decided before the commands are built and after the guard has spoken,
        // because "how much could this charge point use" is a question about the
        // interval the guard left open — and because a switch takes effect on
        // the command that carries it, not a tick later.
        let phases = self.phase_policy(&tick, &desires, &verdict);

        // …and then the guard bounds the mode that is about to be commanded
        // rather than the one being left. Without this the switching tick
        // carries a single-phase ceiling into a three-phase decision, where
        // anything below 4,14 kW is no current at all — so the wallbox is
        // commanded zero, the surplus jumps, and the policy switches it back.
        let modes: BTreeMap<AssetId, PhaseMode> =
            phases.iter().map(|(id, p)| (id.clone(), p.mode)).collect();
        self.guard
            .apply_modes(tick.site, tick.state, tick.now, &modes, &mut verdict);

        // ── 4. Smooth, and 5. explain ──────────────────────────────────────
        let mut setpoints = Vec::new();
        let mut commanded = BTreeMap::new();

        for (id, (want, _)) in &desires.wants {
            let Some(asset) = tick.site.asset(id) else {
                continue;
            };
            let envelope = verdict.envelope(id);
            let previous = tick.previous.get(id).copied();
            let (ramped, ramping) = self.smooth(envelope.clamp(*want), previous, envelope);
            // What the device will actually take. A charge point is off or above
            // 6 A per conductor with nothing in between, and commanding a value
            // in between means commanding nothing while believing otherwise —
            // which the energy tracker then compounds by trying to catch up on
            // energy that was never going to flow.
            let mode = phases
                .get(id)
                .map_or_else(|| tick.state.phase_mode(asset), |p| p.mode);
            let smoothed = envelope.clamp(hems_device::realisable(asset, ramped, mode));
            commanded.insert(id.clone(), smoothed);

            // A change smaller than the deadband is not worth a message — unless
            // a guard rule is what is holding the value, in which case the
            // command is the evidence that the reduction was carried out.
            let guard_rule = verdict.binding_at(id, smoothed);
            let moved = previous.is_none_or(|p| (smoothed - p).abs() > self.config.deadband);
            if !moved && guard_rule.is_none() {
                continue;
            }
            // The value was held short of what was wanted, and neither the plan
            // nor the guard did it: say which of the arbiter's own mechanisms
            // did.
            let correction = ramping.or_else(|| {
                (!moved && previous.is_some_and(|p| p != smoothed))
                    .then_some(RealtimeCause::Hysteresis)
            });

            let reason = Self::reason_for(&tick, &desires, id, guard_rule, correction);

            // Watts are what the physics and the regulation are written in;
            // almost no device speaks them. `hems-device` turns the decision
            // into amperes for a charge point, a contact state for an SG Ready
            // heat pump, a ceiling for an inverter.
            let decision = hems_device::Decision::new(smoothed)
                .guard_limited(guard_rule.is_some())
                .in_phase_mode(mode);
            for command in hems_device::commands_for(asset, decision) {
                if let Ok(setpoint) = Setpoint::new(id.clone(), command, reason, tick.now) {
                    setpoints.push(setpoint);
                }
            }
        }

        Decision {
            setpoints,
            commanded,
            balance_residual: Self::balance_residual(&tick, max_age),
            verdict,
            phases,
        }
    }

    /// Step every switchable charge point's phase policy.
    ///
    /// "Available" is what the asset could use if it were in the right mode:
    /// what it wants, bounded by [`GuardVerdict::phase_headroom`] — the ceiling
    /// *before* the mode-dependent rules. Two things are deliberately left out,
    /// and both of them would close a circle:
    ///
    /// * the mode's own minimum, which is the thing being decided;
    /// * the Schieflast bound, which exists only because the device is
    ///   single-phase and sits just below the power a three-phase session needs
    ///   to start.
    fn phase_policy(
        &self,
        tick: &Tick<'_>,
        desires: &Desires<'_>,
        verdict: &GuardVerdict,
    ) -> BTreeMap<AssetId, PhaseState> {
        let entries = tick.site.assets.iter().filter_map(|asset| {
            let Asset::Evse(evse) = asset else {
                return None;
            };
            let want = desires
                .steady
                .get(asset.id())
                .copied()
                .unwrap_or(Power::ZERO);
            let ceiling = verdict.phase_headroom(asset.id()).max(Power::ZERO);
            Some((asset.id(), evse, want.max(Power::ZERO).min(ceiling)))
        });
        crate::phases::decide_all(entries, tick.phases, tick.now, &self.config.phase_switch)
    }

    /// Step 1: what every controllable asset would do if nothing stopped it.
    fn desires<'a>(&self, tick: &Tick<'a>, max_age: Duration) -> Desires<'a> {
        let plan = tick
            .plan
            .filter(|p| !p.is_stale(tick.now, self.config.max_plan_age));
        let slot_plan = plan.and_then(|p| p.slot_at(tick.now));
        // Distinguish "there was no plan" from "there was one and it had gone
        // off": the second is an operational fault worth surfacing, the first is
        // just a box that has not been given a planner.
        let idle_cause = if tick.plan.is_some() && plan.is_none() {
            FallbackCause::PlanStale
        } else {
            FallbackCause::NoPlan
        };

        // Two views of the same allocation, and the difference is one number.
        //
        // What the arbiter **commands** has to respect the conductor count the
        // charge point is actually in: three conductors deliver 4,14 kW or
        // nothing, so a share of 3,8 kW granted to a three-phase session is not
        // a slow charge, it is no charge — and the measurement that comes back
        // is a zero that makes the surplus look larger, which grants it more,
        // which is a contactor operation every few minutes all afternoon.
        //
        // What the **conductor policy** reads is the other question: how much
        // could this charge point use if it switched? That one has to be
        // computed with the lowest floor its wiring can reach, or a session shed
        // for failing a three-phase minimum can never argue its way down to one
        // conductor.
        let self_consumption = self.self_consumption(tick, max_age, Floors::AsWired);
        let could_absorb = self.self_consumption(tick, max_age, Floors::BestAvailable);

        let mut wants: BTreeMap<AssetId, (Power, f64)> = BTreeMap::new();
        let mut steady: BTreeMap<AssetId, Power> = BTreeMap::new();
        let mut sources: BTreeMap<AssetId, Desire> = BTreeMap::new();

        for asset in &tick.site.assets {
            if !asset
                .capabilities()
                .contains(Capabilities::LIMIT_CONSUMPTION)
                && !asset.capabilities().contains(Capabilities::SET_POWER)
                && !asset
                    .capabilities()
                    .contains(Capabilities::LIMIT_PRODUCTION)
            {
                continue;
            }
            let id = asset.id().clone();
            let planned = slot_plan.and_then(|s| s.target(&id)).map(|t| t.power);
            let (want, weight, source) = match tick.overrides.get(&id) {
                Some(UserOverride::Pause) => (Power::ZERO, 1.0, Desire::User(UserOverride::Pause)),
                Some(UserOverride::Boost) => (
                    asset.meta().connection_power,
                    BOOST_WEIGHT,
                    Desire::User(UserOverride::Boost),
                ),
                Some(UserOverride::Away) | None => match slot_plan.and_then(|s| s.target(&id)) {
                    // A plan target means two different things and only one of
                    // them is an energy commitment. For a store or a load it is
                    // "move this much energy this quarter hour", and falling
                    // behind is something to catch up on. For an inverter it is
                    // a **ceiling** — "feed in at most this" — and there is
                    // nothing to catch up on, because production the plan
                    // refused is gone rather than deferred. Tracking it as
                    // energy divides a whole slot's production by the time left
                    // and asks for more and more of it, which undoes the
                    // curtailment within seconds of the slot starting.
                    Some(target) if generates(asset) => (
                        target.power,
                        target_weight(
                            target.value_or(slot_plan.and_then(|s| s.marginal_eur_per_kwh)),
                        ),
                        Desire::Plan,
                    ),
                    Some(target) => (
                        Self::tracking_power(tick, target),
                        target_weight(
                            target.value_or(slot_plan.and_then(|s| s.marginal_eur_per_kwh)),
                        ),
                        Desire::Plan,
                    ),
                    None => match self_consumption.get(&id) {
                        Some((want, cause)) => (*want, 1.0, Desire::Realtime(*cause)),
                        // An inverter with nothing to say to it runs at its
                        // maximum power point. Every other asset answers a
                        // request for *more* and so reads an absent instruction
                        // as zero; an inverter answers a request for *less*, and
                        // zero is the one value that means "stop". The guard
                        // narrows this to whatever § 9 EEG, an LPP session and
                        // the fuse leave open, which is exactly where the
                        // decision belongs.
                        None if generates(asset) => (
                            Self::maximum_power_point(tick, asset, max_age),
                            1.0,
                            Desire::Realtime(RealtimeCause::MaximumPowerPoint),
                        ),
                        // …and a device that runs itself is handed back to its
                        // own thermostat rather than held at zero.
                        None if self_regulating(asset) => (
                            asset.ratings().ceiling,
                            1.0,
                            Desire::Realtime(RealtimeCause::LocalControl),
                        ),
                        None => (Power::ZERO, 1.0, Desire::Idle(idle_cause)),
                    },
                },
            };
            wants.insert(id.clone(), (want, weight));
            // Under a plan the steady view is the slot's own target; otherwise
            // there is nothing steadier than the want itself.
            steady.insert(
                id.clone(),
                match source {
                    Desire::Plan => planned.unwrap_or(want),
                    // Including the assets the command-side allocation just
                    // switched *off*: a three-phase session shed for failing a
                    // 4,14 kW minimum is exactly the one that should be asking
                    // whether one conductor would do, and reading its `want` of
                    // zero is how it never gets to.
                    _ => could_absorb
                        .get(&id)
                        .map_or(want, |(best, _)| (*best).max(want)),
                },
            );
            sources.insert(id, source);
        }

        Desires {
            wants,
            steady,
            sources,
            plan,
            slot_plan,
        }
    }

    /// What an unconstrained inverter would feed in, in the load convention.
    ///
    /// [`Measurement::available_power`] when the driver publishes it, and the
    /// inverter's own rating when it does not.
    ///
    /// The tempting fallback is the *measured* production, and it is a trap: a
    /// roof curtailed to 5 kW reports 5 kW, so the next tick asks for 5 kW and
    /// the curtailment outlives the reason for it. The rating is optimistic
    /// instead, which is the right direction for a number that only relaxes a
    /// bound — the guard narrows it again on the same tick, and the worst case
    /// is that the export budget is shared out as though a dark roof wanted all
    /// of it.
    fn maximum_power_point(tick: &Tick<'_>, asset: &Asset, max_age: Duration) -> Power {
        let rating = asset.ratings().floor;
        tick.state
            .asset(asset.id())
            .filter(|m| m.freshness(tick.now, max_age).is_fresh())
            .and_then(|m| m.available_power)
            .map_or(rating, |p| (-p.abs()).max(rating))
    }

    /// Step 4: name the authority that produced a value.
    fn reason_for(
        tick: &Tick<'_>,
        desires: &Desires<'_>,
        id: &AssetId,
        guard_rule: Option<GuardRule>,
        correction: Option<RealtimeCause>,
    ) -> Reason {
        match (guard_rule, desires.sources.get(id)) {
            (Some(rule), _) => match tick.limits.steuve_since {
                Some(since) if matches!(rule, GuardRule::Lpc | GuardRule::Failsafe) => {
                    Reason::guard_since(rule, since)
                }
                _ => Reason::guard(rule),
            },
            // The guard is not holding it, but the arbiter is: a ramp or the
            // deadband, and the household should be told which.
            (None, _) if correction.is_some() => Reason::Realtime(correction.expect("checked")),
            (None, Some(Desire::User(o))) => Reason::User(*o),
            (None, Some(Desire::Plan)) => Reason::Plan {
                plan: desires.plan.map_or_else(PlanId::new, |p| p.id),
                slot: Slot::containing(tick.now),
                marginal_eur_per_kwh: desires.slot_plan.and_then(|s| s.marginal_eur_per_kwh),
            },
            (None, Some(Desire::Realtime(cause))) => Reason::Realtime(*cause),
            (None, Some(Desire::Idle(cause))) => Reason::Fallback(*cause),
            (None, None) => Reason::Fallback(FallbackCause::NoPlan),
        }
    }

    /// What the plan is really asking for right now: its **energy**, not its
    /// setpoint.
    ///
    /// A plan made at 12:00 for the quarter hour starting at 12:15 cannot know
    /// that a cloud will pass at 12:19. Its literal setpoint stops being right
    /// the moment it does; "put 2,4 kWh into the battery during this slot" does
    /// not. So the arbiter divides what is left of the slot's energy by what is
    /// left of the slot and asks for that, inside the freedom the plan gave it.
    ///
    /// Two guards on the arithmetic. The remaining time is floored at a second,
    /// so the last instant of a slot cannot produce a division by zero; and the
    /// result is clamped into the plan's own envelope, which is sign-consistent
    /// with the target — so catching up can never turn a charge into a discharge.
    fn tracking_power(tick: &Tick<'_>, target: &AssetTarget) -> Power {
        let slot = Slot::containing(tick.now);
        let remaining = (slot.end() - tick.now).max(Duration::seconds(1));
        let done = tick
            .delivered
            .get(&target.asset)
            .copied()
            .unwrap_or(Energy::ZERO);
        let outstanding = target.energy() - done;
        target.envelope.clamp(outstanding.over(remaining))
    }

    /// What each asset should do when there is no plan to follow.
    ///
    /// The box has to keep working when the planner is gone — a cold start, a
    /// stale plan, a solver that timed out — and "keep working" means the
    /// classical behaviour every home battery ships with: **cover the house from
    /// the roof and the store rather than from the grid**. Exporting at the
    /// feed-in tariff while importing at the retail one is the loss the arbiter
    /// exists to avoid, and it runs in both directions.
    ///
    /// Both directions share one rule: the claims are **increments** on what an
    /// asset is already doing. The imbalance is measured at the connection point
    /// with the assets already running, so an asset charging at 2 kW while 3 kW
    /// leaves the house can absorb 3 kW *more*, not 3 kW in total. Claiming the
    /// total is a mistake that hides itself — the loop still converges, by
    /// oscillating around the answer once a second for as long as the sun is out.
    fn self_consumption(
        &self,
        tick: &Tick<'_>,
        max_age: Duration,
        floors: Floors,
    ) -> BTreeMap<AssetId, (Power, RealtimeCause)> {
        // Without a grid measurement there is no imbalance to be sure of, and
        // guessing one would move every store on the site.
        let Some(grid) = tick
            .state
            .grid
            .as_ref()
            .and_then(|m| m.fresh_power(tick.now, max_age))
        else {
            return BTreeMap::new();
        };

        let surplus = (grid.outflow() - self.config.residual_power).max(Power::ZERO);
        let deficit = (grid.inflow() - self.config.residual_power).max(Power::ZERO);
        let (pool, cause) = if surplus > Power::ZERO {
            (surplus, RealtimeCause::SurplusTracking)
        } else if deficit > Power::ZERO {
            (deficit, RealtimeCause::SelfConsumption)
        } else {
            return BTreeMap::new();
        };

        let current = |asset: &Asset| {
            tick.state
                .power_of(asset.id(), tick.now, max_age)
                .unwrap_or(Power::ZERO)
        };

        let mut claims = Vec::new();
        for asset in &tick.site.assets {
            if !asset.capabilities().contains(Capabilities::SET_POWER) {
                continue;
            }
            if !matches!(asset, Asset::Battery(_) | Asset::Evse(_)) {
                continue;
            }
            let now_p = current(asset);
            let bound = if cause == RealtimeCause::SurplusTracking {
                Self::absorption_ceiling(asset, tick, max_age)
            } else {
                Self::discharge_floor(asset, tick, max_age)
            };
            let headroom = if cause == RealtimeCause::SurplusTracking {
                (bound - now_p).max(Power::ZERO)
            } else {
                (now_p - bound).max(Power::ZERO)
            };
            let mut claim = Claim::new(asset.id().clone(), headroom);
            if cause == RealtimeCause::SurplusTracking {
                let floor = match floors {
                    Floors::AsWired => Self::useful_floor(tick, asset),
                    Floors::BestAvailable => minimum_useful_power(asset),
                };
                claim = claim.with_floor((floor - now_p).max(Power::ZERO).min(headroom));
            }
            claims.push(claim);
        }

        allocate_indivisible(pool, &claims)
            .into_iter()
            .filter(|g| g.power > Power::ZERO)
            .map(|g| {
                let now_p = tick.site.asset(&g.asset).map_or(Power::ZERO, &current);
                let want = if cause == RealtimeCause::SurplusTracking {
                    now_p.max(Power::ZERO) + g.power
                } else {
                    now_p - g.power
                };
                (g.asset, (want, cause))
            })
            .collect()
    }

    /// The least power at which this asset does something in the conductor count
    /// it is in — or heading to, which is the same question one tick earlier.
    fn useful_floor(tick: &Tick<'_>, asset: &Asset) -> Power {
        match asset {
            Asset::Evse(evse) => {
                let mode = tick.phases.get(asset.id()).map(|p| p.mode).map_or_else(
                    || tick.state.phase_mode(asset),
                    |m| asset.meta().phases.clamp_mode(m),
                );
                evse.min_power(mode)
            }
            _ => minimum_useful_power(asset),
        }
    }

    /// The most this asset may be commanded to draw while absorbing surplus.
    ///
    /// A full battery is not an absorber. Leaving it in the pool sends the roof's
    /// output to a store that cannot take it while the car waits — the surplus is
    /// then exported at the feed-in tariff, which is the whole loss the arbiter
    /// exists to avoid.
    fn absorption_ceiling(asset: &Asset, tick: &Tick<'_>, max_age: Duration) -> Power {
        let ceiling = asset.ratings().ceiling;
        match asset {
            Asset::Battery(b) => match tick.state.soc_of(asset.id(), tick.now, max_age) {
                Some(soc) if soc >= b.soc_max => Power::ZERO,
                _ => ceiling,
            },
            // …and neither is a car that already has what it was asked for. A
            // surplus tracker with no notion of *enough* pushes production past
            // the Ladelimit in preference to exporting it, which earns money.
            // The planner has an energy target and a departure instead; this
            // fallback has neither, and it is what runs when the cloud is gone.
            Asset::Evse(e) => match (
                e.charge_limit,
                tick.state.soc_of(asset.id(), tick.now, max_age),
            ) {
                (Some(limit), Some(soc)) if soc >= limit => Power::ZERO,
                _ => ceiling,
            },
            _ => ceiling,
        }
    }

    /// The lowest power this asset may be commanded to while covering a deficit.
    ///
    /// An empty battery is not a source, and one at its backup reserve is a
    /// source the household has asked not to use. The guard enforces both again
    /// afterwards — it must, because it is the guard — but a claim that ignores
    /// them starves whatever else could actually have covered the import.
    fn discharge_floor(asset: &Asset, tick: &Tick<'_>, max_age: Duration) -> Power {
        let floor = asset.ratings().floor;
        match asset {
            Asset::Battery(b) => match tick.state.soc_of(asset.id(), tick.now, max_age) {
                Some(soc) if soc <= b.discharge_floor() => Power::ZERO,
                _ => floor,
            },
            // A one-way charge point cannot discharge; `ratings().floor` is
            // already zero for one, so this needs no case of its own.
            _ => floor.min(Power::ZERO),
        }
    }

    /// Move `wanted` no further than the ramp allows, and never outside the
    /// envelope the guard set.
    ///
    /// Returns the value and, when the arbiter — rather than the plan or the
    /// guard — is what decided it, the cause to put in the reason chain. A
    /// household asking "why is the wallbox at 7 kW when the plan says 11?"
    /// deserves "it is ramping", not silence.
    fn smooth(
        &self,
        wanted: Power,
        previous: Option<Power>,
        envelope: Envelope,
    ) -> (Power, Option<RealtimeCause>) {
        let Some(ramp) = self.config.ramp_per_tick else {
            return (wanted, None);
        };
        let Some(previous) = previous else {
            return (wanted, None);
        };
        let gap = (wanted - previous).abs();
        let step = gap.min(ramp);
        let ramped = if wanted >= previous {
            previous + step
        } else {
            previous - step
        };
        // Ramping may never carry a value back out of the guard's interval: a
        // limit that arrives now takes effect now, not one ramp step from now.
        let clamped = envelope.clamp(ramped);
        let cause = (gap > ramp && clamped != wanted).then_some(RealtimeCause::RampLimit);
        (clamped, cause)
    }

    fn balance_residual(tick: &Tick<'_>, max_age: Duration) -> Option<Power> {
        let grid = tick.state.grid.as_ref()?.fresh_power(tick.now, max_age)?;
        let assets: Power = tick
            .site
            .assets
            .iter()
            .filter(|a| !matches!(a, Asset::Meter(_)))
            .filter_map(|a| tick.state.power_of(a.id(), tick.now, max_age))
            .sum();
        Some(Site::balance_residual(grid, [assets]))
    }
}

/// Whether this asset runs itself when nothing is limiting it.
///
/// A heat pump has a thermostat and a hot-water tank has a temperature sensor,
/// and an energy manager only ever tells either of them to use *less*. A battery
/// and a charge point have no opinion of their own and do nothing at all until
/// somebody asks them to.
///
/// So an absent instruction means two different things, and reading it as "zero"
/// for both is how a box whose planner had stopped let a January house go cold
/// and handed out cold showers in June — while reporting a saving for both,
/// because energy nobody used is energy nobody bought. It is the same mistake
/// the inverter made, in the vocabulary of a different device.
fn self_regulating(asset: &Asset) -> bool {
    matches!(asset, Asset::HeatPump(_) | Asset::Dhw(_))
}

/// Which minimum a claim is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Floors {
    /// The conductor count the charge point is in — what it can do *now*.
    AsWired,
    /// The lowest its contactor can reach — what it could do if it switched.
    BestAvailable,
}

/// Whether this asset produces rather than consumes.
///
/// Not "can it export" — a bidirectional charge point can, and it is still a
/// load with a car on it that wants charging. This is the narrower question of
/// whether an absent instruction means *run flat out*.
fn generates(asset: &Asset) -> bool {
    matches!(asset, Asset::Pv(_))
}

/// Turn the marginal value of energy into an allocation weight.
///
/// The value is **per asset** wherever the planner produced one: the shadow
/// price of that device's own state equation, which is what the plan loses if
/// this device is the one held back. That is the whole point of the weight — a
/// car three hours from its departure and a heat pump in a house that is already
/// warm want the same kilowatt and are not worth the same, and until the
/// planner priced them separately the weighted allocator was weighting nothing.
///
/// The mapping is deliberately **compressive**: a square root, so a device worth
/// a hundred times another gets ten times the share rather than a hundred. Two
/// reasons. A max-min allocator is already a rationing rule and a runaway weight
/// turns it into a winner-take-all one, which strands the heat pump for a whole
/// reduction; and the shortfall prices that dominate the top of the range
/// (€5/kWh for a car, €3/kWh for hot water) are *lexicographic devices* rather
/// than measured willingness to pay, so treating their magnitude literally would
/// import an arbitrary constant into a physical decision.
///
/// The floor and ceiling are what stop a slot with a negative price or an
/// enormous shortfall from leaving a device with nothing at all: every device
/// keeps a share, and the ordering does the work.
fn target_weight(marginal_eur_per_kwh: Option<f64>) -> f64 {
    match marginal_eur_per_kwh {
        Some(v) if v.is_finite() && v > 0.0 => (v / REFERENCE_EUR_PER_KWH).sqrt().clamp(0.2, 5.0),
        _ => 1.0,
    }
}

/// The price a weight of one corresponds to, €/kWh.
///
/// An ordinary German retail kilowatt-hour, so a device the plan values at about
/// what the grid charges is neither favoured nor penalised, and the weights of a
/// household with nothing special going on are all near one.
const REFERENCE_EUR_PER_KWH: f64 = 0.30;

#[cfg(test)]
mod tests {
    use super::*;
    use hems_core::asset::{AssetMeta, Battery, Chemistry, Evse, FlexibleLoad, LoadKind, PvArray};
    use hems_grid::para14a::minimum_power;
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
        .with_capabilities(
            Capabilities::MEASURE | Capabilities::LIMIT_CONSUMPTION | Capabilities::SET_POWER,
        )
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
                    cap_relief: CapRelief::None,
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
                    meta: AssetMeta::new(
                        AssetId::new("haushalt").unwrap(),
                        cid(),
                        PhaseConnection::Three,
                        Power::from_kw(3.0),
                    ),
                    nominal: Power::from_kw(0.5),
                    kind: LoadKind::Fixed,
                }),
            ],
        )
        .unwrap()
    }

    struct Fixture {
        site: Site,
        state: SiteState,
        limits: GridLimits,
        overrides: BTreeMap<AssetId, UserOverride>,
        previous: BTreeMap<AssetId, Power>,
        delivered: BTreeMap<AssetId, Energy>,
        phases: BTreeMap<AssetId, PhaseState>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                site: site(),
                state: SiteState::default(),
                limits: GridLimits::default(),
                overrides: BTreeMap::new(),
                previous: BTreeMap::new(),
                delivered: BTreeMap::new(),
                phases: BTreeMap::new(),
            }
        }

        fn measure(mut self, id: &str, kw: f64) -> Self {
            self.state.assets.insert(
                AssetId::new(id).unwrap(),
                Measurement::power(NOW, Power::from_kw(kw)),
            );
            self
        }

        fn grid(mut self, kw: f64) -> Self {
            self.state.grid = Some(Measurement::power(NOW, Power::from_kw(kw)));
            self
        }

        fn tick(&self, arbiter: &Arbiter, plan: Option<&Plan>) -> Decision {
            arbiter.tick(Tick {
                now: NOW,
                site: &self.site,
                state: &self.state,
                limits: &self.limits,
                plan,
                overrides: &self.overrides,
                previous: &self.previous,
                delivered: &self.delivered,
                phases: &self.phases,
            })
        }
    }

    fn commanded(d: &Decision, id: &str) -> Power {
        d.commanded[&AssetId::new(id).unwrap()]
    }

    #[test]
    fn with_no_plan_the_store_covers_the_house_instead_of_the_grid() {
        // G3 in one test. A box whose planner is gone still has to behave like
        // every home battery ever sold: cover the house from the store rather
        // than buy the evening peak with a full battery sitting behind the
        // meter. Before this the fallback absorbed surplus and nothing else, so
        // an offline box imported at the retail price all evening.
        let f = Fixture::new().grid(0.5).measure("haushalt", 0.5);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert_eq!(commanded(&d, "battery"), Power::from_kw(-0.5));
        // A one-way charge point is not a source and is not asked to be one.
        assert_eq!(commanded(&d, "wallbox"), Power::ZERO);
        assert!(
            d.setpoints
                .iter()
                .any(|s| matches!(s.reason, Reason::Realtime(RealtimeCause::SelfConsumption)))
        );
    }

    #[test]
    fn with_no_plan_and_a_balanced_connection_nothing_is_asked_to_run() {
        let f = Fixture::new().grid(0.0);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert_eq!(commanded(&d, "wallbox"), Power::ZERO);
        assert_eq!(commanded(&d, "battery"), Power::ZERO);
        assert!(
            d.setpoints
                .iter()
                .filter(|s| s.asset != AssetId::new("pv").unwrap())
                .all(|s| matches!(s.reason, Reason::Fallback(FallbackCause::NoPlan)))
        );
    }

    #[test]
    fn an_inverter_with_nothing_to_say_to_it_runs_at_its_maximum_power_point() {
        // Every load answers a request for *more* power, so an absent
        // instruction reads as zero. An inverter answers a request for *less*,
        // and zero is the one value that means "stop" — so reading its silence
        // the same way tells the roof to produce nothing on every tick of every
        // day the plan does not curtail.
        let f = Fixture::new().grid(0.0);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert_eq!(
            commanded(&d, "pv"),
            Power::from_kw(-8.0),
            "an unconstrained inverter is left at its own rating"
        );
        assert!(
            d.setpoints
                .iter()
                .any(|s| s.asset == AssetId::new("pv").unwrap()
                    && s.command == Command::ProductionCeiling(Power::from_kw(8.0))
                    && matches!(s.reason, Reason::Realtime(RealtimeCause::MaximumPowerPoint))),
            "and told so in a ceiling it can explain: {:?}",
            d.setpoints
        );
    }

    #[test]
    fn a_feed_in_cap_is_what_narrows_the_inverter_and_it_says_so() {
        let mut f = Fixture::new().grid(0.0);
        f.limits.feed_in_ceiling = Some(Power::from_kw(5.88));
        f.limits.feed_in_rule = Some(GuardRule::Para9Cap);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert!(
            commanded(&d, "pv") > Power::from_kw(-8.0),
            "the cap should bite, got {}",
            commanded(&d, "pv")
        );
        assert!(
            d.setpoints
                .iter()
                .any(|s| s.asset == AssetId::new("pv").unwrap()
                    && matches!(
                        s.reason,
                        Reason::Guard {
                            rule: GuardRule::Para9Cap,
                            ..
                        }
                    )),
            "and the household is told which rule did it: {:?}",
            d.setpoints
        );
    }

    #[test]
    fn a_battery_at_its_backup_reserve_is_not_a_source() {
        // The reserve is a promise, and the fallback must not spend it. The
        // guard would stop it anyway — it must, because it is the guard — but a
        // claim that ignored the reserve would starve whatever else could
        // actually have covered the import.
        let mut f = Fixture::new().grid(2.0).measure("battery", 0.0);
        if let Some(m) = f.state.assets.get_mut(&AssetId::new("battery").unwrap()) {
            m.soc = Some(Soc::new(0.10).unwrap());
        }
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert_eq!(commanded(&d, "battery"), Power::ZERO);
    }

    #[test]
    fn the_arbiter_tracks_the_plans_energy_and_not_its_setpoint() {
        // "Put 1 kWh into the battery this quarter hour" at 4 kW. Nine minutes
        // in, only 0,2 kWh has gone in — a cloud, a slow inverter, a guard that
        // held it down earlier. Six minutes are left, so 0,8 kWh needs 8 kW, and
        // the plan's envelope is what stops it asking for more than the store
        // can take.
        let now = NOW;
        let slot = Slot::containing(now);
        let mut f = Fixture::new().grid(-4.0);
        f.delivered
            .insert(AssetId::new("battery").unwrap(), Energy::from_kwh(0.2));
        let plan = Plan {
            slots: vec![SlotPlan {
                flexibility_eur_per_kwh: None,
                slot,
                targets: vec![AssetTarget {
                    marginal_eur_per_kwh: None,
                    asset: AssetId::new("battery").unwrap(),
                    power: Power::from_kw(4.0),
                    envelope: Envelope::new(Power::ZERO, Power::from_kw(5.0)),
                }],
                marginal_eur_per_kwh: None,
            }],
            ..Plan::empty(Horizon::new(slot.start(), 1), now)
        };
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), Some(&plan));
        // NOW is 17:05, so the slot ends at 17:15: ten minutes left, 0,8 kWh to
        // go, 4,8 kW.
        assert!(
            (commanded(&d, "battery").kw() - 4.8).abs() < 1e-6,
            "got {}",
            commanded(&d, "battery")
        );
    }

    #[test]
    fn a_plan_already_satisfied_this_slot_asks_for_nothing_more() {
        let slot = Slot::containing(NOW);
        let mut f = Fixture::new().grid(-4.0);
        f.delivered
            .insert(AssetId::new("battery").unwrap(), Energy::from_kwh(1.5));
        let plan = Plan {
            slots: vec![SlotPlan {
                flexibility_eur_per_kwh: None,
                slot,
                targets: vec![AssetTarget {
                    marginal_eur_per_kwh: None,
                    asset: AssetId::new("battery").unwrap(),
                    power: Power::from_kw(4.0),
                    envelope: Envelope::new(Power::ZERO, Power::from_kw(5.0)),
                }],
                marginal_eur_per_kwh: None,
            }],
            ..Plan::empty(Horizon::new(slot.start(), 1), NOW)
        };
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), Some(&plan));
        // Over-delivered, and the envelope's floor of zero keeps catching up
        // from turning into a discharge.
        assert_eq!(commanded(&d, "battery"), Power::ZERO);
    }

    #[test]
    fn surplus_is_absorbed_rather_than_exported() {
        // 8 kW of production, 0,5 kW household → 7,5 kW leaving the house.
        let f = Fixture::new()
            .grid(-7.5)
            .measure("pv", -8.0)
            .measure("haushalt", 0.5);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        let absorbed = commanded(&d, "wallbox") + commanded(&d, "battery");
        assert!(
            (absorbed.kw() - 7.5).abs() < 1e-6,
            "expected the surplus to be taken up, got {absorbed}"
        );
        assert!(
            d.setpoints
                .iter()
                .any(|s| matches!(s.reason, Reason::Realtime(RealtimeCause::SurplusTracking)))
        );
    }

    #[test]
    fn a_plan_is_followed_when_it_is_fresh() {
        let f = Fixture::new().grid(0.5).measure("haushalt", 0.5);
        let horizon = Horizon::new(NOW, 4);
        let plan = Plan {
            slots: horizon
                .slots()
                .map(|slot| SlotPlan {
                    flexibility_eur_per_kwh: None,
                    slot,
                    targets: vec![AssetTarget::fixed(
                        AssetId::new("battery").unwrap(),
                        Power::from_kw(3.0),
                    )],
                    marginal_eur_per_kwh: Some(0.25),
                })
                .collect(),
            ..Plan::empty(horizon, NOW)
        };
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), Some(&plan));
        assert_eq!(commanded(&d, "battery"), Power::from_kw(3.0));
        assert!(
            d.setpoints
                .iter()
                .any(|s| matches!(s.reason, Reason::Plan { .. }))
        );
    }

    #[test]
    fn a_stale_plan_is_ignored_and_says_so() {
        let f = Fixture::new().grid(0.5);
        let horizon = Horizon::new(NOW, 4);
        let plan = Plan {
            slots: horizon
                .slots()
                .map(|slot| SlotPlan {
                    flexibility_eur_per_kwh: None,
                    slot,
                    targets: vec![AssetTarget::fixed(
                        AssetId::new("battery").unwrap(),
                        Power::from_kw(3.0),
                    )],
                    marginal_eur_per_kwh: None,
                })
                .collect(),
            ..Plan::empty(horizon, NOW - Duration::hours(2))
        };
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), Some(&plan));
        // The stale plan wanted 3 kW *into* the battery; what happens instead is
        // the fallback covering the 0,5 kW the house is importing.
        assert_eq!(commanded(&d, "battery"), Power::from_kw(-0.5));
        assert!(
            d.setpoints
                .iter()
                .any(|s| matches!(s.reason, Reason::Fallback(FallbackCause::PlanStale))),
            "the charge point has nothing to fall back to and should say why"
        );
    }

    #[test]
    fn a_grid_limit_beats_the_plan_and_says_which_rule_did_it() {
        // [A1 4.6 S. 3] in one test: the plan wants 5 kW into the battery, the
        // network operator says 2 kW for everything, and the operator wins.
        let mut f = Fixture::new().grid(0.5).measure("haushalt", 0.5);
        f.limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(2.0)),
            steuve_since: Some(NOW - Duration::minutes(3)),
            ..GridLimits::default()
        };
        let horizon = Horizon::new(NOW, 2);
        let plan = Plan {
            slots: horizon
                .slots()
                .map(|slot| SlotPlan {
                    flexibility_eur_per_kwh: None,
                    slot,
                    targets: vec![AssetTarget::fixed(
                        AssetId::new("battery").unwrap(),
                        Power::from_kw(5.0),
                    )],
                    marginal_eur_per_kwh: Some(0.4),
                })
                .collect(),
            ..Plan::empty(horizon, NOW)
        };
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), Some(&plan));
        assert!(commanded(&d, "battery") <= Power::from_kw(2.0));
        let sp = d
            .setpoints
            .iter()
            .find(|s| s.asset.as_str() == "battery")
            .expect("the battery was commanded");
        assert_eq!(sp.authority(), Authority::Guard);
        assert!(
            matches!(
                sp.reason,
                Reason::Guard {
                    rule: GuardRule::Lpc,
                    since: Some(_)
                }
            ),
            "got {:?}",
            sp.reason
        );
    }

    #[test]
    fn a_user_pause_beats_the_plan_but_not_the_guard() {
        let mut f = Fixture::new().grid(-7.5).measure("pv", -8.0);
        f.overrides
            .insert(AssetId::new("wallbox").unwrap(), UserOverride::Pause);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert_eq!(commanded(&d, "wallbox"), Power::ZERO);
        let sp = d
            .setpoints
            .iter()
            .find(|s| s.asset.as_str() == "wallbox")
            .unwrap();
        assert_eq!(sp.reason, Reason::User(UserOverride::Pause));
    }

    #[test]
    fn the_deadband_keeps_a_device_from_chasing_noise() {
        let mut f = Fixture::new()
            .grid(-3.02)
            .measure("pv", -3.5)
            .measure("haushalt", 0.5);
        f.previous
            .insert(AssetId::new("battery").unwrap(), Power::from_kw(3.0));
        f.previous
            .insert(AssetId::new("wallbox").unwrap(), Power::ZERO);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        // 20 W of movement is below the 50 W deadband, so nothing is sent.
        assert!(
            !d.setpoints.iter().any(|s| s.asset.as_str() == "battery"),
            "sent {:?}",
            d.setpoints
        );
    }

    #[test]
    fn ramping_never_carries_a_value_back_over_a_grid_limit() {
        let mut f = Fixture::new().grid(0.5);
        f.previous
            .insert(AssetId::new("battery").unwrap(), Power::from_kw(5.0));
        f.limits = GridLimits {
            steuve_ceiling: Some(Power::from_kw(1.0)),
            ..GridLimits::default()
        };
        let arbiter = Arbiter::new(ArbiterConfig {
            ramp_per_tick: Some(Power::from_kw(0.5)),
            ..ArbiterConfig::default()
        });
        let d = f.tick(&arbiter, None);
        assert!(
            commanded(&d, "battery") <= Power::from_kw(1.0),
            "a limit takes effect now, not one ramp step from now"
        );
    }

    #[test]
    fn a_ramped_value_says_it_is_ramping() {
        // "Why is the battery at 3,5 kW when the plan says 5?" has an answer,
        // and the enum had a variant for it that nothing ever produced.
        let mut f = Fixture::new().grid(-6.0).measure("battery", 3.0);
        f.previous
            .insert(AssetId::new("battery").unwrap(), Power::from_kw(3.0));
        let arbiter = Arbiter::new(ArbiterConfig {
            ramp_per_tick: Some(Power::from_kw(0.5)),
            ..ArbiterConfig::default()
        });
        let d = f.tick(&arbiter, None);
        let sp = d
            .setpoints
            .iter()
            .find(|s| s.asset.as_str() == "battery")
            .expect("the battery was commanded");
        assert_eq!(sp.reason, Reason::Realtime(RealtimeCause::RampLimit));
        assert_eq!(commanded(&d, "battery"), Power::from_kw(3.5));
    }

    #[test]
    fn a_charge_point_is_commanded_in_amperes_and_a_heat_pump_in_watts() {
        // The gap this closes: emitting active power to everything leaves a
        // wallbox and an SG Ready heat pump undriveable.
        let f = Fixture::new().grid(-7.5).measure("pv", -8.0);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        let wallbox = d
            .setpoints
            .iter()
            .find(|s| s.asset.as_str() == "wallbox")
            .expect("the wallbox was commanded");
        assert!(
            matches!(wallbox.command, Command::ChargingCurrent(_)),
            "got {}",
            wallbox.command
        );
        let battery = d
            .setpoints
            .iter()
            .find(|s| s.asset.as_str() == "battery")
            .unwrap();
        assert!(
            matches!(battery.command, Command::ActivePower(_)),
            "got {}",
            battery.command
        );
    }

    #[test]
    fn an_uncontrollable_load_is_never_commanded() {
        let f = Fixture::new().grid(0.5).measure("haushalt", 0.5);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert!(
            !d.setpoints.iter().any(|s| s.asset.as_str() == "haushalt"),
            "the household base load is not ours to command"
        );
    }

    #[test]
    fn the_balance_residual_exposes_a_missing_meter() {
        // The grid meter says 4 kW is coming in; the assets account for 0,5 kW.
        let f = Fixture::new().grid(4.0).measure("haushalt", 0.5);
        let d = f.tick(&Arbiter::new(ArbiterConfig::default()), None);
        assert!((d.balance_residual.unwrap().kw() - 3.5).abs() < 1e-9);
    }

    /// The theorem the whole design exists to make true.
    ///
    /// Deliberately one function: the setup, the randomisation and the four
    /// properties belong together, and splitting them would make a failure
    /// harder to read rather than easier.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn no_input_can_make_the_arbiter_exceed_a_grid_limit() {
        let site = site();
        let arbiter = Arbiter::new(ArbiterConfig::default());
        let steuve = hems_grid::classify_at(&site.assets, NOW);
        let mut rng: u64 = 0xA11CE;
        let mut next = |modulo: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % modulo
        };

        for round in 0..1000 {
            let ceiling = Power::from_kw(next(12) as f64 * 0.7);
            let pv = -Power::from_kw(next(10) as f64);
            let household = Power::from_kw(next(4) as f64 * 0.5);
            let wallbox_now = Power::from_kw(next(12) as f64);
            let battery_now = Power::from_kw(next(6) as f64) - Power::from_kw(2.0);
            let grid = household + wallbox_now + battery_now + pv;

            let mut state = SiteState {
                grid: Some(Measurement::power(NOW, grid)),
                ..SiteState::default()
            };
            for (id, p) in [
                ("pv", pv),
                ("haushalt", household),
                ("wallbox", wallbox_now),
                ("battery", battery_now),
            ] {
                state
                    .assets
                    .insert(AssetId::new(id).unwrap(), Measurement::power(NOW, p));
            }

            let limits = GridLimits {
                steuve_ceiling: Some(ceiling),
                steuve_since: Some(NOW),
                in_failsafe: next(5) == 0,
                ..GridLimits::default()
            };

            // A plan that wants far more than it can have.
            let horizon = Horizon::new(NOW, 1);
            let plan = Plan {
                slots: vec![SlotPlan {
                    flexibility_eur_per_kwh: None,
                    slot: horizon.first,
                    targets: vec![
                        AssetTarget::fixed(AssetId::new("wallbox").unwrap(), Power::from_kw(11.0)),
                        AssetTarget::fixed(AssetId::new("battery").unwrap(), Power::from_kw(5.0)),
                    ],
                    marginal_eur_per_kwh: Some(f64::from(u32::try_from(next(50)).unwrap()) / 100.0),
                }],
                ..Plan::empty(horizon, NOW)
            };

            let mut overrides = BTreeMap::new();
            if next(4) == 0 {
                overrides.insert(AssetId::new("wallbox").unwrap(), UserOverride::Boost);
            }

            let d = arbiter.tick(Tick {
                now: NOW,
                site: &site,
                state: &state,
                limits: &limits,
                plan: Some(&plan),
                overrides: &overrides,
                previous: &BTreeMap::new(),
                delivered: &BTreeMap::new(),
                phases: &BTreeMap::new(),
            });

            // 1. Every decided value is inside the interval the guard left
            //    open. Checked on `commanded` rather than on the setpoints,
            //    because a setpoint may be amperes or a contact state — and a
            //    check that skipped those would silently stop covering the two
            //    device classes most likely to be curtailed.
            for (id, power) in &d.commanded {
                let envelope = d.verdict.envelope(id);
                assert!(
                    envelope.contains(*power) || envelope.is_empty(),
                    "round {round}: {id} commanded {power} outside {envelope}"
                );
            }

            // 2. The netzwirksamer Leistungsbezug of the commanded values stays
            //    inside the network operator's ceiling — the § 14a promise.
            let steuve_ids: Vec<&AssetId> = steuve.iter().flat_map(|s| s.assets.iter()).collect();
            let steuve_total: Power = steuve_ids
                .iter()
                .map(|id| {
                    d.commanded
                        .get(*id)
                        .copied()
                        .unwrap_or(Power::ZERO)
                        .inflow()
                })
                .sum();
            // The same definition the guard uses: only generation that this
            // decision does not itself command counts as surplus.
            let netzwirksam =
                hems_grid::netzwirksamer_leistungsbezug(steuve_total, household, pv.outflow());
            assert!(
                netzwirksam <= ceiling + Power::new(1e-6),
                "round {round}: {netzwirksam} netzwirksam over a ceiling of {ceiling}"
            );

            // 3. Anything the guard is holding is explained by a guard reason,
            //    and nothing below the guard can claim that authority.
            for sp in &d.setpoints {
                if let Reason::Guard { .. } = sp.reason {
                    assert_eq!(sp.authority(), Authority::Guard);
                }
            }

            // 4. The minimum of [A1 4.5] is a property of the site, not of the
            //    moment: it never changes because a limit arrived.
            assert_eq!(
                d.verdict.minimum_power,
                minimum_power(&steuve, hems_grid::ControlMode::Ems),
                "round {round}"
            );
        }
    }
}
