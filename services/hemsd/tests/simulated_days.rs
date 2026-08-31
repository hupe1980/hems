//! Whole days, end to end.
//!
//! These are the tests that would have caught every integration bug the unit
//! tests could not: a plan that respects a limit the guard then re-derives
//! differently, a battery model that disagrees with the optimiser's, a
//! forecast that is not in the same units as the thing it forecasts.

use hems_core::prelude::{CapRelief, Energy, Power, Soc};
use hems_flex::ControlType;
use hems_grid::mispel::{Basisfall, RuleSet, abgrenzung_month};
use hems_grid::sharing::{Aufteilung, Community, Member, allocate_by};
use hemsd::{HouseholdConfig, Scenario, run};
use rust_decimal::Decimal;
use time::Duration;

#[test]
fn a_winter_day_with_a_grid_event_stays_lawful_and_still_saves_money() {
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();

    // The § 14a promise, checked minute by minute against the netzwirksamer
    // Leistungsbezug rather than against intent.
    assert!(
        r.grid_event_respected,
        "the network operator's limit was exceeded by {:.0} W",
        r.worst_overshoot_w
    );
    assert_eq!(r.limited_minutes, 90, "17:00 to 18:30");
    // The three minutes of `init` before the manager concluded that nothing was
    // controlling it are *not* a § 14a event: the network operator said nothing.
    // Counting them as one reports a reduction that never happened.
    assert_eq!(r.failsafe_minutes, 3, "the `init` state, and nothing else");

    // The car still got what it was promised, § 14a event or not.
    assert!(
        r.ev_charged_kwh > 19.0,
        "the car needed 20 kWh, got {:.1}",
        r.ev_charged_kwh
    );

    // And doing all that was cheaper than not thinking about it.
    assert!(r.saving_eur() > 1.0, "saved only {:.2} €", r.saving_eur());
    assert!(r.cost.total() < r.baseline.total());
}

#[test]
fn a_summer_day_runs_the_house_almost_entirely_off_its_own_roof() {
    let r = run(&Scenario::summer_surplus(HouseholdConfig::default())).unwrap();
    assert!(
        r.produced_kwh > 40.0,
        "a clear June day should yield more than {:.1} kWh",
        r.produced_kwh
    );
    assert!(
        r.self_sufficiency > 0.85,
        "self-sufficiency was only {:.0} %",
        r.self_sufficiency * 100.0
    );
    assert!(r.exported_kwh > 0.0, "the surplus has to go somewhere");
    assert!(r.saving_eur() > 3.0, "saved only {:.2} €", r.saving_eur());
}

#[test]
fn a_steuerbox_that_stops_talking_mid_event_puts_the_house_into_the_failsafe() {
    // The outage has to begin *after* the limit is in force. A control box that
    // goes quiet having never said anything does not trigger a failsafe — the
    // manager concluded 120 seconds in that nothing was controlling it
    // (`[LPC-906]`), and that is the right answer for a box nobody configured.
    let mut scenario = Scenario::winter_with_grid_event(HouseholdConfig::default());
    scenario.steuerbox_outage = Some((Duration::minutes(17 * 60 + 30), Duration::hours(23)));
    let r = run(&scenario).unwrap();

    assert!(
        r.failsafe_minutes > 60,
        "spent only {} minutes in the failsafe",
        r.failsafe_minutes
    );
    // The failsafe value is a limit like any other, and it is still respected.
    assert!(
        r.grid_event_respected,
        "overshot by {:.0} W while in the failsafe",
        r.worst_overshoot_w
    );
}

#[test]
fn pricing_battery_wear_moves_less_energy_through_the_battery() {
    // The finding of `specs/arxiv/arxiv-2606.16051.pdf` reproduced end to end:
    // a cost-only optimiser cycles a battery for spreads that do not pay for the
    // damage. The saving looks better and the battery is worse off.
    let cost_only = HouseholdConfig {
        battery_wear_eur_per_kwh: 0.0,
        ..HouseholdConfig::default()
    };
    let realistic = HouseholdConfig::default();

    let steep = HouseholdConfig {
        battery_wear_eur_per_kwh: 1.0,
        ..HouseholdConfig::default()
    };

    let a = run(&Scenario::winter_with_grid_event(cost_only)).unwrap();
    let b = run(&Scenario::winter_with_grid_event(realistic)).unwrap();
    let c = run(&Scenario::winter_with_grid_event(steep)).unwrap();

    // Pricing wear can never make the plan cycle *more*.
    assert!(
        b.battery_throughput_kwh <= a.battery_throughput_kwh + 1e-6,
        "pricing wear increased throughput: {:.2} vs {:.2} kWh",
        b.battery_throughput_kwh,
        a.battery_throughput_kwh
    );
    // And a wear cost above any spread on the day stops it cycling for price at
    // all — what is left is only what the roof forces into it.
    assert!(
        c.battery_throughput_kwh < a.battery_throughput_kwh - 0.5,
        "an absurd wear cost should visibly suppress cycling: {:.2} vs {:.2} kWh",
        c.battery_throughput_kwh,
        a.battery_throughput_kwh
    );
}

#[test]
fn a_backup_reserve_survives_a_whole_day_of_optimisation() {
    let config = HouseholdConfig {
        reserve_soc: Soc::new(0.4).unwrap(),
        battery_kwh: Energy::from_kwh(10.0),
        ..HouseholdConfig::default()
    };
    let r = run(&Scenario::winter_with_grid_event(config)).unwrap();
    // The house still runs, and the reserve was not spent on a cheap hour.
    assert!(r.imported_kwh > 0.0);
    assert!(r.grid_event_respected);
    // The promise, checked minute by minute rather than plan by plan. The
    // planner respecting a reserve is not enough: the arbiter tracks surplus and
    // corrects inside the slot, and it is the guard that has to stop it there.
    assert!(
        r.battery_soc_min >= 0.4 - 1e-3,
        "the backup reserve was spent: fell to {:.1} %",
        r.battery_soc_min * 100.0
    );
}

#[test]
fn a_house_with_no_photovoltaics_still_plans_and_still_complies() {
    let config = HouseholdConfig {
        pv_kwp: Power::from_kw(0.0),
        pv_ac_nominal: Power::from_kw(0.0),
        ..HouseholdConfig::default()
    };
    let r = run(&Scenario::winter_with_grid_event(config)).unwrap();
    assert_eq!(r.produced_kwh, 0.0);
    assert!(r.grid_event_respected);
    assert!(r.ev_charged_kwh > 19.0, "the car still has to be charged");
}

#[test]
fn the_same_day_run_twice_gives_the_same_answer() {
    // Determinism is not a nicety: without it, a regression in the planner is
    // indistinguishable from noise, and no saving figure can be reproduced.
    let a = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    let b = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert!((a.cost.total() - b.cost.total()).abs() < 1e-9);
    assert!((a.imported_kwh - b.imported_kwh).abs() < 1e-9);
    assert_eq!(a.limited_minutes, b.limited_minutes);
}

#[test]
fn the_days_meter_registers_feed_the_mispel_flow_bookkeeping() {
    // The half of the promise that is not control: a manager that decides when
    // to charge a battery from the grid but cannot say afterwards how much of
    // its feed-in was grey has done half the job. This runs the day's own
    // quarter-hour registers through the Abgrenzungsoption of MiSpeL Anlage 1
    // and checks that what comes out is arithmetically consistent with what went
    // in — the integration the two halves would otherwise never make.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert_eq!(r.quarter_hours.len(), 96, "a day is 96 quarter hours");

    // A3: one meter over the battery and the charge point together, which is
    // what this household has.
    let a = abgrenzung_month(
        Basisfall::A3,
        RuleSet::Arbeitsstand20260805,
        &r.quarter_hours,
    )
    .expect("the day's registers are a valid input");

    // The register sums are the day's own KPIs, to the last watt-hour.
    let close = |a: Decimal, b: f64| (a - Decimal::try_from(b).unwrap()).abs() < Decimal::new(1, 3);
    assert!(close(a.grid_draw, r.imported_kwh), "(3) vs imported");
    assert!(close(a.grid_feed_in, r.exported_kwh), "(4) vs exported");

    // Every figure the Festlegung defines as non-negative is.
    for (name, value) in [
        ("(9) grid charged", a.grid_charged),
        ("(10) plant charged", a.plant_charged),
        ("(13) considered feed-in", a.device_feed_in_considered),
        ("(16) settleable", a.settleable_feed_in),
        ("(20) levy reducing", a.levy_reducing),
        ("(21) levied draw", a.levied_grid_draw),
        ("(32) supported", a.supported_feed_in),
    ] {
        assert!(value >= Decimal::ZERO, "{name} came out negative: {value}");
    }
    // (20)'s MIN: the levy reduction can never exceed the draw it reduces.
    assert!(a.levy_reducing <= a.grid_draw);
    assert_eq!(a.levied_grid_draw, a.grid_draw - a.levy_reducing);
    // A2/A3 have no privilegeable storage losses, `[MiSpeL A1 (19)A2,A3]`.
    assert_eq!(a.privilegeable_losses, Decimal::ZERO);
}

#[test]
fn a_summer_day_of_negative_prices_earns_no_support_for_those_quarter_hours() {
    // § 51 EEG through `[MiSpeL A1 (24)]`: the anzulegender Wert is zero while
    // the day-ahead price is negative, so that feed-in counts for the levies and
    // for nothing else. The June scenario has four such hours on purpose.
    //
    // The **capped** day is the one that has to be used for it, and the reason is
    // worth writing down: a household with a 10 kWh store and a car on the cable
    // now absorbs its way through a negative-price hour rather than feeding into
    // it, which is the planner doing exactly what § 51 is meant to make it do.
    // A test that needs feed-in during those hours therefore needs a roof its
    // house cannot absorb — which is what the § 9 EEG day is.
    let r = run(&Scenario::summer_capped(&HouseholdConfig::default())).unwrap();
    let unsupported: Vec<_> = r
        .quarter_hours
        .iter()
        .filter(|q| q.anzulegender_wert.is_zero() && q.grid_feed_in > Decimal::ZERO)
        .collect();
    assert!(
        !unsupported.is_empty(),
        "the June day is meant to feed in during its negative-price hours"
    );

    let a = abgrenzung_month(
        Basisfall::A3,
        RuleSet::Arbeitsstand20260805,
        &r.quarter_hours,
    )
    .unwrap();
    let lost: Decimal = unsupported.iter().map(|q| q.grid_feed_in).sum();
    assert!(
        a.supported_feed_in <= a.grid_feed_in - lost + Decimal::new(1, 3),
        "support was claimed for a negative-price quarter hour"
    );
}

#[test]
fn a_planner_that_re_solves_too_slowly_leaves_the_house_without_one() {
    // The arbiter drops a plan older than `max_plan_age`, because a stale plan
    // was computed against prices and forecasts that have moved on. So the
    // planner has to re-solve *faster* than that tolerance — and for a long time
    // this ran at thirty minutes against a twenty-minute tolerance, which is a
    // ten-minute hole every half hour in which the house quietly fell back to
    // surplus tracking. Nothing failed; the day simply cost €1,50 more.
    for scenario in [
        Scenario::winter_with_grid_event(HouseholdConfig::default()),
        Scenario::winter_evening_deadline(HouseholdConfig::default()),
    ] {
        let r = run(&scenario).unwrap();
        assert_eq!(
            r.minutes_without_a_plan, 0,
            "{} spent {} minutes on the fallback with a planner running",
            scenario.date, r.minutes_without_a_plan
        );
    }
}

#[test]
fn the_store_covers_the_car_through_a_reduction_rather_than_exporting_past_it() {
    // `[A1 2.3]` measures what the controllable devices draw *from the grid*, so
    // a battery discharging into the wallbox is headroom the household owns and
    // the Festlegung allows. Reading the ceiling as a limit on **consumption**
    // instead produces the shape this pins against: a car that arrives as the
    // reduction starts is left short while the house exports, with a full store
    // behind the meter.
    let r = run(&Scenario::winter_evening_deadline(
        HouseholdConfig::default(),
    ))
    .unwrap();

    assert!(
        r.lent_kwh > 3.0,
        "the store lent only {:.1} kWh under a two-and-a-half-hour reduction",
        r.lent_kwh
    );
    assert!(
        r.unmet_charge_kwh < 1.0,
        "the car should leave all but full: short by {:.1} kWh",
        r.unmet_charge_kwh
    );
    assert!(
        r.ev_charged_kwh > 9.0,
        "and most of the 12 kWh it needed reached it through a 4,2 kW ceiling \
         shared with a heat pump: {:.1} kWh",
        r.ev_charged_kwh
    );
    assert!(
        r.grid_event_respected,
        "and none of it may exceed the ceiling: over by {:.0} W",
        r.worst_overshoot_w
    );
}

#[test]
fn the_sixty_percent_cap_costs_a_roof_what_an_intelligent_meter_would_have_saved() {
    // § 9 Abs. 1 EEG caps a system commissioned from 25.02.2025 at 60 % of its
    // installed direct-current power until an intelligent metering system with a
    // control device is **in operation** (§ 9 Abs. 2). This is the pair of runs
    // that says what it costs: same roof, same weather, same store, same seed,
    // and the only difference is whether the Steuerbox is there.
    //
    // The day is in **May**, not June: the cap is a fraction of direct-current
    // power and what a roof delivers against it is decided by cell temperature,
    // so a cool clear day in the middle of May is where a German roof comes
    // closest to its rating — and where the feed-in peak and the negative-price
    // hours actually are.
    let capped = run(&Scenario::summer_capped(&HouseholdConfig::default())).unwrap();
    let relieved = run(&Scenario::summer_capped(&HouseholdConfig {
        cap_relief: CapRelief::ImsysWithControl,
        ..HouseholdConfig::default()
    }))
    .unwrap();

    // The cap **binds**: the roof is held at the ceiling for the hours around
    // solar noon, within the minute the simulated inverter takes to obey a new
    // one. That is the property; the *cost* is the next assertion, and it is
    // deliberately small.
    let ceiling = capped
        .feed_in_ceiling_kw
        .expect("a 20 kWp roof commissioned after 25.02.2025 without an iMSys is capped");
    assert!(
        capped.peak_feed_in_kw <= ceiling * 1.05,
        "the cap was exceeded by more than the inverter's settling time: {:.2} against {ceiling:.2} kW",
        capped.peak_feed_in_kw
    );
    assert!(
        relieved.peak_feed_in_kw > ceiling,
        "with the cap lifted the same roof goes above it: {:.2} against {ceiling:.2} kW",
        relieved.peak_feed_in_kw
    );
    assert_eq!(
        relieved.curtailed_kwh, 0.0,
        "with the cap lifted nothing is thrown away"
    );

    // And what it costs, which is the number worth having and is **far smaller
    // than the rule sounds**. Two reasons, and both are arithmetic rather than
    // opinion:
    //
    // * the cap is 60 % of installed *direct-current* power, and a German roof's
    //   clear-day peak alternating-current output is only about two thirds of
    //   its direct-current rating once system losses, soiling and a 50 °C cell
    //   are taken off — so the 60 % line clips the top tenth of the peak, for
    //   two or three hours, on the clearest days of the year;
    // * and a household with a store, a tank and a heat pump **absorbs** most of
    //   that rather than throwing it away, which is the optimiser preferring
    //   absorption to curtailment.
    //
    // Three things inflate this figure several-fold if any of them is wrong: a
    // June day rather than a May one, a planner shown the weather in advance, and
    // a roof modelled at its datasheet rather than at what a three-year-old one
    // delivers.
    let cost = relieved.exported_kwh - capped.exported_kwh;
    assert!(
        cost > 0.05,
        "the cap has to cost the household something: {cost:.2} kWh"
    );
    assert!(
        cost < 5.0,
        "…and it is a small something, on a well-managed house: {cost:.2} kWh"
    );
    // And now the number worth having, which the saving figure cannot carry:
    // the § 9 EEG cap applies to a household **whether or not** it owns an
    // energy manager, so both sides of the comparison move and the *difference*
    // between them says nothing about the law. What the law costs is the change
    // in each household's own bill.
    let managed = capped.cost.total() - relieved.cost.total();
    let unmanaged = capped.baseline.total() - relieved.baseline.total();
    assert!(
        unmanaged > 0.0,
        "the cap has to cost an unmanaged household something: {unmanaged:.2} €"
    );
    assert!(
        unmanaged > managed,
        "and it has to cost the managed one less — that is the whole case for          owning an energy manager under the Solarspitzengesetz: {managed:.2} €          against {unmanaged:.2} €"
    );
    assert!(
        capped.baseline.curtailment_eur > relieved.baseline.curtailment_eur,
        "the baseline is capped too: a household with no energy manager does not          get to ignore § 9 EEG"
    );
}

#[test]
fn the_hot_water_tank_is_a_store_and_the_plan_uses_it_as_one() {
    // Three hundred litres between 45 and 60 °C hold about five kilowatt-hours
    // of heat, and a hot-water heat pump puts it there for under two of
    // electricity. The plan is supposed to buy that in the cheap hours and let
    // the tank coast through the dear ones — while the household still gets its
    // shower. Both halves are assertions, and the second is the one that makes
    // the first honest.
    let with_tank = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();

    assert!(
        with_tank.dhw_kwh > 2.0,
        "a day's hot water is a couple of kilowatt-hours of electricity, got {:.1}",
        with_tank.dhw_kwh
    );
    assert_eq!(
        with_tank.cold_water_kwh, 0.0,
        "and the household still gets it all: {:.1} kWh short",
        with_tank.cold_water_kwh
    );
    assert!(
        with_tank.tank_min_fill > 0.05,
        "the tank should never be run dry, it reached {:.0} %",
        with_tank.tank_min_fill * 100.0
    );
    // And it is used as a **store** rather than held at a set point, which is
    // the whole difference between a tank and a load. A thermostat keeps it near
    // full all day; a plan lets it run down through the morning peak and refills
    // it when electricity is cheap. Whether that pays is measured in the
    // optimiser's own tests, where the household is not also arguing about a car
    // and a § 14a reduction; what the day has to show is that the store moves.
    assert!(
        with_tank.tank_min_fill < 0.6,
        "the plan should spend the store, not sit on it: emptiest {:.0} %",
        with_tank.tank_min_fill * 100.0
    );
}

#[test]
fn without_a_planner_the_house_still_runs_off_its_own_roof() {
    // G3, measured. No forecast, no prices, no solver: the box on its own does
    // what every home battery has always done — cover the house from the roof
    // and the store, absorb what is left, export the rest.
    let r = run(&Scenario::summer_without_a_planner(
        HouseholdConfig::default(),
    ))
    .unwrap();
    assert!(r.imported_kwh < 5.0, "imported {:.1} kWh", r.imported_kwh);
    assert!(
        r.self_sufficiency > 0.9,
        "self-sufficiency {:.0} %",
        r.self_sufficiency * 100.0
    );
    assert!(r.battery_throughput_kwh > 5.0, "the store was not used");
    assert!(r.ev_charged_kwh > 10.0, "the car took no surplus");
}

#[test]
fn midsummer_is_the_wrong_day_to_measure_a_contactor_on() {
    // On midsummer a 9,8 kWp roof spends the middle of the day well above the
    // 4,14 kW a three-phase session needs to start, so the car reaches the
    // household's Ladelimit either way and a contactor is worth nothing. Any
    // difference a June day shows is the fallback charging past that limit rather
    // than the contactor earning its keep — which is why the day is pinned at
    // *no* difference, and why the shoulder season is where the capability is
    // measured (`a_switchable_charge_point_is_the_whole_session_in_the_shoulder_season`).
    let switchable = run(&Scenario::summer_without_a_planner(HouseholdConfig {
        evse_switchable: true,
        ..HouseholdConfig::default()
    }))
    .unwrap();
    let fixed = run(&Scenario::summer_without_a_planner(HouseholdConfig {
        evse_switchable: false,
        ..HouseholdConfig::default()
    }))
    .unwrap();

    assert!(
        (switchable.ev_charged_kwh - fixed.ev_charged_kwh).abs() < 0.5,
        "on midsummer the car fills either way: {:.1} vs {:.1} kWh",
        switchable.ev_charged_kwh,
        fixed.ev_charged_kwh
    );
    assert!(
        switchable.unmet_charge_kwh < 0.05 && fixed.unmet_charge_kwh < 0.05,
        "and both deliver the whole session"
    );
    assert_eq!(
        fixed.phase_switches, 0,
        "a fixed charge point never switches"
    );
}

#[test]
fn a_switchable_charge_point_never_costs_more_than_a_fixed_one_under_a_plan() {
    // The other half of the measurement, and the reason the planner is offered
    // the single-phase range only while a grid limit is in force. Left on all the
    // time it becomes a continuous power dial — three conductors deliver 0 or
    // 4,14 kW and nothing between — and a plan wanting exactly 2 kW of leftover
    // surplus reaches for one conductor and pays the onboard charger's overhead.
    for make in [
        Scenario::winter_with_grid_event as fn(HouseholdConfig) -> Scenario,
        Scenario::summer_surplus,
    ] {
        let switchable = run(&make(HouseholdConfig {
            evse_switchable: true,
            ..HouseholdConfig::default()
        }))
        .unwrap();
        let fixed = run(&make(HouseholdConfig {
            evse_switchable: false,
            ..HouseholdConfig::default()
        }))
        .unwrap();
        assert!(
            switchable.saving_eur() >= fixed.saving_eur() - 0.02,
            "{}: switching cost {:.2} €",
            switchable.imported_kwh,
            fixed.saving_eur() - switchable.saving_eur()
        );
    }
}

#[test]
fn the_days_own_generation_is_shared_over_a_forty_two_c_community() {
    // § 42c EnWG has applied since 01.06.2026. A rule module nobody invokes is
    // not a feature, and no property catches one: a property is a statement
    // about code that runs. The only thing that finds them is running a whole
    // day and asking why a number is zero.
    //
    // What is shared is an **allocation**, not physics: each quarter hour the
    // community's generation is divided among its consumers by an
    // Aufteilungsschlüssel agreed in writing (§ 42c Abs. 3 Nr. 2), and each
    // member's share is billed at the community's price instead of their
    // supplier's. So this feeds the simulated household's own quarter-hour
    // feed-in into a three-member community and checks the identity the whole
    // settlement rests on.
    let r = run(&Scenario::summer_capped(&HouseholdConfig::default())).unwrap();

    // A flat share for the house that owns the roof and two neighbours who do
    // not — the ordinary shape of a Mehrfamilienhaus community.
    let community = Community::new(
        "11YDE-VE-------2",
        vec![
            Member::new("DE0001111111111111111111111111111", Decimal::new(50, 2)),
            Member::new("DE0002222222222222222222222222222", Decimal::new(30, 2)),
            Member::new("DE0003333333333333333333333333333", Decimal::new(20, 2)),
        ],
    );

    let mut shared = Decimal::ZERO;
    let mut generated = Decimal::ZERO;
    let mut stranded_static = Decimal::ZERO;
    for quarter in &r.quarter_hours {
        // What left this household's connection point is what the community has
        // to divide. The consumers are the two neighbours and the house itself,
        // each taking a plausible household quarter hour.
        let generation = quarter.grid_feed_in;
        let consumption = [Decimal::new(15, 2), Decimal::new(9, 2), Decimal::new(24, 2)];
        generated += generation;

        let dynamic = allocate_by(
            &community,
            quarter.slot,
            generation,
            &consumption,
            Aufteilung::Dynamisch,
        )
        .expect("a valid community and a non-negative quarter hour");
        let statisch = allocate_by(
            &community,
            quarter.slot,
            generation,
            &consumption,
            Aufteilung::Statisch,
        )
        .expect("the same, under the other contract");

        // The identity `metering::allocation` guarantees and the Nachweis rests
        // on: nothing is invented and nothing disappears.
        assert_eq!(
            dynamic.shared_total() + dynamic.unallocated,
            generation,
            "Σ allocated + residual must equal the generation exactly"
        );
        assert_eq!(statisch.shared_total() + statisch.unallocated, generation);

        // No member is ever allocated more than it consumed — the cap that makes
        // this an allocation of *consumption* rather than a paper transfer.
        for (share, used) in dynamic.shares.iter().zip(consumption) {
            assert!(share.shared <= used, "a member cannot use what it did not");
        }

        shared += dynamic.shared_total();
        stranded_static += statisch.unallocated - dynamic.unallocated;
    }

    // The day put real energy through the community: a summer roof that exports
    // more than a hundred kilowatt-hours cannot share nothing.
    assert!(
        generated > Decimal::new(50, 0),
        "the capped day exports over 100 kWh, so there is something to share: {generated}"
    );
    assert!(
        shared > Decimal::ZERO,
        "and some of it reached the members: {shared}"
    );

    // And the two contracts genuinely differ, which is D33: applying the key
    // once and capping each member strands generation on whoever happened to be
    // away; re-offering it shares strictly more. Both are defensible, they give
    // different answers, and § 42c Abs. 3 Nr. 2 makes it the community's choice
    // rather than ours.
    assert!(
        stranded_static > Decimal::ZERO,
        "a static key must strand what a dynamic one re-offers: {stranded_static}"
    );
}

#[test]
fn every_asset_the_arbiter_moves_can_be_described_in_s2() {
    // One flexibility language, so the optimiser never sees a protocol — which
    // is worth nothing if the crate is one nothing imports. Every asset the
    // control stack actually commands has to have an S2 (EN 50491-12-2) control
    // type, and the day reports how many. A device the S2 layer cannot describe
    // is the first thing a real Resource Manager would find.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert_eq!(
        r.s2_resources, 5,
        "battery, charge point, heat pump, hot-water tank and the roof"
    );

    let household =
        hemsd::Household::build(&HouseholdConfig::default()).expect("the reference household");
    for asset in &household.site.assets {
        let control = hems_flex::control_type_for(asset, true);
        let expected = match asset {
            hems_core::asset::Asset::Battery(_)
            | hems_core::asset::Asset::Dhw(_)
            | hems_core::asset::Asset::Evse(_) => ControlType::Frbc,
            hems_core::asset::Asset::Pv(_) | hems_core::asset::Asset::HeatPump(_) => {
                ControlType::Pebc
            }
            _ => ControlType::NotControllable,
        };
        assert_eq!(
            control,
            expected,
            "{} was described as {control:?}",
            asset.id()
        );
        // And an asset that takes instructions has to declare a role, or a
        // Customer Energy Manager has no way to know whether it consumes,
        // produces or stores.
        if control != ControlType::NotControllable {
            assert!(
                !hems_flex::roles_for(asset).is_empty(),
                "{} takes instructions but declares no S2 role",
                asset.id()
            );
        }
    }
}

#[test]
fn the_reference_day_is_not_run_on_perfect_foresight() {
    // A simulated day whose forecast *is* the series the simulator is about to
    // run cannot tell a good planner from one that was shown the answer, and the
    // arbiter's energy tracking (D19) — built to absorb forecast error — is
    // never exercised because the error is identically zero.
    //
    // A test that only checked "the day saves money" would pass either way. This
    // one checks the *forecast* was wrong, which is the property that makes the
    // saving mean something.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();

    assert!(
        r.pv_forecast.samples > 90,
        "every quarter hour of the day should have been scored: {}",
        r.pv_forecast.samples
    );
    assert!(
        r.pv_forecast.crps > 1.0,
        "the production forecast was perfect, which means the planner was told the answer: CRPS {:.3} W",
        r.pv_forecast.crps
    );
    assert!(
        r.load_forecast.crps > 1.0,
        "the load forecast was perfect: CRPS {:.3} W",
        r.load_forecast.crps
    );
    // The box has to have *learned* something: the simulated roof delivers 92 %
    // of what its geometry says and nothing tells the model, so a corrector
    // sitting at exactly 1,00 is one that is not being fed.
    assert!(
        (r.roof_correction - 1.0).abs() > 0.02,
        "the residual corrector learned nothing about a roof that is 8 % down: {:.3}",
        r.roof_correction
    );
    assert!(
        r.history_days >= 14,
        "and it learned it from a fortnight or more"
    );

    // And the comparison that is the point of keeping the old behaviour at all.
    let mut perfect = Scenario::winter_with_grid_event(HouseholdConfig::default());
    perfect.weather = hemsd::WeatherSpec::PERFECT;
    let p = run(&perfect).unwrap();
    assert!(
        p.pv_forecast.crps < r.pv_forecast.crps / 3.0,
        "a day that cannot surprise the planner has to score far better: {:.2} against {:.2} W",
        p.pv_forecast.crps,
        r.pv_forecast.crps
    );
    // It does not score *zero*, and that is right rather than a defect: even
    // with the weather known, the box's own model of the roof is never certain
    // (`ResidualModel::floor_spread`) and the band it publishes has width. A
    // forecast that claimed certainty would let the planner bet a battery on it.
    assert!(p.pv_forecast.crps > 0.0);
    assert!(
        p.saving_eur() > r.saving_eur() + 1.0,
        "knowing the future is worth real money, and that gap is the honest \
         measure of how much a saving figure quoted from it overstates itself: \
         {:.2} € against {:.2} €",
        p.saving_eur(),
        r.saving_eur()
    );
}

#[test]
fn a_household_with_no_store_shares_a_reduction_that_arrives_off_the_grid() {
    // Two things, and both are the ordinary case rather than the exotic one.
    //
    // **No battery.** Millions of German households have a heat pump and a
    // wallbox and no store, and they are the ones a 4,2 kW ceiling is hard on:
    // there is no discharge to lend the controllable devices headroom
    // (`[A1 2.3]`), so the reduction has to be shared and somebody gets less
    // than they wanted.
    //
    // **Seven minutes past the hour.** `[A1 4.2]` presumes a network operator's
    // command goes out within five minutes of the Netzzustandsermittlung, and
    // nothing aligns that to the household's re-planning grid. A reduction that
    // starts exactly on a quarter hour lets the planner re-solve under the new
    // ceiling immediately, so the guard never has to decide anything; the window
    // between the command and the next re-plan is the only time it does, and it
    // is the case D3 exists for.
    let r = run(&Scenario::winter_evening_no_store(
        &HouseholdConfig::default(),
    ))
    .unwrap();

    assert!(
        r.grid_event_respected,
        "the ceiling has to hold through the window the plan did not know about: \
         over by {:.0} W",
        r.worst_overshoot_w
    );
    assert!(
        r.limited_minutes > 120,
        "the reduction ran for over two hours: {} min",
        r.limited_minutes
    );
    assert!(
        r.lent_kwh < 1.0,
        "a household with no store has nothing to lend: {:.2} kWh",
        r.lent_kwh
    );

    // And the number that says what the reduction was worth to this household —
    // the shadow price of the network operator's own ceiling, from the plan
    // living under it. On a household with a store it is cents; here it is
    // euros, because a car will otherwise leave short.
    assert!(
        r.relief_eur_per_kwh > 1.0,
        "relief from a binding ceiling on a household with no store has to be \
         worth real money: {:.2} €/kWh",
        r.relief_eur_per_kwh
    );

    // The planner prices the devices apart rather than handing the guard one
    // number for the slot, which is what makes "a reduction takes power from
    // where it is worth least" a decision rather than a sentence.
    assert!(
        r.widest_asset_value_ratio > 3.0,
        "the assets should be priced far apart under a binding ceiling: {:.1}×",
        r.widest_asset_value_ratio
    );
}

#[test]
fn a_household_with_a_store_is_barely_touched_by_the_same_reduction() {
    // The comparison that makes the previous test mean something, and a result
    // worth having on its own: the *same* reduction, on the same household with
    // its 10 kWh battery, costs almost nothing — the store lends the controllable
    // devices the headroom `[A1 2.3]` allows, and the ceiling stops binding.
    let with = run(&Scenario::winter_evening_deadline(
        HouseholdConfig::default(),
    ))
    .unwrap();
    let without = run(&Scenario::winter_evening_no_store(
        &HouseholdConfig::default(),
    ))
    .unwrap();

    assert!(
        with.lent_kwh > 4.0,
        "the store should lend several kilowatt-hours: {:.1}",
        with.lent_kwh
    );
    assert!(
        with.relief_eur_per_kwh < without.relief_eur_per_kwh / 3.0,
        "and relief should therefore be worth far less to it: {:.2} against {:.2} €/kWh",
        with.relief_eur_per_kwh,
        without.relief_eur_per_kwh
    );
}

#[test]
fn a_reduction_no_reference_day_may_command_is_one_no_reference_day_commands() {
    // § 14a Ziff. 4.5.2: under an energy management system the minimum is one
    // number for everything behind it and it **grows with the number of
    // controllable devices** — `4,2 kW + (n − 1) · GZF(n) · 4,2 kW`. The flat
    // 4,2 kW is the *base* of that formula, and reading it as the whole of it is
    // the easiest mistake in the Festlegung to make: every reference day in this
    // workspace commanded 4,2 kW to a household owed 10,5, and the figure that
    // says so was computed, stored on the evidence record and printed nowhere.
    //
    // Two faults, and they are different faults. An operator commanding below
    // the minimum is unlawful; the box holding *itself* below it on a lost
    // heartbeat is a configuration error of our own.
    for scenario in [
        Scenario::winter_with_grid_event(HouseholdConfig::default()),
        Scenario::winter_evening_deadline(HouseholdConfig::default()),
        Scenario::winter_evening_no_store(&HouseholdConfig::default()),
    ] {
        let label = scenario.date;
        let r = run(&scenario).unwrap();
        assert!(
            r.minimum_power_kw > 4.2,
            "{label}: a household with three controllable devices is owed more \
             than the base of the formula, got {:.2} kW",
            r.minimum_power_kw
        );
        assert!(
            !r.commanded_below_minimum,
            "{label}: the reference reduction is below the § 14a minimum of \
             {:.2} kW — an instruction no operator may send",
            r.minimum_power_kw
        );
        assert!(
            !r.failsafe_below_minimum,
            "{label}: the box's own failsafe restrains the household further \
             than any operator may, on nothing more than a lost heartbeat"
        );
    }
}

#[test]
fn a_car_is_not_planned_to_charge_after_it_has_gone() {
    // The deadline is half-open, and it has to be. Read as "the last slot it can
    // charge in", a car leaving at eight is planned as though it could still be
    // charging at 08:14 — and a plan with room to defer will put the last
    // quarter hour of the session there. At 11 kW that is 2,75 kWh the car never
    // receives.
    //
    // The failure hides wherever a limit was tight enough to force the charging
    // earlier, which is why it survived every § 14a day: the *loosest* ceiling
    // was the one that lost the most charge. So the test sweeps the ceiling and
    // asserts the car arrives full under all of them, which is the shape the bug
    // had rather than the value it took.
    let base = HouseholdConfig::default();
    for ceiling_kw in [4.2_f64, 7.56, 20.0] {
        let mut scenario = Scenario::winter_evening_no_store(&base);
        if let Some((from, until, _)) = scenario.grid_event {
            scenario.grid_event = Some((from, until, Power::from_kw(ceiling_kw)));
        }
        let r = run(&scenario).unwrap();
        assert!(
            r.unmet_charge_kwh < 0.05,
            "under a {ceiling_kw:.2} kW ceiling the car left {:.2} kWh short",
            r.unmet_charge_kwh
        );
    }
}

#[test]
fn the_saving_is_charged_for_the_service_the_plan_did_not_deliver() {
    // Every term of the objective is a term of the report. A plan is allowed to
    // leave the car short and to let the tank run cold — that is what makes both
    // soft rather than infeasible — and if it is not *charged* for doing so, the
    // saving figure treats a service the household did not get as a service it
    // did not have to pay for.
    //
    // The proof is a household whose car cannot possibly be filled: it arrives
    // at seven in the evening needing 40 kWh and leaves at nine.
    let base = HouseholdConfig::default();
    let mut scenario = Scenario::winter_evening_deadline(base);
    scenario.ev = Some(hemsd::EvPlan {
        energy_now: Energy::from_kwh(10.0),
        energy_target: Energy::from_kwh(50.0),
        arrival: Duration::hours(19),
        departure: Duration::hours(21),
    });
    let r = run(&scenario).unwrap();

    assert!(
        r.unmet_charge_kwh > 5.0,
        "40 kWh in two hours through an 11 kW wallbox cannot be done: {:.1} kWh short",
        r.unmet_charge_kwh
    );
    assert!(
        r.cost.unserved_eur > 5.0,
        "and the day has to be charged for it: {:.2} €",
        r.cost.unserved_eur
    );
    assert!(
        r.cost.total() > r.cost.energy_eur,
        "so the cost of the day is more than the electricity bill"
    );
    // The baseline is short too — it has the same two hours — so the comparison
    // stays a comparison rather than becoming a penalty on the side that admits
    // to it.
    assert!(
        r.baseline.unserved_eur > 5.0,
        "an unmanaged wallbox cannot do it either: {:.2} €",
        r.baseline.unserved_eur
    );
}

#[test]
fn a_switchable_charge_point_is_the_whole_session_in_the_shoulder_season() {
    // Midsummer is the wrong test for a contactor. A 9,8 kWp roof under high
    // pressure spends the middle of the day above the 4,14 kW a three-phase
    // session needs to start, so the car fills either way — which is why the
    // June day measured this at nothing once the fallback stopped charging past
    // the household's own Ladelimit.
    //
    // The German shoulder season is the other nine months. Under a September sun
    // the surplus sits in the 1,4 – 4,1 kW band for hours, where three conductors
    // can do nothing with it and one can take all of it.
    let switchable = run(&Scenario::autumn_without_a_planner(
        HouseholdConfig::default(),
    ))
    .unwrap();
    let fixed = run(&Scenario::autumn_without_a_planner(HouseholdConfig {
        evse_switchable: false,
        ..HouseholdConfig::default()
    }))
    .unwrap();

    assert!(
        switchable.phase_switches > 0 && switchable.single_phase_minutes > 60,
        "the surplus spends the day in the single-conductor band: {} switches, {} min",
        switchable.phase_switches,
        switchable.single_phase_minutes
    );
    assert!(
        switchable.ev_charged_kwh > 10.0 * fixed.ev_charged_kwh,
        "a fixed three-phase wallbox can hardly start at all: {:.1} kWh against {:.1}",
        fixed.ev_charged_kwh,
        switchable.ev_charged_kwh
    );
    assert!(
        switchable.unmet_charge_kwh < 0.05,
        "and the switchable one finishes the session: {:.1} kWh short",
        switchable.unmet_charge_kwh
    );
    assert!(
        fixed.unmet_charge_kwh > 3.0,
        "while the fixed one does not: {:.1} kWh short",
        fixed.unmet_charge_kwh
    );
    assert!(
        switchable.saving_eur() > fixed.saving_eur(),
        "which is what the contactor is worth: {:.2} € against {:.2} €",
        switchable.saving_eur(),
        fixed.saving_eur()
    );
}

#[test]
fn the_fallback_stops_at_the_charge_limit_the_household_set() {
    // A surplus tracker with no notion of *enough* pushes production into a car
    // that already has what it was asked for, in preference to exporting it —
    // which earns money. The planner never needs the limit; it is given an energy
    // target and a departure. The fallback has neither, and the fallback is what
    // runs when the cloud is gone.
    let limited = run(&Scenario::summer_without_a_planner(
        HouseholdConfig::default(),
    ))
    .unwrap();
    let unlimited = run(&Scenario::summer_without_a_planner(HouseholdConfig {
        ev_charge_limit: None,
        ..HouseholdConfig::default()
    }))
    .unwrap();

    assert!(
        unlimited.ev_charged_kwh > limited.ev_charged_kwh + 3.0,
        "without a limit the box keeps filling the car: {:.1} kWh against {:.1}",
        unlimited.ev_charged_kwh,
        limited.ev_charged_kwh
    );
    assert!(
        limited.exported_kwh > unlimited.exported_kwh + 3.0,
        "and what it stops putting into the car it exports instead: {:.1} kWh against {:.1}",
        limited.exported_kwh,
        unlimited.exported_kwh
    );
    assert!(
        limited.unmet_charge_kwh < 0.05,
        "the car still gets what it was promised: {:.1} kWh short",
        limited.unmet_charge_kwh
    );
    assert!(
        limited.saving_eur() > unlimited.saving_eur(),
        "so respecting the limit is worth money: {:.2} € against {:.2} €",
        limited.saving_eur(),
        unlimited.saving_eur()
    );
}
