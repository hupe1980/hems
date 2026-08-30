//! What the planner does, checked against what it is supposed to do.
//!
//! Every test builds a household, prices a day, and asserts on the plan rather
//! than on the solver: the model is what has to be right.

use std::collections::BTreeMap;

use hems_core::prelude::{AssetId, Energy, Horizon, Power, Slot, Soc};
use hems_forecast::{Band, Forecast};
use hems_optimizer::model::{
    BatteryModel, DhwModel, EvSession, HeatPumpModel, PlanningLimits, Problem, ThermalModel,
    TimedLimit,
};
use hems_optimizer::solve::{AssetNames, solve};
use hems_tariff::levies::Levies;
use hems_tariff::stack::PriceStack;
use hems_tariff::tariff::{EnergyPrice, FeedIn, NetworkCharge, Tariff};
use rust_decimal::Decimal;
use time::OffsetDateTime;
use time::macros::datetime;

const T0: OffsetDateTime = datetime!(2026-01-15 00:00:00 UTC);

fn horizon(slots: usize) -> Horizon {
    Horizon::new(T0, slots)
}

/// A flat forecast of `watts` over the horizon.
fn flat(h: Horizon, watts: f64) -> Forecast {
    Forecast {
        slots: h.slots().map(|s| (s, Band::certain(watts))).collect(),
    }
}

/// A forecast that is `watts` in the given slot indices and zero elsewhere.
fn shaped(h: Horizon, watts: f64, active: &[usize]) -> Forecast {
    Forecast {
        slots: h
            .slots()
            .enumerate()
            .map(|(i, s)| {
                (
                    s,
                    Band::certain(if active.contains(&i) { watts } else { 0.0 }),
                )
            })
            .collect(),
    }
}

/// A price stack whose energy price is `ct[i]` in slot `i`, everything else flat.
fn prices(h: Horizon, ct: &[i64]) -> PriceStack {
    let spot: BTreeMap<Slot, Decimal> = h
        .slots()
        .enumerate()
        .map(|(i, s)| (s, Decimal::new(ct[i % ct.len()], 0)))
        .collect();
    let tariff = Tariff {
        energy: EnergyPrice::Dynamic {
            spot,
            markup_ct_per_kwh: Decimal::ZERO,
            fallback_ct_per_kwh: Decimal::new(20, 0),
        },
        network: NetworkCharge::None {
            arbeitspreis: Decimal::ZERO,
        },
        levies: Levies {
            stromsteuer: Decimal::ZERO,
            kwkg: Decimal::ZERO,
            para19: Decimal::ZERO,
            offshore: Decimal::ZERO,
            konzessionsabgabe: Decimal::ZERO,
            vat_rate: Decimal::ZERO,
        },
        // Below every import price used in these tests, as an EEG tariff is in
        // reality — so a plan that exports rather than storing is choosing to
        // lose money, not spotting an arbitrage.
        feed_in: FeedIn::Eeg {
            ct_per_kwh: Decimal::new(4, 0),
            negative_price_rule: true,
        },
        standing_charge_eur_per_year: Decimal::ZERO,
    };
    PriceStack::build(&tariff, h)
}

fn battery(kwh: f64, soc: f64, wear_eur_per_kwh: f64) -> BatteryModel {
    BatteryModel {
        capacity: Energy::from_kwh(kwh),
        soc_now: Soc::new(soc).unwrap(),
        max_charge: Power::from_kw(5.0),
        max_discharge: Power::from_kw(5.0),
        efficiency_charge: 0.95,
        efficiency_discharge: 0.95,
        soc_min: Soc::new(0.05).unwrap(),
        soc_max: Soc::FULL,
        reserve_soc: Soc::ZERO_RESERVE,
        degradation_eur_per_kwh: wear_eur_per_kwh,
        grid_charging_allowed: true,
    }
}

fn names() -> AssetNames {
    AssetNames {
        battery: Some(AssetId::new("battery").unwrap()),
        evse: Some(AssetId::new("wallbox").unwrap()),
        pv: Some(AssetId::new("pv").unwrap()),
        heat_pump: Some(AssetId::new("waermepumpe").unwrap()),
        dhw: Some(AssetId::new("warmwasser").unwrap()),
    }
}

#[test]
fn a_household_with_nothing_to_control_simply_imports_its_load() {
    let h = horizon(8);
    let p = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 1000.0);
    let solved = solve(&Problem::new(h, &p, &pv, &load), &names(), T0).unwrap();
    for f in &solved.flows {
        assert_eq!(f.grid_import, Power::new(1000.0));
        assert_eq!(f.grid_export, Power::ZERO);
    }
}

#[test]
fn surplus_production_is_stored_rather_than_exported_when_that_pays() {
    let h = horizon(8);
    // Export earns 8 ct, importing later costs 30 ct: storing is worth it.
    let p = prices(h, &[30]);
    let pv = shaped(h, 4000.0, &[0, 1, 2, 3]);
    let load = flat(h, 500.0);
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_battery(battery(10.0, 0.1, 0.0)),
        &names(),
        T0,
    )
    .unwrap();
    let charged: f64 = solved.flows.iter().map(|f| f.battery_charge.kw()).sum();
    assert!(
        charged > 0.0,
        "the surplus should have gone into the battery"
    );
    let exported: f64 = solved.flows.iter().map(|f| f.grid_export.kw()).sum();
    assert!(
        charged > exported,
        "storing {charged} should beat exporting {exported}"
    );
}

#[test]
fn a_battery_buys_cheap_and_sells_expensive_when_the_spread_covers_the_wear() {
    let h = horizon(8);
    // Four cheap slots, then four dear ones.
    let p = prices(h, &[5, 5, 5, 5, 40, 40, 40, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 1000.0);
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_battery(battery(10.0, 0.1, 0.0)),
        &names(),
        T0,
    )
    .unwrap();
    let early: f64 = solved.flows[..4]
        .iter()
        .map(|f| f.battery_charge.kw())
        .sum();
    let late: f64 = solved.flows[4..]
        .iter()
        .map(|f| f.battery_discharge.kw())
        .sum();
    assert!(early > 0.0, "should have charged while it was cheap");
    assert!(late > 0.0, "should have discharged while it was dear");
}

#[test]
fn wear_stops_a_battery_cycling_for_a_spread_that_does_not_pay() {
    // The finding of `specs/arxiv/arxiv-2606.16051.pdf`, as a test: with no wear
    // term a cost-minimising plan cycles for any spread at all. With one, it
    // only cycles when the spread covers the damage.
    let h = horizon(8);
    let p = prices(h, &[18, 18, 18, 18, 22, 22, 22, 22]); // 4 ct of spread
    let pv = flat(h, 0.0);
    let load = flat(h, 1000.0);

    let cycled = |wear: f64| -> f64 {
        let solved = solve(
            &Problem::new(h, &p, &pv, &load).with_battery(battery(10.0, 0.5, wear)),
            &names(),
            T0,
        )
        .unwrap();
        solved
            .flows
            .iter()
            .map(|f| f.battery_charge.kw() + f.battery_discharge.kw())
            .sum()
    };

    let free = cycled(0.0);
    let realistic = cycled(0.08);
    assert!(
        free > 0.0,
        "a wear-free model always finds the trade worth taking"
    );
    assert!(
        realistic < free,
        "pricing wear at 8 ct/kWh should suppress a 4 ct trade: {realistic} vs {free}"
    );
}

#[test]
fn a_car_reaches_its_target_before_it_leaves() {
    let h = horizon(16);
    let p = prices(h, &[30, 30, 5, 5, 30, 30, 30, 30]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let ev = EvSession {
        arrival: None,
        energy_now: Energy::from_kwh(10.0),
        energy_target: Energy::from_kwh(20.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 0.92,
        departure: h.get(11).unwrap(),
    };
    let solved = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0).unwrap();

    let at_departure = solved.flows[11].ev_charge;
    let _ = at_departure;
    let charged: f64 = solved.flows[..=11]
        .iter()
        .map(|f| f.ev_charge.kw() * 0.25 * 0.92)
        .sum();
    assert!(
        charged >= 9.9,
        "the car should have gained ~10 kWh, got {charged}"
    );
    // …and nothing after it left.
    assert!(
        solved.flows[12..]
            .iter()
            .all(|f| f.ev_charge == Power::ZERO),
        "charged a car that had gone"
    );
}

#[test]
fn the_paragraph_14a_ceiling_bounds_what_the_plan_asks_for() {
    let h = horizon(8);
    let p = prices(h, &[5]); // cheap enough that the plan would charge flat out
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let limits = PlanningLimits::default().with_steuve(TimedLimit::always(Power::from_kw(2.0)));
    let solved = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_battery(battery(20.0, 0.1, 0.0))
            .with_limits(limits.clone()),
        &names(),
        T0,
    )
    .unwrap();
    for (i, f) in solved.flows.iter().enumerate() {
        assert!(
            f.battery_charge <= Power::from_kw(2.0) + Power::new(1e-6),
            "slot {i} charged at {}",
            f.battery_charge
        );
    }
}

#[test]
fn surplus_lifts_the_paragraph_14a_ceiling_exactly_as_the_festlegung_says() {
    // [A1 2.3]: the limit is on the *netzwirksamer* draw, so production that
    // covers the rest of the house does not count against it.
    let h = horizon(4);
    let p = prices(h, &[5]);
    let pv = flat(h, 6000.0);
    let load = flat(h, 1000.0); // 5 kW of surplus
    let limits = PlanningLimits::default().with_steuve(TimedLimit::always(Power::from_kw(2.0)));
    let solved = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_battery(battery(20.0, 0.1, 0.0))
            .with_limits(limits.clone()),
        &names(),
        T0,
    )
    .unwrap();
    let charge = solved.flows[0].battery_charge;
    assert!(
        charge > Power::from_kw(2.0),
        "with 5 kW of surplus the battery may exceed the 2 kW ceiling, got {charge}"
    );
    assert!(
        charge <= Power::from_kw(5.0) + Power::new(1e-6),
        "but not beyond its own rating"
    );
}

#[test]
fn a_feed_in_ceiling_forces_curtailment_only_when_nothing_else_will_do() {
    let h = horizon(4);
    let p = prices(h, &[20]);
    let pv = flat(h, 8000.0);
    let load = flat(h, 500.0);
    let limits = PlanningLimits::default().with_feed_in(TimedLimit::always(Power::from_kw(3.0)));

    // With a battery, the surplus goes in rather than being thrown away.
    let with_battery = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_battery(battery(20.0, 0.1, 0.0))
            .with_limits(limits.clone()),
        &names(),
        T0,
    )
    .unwrap();
    let stored: f64 = with_battery
        .flows
        .iter()
        .map(|f| f.battery_charge.kw())
        .sum();
    let thrown: f64 = with_battery.flows.iter().map(|f| f.curtailed.kw()).sum();
    assert!(
        stored > 0.0,
        "the battery should absorb what cannot be exported"
    );

    // Without one, there is nowhere for it to go.
    let without = solve(
        &Problem::new(h, &p, &pv, &load).with_limits(limits.clone()),
        &names(),
        T0,
    )
    .unwrap();
    let thrown_without: f64 = without.flows.iter().map(|f| f.curtailed.kw()).sum();
    assert!(
        thrown_without > thrown,
        "with no battery the excess has to be curtailed"
    );
    for f in &without.flows {
        assert!(f.grid_export <= Power::from_kw(3.0) + Power::new(1e-6));
    }
}

#[test]
fn a_backup_reserve_is_never_planned_away() {
    let h = horizon(8);
    let p = prices(h, &[40]); // expensive: the plan would love to discharge
    let pv = flat(h, 0.0);
    let load = flat(h, 3000.0);
    let mut b = battery(10.0, 0.8, 0.0);
    b.reserve_soc = Soc::new(0.5).unwrap();
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_battery(b),
        &names(),
        T0,
    )
    .unwrap();
    for (i, f) in solved.flows.iter().enumerate() {
        assert!(
            f.battery_energy >= Energy::from_kwh(5.0) - Energy::new(1e-6),
            "slot {i}: planned down to {} despite a 50 % reserve",
            f.battery_energy
        );
    }
}

#[test]
fn a_battery_barred_from_the_grid_only_takes_what_the_roof_makes() {
    let h = horizon(4);
    let p = prices(h, &[5]);
    let pv = flat(h, 1000.0);
    let load = flat(h, 0.0);
    let mut b = battery(20.0, 0.1, 0.0);
    b.grid_charging_allowed = false;
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_battery(b),
        &names(),
        T0,
    )
    .unwrap();
    for (i, f) in solved.flows.iter().enumerate() {
        assert!(
            f.battery_charge <= Power::new(1000.0) + Power::new(1e-6),
            "slot {i} charged {} from a 1 kW roof",
            f.battery_charge
        );
    }
}

#[test]
fn a_plan_names_the_assets_it_commands_and_reports_a_saving() {
    let h = horizon(8);
    let p = prices(h, &[5, 5, 5, 5, 40, 40, 40, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 1000.0);
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_battery(battery(10.0, 0.1, 0.0)),
        &names(),
        T0,
    )
    .unwrap();

    assert_eq!(solved.plan.slots.len(), 8);
    assert!(
        solved
            .plan
            .slots
            .iter()
            .all(|s| s.targets.iter().any(|t| t.asset.as_str() == "battery")),
        "every slot should command the battery"
    );
    let saving = solved
        .plan
        .expected_saving_eur()
        .expect("both costs are reported");
    assert!(
        saving > 0.0,
        "shifting into the cheap hours should save something, got {saving}"
    );
    assert!(
        solved
            .plan
            .slots
            .iter()
            .all(|s| s.marginal_eur_per_kwh.is_some())
    );
}

#[test]
fn an_empty_horizon_is_an_error_not_an_empty_plan() {
    let h = horizon(0);
    let p = prices(horizon(1), &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 0.0);
    assert!(matches!(
        solve(&Problem::new(h, &p, &pv, &load), &names(), T0),
        Err(hems_optimizer::SolveError::EmptyHorizon)
    ));
}

// ── Heating ─────────────────────────────────────────────────────────────────

fn thermal(indoor_c: f64, modulating: bool) -> ThermalModel {
    let mut hp = HeatPumpModel::modulating(Power::from_kw(5.0));
    hp.modulating = modulating;
    ThermalModel {
        comfort_min_c: 20.0,
        comfort_max_c: 23.0,
        ..ThermalModel::house(indoor_c, hp)
    }
}

#[test]
fn a_heat_pump_keeps_the_house_inside_its_comfort_band() {
    let h = horizon(32);
    let p = prices(h, &[25]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-2.0; 32];

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(thermal(21.0, true), &outdoor),
        &names(),
        T0,
    )
    .unwrap();

    for (i, f) in solved.flows.iter().enumerate() {
        assert!(
            f.indoor_c > 19.0,
            "slot {i}: the house was allowed to fall to {:.1} °C",
            f.indoor_c
        );
    }
    let heat: f64 = solved.flows.iter().map(|f| f.heat_pump.kw() * 0.25).sum();
    assert!(
        heat > 1.0,
        "at −2 °C outside the house needs heating, got {heat:.1} kWh"
    );
}

#[test]
fn the_house_is_pre_heated_into_the_cheap_hours() {
    // The building's thermal mass is several times the household battery, and
    // free. A planner that cannot use it leaves the largest store in the house
    // on the table.
    let h = horizon(32);
    // Cheap for the first half, dear for the second.
    let p = prices(
        h,
        &[
            8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 45, 45, 45, 45, 45, 45, 45, 45, 45, 45,
            45, 45, 45, 45, 45, 45,
        ],
    );
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![0.0; 32];

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(thermal(21.0, true), &outdoor),
        &names(),
        T0,
    )
    .unwrap();

    let cheap: f64 = solved.flows[..16].iter().map(|f| f.heat_pump.kw()).sum();
    let dear: f64 = solved.flows[16..].iter().map(|f| f.heat_pump.kw()).sum();
    assert!(
        cheap > dear,
        "should have heated in the cheap half: {cheap:.1} vs {dear:.1} kW-slots"
    );
    // …and the house should be warmer when the dear half begins than it was.
    assert!(
        solved.flows[15].indoor_c > 21.0,
        "the plan banked no heat: {:.2} °C",
        solved.flows[15].indoor_c
    );
}

#[test]
fn a_colder_day_costs_more_heat_because_the_coefficient_of_performance_falls() {
    let h = horizon(16);
    let p = prices(h, &[25]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);

    let heat_at = |outdoor_c: f64| -> f64 {
        let outdoor = vec![outdoor_c; 16];
        let solved = solve(
            &Problem::new(h, &p, &pv, &load).with_thermal(thermal(21.0, true), &outdoor),
            &names(),
            T0,
        )
        .unwrap();
        solved.flows.iter().map(|f| f.heat_pump.kw() * 0.25).sum()
    };

    let mild = heat_at(8.0);
    let cold = heat_at(-8.0);
    assert!(
        cold > mild * 1.5,
        "−8 °C should cost far more than +8 °C: {cold:.1} vs {mild:.1} kWh"
    );
}

#[test]
fn the_paragraph_14a_ceiling_binds_the_heat_pump_too() {
    // [A1 2.4.1.b] — a heat pump is a controllable device, and shares the
    // ceiling with the battery and the car.
    let h = horizon(8);
    let p = prices(h, &[10]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-10.0; 8];
    let limits = PlanningLimits::default().with_steuve(TimedLimit::always(Power::from_kw(2.0)));

    let solved = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_thermal(thermal(21.0, true), &outdoor)
            .with_battery(battery(10.0, 0.5, 0.0))
            .with_limits(limits.clone()),
        &names(),
        T0,
    )
    .unwrap();

    for (i, f) in solved.flows.iter().enumerate() {
        let steuve = f.heat_pump + f.battery_charge;
        assert!(
            steuve <= Power::from_kw(2.0) + Power::new(1e-6),
            "slot {i}: controllable devices drew {steuve} over a 2 kW ceiling"
        );
    }
}

#[test]
fn a_cold_snap_makes_the_house_uncomfortable_rather_than_the_problem_infeasible() {
    // A 5 kW heat pump cannot hold 20 °C at −25 °C through a badly insulated
    // wall. The right answer is a cold house and a number the household can be
    // shown, not a solver error.
    let h = horizon(16);
    let p = prices(h, &[25]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-25.0; 16];
    let mut t = thermal(18.0, true);
    t.building.r_air_out_k_per_kw = 1.5;

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
        &names(),
        T0,
    )
    .expect("a cold snap is uncomfortable, not infeasible");

    assert!(
        solved.flows.iter().any(|f| f.discomfort_k > 0.0),
        "nobody was cold?"
    );
    // And the heat pump was running flat out while it happened.
    assert!(solved.flows[8].heat_pump > Power::from_kw(4.0));
}

#[test]
fn an_on_off_heat_pump_does_not_short_cycle() {
    // Minimum runtime, [HeatPumpModel::min_on_slots]. A compressor is damaged by
    // switching far faster than by running.
    let h = horizon(24);
    let p = prices(h, &[10, 40, 10, 40, 10, 40, 10, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![0.0; 24];
    let mut t = thermal(21.0, false);
    t.heat_pump.min_on_slots = 4;
    t.heat_pump.min_off_slots = 4;

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
        &names(),
        T0,
    )
    .unwrap();

    // Count how often it changed state.
    let on: Vec<bool> = solved
        .flows
        .iter()
        .map(|f| f.heat_pump > Power::new(1.0))
        .collect();
    let switches = on.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(
        switches <= 6,
        "the compressor was switched {switches} times in six hours"
    );
}

#[test]
fn a_plan_commands_the_heat_pump_by_name() {
    let h = horizon(8);
    let p = prices(h, &[25]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![0.0; 8];
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(thermal(21.0, true), &outdoor),
        &names(),
        T0,
    )
    .unwrap();
    assert!(
        solved
            .plan
            .slots
            .iter()
            .all(|s| s.targets.iter().any(|t| t.asset.as_str() == "waermepumpe")),
        "every slot should carry a heat-pump target"
    );
}

// ── The charge point's minimum current ───────────────────────────────────────

#[test]
fn a_charge_point_is_never_asked_for_a_current_it_cannot_deliver() {
    // The gap this closes: without a semi-continuous bound the cheapest way to
    // meet a modest target is to trickle a few hundred watts across many hours.
    // A wallbox below 6 A is not charging slowly — it is idle — so that plan
    // delivers nothing and nobody finds out until the morning.
    let h = horizon(24);
    let p = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let ev = EvSession {
        arrival: None,
        energy_now: Energy::from_kwh(10.0),
        energy_target: Energy::from_kwh(11.0), // 1 kWh over six hours
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 0.92,
        departure: h.get(23).unwrap(),
    };
    let solved = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0).unwrap();
    for (k, f) in solved.flows.iter().enumerate() {
        assert!(
            f.ev_charge == Power::ZERO || f.ev_charge >= Power::from_kw(4.14) - Power::new(1.0),
            "slot {k} asks for {} — below the 6 A the standard mandates",
            f.ev_charge
        );
    }
    let charged: f64 = solved
        .flows
        .iter()
        .map(|f| f.ev_charge.kw() * 0.25 * 0.92)
        .sum();
    assert!(charged >= 0.99, "the car should still be filled: {charged}");
}

#[test]
fn a_departure_beyond_the_horizon_still_gets_the_car_charging() {
    // A 48-hour horizon and a car that leaves in four days: no deadline falls
    // inside the plan, so an unconstrained model charges nothing, re-plans five
    // minutes later, and charges nothing again — until the deadline finally
    // comes into view and the car can no longer be filled in time.
    let h = horizon(16);
    let p = prices(h, &[30]);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let ev = EvSession {
        arrival: None,
        energy_now: Energy::ZERO,
        energy_target: Energy::from_kwh(40.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 0.92,
        // Four times the horizon away.
        departure: h.first.offset(64),
    };
    let solved = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0).unwrap();
    let charged: f64 = solved
        .flows
        .iter()
        .map(|f| f.ev_charge.kw() * 0.25 * 0.92)
        .sum();
    // A quarter of the way to the departure, so about a quarter of the energy.
    assert!(
        charged >= 9.0,
        "the plan should be a quarter of the way there, got {charged} kWh"
    );
}

// ── The baseline ─────────────────────────────────────────────────────────────

#[test]
fn the_baseline_delivers_the_same_service_as_the_plan() {
    // The comparison that flatters itself: an earlier baseline priced a house
    // with no car at all, so the "saving" included energy the optimiser never
    // had to buy. Here the car has to be filled either way, and the difference
    // is only *when*.
    let h = horizon(24);
    let p = prices(h, &[30, 30, 30, 30, 5, 5, 5, 5, 30, 30, 30, 30]);
    let pv = flat(h, 0.0);
    let load = flat(h, 400.0);
    let ev = EvSession {
        arrival: None,
        energy_now: Energy::from_kwh(10.0),
        energy_target: Energy::from_kwh(20.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 0.92,
        departure: h.get(23).unwrap(),
    };
    let solved = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0).unwrap();
    let baseline = solved.plan.baseline_cost.unwrap().total();
    let cost = solved.plan.expected_cost.unwrap().total();

    // The unmanaged house charges immediately, at 30 ct; the plan waits for the
    // 5 ct window. Both buy the same ~10,9 kWh.
    assert!(
        baseline > cost,
        "waiting for the cheap window should beat charging on plug-in: {cost} vs {baseline}"
    );
    // …and the baseline is not the "no car at all" number, which for this day
    // would be the household load alone.
    let load_only: f64 = (0..24).map(|k| 0.4 * 0.25 * p.slots[k].import_f64()).sum();
    assert!(
        baseline > load_only * 1.5,
        "the baseline forgot to charge the car: {baseline} vs {load_only}"
    );
}

// ── Objectives in one currency ───────────────────────────────────────────────

#[test]
fn a_carbon_price_shifts_the_plan_without_changing_the_units_it_is_measured_in() {
    // Every term in the objective is euros. A carbon price is what the household
    // will pay to avoid a kilogram, so it *adds* to the import price rather than
    // replacing it — which is what keeps it comparable with battery wear.
    let h = horizon(8);
    // Flat energy price, so only the carbon intensity can move the plan.
    let mut p = prices(h, &[20]);
    for (k, slot) in p.slots.iter_mut().enumerate() {
        slot.co2_g_per_kwh = Some(if (2..4).contains(&k) { 50.0 } else { 700.0 });
    }
    let pv = flat(h, 0.0);
    // Enough load that a discharge displaces an import rather than exporting at
    // the feed-in tariff, and an empty battery so charging is a decision.
    let load = flat(h, 5_000.0);
    let battery = battery(10.0, 0.05, 0.0);

    let dirty = solve(
        &Problem::new(h, &p, &pv, &load).with_battery(battery),
        &names(),
        T0,
    )
    .unwrap();
    let clean = solve(
        &{
            let mut problem = Problem::new(h, &p, &pv, &load).with_battery(battery);
            problem.objective = hems_optimizer::model::Objective::cost().with_carbon_price(0.20);
            problem
        },
        &names(),
        T0,
    )
    .unwrap();

    let charged_in_clean_slots = |s: &hems_optimizer::solve::Solved| -> f64 {
        s.flows[2..4].iter().map(|f| f.battery_charge.kw()).sum()
    };
    assert!(
        charged_in_clean_slots(&clean) > charged_in_clean_slots(&dirty),
        "a carbon price should move charging into the clean hours"
    );
    assert_eq!(
        charged_in_clean_slots(&dirty),
        0.0,
        "with a flat price and no carbon term there is nothing to arbitrage"
    );
}

#[test]
fn a_deadline_that_cannot_be_met_returns_the_best_plan_and_says_how_short_it_is() {
    // A hard deadline would return nothing at all. "I could not do all of it" is a better
    // answer than "I could not do any of it", and a plan that fails leaves the
    // arbiter on the fallback — which may charge the car less than the best
    // achievable schedule would have.
    let h = horizon(4); // one hour
    let p = prices(h, &[20, 20, 20, 20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let ev = EvSession {
        arrival: None,
        energy_now: Energy::ZERO,
        // 40 kWh in one hour through an 11 kW charge point: impossible.
        energy_target: Energy::from_kwh(40.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 0.92,
        departure: h.get(3).unwrap(),
    };
    let solved = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0)
        .expect("an impossible deadline is still a plan");

    // It charged as hard as it could…
    let charged: f64 = solved
        .flows
        .iter()
        .map(|f| f.ev_charge.kw() * 0.25 * 0.92)
        .sum();
    assert!(
        charged > 9.0,
        "only charged {charged:.1} kWh of a possible ~10"
    );
    // …and it says how far short that left it.
    assert!(
        (solved.unmet_charge.kwh() - (40.0 - charged)).abs() < 0.2,
        "shortfall {:.1} kWh against {:.1} delivered",
        solved.unmet_charge.kwh(),
        charged
    );
}

#[test]
fn a_deadline_that_can_be_met_is_met_and_reports_no_shortfall() {
    let h = horizon(24);
    let p = prices(h, &[30, 30, 30, 30, 5, 5, 5, 5, 30, 30, 30, 30]);
    let pv = flat(h, 0.0);
    let load = flat(h, 400.0);
    let ev = EvSession {
        arrival: None,
        energy_now: Energy::from_kwh(10.0),
        energy_target: Energy::from_kwh(20.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 0.92,
        departure: h.get(23).unwrap(),
    };
    let solved = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0).unwrap();
    assert!(
        solved.unmet_charge.kwh() < 1e-6,
        "an achievable target should not report a shortfall: {:.3} kWh",
        solved.unmet_charge.kwh()
    );
}

#[test]
fn a_hot_water_tank_is_filled_when_electricity_is_cheap_and_coasts_when_it_is_not() {
    // Three hundred litres between 45 and 60 °C hold about five kilowatt-hours
    // of heat. A tank is worth planning with precisely because that heat can be
    // bought hours before it is used — so the plan should fill it through the
    // cheap slots and let it drain through the dear ones, and the household
    // should not be able to tell.
    let h = horizon(8);
    // Four cheap slots, then four dear ones.
    let p = prices(h, &[5, 5, 5, 5, 40, 40, 40, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    // A litre of hot water drawn in each of the dear slots.
    let draw = [0.0, 0.0, 0.0, 0.0, 900.0, 900.0, 900.0, 900.0];
    let tank = DhwModel {
        stored_now: Energy::from_kwh(1.0),
        standing_loss: Power::new(45.0),
        ..DhwModel::tank(Energy::from_kwh(5.0), Power::from_kw(1.5))
    };

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_dhw(tank, &draw),
        &names(),
        T0,
    )
    .unwrap();

    let cheap: f64 = solved.flows[..4].iter().map(|f| f.dhw.kw()).sum();
    let dear: f64 = solved.flows[4..].iter().map(|f| f.dhw.kw()).sum();
    assert!(
        cheap > dear,
        "the tank should be filled in the cheap hours: {cheap:.2} kW against {dear:.2} kW"
    );
    assert_eq!(
        solved.unmet_hot_water,
        Energy::ZERO,
        "and the household still gets its hot water"
    );
}

#[test]
fn a_tank_that_cannot_be_filled_in_time_says_how_short_it_is() {
    // The same argument as the charging deadline: a cold shower is expensive,
    // not impossible, and "this schedule, and it is two kilowatt-hours short" is
    // a better answer than "no schedule exists".
    let h = horizon(4);
    let p = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let draw = [4000.0, 0.0, 0.0, 0.0];
    let tank = DhwModel {
        stored_now: Energy::ZERO,
        standing_loss: Power::ZERO,
        ..DhwModel::tank(Energy::from_kwh(5.0), Power::from_kw(0.5))
    };
    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_dhw(tank, &draw),
        &names(),
        T0,
    )
    .unwrap();
    assert!(
        solved.unmet_hot_water > Energy::ZERO,
        "an empty tank cannot deliver 4 kWh in a quarter of an hour"
    );
}

// ── Shadow prices ───────────────────────────────────────────────────────────

#[test]
fn the_site_shadow_price_is_the_price_of_energy_where_nothing_binds() {
    // The sanity check the rest of these rest on. With no limit binding, one
    // more kilowatt-hour of load costs exactly what importing it costs, so the
    // dual of the energy balance has to come back as the tariff.
    let h = horizon(8);
    let p = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 1000.0);
    let solved = solve(&Problem::new(h, &p, &pv, &load), &names(), T0).unwrap();

    for slot in &solved.plan.slots {
        let marginal = slot.marginal_eur_per_kwh.expect("a shadow price");
        assert!(
            (marginal - 0.20).abs() < 1e-3,
            "the balance dual should be the 20 ct import price, got {marginal:.4} €/kWh"
        );
    }
}

#[test]
fn a_binding_ceiling_prices_the_relief_the_operator_did_not_give() {
    // What a household's flexibility is actually worth, computed from its own
    // plan. Under a § 14a reduction the car cannot be charged at the tariff — it
    // has to give up the *next best use* of the power the household is allowed,
    // and where there is no next best use it simply arrives short. That is what
    // the ceiling's own shadow price says and what a tariff cannot.
    //
    // It is also the number a § 41e offer should be priced from. "Assume 30 % of
    // nominal" is what aggregators do instead, and it prices a household whose
    // car will leave short exactly like one that would have shifted its tank an
    // hour anyway.
    let h = horizon(8);
    let p = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let ev = EvSession {
        energy_now: Energy::from_kwh(10.0),
        energy_target: Energy::from_kwh(40.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::ZERO,
        efficiency: 0.95,
        arrival: None,
        departure: h.get(7).unwrap(),
    };
    let free = solve(&Problem::new(h, &p, &pv, &load).with_ev(ev), &names(), T0).unwrap();
    let squeezed = solve(
        &Problem::new(h, &p, &pv, &load).with_ev(ev).with_limits(
            PlanningLimits::default().with_steuve(TimedLimit::always(Power::from_kw(2.0))),
        ),
        &names(),
        T0,
    )
    .unwrap();

    assert!(
        free.plan.slots[0].flexibility_eur_per_kwh.is_none(),
        "with no ceiling in force there is no relief to price"
    );
    let relief = squeezed.plan.slots[0]
        .flexibility_eur_per_kwh
        .expect("a ceiling is in force");
    assert!(
        relief > 1.0,
        "a two-kilowatt ceiling against a car that needs 30 kWh in two hours has \
         to make relief worth far more than the tariff: {relief:.3} €/kWh"
    );

    // And the household-load side is *not* moved by it, which is the check that
    // says the two duals are measuring different things: § 14a bounds the
    // controllable devices, so one more kilowatt-hour of kettle still costs
    // exactly what importing it costs.
    let site = |s: &hems_optimizer::solve::Solved| {
        s.plan.slots[0]
            .marginal_eur_per_kwh
            .expect("a shadow price")
    };
    assert!((site(&squeezed) - site(&free)).abs() < 1e-3);
    assert!((site(&free) - 0.20).abs() < 1e-3);
}

#[test]
fn a_car_that_will_be_short_outbids_a_tank_that_will_not() {
    // The finding this whole mechanism exists for. Two devices want the same
    // kilowatt under the same ceiling; one is three hours from a departure it
    // cannot meet and the other is a hot-water tank with a full afternoon ahead
    // of it. Until the planner priced them separately the guard gave them the
    // same weight, which is not a ranking at all.
    let h = horizon(12);
    let p = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let ev = EvSession {
        energy_now: Energy::from_kwh(10.0),
        // Far more than three hours at the ceiling can deliver, so the plan is
        // short and the shortfall price is what the next kilowatt-hour is worth.
        energy_target: Energy::from_kwh(60.0),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::ZERO,
        efficiency: 0.95,
        arrival: None,
        departure: h.get(11).unwrap(),
    };
    let dhw = DhwModel::tank(Energy::from_kwh(5.0), Power::from_kw(2.0));
    let solved = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_ev(ev)
            .with_dhw(dhw, &[])
            .with_limits(
                PlanningLimits::default().with_steuve(TimedLimit::always(Power::from_kw(3.0))),
            ),
        &names(),
        T0,
    )
    .unwrap();

    let slot = &solved.plan.slots[0];
    let value = |id: &str| {
        slot.target(&AssetId::new(id).unwrap())
            .expect("a target")
            .marginal_eur_per_kwh
            .expect("a per-asset shadow price")
    };
    assert!(
        value("wallbox") > value("warmwasser") * 2.0,
        "a car that will be short has to outbid a tank that will not: \
         {:.3} against {:.3} €/kWh",
        value("wallbox"),
        value("warmwasser")
    );
    // And it has to be recognisably the shortfall price rather than the tariff:
    // this is the number that makes the guard hand the car the reduction's
    // remaining kilowatts rather than splitting them evenly.
    assert!(
        value("wallbox") > 1.0,
        "the car's value should approach the €5/kWh shortfall price, got {:.3}",
        value("wallbox")
    );
}

#[test]
fn shadow_prices_can_be_switched_off_and_the_plan_is_otherwise_the_same() {
    // The dual pass is a second solve, so a caller that only wants flows should
    // be able to decline it — and declining it must not change the plan.
    let h = horizon(8);
    let p = prices(h, &[20, 30, 10, 40, 20, 30, 10, 40]);
    let pv = shaped(h, 3000.0, &[2, 3]);
    let load = flat(h, 800.0);
    let base = Problem::new(h, &p, &pv, &load).with_battery(battery(10.0, 0.3, 0.05));

    let with = solve(&base, &names(), T0).unwrap();
    let mut without = base.clone();
    without.shadow_prices = false;
    let without = solve(&without, &names(), T0).unwrap();

    assert_eq!(with.flows, without.flows, "the plan must not depend on it");
    assert!(with.plan.slots[0].targets[0].marginal_eur_per_kwh.is_some());
    assert!(
        without.plan.slots[0].targets[0]
            .marginal_eur_per_kwh
            .is_none()
    );
    // …and the slot marginal still comes back, from the price the plan faces.
    assert!(without.plan.slots[0].marginal_eur_per_kwh.is_some());
}
