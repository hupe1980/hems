//! What the optimiser decided, in a form the arbiter can follow.
//!
//! A plan lives in the core rather than in `hems-optimizer` because both the
//! producer and the consumer need it, and the consumer — a one-second control
//! loop — must not have to link a solver.
//!
//! # Intent, not literal setpoints
//!
//! An [`AssetTarget`] says two things, and they are not the same thing. Its
//! `power` is the average the plan intends, and through [`AssetTarget::energy`]
//! it is the quarter hour's **energy commitment**: "put 2,4 kWh into the battery
//! during this slot". Its `envelope` is the freedom the plan gives away — the
//! direction it committed to, out to what the hardware can do.
//!
//! The arbiter follows the *energy*, spending the envelope to deliver it. "Put
//! 2,4 kWh in during this slot" survives a cloud passing over the roof at 12:19,
//! a network operator's reduction that costs three minutes of it, and a driver
//! that took a while to answer; "charge at 9,6 kW" survives none of them. The
//! power is what the household is shown, and where the arbiter starts.

use time::OffsetDateTime;

use crate::envelope::Envelope;
use crate::ids::{AssetId, PlanId};
use crate::slot::{Horizon, SLOT, Slot};
use crate::units::{Energy, Power};

/// What one asset should do in one slot.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetTarget {
    /// The asset.
    pub asset: AssetId,
    /// The average power the plan intends, load convention.
    ///
    /// Through [`AssetTarget::energy`] this is the slot's energy commitment,
    /// which is what the arbiter actually tracks.
    pub power: Power,
    /// The range the arbiter may move inside without breaking the plan.
    ///
    /// This is the S2 `PEBC` power envelope: the planner says how much freedom
    /// it is giving away, rather than the arbiter guessing. It is normally the
    /// *direction* the plan committed to out to the device's own rating — wide
    /// enough that the energy can still be delivered after part of the slot has
    /// been lost, narrow enough that catching up can never turn a charge into a
    /// discharge.
    pub envelope: Envelope,
    /// What a kilowatt-hour delivered to **this asset** in this slot is worth to
    /// the plan, €/kWh.
    ///
    /// The shadow price of the asset's own state equation — what the plan loses
    /// if the device is held back by a kilowatt-hour here, with every
    /// consequence the horizon can see already in it: the departure time, the
    /// comfort band, the wear, the terminal value, the reduction that will bind
    /// at teatime. `None` where the planner could not produce one.
    ///
    /// # Why per asset and not per slot
    ///
    /// This is what weights the guard's allocation when a shared limit binds,
    /// and a weight that is the same for every device does not weight anything:
    /// the weighted max-min allocator degenerates to plain max-min and "a
    /// reduction takes power from where it is worth least" becomes a sentence in
    /// a document. Under a § 14a ceiling with a car three hours from its
    /// departure and a heat pump in a house that is already warm, the answer is
    /// obvious to a household and was invisible to the guard.
    #[cfg_attr(feature = "serde", serde(default))]
    pub marginal_eur_per_kwh: Option<f64>,
}

impl AssetTarget {
    /// A target with no freedom around it.
    #[must_use]
    pub fn fixed(asset: AssetId, power: Power) -> Self {
        Self {
            asset,
            power,
            envelope: Envelope::exactly(power),
            marginal_eur_per_kwh: None,
        }
    }

    /// The value of a kilowatt-hour to this asset, falling back to the slot's.
    #[must_use]
    pub fn value_or(&self, slot_marginal: Option<f64>) -> Option<f64> {
        self.marginal_eur_per_kwh.or(slot_marginal)
    }

    /// The energy the plan intends to move in one slot — the quantity the
    /// arbiter actually tracks.
    #[must_use]
    pub fn energy(&self) -> Energy {
        self.power.over(SLOT)
    }
}

/// One slot of a plan.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SlotPlan {
    /// Which quarter hour.
    pub slot: Slot,
    /// What each controllable asset should do.
    pub targets: Vec<AssetTarget>,
    /// The marginal value of a kilowatt-hour anywhere in the house in this
    /// slot, €/kWh.
    ///
    /// The **shadow price of the energy balance** where the planner could
    /// produce one, which above a binding limit is not the tariff at all: a slot
    /// in which a § 14a ceiling is what binds prices energy at what the next
    /// best use of it was worth. Where the dual pass did not run it falls back
    /// to the price the plan faces, which still ranks slots correctly.
    ///
    /// For deciding **which device** keeps its power during a reduction, use
    /// [`AssetTarget::marginal_eur_per_kwh`]: this one is the same number for
    /// every device in the slot and therefore ranks none of them.
    #[cfg_attr(feature = "serde", serde(default))]
    pub marginal_eur_per_kwh: Option<f64>,
    /// What a kilowatt-hour of **relief from the § 14a ceiling** would be worth
    /// to this household in this slot, €/kWh.
    ///
    /// `None` where no ceiling is in force, and `Some(0.0)` where one is but is
    /// not what binds — which is most of a reduction on most households, and is
    /// worth knowing on its own: a network operator's limit that costs nothing
    /// is a limit nobody should be compensated for.
    ///
    /// This is what a household's flexibility is *actually* worth, computed from
    /// its own plan rather than assumed from a nameplate, and it is what a § 41e
    /// Aggregatorvertrag offer or an OpenADR bid should be priced from.
    #[cfg_attr(feature = "serde", serde(default))]
    pub flexibility_eur_per_kwh: Option<f64>,
}

impl SlotPlan {
    /// The target for one asset, if the plan has one.
    #[must_use]
    pub fn target(&self, asset: &AssetId) -> Option<&AssetTarget> {
        self.targets.iter().find(|t| &t.asset == asset)
    }
}

/// What following a plan costs, term by term.
///
/// The objective the planner minimises prices battery wear, curtailed
/// production, time outside the comfort band and **service it decided not to
/// deliver** alongside the energy bill, all in euros, so that the terms can
/// honestly be added up. Reporting only the energy bill would put the saving
/// back exactly where § 9.2 of the concept says it must not be: a number that
/// credits the optimiser for a cycle it paid for in battery life.
///
/// So the same terms are reported for the plan **and** for the baseline it is
/// measured against. A baseline with no battery pays no wear, which is
/// precisely the point — the comparison is only fair once both sides carry
/// every cost they incur.
///
/// # Every term of the objective is a term of the report
///
/// The invariant this type exists to hold. A term the plan may *spend* and is
/// not *charged* for is a discount the optimiser helps itself to: leave the car
/// two kilowatt-hours short, let the tank run cold before the morning shower,
/// and the electricity bill falls while nothing else on the report moves.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CostBreakdown {
    /// Grid energy: what was imported, less what feeding in earned.
    pub energy_eur: f64,
    /// Battery life spent moving energy through the store.
    pub wear_eur: f64,
    /// Production that could be neither used nor exported.
    pub curtailment_eur: f64,
    /// Kelvin-hours outside the comfort band, at the household's own price.
    pub discomfort_eur: f64,
    /// Energy the period **borrowed** from the stores, valued at what it would
    /// cost to put back.
    ///
    /// A plan that starts the day with a full battery and ends with an empty one
    /// has not saved what the meter says it saved; it has spent something it
    /// started with, and a comparison over one day would otherwise credit it for
    /// exactly that. The objective already values what it leaves behind
    /// (`terminal_value_factor`), so this is the same number on the other side
    /// of the ledger and the plan and the period that judges it agree.
    ///
    /// **Never negative, on purpose.** The mirror case — ending with *more* in
    /// the stores than the period began with — is a real credit and it is
    /// deliberately not taken, because the baseline the saving is measured
    /// against has no battery to store anything in and so can never earn it. The
    /// asymmetry runs the safe way: a saving figure may understate itself, and it
    /// may not flatter itself. Over a period that starts and ends in the same
    /// state, which is what a week or a month does, it is zero either way.
    #[cfg_attr(feature = "serde", serde(default))]
    pub stored_eur: f64,
    /// Charge put into the car **beyond what the household asked for**, valued
    /// at what it would cost to buy — a credit, and zero on any period that
    /// delivered exactly the service it was asked to.
    ///
    /// # Why the excess and not the whole charge
    ///
    /// Energy delivered *up to* the target is the service the household bought;
    /// it is already in the bill, and crediting it again would report a day with
    /// a €21 electricity bill as costing €12. Energy delivered *past* it is
    /// something nobody asked for, and that is the case this entry exists for.
    ///
    /// # Why it is not [`CostBreakdown::stored_eur`]
    ///
    /// `stored_eur` is deliberately one-sided: the baseline has no battery and no
    /// managed tank, so it can never earn the credit and taking it would be a
    /// saving flattering itself. **Both households have the same car.** A
    /// kilowatt-hour a controller pushed into it past the target is a
    /// kilowatt-hour nobody buys later, and refusing to credit it measures a
    /// manager that absorbed a sunny afternoon into the car against one that
    /// exported the same energy at the feed-in tariff — and loses. That is not a
    /// modelling nicety: it is how a switchable wallbox came to *appear* to be
    /// rewarded for charging past the household's own Ladelimit.
    ///
    /// The other direction — a car left short — is
    /// [`CostBreakdown::unserved_eur`]'s, at the household's own price for it.
    #[cfg_attr(feature = "serde", serde(default))]
    pub vehicle_eur: f64,
    /// Service the household asked for and did not get, at its own price: the
    /// kilowatt-hours the car was promised and did not receive by its
    /// departure, and the hot water drawn from a tank that had none left.
    ///
    /// The objective prices both — that is what makes the charging deadline and
    /// the morning shower *soft* rather than infeasible — so a report that left
    /// them out would reward giving up: a plan abandoning the last two
    /// kilowatt-hours of a charging session buys two kilowatt-hours less
    /// electricity. Energy nobody used is energy nobody bought.
    ///
    /// It is charged on **both** sides. A thermostat and an unmanaged wallbox
    /// can fall short too — a car plugged in an hour before it leaves is short
    /// whatever anybody does — and a baseline that never pays this while the
    /// plan does would be the same mistake pointing the other way.
    #[cfg_attr(feature = "serde", serde(default))]
    pub unserved_eur: f64,
    /// What a § 42c energy-sharing community took off the bill — a **credit**,
    /// so it is negative or zero.
    ///
    /// Kept as an entry of its own rather than folded into
    /// [`CostBreakdown::energy_eur`] because it answers a question the energy
    /// line cannot: *what did belonging to the community buy?* A household
    /// deciding whether to join one, or an operator setting the Aufteilungs-
    /// schlüssel, needs the number on its own — and it belongs in
    /// [`CostBreakdown::billed_eur`] as well as in the total, because unlike
    /// wear and comfort it really is on the invoice.
    ///
    /// The **baseline is in the same community**, so the saving a report shows
    /// is the value of *shifting load into the community's generation*, not the
    /// value of the membership. Crediting the plan with the membership would be
    /// the same asymmetry as measuring it against a household that ignored the
    /// network operator.
    #[cfg_attr(feature = "serde", serde(default))]
    pub sharing_eur: f64,
}

impl CostBreakdown {
    /// A cost with nothing but an energy bill — a household with no store, no
    /// curtailment and no heating to trade off.
    #[must_use]
    pub fn energy_only(energy_eur: f64) -> Self {
        Self {
            energy_eur,
            ..Self::default()
        }
    }

    /// Everything the plan costs, in euros.
    #[must_use]
    pub fn total(&self) -> f64 {
        self.energy_eur
            + self.wear_eur
            + self.curtailment_eur
            + self.discomfort_eur
            + self.stored_eur
            + self.vehicle_eur
            + self.unserved_eur
            + self.sharing_eur
    }

    /// Everything except the terms that are preferences rather than bills.
    ///
    /// What appears on the invoice. Shown beside [`CostBreakdown::total`] so a
    /// household can see both what it paid and what it gave up.
    ///
    /// [`CostBreakdown::stored_eur`] is left out: energy sitting in a battery at
    /// midnight is real, and it is not on anybody's bill.
    #[must_use]
    pub fn billed_eur(&self) -> f64 {
        self.energy_eur + self.sharing_eur
    }
}

/// A finished plan.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Plan {
    /// Identity, so a setpoint can point back at the decision that made it.
    pub id: PlanId,
    /// When it was produced.
    #[cfg_attr(feature = "serde", serde(with = "time::serde::rfc3339"))]
    pub created_at: OffsetDateTime,
    /// The slots it covers.
    pub horizon: Horizon,
    /// One entry per slot of `horizon`, in order.
    pub slots: Vec<SlotPlan>,
    /// What following it is expected to cost over the horizon, term by term.
    #[cfg_attr(feature = "serde", serde(default))]
    pub expected_cost: Option<CostBreakdown>,
    /// What the same horizon costs without any optimisation — the baseline the
    /// saving is measured against, priced with the same four terms.
    #[cfg_attr(feature = "serde", serde(default))]
    pub baseline_cost: Option<CostBreakdown>,
}

impl Plan {
    /// An empty plan over a horizon.
    #[must_use]
    pub fn empty(horizon: Horizon, created_at: OffsetDateTime) -> Self {
        Self {
            id: PlanId::new(),
            created_at,
            horizon,
            slots: Vec::new(),
            expected_cost: None,
            baseline_cost: None,
        }
    }

    /// The plan for the slot containing `instant`, if the horizon covers it.
    #[must_use]
    pub fn slot_at(&self, instant: OffsetDateTime) -> Option<&SlotPlan> {
        let slot = Slot::containing(instant);
        self.slots.iter().find(|s| s.slot == slot)
    }

    /// How old the plan is at `now`.
    #[must_use]
    pub fn age(&self, now: OffsetDateTime) -> time::Duration {
        now - self.created_at
    }

    /// Whether the plan is too old to follow.
    ///
    /// A stale plan is worse than none: it was computed against prices,
    /// forecasts and a grid situation that have moved on, and following it looks
    /// like deliberate behaviour while being nothing of the kind.
    #[must_use]
    pub fn is_stale(&self, now: OffsetDateTime, max_age: time::Duration) -> bool {
        self.age(now) > max_age
    }

    /// The saving the plan expects against its own baseline, in euros.
    ///
    /// Every term counts on both sides. Comparing energy bills alone credits the
    /// optimiser for a battery cycle it has already paid for in cell life and
    /// for a degree of cold it decided to accept.
    #[must_use]
    pub fn expected_saving_eur(&self) -> Option<f64> {
        Some(self.baseline_cost?.total() - self.expected_cost?.total())
    }

    /// The saving on the electricity bill alone, ignoring wear and comfort.
    ///
    /// The number a household recognises from its invoice. It is deliberately
    /// *not* the headline: it is always the flattering one.
    #[must_use]
    pub fn expected_bill_saving_eur(&self) -> Option<f64> {
        Some(self.baseline_cost?.billed_eur() - self.expected_cost?.billed_eur())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-05-01 10:00:00 UTC);

    fn plan() -> Plan {
        let horizon = Horizon::new(T0, 4);
        let asset = AssetId::new("battery").unwrap();
        Plan {
            slots: horizon
                .slots()
                .map(|slot| SlotPlan {
                    flexibility_eur_per_kwh: None,
                    slot,
                    targets: vec![AssetTarget::fixed(asset.clone(), Power::from_kw(4.0))],
                    marginal_eur_per_kwh: Some(0.28),
                })
                .collect(),
            expected_cost: Some(CostBreakdown {
                energy_eur: 2.8,
                wear_eur: 0.2,
                ..CostBreakdown::default()
            }),
            baseline_cost: Some(CostBreakdown::energy_only(4.2)),
            ..Plan::empty(horizon, T0)
        }
    }

    #[test]
    fn a_plan_finds_the_slot_for_an_instant() {
        let p = plan();
        let s = p.slot_at(T0 + time::Duration::minutes(20)).unwrap();
        assert_eq!(s.slot, Slot::containing(T0 + time::Duration::minutes(15)));
        assert!(p.slot_at(T0 + time::Duration::hours(5)).is_none());
    }

    #[test]
    fn a_target_converts_power_into_the_energy_the_arbiter_follows() {
        let t = AssetTarget::fixed(AssetId::new("battery").unwrap(), Power::from_kw(4.0));
        assert!(
            (t.energy().kwh() - 1.0).abs() < 1e-12,
            "4 kW for 15 minutes is 1 kWh"
        );
    }

    #[test]
    fn staleness_is_the_callers_choice() {
        let p = plan();
        assert!(!p.is_stale(T0 + time::Duration::minutes(5), time::Duration::minutes(10)));
        assert!(p.is_stale(
            T0 + time::Duration::minutes(20),
            time::Duration::minutes(10)
        ));
    }

    #[test]
    fn the_expected_saving_needs_both_numbers() {
        assert!((plan().expected_saving_eur().unwrap() - 1.2).abs() < 1e-9);
        let mut p = plan();
        p.baseline_cost = None;
        assert_eq!(p.expected_saving_eur(), None);
    }

    #[test]
    fn wear_is_counted_against_the_plan_and_not_against_the_baseline() {
        // The whole reason the breakdown exists: comparing bills alone hands the
        // optimiser 20 cents of battery life it actually spent.
        let p = plan();
        assert!((p.expected_bill_saving_eur().unwrap() - 1.4).abs() < 1e-9);
        assert!((p.expected_saving_eur().unwrap() - 1.2).abs() < 1e-9);
        assert!(p.expected_saving_eur() < p.expected_bill_saving_eur());
    }
}
