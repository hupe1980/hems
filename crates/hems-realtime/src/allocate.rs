//! Sharing a limited budget between devices that all want more.
//!
//! A § 14a reduction gives a household one number for everything behind the
//! energy management system (`[A1 4.4.b]`) and leaves the distribution to the
//! household (`[A1 4.5.2 S. 6]`). That freedom is the reason to own an energy
//! manager at all, and it needs an allocation rule that is defensible when the
//! car does not finish charging.
//!
//! hems uses **weighted max-min fair water filling**:
//!
//! 1. everyone who can be served in full is served in full;
//! 2. what remains is poured into the unsatisfied claims at a rate proportional
//!    to their weight, until the budget runs out.
//!
//! The properties this buys — all of them checked in the tests below — are that
//! the total never exceeds the budget, nobody is given more than they asked for,
//! raising the budget never takes anything away from anybody, and a claim that
//! fits is always granted in full. The weights carry priority: the planner
//! passes the marginal value of energy for each device, so during a reduction
//! the kilowatt-hours go where they are worth most, rather than to whichever
//! driver happened to poll first.

use hems_core::prelude::{AssetId, Power};

/// One device's request for a share of the budget.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    /// Which device.
    pub asset: AssetId,
    /// What it would use if nothing were limiting it. Non-negative.
    pub want: Power,
    /// The least it can usefully run at — a charge point cannot go below the
    /// 6 A its standard mandates, so granting it 0,5 kW is the same as granting
    /// it nothing. Non-negative, and never above `want`.
    pub floor: Power,
    /// Relative priority. Any positive number; the planner passes the marginal
    /// value of a kilowatt-hour for this device.
    pub weight: f64,
}

impl Claim {
    /// A claim with no minimum and equal priority.
    #[must_use]
    pub fn new(asset: AssetId, want: Power) -> Self {
        Self {
            asset,
            want,
            floor: Power::ZERO,
            weight: 1.0,
        }
    }

    /// Set the minimum useful power.
    #[must_use]
    pub fn with_floor(mut self, floor: Power) -> Self {
        self.floor = floor;
        self
    }

    /// Set the priority weight.
    #[must_use]
    pub fn with_weight(mut self, weight: f64) -> Self {
        self.weight = weight;
        self
    }

    fn sanitised(&self) -> (Power, Power, f64) {
        let want = if self.want.is_finite() {
            self.want.max(Power::ZERO)
        } else {
            Power::ZERO
        };
        let floor = if self.floor.is_finite() {
            self.floor.max(Power::ZERO).min(want)
        } else {
            Power::ZERO
        };
        let weight = if self.weight.is_finite() && self.weight > 0.0 {
            self.weight
        } else {
            1.0
        };
        (want, floor, weight)
    }
}

/// What one device was granted.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    /// Which device.
    pub asset: AssetId,
    /// What it may use.
    pub power: Power,
    /// `true` when it got less than it asked for.
    pub curtailed: bool,
    /// `true` when it got less than its minimum useful power — the budget could
    /// not cover the floors, so somebody has to be switched off or run below
    /// what it can actually do. Worth surfacing rather than hiding: it is the
    /// case where the household notices the reduction.
    pub below_floor: bool,
}

/// Share `budget` between `claims`.
///
/// The result is in the same order as the input.
#[must_use]
pub fn allocate(budget: Power, claims: &[Claim]) -> Vec<Grant> {
    let budget = if budget.is_finite() {
        budget.max(Power::ZERO)
    } else {
        Power::ZERO
    };
    let prepared: Vec<(Power, Power, f64)> = claims.iter().map(Claim::sanitised).collect();

    let total_want: Power = prepared.iter().map(|(w, _, _)| *w).sum();
    let total_floor: Power = prepared.iter().map(|(_, f, _)| *f).sum();

    // Everything fits: nothing to decide.
    if total_want <= budget {
        return claims
            .iter()
            .zip(&prepared)
            .map(|(claim, (want, _, _))| Grant {
                asset: claim.asset.clone(),
                power: *want,
                curtailed: false,
                below_floor: false,
            })
            .collect();
    }

    // Not even the minimums fit. Scale them down together rather than picking
    // winners: at this point the household is being asked for more than it can
    // give, and every device is equally unable to run properly.
    if total_floor > budget {
        let scale = if total_floor > Power::ZERO {
            budget / total_floor
        } else {
            0.0
        };
        return claims
            .iter()
            .zip(&prepared)
            .map(|(claim, (_, floor, _))| Grant {
                asset: claim.asset.clone(),
                power: *floor * scale,
                curtailed: true,
                below_floor: true,
            })
            .collect();
    }

    // Water filling over the room above each floor.
    let mut remaining = budget - total_floor;
    let mut headroom: Vec<Power> = prepared.iter().map(|(w, f, _)| *w - *f).collect();
    let mut granted: Vec<Power> = prepared.iter().map(|(_, f, _)| *f).collect();
    let mut active: Vec<bool> = headroom.iter().map(|h| *h > Power::ZERO).collect();

    // Each pass either exhausts the budget or satisfies at least one claim, so
    // the loop runs at most once per claim.
    for _ in 0..=claims.len() {
        let weight_sum: f64 = prepared
            .iter()
            .zip(&active)
            .filter(|(_, a)| **a)
            .map(|((_, _, w), _)| *w)
            .sum();
        if weight_sum <= 0.0 || remaining <= Power::ZERO {
            break;
        }
        // The level at which the budget would run out if nobody were satisfied.
        let level = remaining / weight_sum;
        // The first claim to be satisfied at that level, if any.
        let binding = prepared
            .iter()
            .zip(headroom.iter().zip(active.iter()))
            .filter(|(_, (_, a))| **a)
            .map(|((_, _, w), (h, _))| h.get() / w)
            .fold(f64::INFINITY, f64::min);

        if binding >= level.get() {
            // Nobody is satisfied at this level: pour and stop.
            for (i, (_, _, w)) in prepared.iter().enumerate() {
                if active[i] {
                    granted[i] += level * *w;
                }
            }
            break;
        }

        // Pour up to the level that satisfies the tightest claim, then repeat
        // with it removed.
        for (i, (_, _, w)) in prepared.iter().enumerate() {
            if !active[i] {
                continue;
            }
            let share = Power::new(binding * *w);
            granted[i] += share;
            headroom[i] -= share;
            remaining -= share;
            if headroom[i] <= Power::new(1e-9) {
                active[i] = false;
            }
        }
    }

    claims
        .iter()
        .zip(prepared.iter().zip(granted))
        .map(|(claim, ((want, floor, _), power))| Grant {
            asset: claim.asset.clone(),
            power: power.min(*want),
            curtailed: power < *want - Power::new(1e-9),
            below_floor: power < *floor - Power::new(1e-9),
        })
        .collect()
}

/// Share `budget` between claims that cannot run below their floor.
///
/// [`allocate`] treats a floor as "serve this first". That is right for a
/// § 14a reduction, where every device is owed something. It is wrong when the
/// floor means *indivisible*: a charge point below the 6 A of IEC 61851 is not
/// charging slowly, it is not charging, and the kilowatts handed to it are
/// simply lost to the battery that could have used them.
///
/// This variant switches such devices off and shares what they were holding
/// among the rest, repeating until the set is stable. At most one device ends up
/// below its floor, and only when nothing else can use the power either.
///
/// # One at a time
///
/// Shedding **every** below-floor claim in the same pass is wrong in the case
/// this function exists for: two charge points with a 4,14 kW floor sharing 5 kW
/// each come out at 2,5 kW, both below their floor, both switched off — and the
/// answer is that nobody charges, when one of them could have taken all 5 kW.
///
/// So one goes: the lowest weight, then the largest floor — cheapest to give up
/// and hardest to satisfy — with the asset identifier as a deterministic
/// tie-break.
///
/// # Panics
/// Never in practice: the internal invariant is one grant per active claim, and
/// the loop maintains it.
#[must_use]
pub fn allocate_indivisible(budget: Power, claims: &[Claim]) -> Vec<Grant> {
    let mut excluded = vec![false; claims.len()];

    // Each pass switches off exactly one device, so it runs at most once per
    // claim before the set stops changing.
    for _ in 0..=claims.len() {
        let active: Vec<Claim> = claims
            .iter()
            .zip(&excluded)
            .filter(|(_, off)| !**off)
            .map(|(c, _)| c.clone())
            .collect();
        if active.is_empty() {
            break;
        }
        let grants = allocate(budget, &active);

        // The least valuable claim that cannot reach its floor — unless it is
        // the only one left, in which case there is nobody to give it to.
        // Lower weight first, then the larger floor, then the identifier: the
        // device that is cheapest to give up and hardest to satisfy, with a
        // deterministic tie-break so two identical charge points shed in a
        // defined order.
        let worse_than = |a: usize, b: usize| -> bool {
            let (x, y) = (&claims[a], &claims[b]);
            (x.weight, -x.floor.get(), &x.asset) < (y.weight, -y.floor.get(), &y.asset)
        };
        let mut shed: Option<usize> = None;
        if active.len() > 1 {
            let mut cursor = 0;
            for (i, off) in excluded.iter().enumerate() {
                if *off {
                    continue;
                }
                let below = grants[cursor].below_floor;
                cursor += 1;
                if below && claims[i].floor > Power::ZERO && shed.is_none_or(|w| worse_than(i, w)) {
                    shed = Some(i);
                }
            }
        }

        let Some(shed) = shed else {
            return claims
                .iter()
                .zip(&excluded)
                .scan(grants.into_iter(), |grants, (claim, off)| {
                    Some(if *off {
                        Grant {
                            asset: claim.asset.clone(),
                            power: Power::ZERO,
                            curtailed: true,
                            below_floor: false,
                        }
                    } else {
                        grants.next().expect("one grant per active claim")
                    })
                })
                .collect();
        };
        excluded[shed] = true;
    }

    claims
        .iter()
        .map(|c| Grant {
            asset: c.asset.clone(),
            power: Power::ZERO,
            curtailed: true,
            below_floor: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(id: &str, want_kw: f64) -> Claim {
        Claim::new(AssetId::new(id).unwrap(), Power::from_kw(want_kw))
    }

    fn total(grants: &[Grant]) -> Power {
        grants.iter().map(|g| g.power).sum()
    }

    #[test]
    fn a_budget_that_covers_everything_grants_everything() {
        let claims = [claim("wallbox", 11.0), claim("wp", 3.0)];
        let grants = allocate(Power::from_kw(20.0), &claims);
        assert_eq!(grants[0].power, Power::from_kw(11.0));
        assert_eq!(grants[1].power, Power::from_kw(3.0));
        assert!(grants.iter().all(|g| !g.curtailed));
    }

    #[test]
    fn equal_weights_split_a_shortage_evenly() {
        let claims = [claim("a", 10.0), claim("b", 10.0)];
        let grants = allocate(Power::from_kw(6.0), &claims);
        assert!((grants[0].power.kw() - 3.0).abs() < 1e-9);
        assert!((grants[1].power.kw() - 3.0).abs() < 1e-9);
        assert!(grants.iter().all(|g| g.curtailed));
    }

    #[test]
    fn a_small_claim_is_served_in_full_before_a_large_one_is_curtailed() {
        // Max-min fairness: the heat pump wanting 1 kW gets its kilowatt; the
        // wallbox absorbs the whole shortage.
        let claims = [claim("wallbox", 11.0), claim("wp", 1.0)];
        let grants = allocate(Power::from_kw(6.0), &claims);
        assert!(
            (grants[1].power.kw() - 1.0).abs() < 1e-9,
            "small claim served in full"
        );
        assert!((grants[0].power.kw() - 5.0).abs() < 1e-9);
        assert!(total(&grants) <= Power::from_kw(6.0));
    }

    #[test]
    fn weights_move_energy_towards_where_it_is_worth_most() {
        let claims = [
            claim("wallbox", 10.0).with_weight(3.0),
            claim("wp", 10.0).with_weight(1.0),
        ];
        let grants = allocate(Power::from_kw(8.0), &claims);
        assert!((grants[0].power.kw() - 6.0).abs() < 1e-9);
        assert!((grants[1].power.kw() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn floors_are_served_before_anything_else() {
        // A charge point cannot run below 6 A; a heat pump can modulate to zero.
        let claims = [
            claim("wallbox", 11.0).with_floor(Power::from_kw(4.2)),
            claim("wp", 9.0),
        ];
        let grants = allocate(Power::from_kw(6.0), &claims);
        assert!(
            grants[0].power >= Power::from_kw(4.2),
            "the floor is honoured"
        );
        assert!(!grants[0].below_floor);
        assert!(total(&grants) <= Power::from_kw(6.0) + Power::new(1e-9));
    }

    #[test]
    fn a_budget_below_the_floors_is_shared_out_and_flagged() {
        let claims = [
            claim("a", 11.0).with_floor(Power::from_kw(4.2)),
            claim("b", 11.0).with_floor(Power::from_kw(4.2)),
        ];
        let grants = allocate(Power::from_kw(4.2), &claims);
        assert!(
            grants.iter().all(|g| g.below_floor),
            "the household will notice this"
        );
        assert!((total(&grants).kw() - 4.2).abs() < 1e-9);
    }

    #[test]
    fn nothing_is_granted_out_of_an_empty_budget() {
        let claims = [claim("a", 11.0)];
        let grants = allocate(Power::ZERO, &claims);
        assert_eq!(grants[0].power, Power::ZERO);
        assert!(grants[0].curtailed);
    }

    #[test]
    fn no_claims_means_no_grants() {
        assert!(allocate(Power::from_kw(10.0), &[]).is_empty());
    }

    #[test]
    fn nonsense_input_cannot_produce_nonsense_output() {
        let claims = [
            Claim {
                asset: AssetId::new("a").unwrap(),
                want: Power::new_const(f64::NAN),
                floor: Power::from_kw(-3.0),
                weight: -1.0,
            },
            claim("b", 5.0),
        ];
        let grants = allocate(Power::from_kw(4.0), &claims);
        assert!(
            grants
                .iter()
                .all(|g| g.power.is_finite() && g.power >= Power::ZERO)
        );
        assert!(total(&grants) <= Power::from_kw(4.0) + Power::new(1e-9));
    }

    #[test]
    fn an_indivisible_device_that_cannot_run_hands_its_share_to_one_that_can() {
        // 3 kW of surplus, a wallbox that needs 4,14 kW to charge at all, and a
        // battery that will take anything. The battery should get all of it.
        let claims = [
            claim("wallbox", 11.0).with_floor(Power::from_kw(4.14)),
            claim("battery", 5.0),
        ];
        let grants = allocate_indivisible(Power::from_kw(3.0), &claims);
        assert_eq!(
            grants[0].power,
            Power::ZERO,
            "the wallbox cannot charge on 3 kW"
        );
        assert!(
            (grants[1].power.kw() - 3.0).abs() < 1e-9,
            "so the battery takes it"
        );
    }

    #[test]
    fn an_indivisible_device_keeps_its_share_when_it_can_run() {
        let claims = [
            claim("wallbox", 11.0).with_floor(Power::from_kw(4.14)),
            claim("battery", 5.0),
        ];
        let grants = allocate_indivisible(Power::from_kw(9.0), &claims);
        assert!(grants[0].power >= Power::from_kw(4.14));
        assert!((total(&grants).kw() - 9.0).abs() < 1e-9);
    }

    #[test]
    fn two_indivisible_devices_under_one_budget_leave_one_of_them_running() {
        // Two charge points that each need 4,14 kW, and 5 kW to share. Shedding
        // every device that came out below its floor in the same pass switches
        // both off and answers "nobody charges" — while one of them could have
        // taken the whole 5 kW. This is the case that argued for shedding one at
        // a time, and the household would have noticed it as a car that sat
        // there all evening next to a working wallbox.
        let claims = [
            claim("garage", 11.0).with_floor(Power::from_kw(4.14)),
            claim("carport", 11.0).with_floor(Power::from_kw(4.14)),
        ];
        let grants = allocate_indivisible(Power::from_kw(5.0), &claims);
        let running = grants.iter().filter(|g| g.power > Power::ZERO).count();
        assert_eq!(running, 1, "exactly one of them charges: {grants:?}");
        assert!((total(&grants).kw() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn the_least_valuable_indivisible_device_is_the_one_switched_off() {
        // The weight is the planner's marginal value of energy, so a reduction
        // has to take the power away from where it is worth least.
        let claims = [
            claim("cheap", 11.0)
                .with_floor(Power::from_kw(4.14))
                .with_weight(0.5),
            claim("dear", 11.0)
                .with_floor(Power::from_kw(4.14))
                .with_weight(4.0),
        ];
        let grants = allocate_indivisible(Power::from_kw(5.0), &claims);
        assert_eq!(grants[0].power, Power::ZERO);
        assert!(grants[1].power > Power::ZERO);
    }

    #[test]
    fn the_last_device_standing_keeps_what_there_is() {
        let claims = [claim("wallbox", 11.0).with_floor(Power::from_kw(4.14))];
        let grants = allocate_indivisible(Power::from_kw(1.0), &claims);
        assert!(
            (grants[0].power.kw() - 1.0).abs() < 1e-9,
            "nobody else could use it"
        );
    }

    #[test]
    fn indivisible_allocation_still_respects_the_budget() {
        let mut state = 0x0000_BEEF_u64;
        let mut next = |m: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % m
        };
        for round in 0..300 {
            let claims: Vec<Claim> = (0..=next(4))
                .map(|i| {
                    let want = next(12) as f64;
                    Claim::new(AssetId::new(format!("a{i}")).unwrap(), Power::from_kw(want))
                        .with_floor(Power::from_kw(want * next(9) as f64 / 10.0))
                })
                .collect();
            let budget = Power::from_kw(next(15) as f64);
            let grants = allocate_indivisible(budget, &claims);
            assert!(
                total(&grants) <= budget + Power::new(1e-6),
                "round {round}: over budget"
            );
            for (g, c) in grants.iter().zip(&claims) {
                assert!(
                    g.power <= c.want + Power::new(1e-6),
                    "round {round}: over-granted"
                );
            }
        }
    }

    // ── Properties ────────────────────────────────────────────────────────

    /// A deterministic xorshift, so a failure is reproducible from the seed.
    fn rng(state: &mut u64) -> f64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state % 10_000) as f64 / 1000.0
    }

    #[test]
    fn the_total_never_exceeds_the_budget_and_nobody_gets_more_than_they_asked() {
        let mut state = 0x00C0_FFEE;
        for round in 0..500 {
            let n = 1 + (round % 6);
            let claims: Vec<Claim> = (0..n)
                .map(|i| {
                    let want = rng(&mut state);
                    Claim::new(AssetId::new(format!("a{i}")).unwrap(), Power::from_kw(want))
                        .with_floor(Power::from_kw(want * rng(&mut state) / 10.0))
                        .with_weight(0.1 + rng(&mut state))
                })
                .collect();
            let budget = Power::from_kw(rng(&mut state) * 3.0);
            let grants = allocate(budget, &claims);

            assert!(
                total(&grants) <= budget + Power::new(1e-6),
                "round {round}: allocated {} over a budget of {budget}",
                total(&grants)
            );
            for (g, c) in grants.iter().zip(&claims) {
                assert!(
                    g.power <= c.want + Power::new(1e-6),
                    "round {round}: over-granted"
                );
                assert!(g.power >= Power::ZERO, "round {round}: negative grant");
            }
        }
    }

    #[test]
    fn a_larger_budget_never_takes_anything_away() {
        let claims = [
            claim("a", 11.0).with_floor(Power::from_kw(1.4)),
            claim("b", 4.0).with_weight(2.0),
            claim("c", 7.0),
        ];
        let mut previous = vec![Power::ZERO; claims.len()];
        for step in 0..=40 {
            let grants = allocate(Power::from_kw(f64::from(step) * 0.6), &claims);
            for (i, g) in grants.iter().enumerate() {
                assert!(
                    g.power >= previous[i] - Power::new(1e-6),
                    "step {step}, claim {i}: {} dropped below {}",
                    g.power,
                    previous[i]
                );
                previous[i] = g.power;
            }
        }
    }
}
