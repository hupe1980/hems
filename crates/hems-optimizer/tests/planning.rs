//! What the planner does, checked against what it is supposed to do.
//!
//! Every test builds a household, prices a day, and asserts on the plan rather
//! than on the solver: the model is what has to be right.

use std::collections::BTreeMap;

use hems_core::prelude::{AssetId, CompressorState, Energy, Horizon, Power, Programme, Slot, Soc};
use hems_forecast::{Band, Forecast};
use hems_optimizer::Risk;
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
        feed_in: FeedIn::eeg(Decimal::new(4, 0))
            .under_para51_from(Some(time::macros::date!(2020 - 01 - 01))),
        sharing: None,
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
        shiftable: vec![AssetId::new("spuelmaschine").unwrap()],
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
        // The first slot the car is gone.
        departure: h.get(11).unwrap().next(),
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
        // The first slot the car is gone.
        departure: h.get(23).unwrap().next(),
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
        // The first slot the car is gone.
        departure: h.get(23).unwrap().next(),
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
        // The first slot the car is gone.
        departure: h.get(3).unwrap().next(),
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
        // The first slot the car is gone.
        departure: h.get(23).unwrap().next(),
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
        // The first slot the car is gone.
        departure: h.get(7).unwrap().next(),
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
        // The first slot the car is gone.
        departure: h.get(11).unwrap().next(),
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

// ── Shiftable appliances (S2 PPBC) ──────────────────────────────────────────

/// A dishwasher: a heating quarter hour, four of washing, a heating one to dry.
///
/// Deliberately *shaped* rather than flat, because the shape is the whole reason
/// the model carries a programme instead of a duration and an average.
fn h_ct() -> [i64; 12] {
    // Dear, cheap, dear — four quarter hours each.
    [40, 40, 40, 40, 5, 5, 5, 5, 40, 40, 40, 40]
}

fn dishwasher() -> Programme {
    Programme::from_steps([
        Power::from_kw(2.0),
        Power::from_kw(0.2),
        Power::from_kw(0.2),
        Power::from_kw(0.2),
        Power::from_kw(0.2),
        Power::from_kw(1.8),
    ])
}

#[test]
fn a_programme_runs_in_the_cheapest_window_its_deadline_allows() {
    // Twelve quarter hours: the first four are dear, the next four cheap, the
    // last four dear again. A six-slot programme has one obviously right place.
    let h = horizon(12);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let problem = Problem::new(h, &p, &pv, &load).with_shiftable(
        hems_optimizer::ShiftableRun::before(dishwasher(), h.get(11).unwrap()).worth(5.0),
    );
    let solved = solve(&problem, &names(), T0).expect("a plan");

    let start = solved
        .flows
        .iter()
        .position(|f| f.shiftable > Power::ZERO)
        .expect("the machine runs");
    assert!(
        (3..=6).contains(&start),
        "the programme should straddle the cheap hours, started at {start}"
    );

    // Every kilowatt-hour of the programme is accounted for, once.
    let ran: f64 = solved.flows.iter().map(|f| f.shiftable.kw() * 0.25).sum();
    assert!(
        (ran - dishwasher().energy().kwh()).abs() < 1e-6,
        "the whole programme ran exactly once: {ran} kWh"
    );
}

#[test]
fn a_programme_keeps_its_shape_rather_than_being_smeared() {
    // The failure this exists to catch: a planner that treats a shiftable load
    // as movable energy schedules 400 W into every sunny slot, which is a
    // schedule no dishwasher will carry out.
    let h = horizon(12);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let problem = Problem::new(h, &p, &pv, &load).with_shiftable(
        hems_optimizer::ShiftableRun::before(dishwasher(), h.get(11).unwrap()).worth(5.0),
    );
    let solved = solve(&problem, &names(), T0).expect("a plan");

    let start = solved
        .flows
        .iter()
        .position(|f| f.shiftable > Power::ZERO)
        .expect("the machine runs");
    for (offset, step) in dishwasher().steps.iter().enumerate() {
        let actual = solved.flows[start + offset].shiftable;
        assert!(
            (actual - *step).abs() < Power::new(1.0),
            "slot {offset} of the programme drew {actual} instead of {step}"
        );
    }
}

#[test]
fn a_window_too_tight_for_the_programme_costs_its_price_rather_than_the_plan() {
    // Four slots of window for a six-slot programme. A hard constraint would
    // return no plan at all; the household is better served by one that says
    // "not this wash".
    let h = horizon(12);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let run = hems_optimizer::ShiftableRun::before(dishwasher(), h.get(4).unwrap()).worth(5.0);
    let problem = Problem::new(h, &p, &pv, &load).with_shiftable(run);
    let solved = solve(&problem, &names(), T0).expect("a plan, not an infeasibility");

    assert!(
        solved.flows.iter().all(|f| f.shiftable == Power::ZERO),
        "there is nowhere to put it"
    );
    let cost = solved.plan.expected_cost.expect("a cost");
    assert!(
        cost.unserved_eur >= 5.0,
        "and the report says what was given up: {cost:?}"
    );
}

#[test]
fn the_earliest_start_is_respected() {
    let h = horizon(24);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    // The cheap hours are slots 4..8; the household is out until slot 10.
    let run = hems_optimizer::ShiftableRun::before(dishwasher(), h.get(23).unwrap())
        .not_before(h.get(10).unwrap())
        .worth(5.0);
    let problem = Problem::new(h, &p, &pv, &load).with_shiftable(run);
    let solved = solve(&problem, &names(), T0).expect("a plan");

    let start = solved
        .flows
        .iter()
        .position(|f| f.shiftable > Power::ZERO)
        .expect("the machine runs");
    assert!(start >= 10, "started at {start}, before it was allowed to");
}

#[test]
fn moving_a_programme_into_the_cheap_hours_is_what_the_saving_is_made_of() {
    // The baseline presses start when the machine is loaded; the plan waits for
    // the cheap window. Nothing else differs, so the whole difference is the
    // shift — and it is charged on both sides if it cannot happen.
    let h = horizon(12);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let problem = Problem::new(h, &p, &pv, &load).with_shiftable(
        hems_optimizer::ShiftableRun::before(dishwasher(), h.get(11).unwrap()).worth(5.0),
    );
    let solved = solve(&problem, &names(), T0).expect("a plan");

    let saving = solved
        .plan
        .expected_saving_eur()
        .expect("both sides priced");
    assert!(
        saving > 0.0,
        "shifting the wash into the cheap hours has to be worth something: {saving}"
    );
    // …and neither side is charged for a wash it did do.
    let cost = solved.plan.expected_cost.expect("a cost");
    let baseline = solved.plan.baseline_cost.expect("a baseline");
    assert_eq!(cost.unserved_eur, 0.0);
    assert_eq!(baseline.unserved_eur, 0.0);
}

#[test]
fn two_appliances_do_not_both_take_the_same_cheapest_slot_for_free() {
    // Both want the cheap window; the model has to place both programmes and
    // account for both, which is the property a single-appliance implementation
    // silently loses.
    let h = horizon(16);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let names = AssetNames {
        shiftable: vec![
            AssetId::new("spuelmaschine").unwrap(),
            AssetId::new("waschmaschine").unwrap(),
        ],
        ..names()
    };
    let problem = Problem::new(h, &p, &pv, &load)
        .with_shiftable(
            hems_optimizer::ShiftableRun::before(dishwasher(), h.get(15).unwrap()).worth(5.0),
        )
        .with_shiftable(
            hems_optimizer::ShiftableRun::before(
                Programme::uniform(Power::from_kw(1.0), 4),
                h.get(15).unwrap(),
            )
            .worth(5.0),
        );
    let solved = solve(&problem, &names, T0).expect("a plan");

    let ran: f64 = solved.flows.iter().map(|f| f.shiftable.kw() * 0.25).sum();
    let expected = dishwasher().energy().kwh() + 1.0;
    assert!(
        (ran - expected).abs() < 1e-6,
        "both programmes ran exactly once: {ran} against {expected} kWh"
    );
    // Both appear in the plan, so a household can see which is which.
    for slot in &solved.plan.slots {
        assert_eq!(
            slot.targets
                .iter()
                .filter(|t| names.shiftable.contains(&t.asset))
                .count(),
            2,
        );
    }
}

// ── Uncertainty: scenarios and the tail ─────────────────────────────────────

/// A forecast that is genuinely uncertain: a median with a wide band around it.
fn uncertain(h: Horizon, median: f64, spread: f64, active: &[usize]) -> Forecast {
    Forecast {
        slots: h
            .slots()
            .enumerate()
            .map(|(i, s)| {
                let m = if active.contains(&i) { median } else { 0.0 };
                (s, Band::relative(m, spread))
            })
            .collect(),
    }
}

#[test]
fn a_single_future_is_the_model_the_planner_always_had() {
    // The property that makes `Risk::deterministic` an honest comparison rather
    // than a differently-conditioned model: with one future and no weight on the
    // tail, not one variable or row of the scenario machinery is declared, and
    // the plan is the one the deterministic planner produced.
    let h = horizon(12);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = uncertain(h, 4000.0, 0.5, &[4, 5, 6, 7]);
    let load = flat(h, 500.0);

    let problem = |risk| {
        Problem::new(h, &p, &pv, &load)
            .with_battery(battery(10.0, 0.3, 0.0))
            .with_risk(risk)
    };
    let plain = solve(&problem(Risk::deterministic()), &names(), T0).expect("a plan");
    let explicit = solve(
        &problem(Risk::at_quantile(
            hems_optimizer::Quantile::P50,
            hems_optimizer::Quantile::P50,
        )),
        &names(),
        T0,
    )
    .expect("a plan");
    assert_eq!(plain.flows, explicit.flows);
}

#[test]
fn a_plan_over_three_futures_commits_between_what_the_pessimist_and_the_optimist_would() {
    // What "it prices all three" means, as a decision rather than a number.
    //
    // The sun is uncertain from the very first quarter hour, so the optimist and
    // the pessimist disagree about what the battery should do *now* — and now is
    // the one slot the plan has to commit (`non_anticipativity`). A plan built on
    // three futures cannot take either side: its first slot lies between them,
    // which is the whole content of reading a band as a distribution instead of
    // picking a point on it.
    //
    // A single quantile, by contrast, *is* one of those sides. That is the
    // difference between a robustness knob and a stochastic program, and it is
    // why `ScenarioSet::Quantile` is kept only as the old behaviour.
    let h = horizon(16);
    let ct = [30_i64; 16];
    let p = prices(h, &ct);
    //
    // The sun is uncertain in the **first slot only**, and that is what makes
    // the measurement possible rather than merely plausible. With a flat tariff
    // and no wear, *when* a surplus is stored is undetermined — charging now and
    // charging in an hour cost the same — so two backends can return different,
    // equally optimal plans and the first slot would say nothing about risk. A
    // surplus that exists only now must be taken now or exported at the feed-in
    // tariff, which pins the first slot to the optimum on any backend.
    let pv = uncertain(h, 6000.0, 0.9, &[0]);
    let load = flat(h, 500.0);

    let first_slot = |risk| -> f64 {
        let problem = Problem::new(h, &p, &pv, &load)
            .with_battery(battery(10.0, 0.2, 0.0))
            .with_risk(risk);
        let solved = solve(&problem, &names(), T0).expect("a plan");
        solved.flows[0].battery_charge.kw() - solved.flows[0].battery_discharge.kw()
    };

    let pessimist = first_slot(Risk::at_quantile(
        hems_optimizer::Quantile::P10,
        hems_optimizer::Quantile::P90,
    ));
    let optimist = first_slot(Risk::at_quantile(
        hems_optimizer::Quantile::P90,
        hems_optimizer::Quantile::P10,
    ));
    let three = first_slot(Risk {
        cvar_weight: 0.0,
        ..Risk::hedged()
    });

    assert!(
        optimist > pessimist + 0.1,
        "the two extremes have to disagree for the test to mean anything: \
         {pessimist:.2} against {optimist:.2} kW"
    );
    assert!(
        (pessimist - 1e-6..=optimist + 1e-6).contains(&three),
        "three futures commit between the two extremes: {three:.2} kW is outside \
         [{pessimist:.2}, {optimist:.2}]"
    );
}

#[test]
fn the_tail_pulls_the_commitment_towards_the_pessimist() {
    // …and weight on the tail moves it towards the dull future, which is the
    // whole of what `cvar_weight` buys. A knob that changed nothing would be a
    // knob nobody should ship.
    let h = horizon(16);
    let ct = [30_i64; 16];
    let p = prices(h, &ct);
    // Surplus in the first slot only, for the reason the previous test gives:
    // with a flat tariff the *timing* of storing it is otherwise undetermined.
    let pv = uncertain(h, 6000.0, 0.9, &[0]);
    let load = flat(h, 500.0);

    let first_slot = |risk| -> f64 {
        let problem = Problem::new(h, &p, &pv, &load)
            .with_battery(battery(10.0, 0.2, 0.0))
            .with_risk(risk);
        let solved = solve(&problem, &names(), T0).expect("a plan");
        solved.flows[0].battery_charge.kw() - solved.flows[0].battery_discharge.kw()
    };

    let neutral = first_slot(Risk {
        cvar_weight: 0.0,
        ..Risk::hedged()
    });
    let hedged = first_slot(Risk {
        cvar_weight: 1.0,
        ..Risk::hedged()
    });
    assert!(
        hedged <= neutral + 1e-6,
        "weight on the tail cannot make the plan *more* optimistic about the sun: \
         {hedged:.3} against {neutral:.3} kW"
    );
    // …and it has to move something. Without this the test passes on a plan
    // where both are zero, which is exactly the knob-that-does-nothing it exists
    // to rule out.
    assert!(
        neutral > 0.1,
        "the risk-neutral plan should be storing the surplus it can see: \
         {neutral:.3} kW"
    );
}

#[test]
fn weight_on_the_tail_never_makes_the_expected_cost_better() {
    // The defining property of a hedge, and the one that makes it *measurable*:
    // it costs something in expectation. A "risk-aware" plan that were cheaper
    // on average as well would not be a hedge, it would be a bug in the
    // risk-neutral objective.
    let h = horizon(24);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = uncertain(h, 5000.0, 0.7, &(8..16).collect::<Vec<_>>());
    let load = flat(h, 400.0);
    let draw: Vec<f64> = (0..24).map(|k| if k >= 20 { 900.0 } else { 0.0 }).collect();

    let cost_of = |risk| {
        let problem = Problem::new(h, &p, &pv, &load)
            .with_dhw(
                DhwModel {
                    stored_now: Energy::from_kwh(0.4),
                    ..DhwModel::tank(Energy::from_kwh(5.0), Power::from_kw(1.5))
                },
                &draw,
            )
            .with_risk(risk);
        solve(&problem, &names(), T0)
            .expect("a plan")
            .plan
            .expected_cost
            .expect("a cost")
            .total()
    };

    let neutral = cost_of(Risk {
        cvar_weight: 0.0,
        ..Risk::hedged()
    });
    let hedged = cost_of(Risk::hedged());
    assert!(
        hedged >= neutral - 1e-6,
        "a hedge costs something in expectation: {hedged} against {neutral}"
    );
}

#[test]
fn the_first_slot_is_one_decision_however_many_futures_there_are() {
    // Non-anticipativity. The arbiter is about to commit the next fifteen
    // minutes; a plan that gave three different answers for it would be three
    // plans and a coin. Everything after it is recourse, which is what makes
    // hedging affordable.
    let h = horizon(16);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = uncertain(h, 4000.0, 0.8, &(0..12).collect::<Vec<_>>());
    let load = flat(h, 600.0);
    let problem = Problem::new(h, &p, &pv, &load)
        .with_battery(battery(10.0, 0.5, 0.05))
        .with_risk(Risk::hedged());
    let solved = solve(&problem, &names(), T0).expect("a plan");

    // The reported flows are the central future's; the property under test is
    // that its first slot is a commitment the plan can actually keep, so the
    // target the arbiter reads is finite and inside the battery's own rating.
    let first = solved.flows[0];
    assert!(first.battery_charge.is_finite() && first.battery_discharge.is_finite());
    let target = solved.plan.slots[0]
        .target(&AssetId::new("battery").unwrap())
        .expect("the battery is commanded");
    assert!(
        (target.power - (first.battery_charge - first.battery_discharge)).abs() < Power::new(1.0),
        "the plan's first slot and the flows it reports are the same decision"
    );
}

#[test]
fn swansons_weights_are_a_probability_distribution() {
    // Three futures, 0,3 / 0,4 / 0,3 — Swanson's rule on a P10/P50/P90 band, so
    // the scenario set costs nothing to produce beyond the band the forecast
    // already publishes.
    let futures = hems_optimizer::ScenarioSet::Swanson.realisations();
    assert_eq!(futures.len(), 3);
    let total: f64 = futures.iter().map(|r| r.probability).sum();
    assert!((total - 1.0).abs() < 1e-12, "{total}");
    // And the misfortune is comonotone: the pessimistic future is dull *and*
    // hungry, which is what a household's bad day actually looks like.
    assert_eq!(futures[0].pv, hems_optimizer::Quantile::P10);
    assert_eq!(futures[0].load, hems_optimizer::Quantile::P90);

    // A single-future set is still a distribution.
    for set in [
        hems_optimizer::ScenarioSet::Median,
        hems_optimizer::ScenarioSet::Quantile {
            pv: hems_optimizer::Quantile::P10,
            load: hems_optimizer::Quantile::P90,
        },
    ] {
        let r = set.realisations();
        assert_eq!(r.len(), 1);
        assert!((r[0].probability - 1.0).abs() < 1e-12);
    }
}

#[test]
fn a_nonsense_risk_setting_falls_back_rather_than_exploding() {
    // `α → 1` makes `1/(1 − α)` infinite and a weight outside `[0, 1]` is a
    // caller error. Neither may reach the solver: an objective with an infinite
    // coefficient is not a worse plan, it is no plan at all.
    let h = horizon(8);
    let ct = h_ct();
    let p = prices(h, &ct);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let problem = Problem::new(h, &p, &pv, &load).with_risk(Risk {
        scenarios: hems_optimizer::ScenarioSet::Swanson,
        cvar_alpha: f64::NAN,
        cvar_weight: 7.0,
    });
    let solved = solve(&problem, &names(), T0).expect("a plan, not an explosion");
    assert!(solved.flows.iter().all(|f| f.grid_import.is_finite()));
}

#[test]
fn a_band_that_narrows_to_nothing_costs_no_premium() {
    // The premium a scenario plan pays is a function of the **width of the
    // band**, and this is that statement as a limit: shrink the band and the
    // three futures become one, so there is nothing left to insure and the plan
    // costs exactly what the deterministic one costs.
    //
    // It is the reason `hemsd risk` reports what it does, and why a calibrated
    // band is a *precondition* for the scenario planner rather than an
    // improvement to it. A band that covers 93 % of outcomes against the 80 % it
    // promises makes the pessimistic future worse than any day that happens, and
    // a plan that insures against it pays a premium for a claim nobody makes.
    //
    // Compared on **cost**, not on a decision: with surplus in every slot there
    // are many equally good times to charge a battery, and "the two plans chose
    // differently" would be a statement about the solver's tie-breaking.
    let h = horizon(16);
    let ct = h_ct();
    let p = prices(h, &ct);
    let load = flat(h, 500.0);

    let cost_of = |spread: f64, risk| -> f64 {
        let pv = uncertain(h, 6000.0, spread, &(0..10).collect::<Vec<_>>());
        let problem = Problem::new(h, &p, &pv, &load)
            .with_battery(battery(10.0, 0.2, 0.05))
            .with_risk(risk);
        solve(&problem, &names(), T0)
            .expect("a plan")
            .plan
            .expected_cost
            .expect("a cost")
            .total()
    };
    let premium = |spread: f64| -> f64 {
        cost_of(spread, Risk::hedged()) - cost_of(spread, Risk::deterministic())
    };

    let narrow = premium(0.0);
    assert!(
        narrow.abs() < 0.01,
        "a band of no width is a certainty, and there is no premium: {narrow:.4} €"
    );
}

#[test]
fn tightness_is_the_share_of_a_sessions_own_capacity_the_promise_uses() {
    let session = |now_kwh: f64, target_kwh: f64, slots: i64| EvSession {
        energy_now: Energy::from_kwh(now_kwh),
        energy_target: Energy::from_kwh(target_kwh),
        capacity: Energy::from_kwh(60.0),
        max_charge: Power::from_kw(11.0),
        min_charge: Power::from_kw(4.14),
        efficiency: 1.0,
        arrival: None,
        departure: Slot::containing(T0 + time::Duration::minutes(15 * slots)),
    };
    let now = Slot::containing(T0);

    // A car that is already full asks nothing of the session it has left.
    assert_eq!(session(40.0, 40.0, 40).tightness(now), 0.0);

    // Fourteen hours to take 20 kWh at 11 kW: a seventh of what the cable could
    // deliver, and nothing whatever to insure.
    let overnight = session(18.0, 38.0, 56).tightness(now);
    assert!(overnight < 0.15, "{overnight}");

    // Three hours to take 13 kWh: two fifths, and the evening the reference
    // `deadline` day is built around.
    let teatime = session(18.0, 31.0, 12).tightness(now);
    assert!((0.35..0.45).contains(&teatime), "{teatime}");

    // A departure that has already been and gone leaves no capacity at all, and
    // the answer is one rather than a division by zero.
    assert_eq!(session(10.0, 40.0, 0).tightness(now), 1.0);
}

// ── The inputs have to describe the horizon they are planned over ───────────

#[test]
fn a_forecast_that_stops_short_of_the_horizon_is_refused_rather_than_read_as_zero() {
    // The horizon runs 16 slots; the forecasts cover 8. Read as zero, the last
    // eight slots say the roof is dark *and* the house is empty — a lie in both
    // directions, and one that produces a confident plan nothing complains
    // about. See `SolveError::ForecastTooShort`.
    let h = horizon(16);
    let short = horizon(8);
    let p = prices(h, &[20]);
    let pv = flat(short, 0.0);
    let load = flat(short, 1000.0);

    let err = solve(&Problem::new(h, &p, &pv, &load), &names(), T0).unwrap_err();
    assert!(
        matches!(
            err,
            hems_optimizer::SolveError::ForecastTooShort {
                series: "production",
                covered: 8,
                slots: 16
            }
        ),
        "{err:?}"
    );

    // And the load is named separately, because the two come from different
    // estimators and a caller has to know which one it truncated.
    let pv_full = flat(h, 0.0);
    let err = solve(&Problem::new(h, &p, &pv_full, &load), &names(), T0).unwrap_err();
    assert!(
        matches!(
            err,
            hems_optimizer::SolveError::ForecastTooShort { series: "load", .. }
        ),
        "{err:?}"
    );
}

#[test]
fn a_price_stack_for_the_wrong_hours_is_refused_and_a_short_one_is_not() {
    let h = horizon(8);
    let pv = flat(h, 0.0);
    let load = flat(h, 1000.0);

    // Built for a day that starts an hour later: every entry is the right shape
    // and the wrong hour, which is the one misalignment nothing downstream can
    // notice. The stack carries its own slot, so this costs one comparison.
    let elsewhere = prices(Horizon::new(T0 + time::Duration::hours(1), 8), &[20]);
    let err = solve(&Problem::new(h, &elsewhere, &pv, &load), &names(), T0).unwrap_err();
    assert!(
        matches!(
            err,
            hems_optimizer::SolveError::PricesMisaligned { position: 0, .. }
        ),
        "{err:?}"
    );

    // A stack that simply stops short is *not* an error: a horizon can run past
    // the last published auction, and a flat default out there is an honest
    // answer rather than an invented one.
    let stops_short = prices(horizon(4), &[20]);
    assert!(solve(&Problem::new(h, &stops_short, &pv, &load), &names(), T0).is_ok());
}

// ── A compressor's minimum runtime survives the re-plan ─────────────────────

#[test]
fn a_compressor_that_has_just_started_is_not_stopped_by_the_next_plan() {
    // The defect this pins: the minimum-runtime rows are written against
    // `k − 1`, so they say nothing about the first slot — the only one a
    // receding horizon executes. A box could therefore start the compressor,
    // commit that quarter hour, re-plan against a model with no memory, and stop
    // it again, for ever, with every individual plan feasible.
    let h = horizon(16);
    // Dear from the start, so a planner with a free hand keeps the unit off.
    let p = prices(h, &[40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let outdoor = vec![8.0; 16];

    let plan_with = |compressor: CompressorState| {
        let mut hp = HeatPumpModel::on_off(Power::from_kw(5.0));
        hp.min_on_slots = 3;
        hp.min_off_slots = 3;
        hp.compressor = compressor;
        let t = ThermalModel {
            comfort_min_c: 20.0,
            comfort_max_c: 23.0,
            ..ThermalModel::house(22.5, hp)
        };
        solve(
            &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
            &names(),
            T0,
        )
        .unwrap()
    };

    // A warm house and an expensive hour: with the compressor settled and idle,
    // the plan leaves it alone.
    let idle = plan_with(CompressorState::settled(false));
    assert_eq!(
        idle.flows[0].heat_pump,
        Power::ZERO,
        "nothing should make a warm house heat at 40 ct"
    );

    // The same house, the same hour — but the compressor started one slot ago
    // and owes two more. It has to keep running, and the plan has to say so.
    let just_started = plan_with(CompressorState {
        running: true,
        slots_in_state: 1,
    });
    for k in 0..2 {
        assert!(
            just_started.flows[k].heat_pump > Power::ZERO,
            "slot {k}: a compressor one slot into a three-slot minimum owes two \
             more, and the plan stopped it instead"
        );
    }
    assert_eq!(
        just_started.flows[2].heat_pump,
        Power::ZERO,
        "once the minimum has run the plan is free again, and a warm house at \
         40 ct should stop"
    );
}

#[test]
fn a_settled_compressor_is_free_and_a_modulating_unit_has_no_memory_at_all() {
    let free =
        HeatPumpModel::on_off(Power::from_kw(5.0)).with_compressor(CompressorState::settled(true));
    assert_eq!(free.committed(), None, "a settled unit owes nothing");

    let mut owing = HeatPumpModel::on_off(Power::from_kw(5.0));
    owing.min_on_slots = 4;
    owing.compressor = CompressorState {
        running: true,
        slots_in_state: 1,
    };
    assert_eq!(owing.committed(), Some((true, 3)));

    // A modulating unit has one output range and no cycling to schedule, so the
    // question does not arise however its state field is filled in.
    let mut modulating = HeatPumpModel::modulating(Power::from_kw(5.0));
    modulating.compressor = CompressorState {
        running: true,
        slots_in_state: 0,
    };
    assert_eq!(modulating.committed(), None);
}

// ── The commitment horizon ───────────────────────────────────────────────────

/// Whether the compressor is running in each slot, read off a solved plan.
fn compressor_track(solved: &hems_optimizer::Solved) -> Vec<bool> {
    solved
        .flows
        .iter()
        .map(|f| f.heat_pump > Power::new(1.0))
        .collect()
}

#[test]
fn a_blocked_tail_still_obeys_the_minimum_runtime() {
    // Coarsening is a *restriction* of the feasible set, never a relaxation, so
    // no constraint the model states can be escaped by it. The minimum runtime
    // is the one that would be most obviously embarrassing to lose, because
    // `minimum_runtime` now skips the rows a shared variable makes trivial —
    // and "trivial" has to mean "already implied", not "not built".
    let h = horizon(48);
    let p = prices(h, &[10, 40, 10, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-2.0; 48];
    let mut t = thermal(21.0, false);
    t.heat_pump.min_on_slots = 3;
    t.heat_pump.min_off_slots = 3;

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
        &names(),
        T0,
    )
    .unwrap();

    // Every run of equal states, except the one the horizon cuts off at each
    // end, has to be at least three slots long.
    let on = compressor_track(&solved);
    let mut runs: Vec<(bool, usize)> = Vec::new();
    for state in on {
        match runs.last_mut() {
            Some((s, n)) if *s == state => *n += 1,
            _ => runs.push((state, 1)),
        }
    }
    for (state, length) in runs.iter().skip(1).take(runs.len().saturating_sub(2)) {
        assert!(
            *length >= 3,
            "a {} run of {length} slots against a minimum of three: {runs:?}",
            if *state { "running" } else { "idle" }
        );
    }
}

#[test]
fn the_fine_head_is_where_every_executed_slot_lives() {
    // A receding horizon executes the first slot and throws the rest away, so
    // the head is the only part a coarser tail may not touch. `fine()` decides
    // every slot on its own; the default blocks the tail — and the two have to
    // agree about the slots that will actually be commanded.
    //
    // The price steps on the **hour**, which is where a day-ahead curve steps
    // and where the blocks are anchored. That is the whole claim: an hourly
    // block over an hourly price throws nothing away. What it costs when the
    // price moves faster than the block is the next test.
    let h = horizon(48);
    let p = prices(h, &[10, 10, 10, 10, 40, 40, 40, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-2.0; 48];
    let t = thermal(20.5, false);

    let blocked = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
        &names(),
        T0,
    )
    .unwrap();
    let fine = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_thermal(t, &outdoor)
            .with_commitment_horizon(hems_optimizer::CommitmentHorizon::fine()),
        &names(),
        T0,
    )
    .unwrap();

    assert_eq!(
        compressor_track(&blocked)[0],
        compressor_track(&fine)[0],
        "the slot the arbiter is about to commit is the same either way"
    );
    // And the price of the coarser tail is small enough to be worth what it
    // buys. Both plans are feasible over the same horizon, so the blocked one
    // can only be worse, and this says by how much.
    let cost = |s: &hems_optimizer::Solved| -> f64 {
        s.plan
            .expected_cost
            .as_ref()
            .map_or(0.0, hems_core::prelude::CostBreakdown::total)
    };
    // And the price of the coarser tail is bounded rather than assumed. The
    // blocked plan is feasible for the fine model too, so it can only be worse,
    // and the gap is what an hour of commitment costs: the tail can no longer
    // idle for part of an hour, only for all of it. On the reference winter day
    // it is worth a fraction of a cent against ten minutes of solver time.
    let (blocked_eur, fine_eur) = (cost(&blocked), cost(&fine));
    assert!(
        blocked_eur <= fine_eur * 1.03,
        "blocking the tail cost {blocked_eur:.4} € against {fine_eur:.4} €"
    );
}

#[test]
fn a_price_that_moves_faster_than_the_block_is_what_blocking_costs() {
    // The honest other half, stated as a test so nobody has to discover it: a
    // block can only be free where the thing it is coarsening is constant
    // across it. Alternate the price every quarter hour — four times the
    // resolution the blocks are cut at — and the coarse plan is measurably
    // worse, because the fine one is switching the compressor on every cheap
    // slot and the coarse one cannot.
    //
    // It is not an argument against the default. It is the argument for
    // `CommitmentHorizon::fine()` being reachable, and for the blocks being
    // anchored to the clock the tariff steps on rather than to the plan.
    let h = horizon(48);
    let p = prices(h, &[10, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-2.0; 48];
    let t = thermal(20.5, false);

    let cost = |horizon: hems_optimizer::CommitmentHorizon| -> f64 {
        solve(
            &Problem::new(h, &p, &pv, &load)
                .with_thermal(t, &outdoor)
                .with_commitment_horizon(horizon),
            &names(),
            T0,
        )
        .unwrap()
        .plan
        .expected_cost
        .as_ref()
        .map_or(0.0, hems_core::prelude::CostBreakdown::total)
    };
    let fine = cost(hems_optimizer::CommitmentHorizon::fine());
    let blocked = cost(hems_optimizer::CommitmentHorizon::default());
    assert!(
        blocked > fine,
        "a quarter-hourly price is where a block costs something: \
         {blocked:.4} € against {fine:.4} €"
    );
    // …and a half-hourly block recovers most of it, which is the knob to reach
    // for on a tariff that really does move every quarter hour.
    let half = cost(hems_optimizer::CommitmentHorizon {
        fine_slots: 8,
        block_slots: 2,
    });
    assert!(half < blocked, "{half:.4} € against {blocked:.4} €");
}

#[test]
fn blocking_never_swallows_a_committed_slot() {
    // `heat_pump_binary` pins the slots the compressor's own history has
    // already decided (D65). Pinning the *start* of a block would pin the whole
    // block, so the fine head has to be at least as long as anything the unit
    // still owes — which is what `fine_for` guarantees and what this checks
    // through the plan rather than through the arithmetic.
    let h = horizon(32);
    // Dear now, cheap immediately after: a plan with a free hand would stop the
    // compressor at once.
    let p = prices(h, &[90, 90, 5, 5, 5, 5, 5, 5]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-2.0; 32];
    let mut t = thermal(22.5, false);
    t.heat_pump.min_on_slots = 6;
    t.heat_pump.min_off_slots = 2;
    // Running, and one slot into a six-slot minimum: five are still owed.
    t.heat_pump.compressor = CompressorState {
        running: true,
        slots_in_state: 1,
    };

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
        &names(),
        T0,
    )
    .unwrap();
    let on = compressor_track(&solved);
    assert!(
        on[..5].iter().all(|running| *running),
        "the five slots the compressor still owes are not decisions: {on:?}"
    );
}

#[test]
fn a_modulating_unit_is_untouched_by_the_commitment_grid() {
    // The grid coarsens a *binary*, and a modulating unit has none. Its plan
    // must be identical whatever the grid says, or the knob has leaked into the
    // configuration every household actually runs.
    let h = horizon(48);
    let p = prices(h, &[10, 40, 10, 40]);
    let pv = flat(h, 0.0);
    let load = flat(h, 500.0);
    let outdoor = vec![-2.0; 48];
    let t = thermal(21.0, true);

    let a = solve(
        &Problem::new(h, &p, &pv, &load).with_thermal(t, &outdoor),
        &names(),
        T0,
    )
    .unwrap();
    let b = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_thermal(t, &outdoor)
            .with_commitment_horizon(hems_optimizer::CommitmentHorizon::fine()),
        &names(),
        T0,
    )
    .unwrap();
    for (x, y) in a.flows.iter().zip(&b.flows) {
        assert!(
            (x.heat_pump.get() - y.heat_pump.get()).abs() < 1e-6,
            "{x:?} against {y:?}"
        );
    }
}

// ── § 42c energy sharing ─────────────────────────────────────────────────────

/// The same price stack, inside a community that sells at `ct_per_kwh` net.
fn prices_in_community(h: Horizon, ct: &[i64], community_ct: i64) -> PriceStack {
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
        feed_in: FeedIn::eeg(Decimal::new(4, 0))
            .under_para51_from(Some(time::macros::date!(2020 - 01 - 01))),
        sharing: Some(hems_tariff::tariff::SharingTariff::at(Decimal::new(
            community_ct,
            0,
        ))),
        standing_charge_eur_per_year: Decimal::ZERO,
    };
    PriceStack::build(&tariff, h)
}

#[test]
fn a_community_share_moves_the_flexible_load_into_the_neighbours_daylight() {
    // The whole behavioural point of § 42c, and the thing `hems-grid::sharing`
    // could settle but the planner could not act on. The household's own roof is
    // dark; the community's is not, and its generation is offered in slots 4..8
    // only. The tank has a whole horizon to heat in and every reason to do it
    // there.
    let h = horizon(16);
    // Flat energy price, so the *only* thing that can move the load is the
    // community share. Anything that shifted here without § 42c would be
    // shifting for a reason this test did not put in.
    let p = prices_in_community(h, &[30], 5);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let share: Vec<f64> = (0..16)
        .map(|k| if (4..8).contains(&k) { 3000.0 } else { 0.0 })
        .collect();
    let tank = DhwModel {
        stored_now: Energy::from_kwh(0.5),
        ..DhwModel::tank(Energy::from_kwh(5.0), Power::from_kw(2.0))
    };

    let solved = solve(
        &Problem::new(h, &p, &pv, &load)
            .with_dhw(tank, &[0.0; 16])
            .in_community(&share),
        &names(),
        T0,
    )
    .unwrap();

    let inside: f64 = (4..8).map(|k| solved.flows[k].dhw.get()).sum();
    let outside: f64 = (0..16)
        .filter(|k| !(4..8).contains(k))
        .map(|k| solved.flows[k].dhw.get())
        .sum();
    assert!(
        inside > outside,
        "the tank should heat inside the community's window: {inside:.0} W in, \
         {outside:.0} W out"
    );
    // And the allocation is reported, because a discount the household is not
    // shown is a discount nobody can check.
    let allocated: f64 = solved.flows.iter().map(|f| f.shared_import.get()).sum();
    assert!(allocated > 0.0, "nothing was allocated at all");
}

#[test]
fn a_member_is_never_allocated_more_than_it_drew_or_more_than_its_share() {
    // The two caps that make this an allocation of *consumption* rather than a
    // paper transfer — the same pair `hems_grid::sharing` settles after the fact.
    let h = horizon(12);
    let p = prices_in_community(h, &[30], 5);
    let pv = flat(h, 0.0);
    let load = flat(h, 400.0);
    let share = vec![9000.0; 12];

    let solved = solve(
        &Problem::new(h, &p, &pv, &load).in_community(&share),
        &names(),
        T0,
    )
    .unwrap();
    for (k, f) in solved.flows.iter().enumerate() {
        assert!(
            f.shared_import <= f.grid_import + Power::new(1e-6),
            "slot {k}: allocated {} against an import of {}",
            f.shared_import,
            f.grid_import
        );
        assert!(
            f.shared_import.get() <= share[k] + 1e-6,
            "slot {k}: allocated more than the Aufteilungsschlüssel offered"
        );
    }
}

#[test]
fn a_community_dearer_than_the_supplier_is_priced_at_no_advantage() {
    // The concave case, and the one an optional-discount model would get wrong
    // in the unsafe direction. A community that charges more than the supplier
    // cannot be declined — the Aufteilungsschlüssel applies whatever anybody
    // prefers — so the honest answer is to claim no advantage from it rather
    // than to invent one, and above all not to let the plan believe it can opt
    // out. Nothing is allocated, and the plan is the one it would have made
    // without a community at all.
    let h = horizon(12);
    let dear = prices_in_community(h, &[20], 40);
    let plain = prices(h, &[20]);
    let pv = flat(h, 0.0);
    let load = flat(h, 400.0);
    let share = vec![9000.0; 12];

    let with_community = solve(
        &Problem::new(h, &dear, &pv, &load).in_community(&share),
        &names(),
        T0,
    )
    .unwrap();
    let without = solve(&Problem::new(h, &plain, &pv, &load), &names(), T0).unwrap();

    assert!(
        with_community
            .flows
            .iter()
            .all(|f| f.shared_import == Power::ZERO),
        "a dearer community must not look like a discount"
    );
    for (a, b) in with_community.flows.iter().zip(&without.flows) {
        assert!((a.grid_import.get() - b.grid_import.get()).abs() < 1e-6);
    }
}

#[test]
fn the_baseline_is_in_the_same_community_as_the_plan() {
    // A household joins a community and then does nothing about it: the
    // Aufteilungsschlüssel still allocates it whatever its unmanaged draw
    // overlaps. Crediting the plan with the *membership* rather than with the
    // shifting is the same asymmetry as measuring a saving against a household
    // that ignored the network operator — so the baseline's own bill has to fall
    // when a community appears.
    let h = horizon(16);
    let pv = flat(h, 0.0);
    let load = flat(h, 300.0);
    let share = vec![2000.0; 16];
    let tank = DhwModel {
        stored_now: Energy::from_kwh(0.5),
        ..DhwModel::tank(Energy::from_kwh(5.0), Power::from_kw(2.0))
    };
    let baseline = |stack: &PriceStack, community: &[f64]| -> f64 {
        solve(
            &Problem::new(h, stack, &pv, &load)
                .with_dhw(tank, &[0.0; 16])
                .in_community(community),
            &names(),
            T0,
        )
        .unwrap()
        .plan
        .baseline_cost
        .as_ref()
        .map_or(0.0, hems_core::prelude::CostBreakdown::total)
    };
    let plain = prices(h, &[30]);
    let community = prices_in_community(h, &[30], 5);
    assert!(
        baseline(&community, &share) < baseline(&plain, &[]) - 1e-6,
        "the unmanaged household is allocated too"
    );
}
