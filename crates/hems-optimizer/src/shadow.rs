//! What a kilowatt-hour is worth, **per asset**, in each slot.
//!
//! # Why per asset
//!
//! The guard shares a limited budget between devices with a weighted max-min
//! allocator, and "a reduction takes power from where it is worth least" is what
//! the weights are for. A marginal value per **slot** is the same number for
//! every device in it, so a weighted allocator handed one degenerates to plain
//! max-min: a ranking that ranks nothing.
//!
//! Under a § 14a ceiling with a car three hours from its departure and a heat
//! pump in a house that is already warm, "worth least" has an obvious answer,
//! and only a per-asset price can express it.
//!
//! # Where the numbers come from
//!
//! The dual of a constraint is the rate at which the objective would worsen if
//! its right-hand side moved. For the **state equation of a store** — the row
//! that says how much is in the battery, the car, the tank or the fabric at the
//! end of a slot — that is precisely *what a kilowatt-hour held there is worth
//! to this household right now*, in euros, with every downstream consequence the
//! horizon can see already priced in: the deadline, the comfort band, the wear,
//! the terminal value, the § 14a ceiling that will bind at teatime.
//!
//! So the weight is the dual of each asset's own state equation, converted into
//! euros per kilowatt-hour **delivered to that asset** — which is where the
//! round-trip efficiency and the coefficient of performance come in, because a
//! kilowatt-hour bought for a heat pump arrives as three kilowatt-hours of heat.
//!
//! # Why this needs a second solve, and why it costs no toolchain
//!
//! A mixed-integer program has no duals: the value function is not convex, and
//! whatever a branch-and-bound solver reports at its final node is a dual of
//! *some* relaxation rather than of the problem. The standard construction is to
//! fix the integer variables at their optimal values and re-solve the resulting
//! **linear** program, whose duals are the marginal values *conditional on the
//! discrete decisions the plan made* — which is exactly the right conditioning
//! here, because the arbiter is not going to re-open them either.
//!
//! That second solve always runs on **Clarabel**, whichever backend solved the
//! mixed-integer problem. Clarabel is a pure-Rust interior-point solver, so this
//! adds no C++ toolchain to a gateway box — and, more importantly, it leaves no
//! *backend split*: a box built with `microlp` and a box built with HiGHS get the
//! same shadow prices, so a plan does not mean different things on two builds of
//! the same software.
//!
//! An interior-point method also gives a **central** dual rather than a basic
//! one. For a degenerate linear program — and a household model with several
//! binding bounds is degenerate constantly — the simplex answer is one arbitrary
//! vertex of the dual polytope and the interior-point answer is the middle of it.
//! For ranking assets, the middle is the honest one.
//!
//! # Two kinds of row, and they answer different questions
//!
//! The **equalities that define a state** — what the battery, the car, the tank
//! and the fabric hold — price a *stored quantity*, and that is what weights an
//! allocation: a question about the devices.
//!
//! The **§ 14a ceiling** is an inequality, and its dual prices a *limit*: what a
//! kilowatt-hour of relief from the network operator would be worth. That is a
//! different question and it has a different customer — it is what a § 41e
//! offer or an OpenADR bid should be built from, and it is the honest answer to
//! "what is your flexibility worth", asked of the household's own plan rather
//! than guessed at thirty per cent of nominal. It rides on
//! `SlotPlan::flexibility_eur_per_kwh`.
//!
//! The other inequalities — the fuse, the feed-in cap, the device ratings — have
//! duals too and are not collected: nothing consumes them yet, and a number
//! nobody reads is the thing this workspace keeps finding in itself.

use hems_core::prelude::AssetId;

use crate::model::Problem;

/// Watt-hours in a kilowatt-hour — the factor between a dual on a state
/// equation written in watt-hours and a price in euros per kilowatt-hour.
const WH_PER_KWH: f64 = 1000.0;

/// Hours in a slot.
const DT_HOURS: f64 = 0.25;

/// The marginal value of energy in one slot, per asset.
///
/// Every field is euros per kilowatt-hour **delivered to that asset**, so they
/// are directly comparable with each other and with a tariff. A `None` means the
/// asset is not in the problem.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Shadow {
    /// What a kilowatt-hour delivered anywhere in the house costs the plan —
    /// the dual of the energy balance.
    ///
    /// On an ordinary slot this is close to the tariff, and where a limit binds
    /// it is not: a slot in which the § 14a ceiling is the binding constraint
    /// prices energy at what the *next best* use of it was worth, which can be
    /// far above the price of importing it.
    pub site: f64,
    /// A kilowatt-hour into the stationary battery.
    pub battery: Option<f64>,
    /// A kilowatt-hour into the car.
    ///
    /// The one that moves most: it is near the import price with a whole night
    /// ahead of it, and near `Problem::unmet_charge_eur_per_kwh` when the
    /// departure is close and the plan is short.
    pub ev: Option<f64>,
    /// A kilowatt-hour of *electricity* into the hot-water tank, so the
    /// coefficient of performance is already in it.
    pub dhw: Option<f64>,
    /// A kilowatt-hour of *electricity* into the heat pump, likewise.
    pub heat_pump: Option<f64>,
    /// What one kilowatt-hour of **relief from the § 14a ceiling** would be
    /// worth to this household, €/kWh.
    ///
    /// `None` where no ceiling binds in this slot, and zero where one is in
    /// force but is not the constraint that binds — which is most of a reduction
    /// on most households, and is itself worth knowing.
    ///
    /// This is the answer to "what is your flexibility worth", asked from the
    /// household's side rather than guessed from a nameplate. It is the number a
    /// § 41e Aggregatorvertrag offer or an OpenADR bid should be built from: a
    /// household whose relief is worth €4/kWh because a car will otherwise leave
    /// short is not the same counterparty as one whose relief is worth two cents
    /// because it would have shifted the tank an hour anyway, and pricing them
    /// alike is why aggregators assume thirty per cent of nominal and hope.
    pub flexibility: Option<f64>,
}

impl Shadow {
    /// The value for a named asset, falling back to the site's own price.
    ///
    /// The fallback is what makes this safe to use as an allocation weight for
    /// an asset the planner does not model — a relay, a load with no state —
    /// rather than handing it a zero and shedding it first.
    #[must_use]
    pub fn for_asset(&self, names: &crate::solve::AssetNames, asset: &AssetId) -> f64 {
        let same = |id: &Option<AssetId>| id.as_ref().is_some_and(|i| i == asset);
        if same(&names.battery) {
            self.battery.unwrap_or(self.site)
        } else if same(&names.evse) {
            self.ev.unwrap_or(self.site)
        } else if same(&names.dhw) {
            self.dhw.unwrap_or(self.site)
        } else if same(&names.heat_pump) {
            self.heat_pump.unwrap_or(self.site)
        } else {
            self.site
        }
    }
}

/// The raw duals of one slot, before they are turned into prices.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RawDuals {
    pub balance: f64,
    pub battery: Option<f64>,
    pub ev: Option<f64>,
    pub dhw: Option<f64>,
    pub air: Option<f64>,
    pub mass: Option<f64>,
    pub steuve: Option<f64>,
}

impl RawDuals {
    /// Convert one slot's raw duals into per-asset prices.
    ///
    /// Three conversions and each is a unit, not a fudge:
    ///
    /// * the **balance** row is in watts and the objective in euros, so its dual
    ///   is €/W-of-load-in-this-slot; a kilowatt-hour delivered over a quarter
    ///   hour is four kilowatts, hence `× 1000 / Δ`;
    /// * a **store's** row is in watt-hours, so `× 1000` — and then `× η` or
    ///   `× COP`, because the question is what a kilowatt-hour *bought* is
    ///   worth, and a kilowatt-hour bought for a heat pump arrives as three
    ///   kilowatt-hours of heat;
    /// * the **building's** rows are in kelvin. One kilowatt-hour of electricity
    ///   held across a slot is `1/Δ` kilowatts, and `b_heat` is the temperature
    ///   response to one kilowatt held for the step, so the two nodes together
    ///   give the kelvin a kilowatt-hour buys.
    ///
    /// # Two signs, and they are not the same sign
    ///
    /// A shadow price is `d(objective)/d(right-hand side)`. For the **balance**
    /// the right-hand side is the household's load, so one more watt of it costs
    /// money and the dual is positive as it stands. For a **store's state
    /// equation** and for the **§ 14a ceiling** the right-hand side is the
    /// opposite favour — a watt-hour appearing in the battery for nothing, a
    /// watt of ceiling the operator did not take — so the objective *falls* and
    /// the dual comes back negative. Both are what a household would call a
    /// value, so the store rows and the ceiling row are negated here rather than
    /// somewhere further out where the convention would have to be remembered.
    ///
    /// Everything is clamped at zero. A negative marginal value means the plan
    /// would rather the store were emptier — which happens in a negative-price
    /// hour and is a real fact — but this is on its way to being an **allocation
    /// weight**, and a negative weight is not a lower priority, it is a
    /// different arithmetic. The sign belongs in a flexibility offer, not in a
    /// max-min share.
    pub(crate) fn into_shadow(self, problem: &Problem<'_>, k: usize) -> Shadow {
        Shadow {
            site: (self.balance * WH_PER_KWH / DT_HOURS).max(0.0),
            battery: self
                .battery
                .zip(problem.battery)
                .map(|(d, b)| (-d * WH_PER_KWH * b.efficiency_charge).max(0.0)),
            ev: self
                .ev
                .zip(problem.ev)
                .map(|(d, e)| (-d * WH_PER_KWH * e.efficiency).max(0.0)),
            dhw: self
                .dhw
                .zip(problem.dhw)
                .map(|(d, t)| (-d * WH_PER_KWH * t.cop).max(0.0)),
            heat_pump: self.heat_pump_price(problem, k).map(|v| v.max(0.0)),
            flexibility: self.steuve.map(|d| (-d * WH_PER_KWH / DT_HOURS).max(0.0)),
        }
    }

    fn heat_pump_price(self, problem: &Problem<'_>, k: usize) -> Option<f64> {
        let t = problem.thermal?;
        let air = self.air?;
        let mass = self.mass.unwrap_or(0.0);
        let d = problem.thermal_step();
        // A kilowatt-hour of electricity across the slot is `1/Δ` kW; `b_heat`
        // is kelvin per kilowatt of *heat* held for the step; the coefficient of
        // performance turns the one into the other.
        let kw_per_kwh = 1.0 / DT_HOURS;
        // The coefficient of performance of **this** slot. Reading slot zero's
        // priced every hour of a winter day at the small hours' COP: a 2,7 at
        // four in the morning against a 3,6 at two in the afternoon is a third
        // of the heat pump's own marginal value, and it is the weight the guard
        // uses to decide which device keeps its power under a teatime reduction.
        let cop = t.heat_pump.cop(problem.outdoor_at(k));
        let kelvin = (air * d.b_heat[0] + mass * d.b_heat[1]) * cop * kw_per_kwh;
        Some(-kelvin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solve::AssetNames;
    use hems_core::prelude::AssetId;

    fn names() -> AssetNames {
        AssetNames {
            battery: Some(AssetId::new("battery").expect("valid")),
            evse: Some(AssetId::new("wallbox").expect("valid")),
            pv: None,
            heat_pump: Some(AssetId::new("wp").expect("valid")),
            dhw: Some(AssetId::new("tank").expect("valid")),
            shiftable: Vec::new(),
        }
    }

    #[test]
    fn an_asset_the_planner_does_not_model_falls_back_to_the_site_price() {
        let s = Shadow {
            site: 0.31,
            battery: Some(0.2),
            ..Shadow::default()
        };
        let n = names();
        assert!((s.for_asset(&n, &AssetId::new("relay").expect("valid")) - 0.31).abs() < 1e-12);
        // …and so does one the planner *names* but has no model for, which is
        // the case that matters: shedding it first because its weight was zero
        // would be a decision nobody made.
        assert!((s.for_asset(&n, &AssetId::new("wp").expect("valid")) - 0.31).abs() < 1e-12);
        assert!((s.for_asset(&n, &AssetId::new("battery").expect("valid")) - 0.2).abs() < 1e-12);
    }
}
