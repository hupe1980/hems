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
    // The two minutes of `init` before the manager concluded that nothing was
    // controlling it are *not* a § 14a event: the network operator said nothing.
    // Counting them as one reports a reduction that never happened.
    //
    // Two, because `[LPC-906]`'s 120 s are up on the stroke of minute 2 — the
    // second this figure turns on, and the one `lpc_one_machine.rs` holds both
    // state machines to.
    assert_eq!(r.failsafe_minutes, 2, "the `init` state, and nothing else");

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
    // Eighty per cent, not eighty-five: a threshold is calibrated against
    // whatever the model can do, and forbidding one pack to charge and discharge
    // at once (D214) is worth three points of this day's self-sufficiency.
    assert!(
        r.self_sufficiency > 0.80,
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
    let with_wear = |wear: f64| {
        let base = HouseholdConfig::default();
        HouseholdConfig {
            battery: base.battery.map(|b| hemsd::BatteryConfig {
                wear_eur_per_kwh: wear,
                ..b
            }),
            ..base
        }
    };
    let cost_only = with_wear(0.0);
    let realistic = HouseholdConfig::default();
    let steep = with_wear(1.0);

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
    let base = HouseholdConfig::default();
    let config = HouseholdConfig {
        battery: base.battery.map(|b| hemsd::BatteryConfig {
            reserve_soc: Soc::new(0.4).unwrap(),
            kwh: Energy::from_kwh(10.0),
            ..b
        }),
        ..base
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
        pv: None,
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
    // **Heating only**, and that is the control rather than a convenience. The
    // seam this day watches is a *reactive* limiter meeting an uncommanded
    // export step, and a reversible unit absorbing surplus at exactly those
    // moments smooths the step away — the overshoot went structurally to zero
    // when cooling arrived (D202), which would have left the bound below
    // guarding nothing. A day that measures the § 9 seam measures that and not a
    // second consumer of the same kilowatts.
    let capped = run(&Scenario::summer_capped(&heating_only())).unwrap();
    let relieved = run(&Scenario::summer_capped(&HouseholdConfig {
        // The operator's first successful Ansteuerbarkeit test has happened,
        // which is the only thing § 9 Abs. 2 waits for. The intelligent
        // metering system itself is unchanged, so § 51's negative quarter
        // hours are identical on both sides and what moves is the cap.
        pv: heating_only().pv.map(|pv| hemsd::PvConfig {
            para9: pv.para9.with_relief(CapRelief::ImsysWithControl),
            ..pv
        }),
        ..heating_only()
    }))
    .unwrap();

    // The cap **binds**, and the quarter-hour register the settlement is built
    // from stays at it — within what one control period of excursion can put
    // into a quarter-hour mean.
    //
    // The statutory quantity is **not** this register. § 9 Abs. 2 Satz 1 Nr. 3
    // EEG says "die maximale Wirkleistungseinspeisung auf 60 Prozent der
    // installierten Leistung begrenzen", and § 8a Abs. 1 says the limit is to be
    // held "jederzeit": a power, at the connection point, at every instant. The
    // register is a **diagnostic** (D27, corrected), and what is asserted about
    // the statutory quantity itself is the instantaneous bound below.
    //
    // The allowance here is therefore derived rather than chosen: the register
    // is a mean over `SLOT`, so an excursion lasting one control period can lift
    // it by at most the excursion times `CONTROL_PERIOD / SLOT`. At this day's
    // one-minute cadence that is a fifteenth; at the one **second** a box runs
    // at it is a nine-hundredth of the same excursion, which is the sense in
    // which this day is a pessimistic reading of a box rather than a lenient
    // one.
    let ceiling = capped
        .feed_in_ceiling_kw
        .expect("a 20 kWp roof commissioned after 25.02.2025 without an iMSys is capped");
    let cadence = hemsd::scenario::CONTROL_PERIOD.as_seconds_f64()
        / hems_core::prelude::SLOT.as_seconds_f64();
    let allowance = capped.worst_uncommanded_export_step_w / 1000.0 * cadence;
    assert!(
        capped.peak_feed_in_kw <= ceiling + allowance,
        "the quarter-hour feed-in register sat {:.3} kW above the § 9 EEG ceiling, \
         against {allowance:.3} kW that one control period of a {:.0} W uncommanded \
         step can put into a quarter-hour mean",
        capped.peak_feed_in_kw - ceiling,
        capped.worst_uncommanded_export_step_w
    );

    // …and what is left of the *instantaneous* limit, which is the one the
    // statute writes and which no reactive controller can hold across a step it
    // did not command. The bound is **derived from the day** rather than
    // written down: whatever the guard did last period, the connection point
    // can be over the ceiling this period by at most the rise in uncommanded
    // net export between the two — the roof going up, the household's own draw
    // going down. A real box re-derives every second and holds each excursion
    // to a second; this day re-derives every minute, so both numbers below are
    // sixty times a box's.
    //
    // It was a hard-coded kilowatt, and that number was measuring the wrong
    // thing. Its comment attributed the excursion to "the household's own
    // uncommanded load steps"; at the worst tick of this day the household is
    // drawing 288 W and the guard has lent a sixth of it. The step is almost
    // entirely the **roof** — a cloud edge clearing on a 20 kWp array inside a
    // minute — so the bound scaled with the array and the constant did not, and
    // the first change to the irradiance model walked it over the line.
    assert!(
        capped.worst_feed_in_overshoot_w <= capped.worst_uncommanded_export_step_w,
        "the connection point was {:.0} W over the § 9 EEG ceiling for {} min, \
         against an uncommanded export step of {:.0} W — a reactive guard cannot \
         be over by more than the step it did not see coming",
        capped.worst_feed_in_overshoot_w,
        capped.feed_in_over_minutes,
        capped.worst_uncommanded_export_step_w
    );
    // And the excursion is real rather than a rounding artefact, so the bound
    // above is a bound on something. A structurally zero overshoot here would
    // mean this assertion had stopped watching the seam it exists for.
    assert!(
        capped.worst_feed_in_overshoot_w > 0.0,
        "a reactive limiter on a 20 kWp roof under a 12 kW cap crosses it; \
         a zero here is a day that stopped exercising § 9 Abs. 2"
    );
    assert_eq!(
        relieved.worst_feed_in_overshoot_w, 0.0,
        "with the cap lifted there is no ceiling to cross"
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
    // Four things move this figure several-fold: a June day rather than a May
    // one, a planner shown the weather in advance, a roof modelled at its
    // datasheet rather than at what a three-year-old one delivers — and **how
    // much flexible load the house actually has** at the hour the cap binds,
    // which is the one that moved it.
    //
    // That last one is why the bound below is a share of what the roof made
    // rather than a number of kilowatt-hours. It was `< 5,0 kWh`, and it was
    // calibrated against a house that ran its heat pump for 10,4 kWh on a sunny
    // 15 May — because the model had no solar or internal gains and thought the
    // building needed heating (D175). Give the house its free heat and the heat
    // pump takes 2,7 kWh, so there is 7 kWh less absorption exactly when the
    // roof is at its peak, and the cap costs more. The claim being made is
    // "small against what the roof made", and stating it that way is the only
    // version of it that survives a change to the household.
    let cost = relieved.exported_kwh - capped.exported_kwh;
    assert!(
        cost > 0.05,
        "the cap has to cost the household something: {cost:.2} kWh"
    );
    assert!(
        cost < capped.produced_kwh * 0.10,
        "…and it is a small something, on a well-managed house: {cost:.2} kWh of \
         {:.1} kWh produced ({:.1} %)",
        capped.produced_kwh,
        cost / capped.produced_kwh * 100.0
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
fn an_older_meter_brings_paragraph_51_with_it_and_it_is_a_different_clock() {
    // One intelligent metering system, two rules, two clocks — and reading one
    // fact for both makes the ordinary German household of 2026 impossible to
    // describe.
    //
    // § 9 Abs. 2 EEG lifts the 60 % feed-in cap only once the system is in
    // operation **and** the network operator's first Ansteuerbarkeit test has
    // succeeded. § 51 Abs. 2 Nr. 1 EEG stops exempting a plant below 100 kW from
    // the negative-price rule at the end of the calendar year the meter was
    // **fitted** in, and asks nothing about a test. A meter can sit in the
    // cupboard for a year between the two.
    //
    // This is the § 51 clock on its own: the cap is on in both runs, and the
    // only difference is whether the meter went in this year or last.
    let base = HouseholdConfig::default();
    let meter_in = |date: time::Date| HouseholdConfig {
        pv: base.pv.map(|pv| hemsd::PvConfig {
            para9: pv.para9.with_imsys_since(date),
            ..pv
        }),
        ..base.clone()
    };
    let this_year = run(&Scenario::summer_capped(&meter_in(time::macros::date!(
        2026 - 03 - 01
    ))))
    .unwrap();
    let last_year = run(&Scenario::summer_capped(&meter_in(time::macros::date!(
        2025 - 03 - 01
    ))))
    .unwrap();

    assert_eq!(
        this_year.feed_in_ceiling_kw, last_year.feed_in_ceiling_kw,
        "the § 9 cap is on in both: this is § 51 alone"
    );
    let para51_cost = last_year.cost.total() - this_year.cost.total();
    assert!(
        para51_cost > 0.0,
        "§ 51 can only cost a household that feeds in, never pay it: \
         {para51_cost:.2} €"
    );
    // …and it costs the *unmanaged* household more, because a box that can
    // absorb a negative afternoon into a battery, a tank and a car is the whole
    // answer to § 51 and a box that cannot has none.
    let unmanaged = last_year.baseline.total() - this_year.baseline.total();
    assert!(
        unmanaged > para51_cost,
        "an energy manager is worth more once § 51 starts, not less: \
         {para51_cost:.2} € against {unmanaged:.2} €"
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

/// The reference household with a **heating-only** heat pump.
///
/// For the controlled experiments — the contactor, the charge limit — whose
/// question is what happens to a *surplus*. A reversible unit is a second
/// consumer of exactly that surplus (D202), and a day that measures a contactor
/// while cooling competes for the same kilowatts measures two things and
/// attributes the sum to one. It is the same rule the autumn day's unloaded
/// dishwasher follows.
fn heating_only() -> HouseholdConfig {
    let base = HouseholdConfig::default();
    HouseholdConfig {
        heat_pump: base.heat_pump.map(|hp| hemsd::HeatPumpConfig {
            cooling_electrical: None,
            ..hp
        }),
        ..base
    }
}

#[test]
fn without_a_planner_the_house_still_runs_off_its_own_roof() {
    // The house is never worse off without the cloud, measured. No forecast, no
    // prices, no solver: the box on its own does
    // what every home battery has always done — cover the house from the roof
    // and the store, absorb what is left, export the rest.
    let r = run(&Scenario::summer_without_a_planner(
        HouseholdConfig::default(),
    ))
    .unwrap();
    // Seven rather than five, and 85 % rather than 90 %, because the household
    // now **cools** (D202). A reversible unit on its own thermostat answers a hot
    // evening room after the roof has finished for the day, so the fallback
    // imports more and is less self-sufficient than it was when the same house
    // simply sweltered. That is a worse *number* and a better *house*, and the
    // gap it opens is what the planned day closes by pre-cooling into the peak:
    // €7,58 saved against this day's €5,78.
    assert!(r.imported_kwh < 7.0, "imported {:.1} kWh", r.imported_kwh);
    assert!(
        r.self_sufficiency > 0.85,
        "self-sufficiency {:.0} %",
        r.self_sufficiency * 100.0
    );
    // …and **not** a hundred per cent, because it drew from the grid. The
    // figure this replaced put `production − export` over the loads it could
    // see, so a June day whose surplus went into a battery counted the charging
    // in the numerator and not the denominator, passed one, and clamped —
    // reporting perfect autarky and 2,6 kWh of import in the same table (D125).
    assert!(
        r.self_sufficiency < 1.0,
        "a household that imported {:.1} kWh is not wholly self-sufficient",
        r.imported_kwh
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
        evse: heating_only().evse.map(|e| hemsd::EvseConfig {
            switchable: true,
            ..e
        }),
        ..heating_only()
    }))
    .unwrap();
    let fixed = run(&Scenario::summer_without_a_planner(HouseholdConfig {
        evse: heating_only().evse.map(|e| hemsd::EvseConfig {
            switchable: false,
            ..e
        }),
        ..heating_only()
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
            evse: HouseholdConfig::default().evse.map(|e| hemsd::EvseConfig {
                switchable: true,
                ..e
            }),
            ..HouseholdConfig::default()
        }))
        .unwrap();
        let fixed = run(&make(HouseholdConfig {
            evse: HouseholdConfig::default().evse.map(|e| hemsd::EvseConfig {
                switchable: false,
                ..e
            }),
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

    // This asks the allocation a question about **arithmetic** — does
    // `Σ shared + residual` equal the generation, under either contract — rather
    // than claiming this household was allocated anything on 15 May 2026. That
    // is why `allocate_by` does not check the date and `Scenario::run` does: the
    // conservation identity is defined by the community's own contract, and the
    // day a network operator must make sharing possible is a different fact
    // about a different party (D179).
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

    // And the two contracts genuinely differ: applying the key
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
        r.s2_resources, 6,
        "battery, charge point, heat pump, hot-water tank, dishwasher and the roof"
    );
    // And the figure counts descriptions that were **built**, not assets whose
    // control type is merely not `NotControllable`. The difference is the whole
    // reason it is worth reporting: the second number goes up when a device is
    // added and never notices that no `describe_*` was ever written for it,
    // which is how the hot-water tank sat in it for four versions with nothing
    // to send.
    assert_eq!(
        r.s2_undescribed, 0,
        "nothing claims a control type this workspace cannot express"
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
            hems_core::asset::Asset::Load(l) if l.programme().is_some() => ControlType::Ppbc,
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

/// A day shown the weather in advance says so — and is **the same day**.
///
/// Two properties, and the second is the one that took three versions to get
/// right. `--perfect-foresight` used to be a `WeatherSpec` with every amplitude
/// zeroed and the soiling set to one, which ran a *different* day: a roof 8,7 %
/// cleaner, a January night five kelvin milder, no cloud variability at all. The
/// difference between two different days was then published as the price of
/// imperfect knowledge — "54 % of the headline saving is foresight" (D197).
///
/// The giveaway is the assertion in the middle of this test. A forecast is
/// something only the *managed* household makes, so a household with no planner
/// in it cannot move by a cent between the two runs. It used to move by about €1,70,
/// and nothing about a forecast can do that. Holding the baseline equal is
/// therefore not a nicety — it is the whole of what makes the difference
/// attributable to foresight, and it is cheap to state and impossible to satisfy
/// by accident.
#[test]
fn a_day_shown_the_weather_in_advance_says_so_about_itself() {
    let honest = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert!(
        !honest.foresight_is_perfect,
        "the reference day is run against a forecast that can be wrong"
    );

    let mut oracle = Scenario::winter_with_grid_event(HouseholdConfig::default());
    oracle.weather = oracle.weather.with_perfect_forecast();
    let oracle = run(&oracle).unwrap();
    assert!(
        oracle.foresight_is_perfect,
        "a day handed the simulator's own series has to label itself"
    );

    // **The same day.** Nothing about what the box knows may reach what the
    // weather does, and the unmanaged household is the instrument that says so:
    // it makes no forecast, so it must come out identical to the cent.
    assert!(
        (oracle.baseline.total() - honest.baseline.total()).abs() < 1e-9,
        "perfect foresight moved the household that has no planner in it: {:.4} € against \
         {:.4} € — the flag is changing the day rather than the forecast",
        oracle.baseline.total(),
        honest.baseline.total()
    );

    // And the roof correction is one, because an oracle has nothing left to
    // learn about its own roof — the cheapest possible check that the forecast
    // really is the realisation. Not *exactly* one: the corrector compares a
    // slot's metered energy, integrated minute by minute, against a forecast
    // that samples the slot's middle, so a quarter hour in which the sun is
    // moving leaves a few parts in a thousand behind. The honest day sits at
    // 0,90.
    assert!(
        (oracle.roof_correction - 1.0).abs() < 0.01,
        "an oracle still had a roof correction of {:.3} — it is being told \
         something other than what its roof will do",
        oracle.roof_correction
    );

    // Knowing the weather has to be worth something — and on a January day it is
    // worth very little, which is the honest answer rather than the one the
    // different-day comparison used to give.
    assert!(
        oracle.saving_eur() > honest.saving_eur(),
        "and knowing the weather has to be worth something: {:.4} € against {:.4} €",
        oracle.saving_eur(),
        honest.saving_eur()
    );
}

#[test]
fn the_reference_day_is_not_run_on_perfect_foresight() {
    // A simulated day whose forecast *is* the series the simulator is about to
    // run cannot tell a good planner from one that was shown the answer, and the
    // arbiter's energy tracking — built to absorb forecast error — is
    // never exercised because the error is identically zero.
    //
    // A test that only checked "the day saves money" would pass either way. This
    // one checks the *forecast* was wrong, which is the property that makes the
    // saving mean something.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();

    // The *lit* quarter hours, and only those. A January day is dark for two
    // thirds of its length, and a band of nothing against an outcome of nothing
    // is midnight rather than a forecast that came true — the assertion that
    // used to stand here demanded all ninety-six and was, without anybody
    // noticing, requiring the defect that made a 93 % coverage figure out of a
    // band that covers 80 %.
    assert!(
        (20..=40).contains(&r.pv_forecast.samples),
        "a mid-January day is lit for about eight hours: {} slots scored",
        r.pv_forecast.samples
    );
    assert_eq!(
        r.pv_forecast.samples + r.pv_forecast.skipped,
        96,
        "and the dark ones are counted rather than quietly dropped"
    );
    assert_eq!(
        r.load_forecast.skipped, 0,
        "a household's load is never nothing, so nothing is skipped there"
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
    perfect.weather = perfect.weather.with_perfect_forecast();
    let p = run(&perfect).unwrap();
    assert!(
        p.pv_forecast.crps < r.pv_forecast.crps / 3.0,
        "a day that cannot surprise the planner has to score far better: {:.2} against {:.2} W",
        p.pv_forecast.crps,
        r.pv_forecast.crps
    );
    // It does not score *zero*, and that is right rather than a defect: even
    // with the weather known, the box's own model of the roof is never certain
    // — the calibrated tails bottom out at `HARD_MIN_SPREAD` — and the band it
    // publishes has width. A forecast that claimed certainty would let the
    // planner bet a battery on it.
    assert!(p.pv_forecast.crps > 0.0);
    // And it is worth something on the **bill**, which is the term a weather
    // forecast actually touches.
    //
    // Only a little, and that is the honest answer rather than a weak test. This
    // assertion used to demand a whole euro and got one, because the flag it
    // rested on was running a sunnier, milder day (D197); against the *same* day
    // the January premium is about twenty-eight cents of a fifty-seven cent bill
    // saving. It should be small: this day imports 52,6 kWh and produces 7,4, the
    // prices that drive the plan are known exactly on both runs, and the car's
    // deadline is a constraint rather than a forecast. Where a household's
    // outcome really does turn on the sky — the June day — the premium is twelve
    // cents of €7,56, which is the same story.
    //
    // The number worth quoting from this test is therefore not a foresight
    // premium at all. It is that a saving figure published from a
    // perfect-foresight run overstates itself by **percent, not by half** — once
    // the comparison is made against the same day.
    let honest_bill = r.baseline.energy_eur - r.cost.energy_eur;
    let oracle_bill = p.baseline.energy_eur - p.cost.energy_eur;
    assert!(
        oracle_bill > honest_bill,
        "knowing the weather has to be worth something on the bill: {oracle_bill:.4} € \
         against {honest_bill:.4} €"
    );
    assert!(
        oracle_bill < honest_bill + 1.0,
        "the January weather-foresight premium is cents, not euros — {:.4} € against \
         {:.4} € means the two runs are no longer the same day",
        oracle_bill,
        honest_bill
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
    // is the case the guard exists for.
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

    // Only that it is a price. `relief_eur_per_kwh` is the dual of the § 14a row
    // in a linear program with every binary **pinned** (D42), and that method
    // cannot see relief whose value lies in changing a discrete decision — a car
    // that is off because `ev_on = 0` stays off, so relaxing the ceiling prices
    // at zero however much it is worth. A threshold on the magnitude measures
    // the method rather than the household (R39).
    assert!(
        r.relief_eur_per_kwh >= 0.0,
        "a relief figure is a price and cannot be negative: {:.2} €/kWh",
        r.relief_eur_per_kwh
    );

    // The planner prices the devices apart rather than handing the guard one
    // number for the slot, which is what makes "a reduction takes power from
    // where it is worth least" a decision rather than a sentence.
    //
    // **That they differ, not by how much.** Same pinned program as above, same
    // weakness (R39, R15): the magnitude of a dual is a property of whichever
    // optimum the pins froze. A ratio above one is the claim that survives a
    // change of optimum, and the only one the guard's allocator consumes.
    assert!(
        r.widest_asset_value_ratio > 1.0,
        "the assets have to be priced apart under a binding ceiling, or the \
         weighted allocator is ranking nothing: {:.1}×",
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
        evse: HouseholdConfig::default().evse.map(|e| hemsd::EvseConfig {
            switchable: false,
            ..e
        }),
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

    // And the seam this day is the one that exercises. A contactor switches on
    // whole conductors, so the surplus tracker keeps asking for values that fall
    // between what three conductors and one can hold — and
    // `hems_device::realisable` answers each with zero, correctly and silently.
    // On every other reference day this number is nought; here it is the cost of
    // the mechanism, and it has to stay visible rather than being absorbed into
    // "the car charged a bit less than it might have".
    assert!(
        switchable.clipped_ticks > 0 && switchable.clipped_kwh > 0.0,
        "a switching wallbox spends part of the shoulder season being asked for \
         power it cannot hold: {} ticks, {:.2} kWh",
        switchable.clipped_ticks,
        switchable.clipped_kwh
    );
    assert!(
        switchable.clipped_kwh < 1.0,
        "but it is a rounding error rather than a lost session: {:.2} kWh",
        switchable.clipped_kwh
    );
}

#[test]
fn the_fallback_stops_at_the_charge_limit_the_household_set() {
    // A surplus tracker with no notion of *enough* pushes production into a car
    // that already has what it was asked for, in preference to exporting it —
    // which earns money. The planner never needs the limit; it is given an energy
    // target and a departure. The fallback has neither, and the fallback is what
    // runs when the cloud is gone.
    let limited = run(&Scenario::summer_without_a_planner(heating_only())).unwrap();
    let unlimited = run(&Scenario::summer_without_a_planner(HouseholdConfig {
        evse: heating_only().evse.map(|e| hemsd::EvseConfig {
            charge_limit: None,
            ..e
        }),
        ..heating_only()
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
    // And respecting it **costs** money, which is the honest answer and was not
    // the one this test used to assert.
    //
    // Until the day's ledger valued what was left in the car
    // (`CostBreakdown::vehicle_eur`), a kilowatt-hour pushed into a car past the
    // household's own limit was worth nothing and exporting the same
    // kilowatt-hour earned the feed-in tariff — so ignoring the limit looked
    // expensive, and the mechanism appeared to pay for itself. It does not. A
    // kilowatt-hour in a car is a kilowatt-hour nobody buys later, so filling
    // one past the limit is worth roughly the retail price, and the reason to
    // stop is **lithium ageing**, which is a cost this ledger does not price and
    // the household is entitled to weigh for itself.
    //
    // Asserting it the other way round was a saving figure flattering a
    // behaviour by leaving out what it produced.
    assert!(
        unlimited.saving_eur() > limited.saving_eur(),
        "the limit costs money on the ledger, and buys battery life that is not          on it: {:.2} € against {:.2} €",
        limited.saving_eur(),
        unlimited.saving_eur()
    );
}

#[test]
fn the_dishwasher_is_moved_and_the_move_is_reported() {
    // The one piece of household flexibility a household can *see*, and the one
    // it can check: the machine ran, it ran later than a household with no
    // energy manager would have started it, and the day says by how much.
    //
    // A structural zero here is the failure this workspace keeps finding in
    // itself — a mechanism that is implemented, tested, and decides nothing —
    // so the number is asserted rather than printed and hoped for.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert!(r.appliance_ran(), "the wash has to happen");
    assert!(
        r.appliance_kwh > 1.0,
        "and it has to draw its programme: {:.2} kWh",
        r.appliance_kwh
    );
    assert!(
        r.appliance_shift_minutes > 0,
        "the planner moved it by {} minutes, which is nothing",
        r.appliance_shift_minutes
    );
    // Never before the household said it could start, whatever it costs.
    assert!(r.appliance_shift_minutes >= 0);
}

#[test]
fn a_programme_the_household_leaves_no_room_for_is_charged_on_both_sides() {
    // A window shorter than the programme is a household asking for something
    // impossible. The plan must come back — "not this wash" is a better answer
    // than no plan at all — and both sides of the comparison must pay for it, or
    // the saving is made of a wash nobody got.
    let mut scenario = Scenario::winter_with_grid_event(HouseholdConfig::default());
    scenario.dishwasher = Some((
        time::Duration::hours(12),
        time::Duration::minutes(12 * 60 + 30),
    ));
    let r = run(&scenario).unwrap();
    assert!(!r.appliance_ran(), "there is nowhere to put it");
    assert!(r.cost.unserved_eur > 0.0, "the plan is charged for it");
    assert!(r.baseline.unserved_eur > 0.0, "and so is the baseline");
}

#[test]
fn a_household_with_no_shiftable_appliance_still_plans() {
    // The appliance is optional, and its absence must not be a special case
    // anywhere: a house with nothing to shift is most houses.
    let config = HouseholdConfig {
        dishwasher: None,
        ..HouseholdConfig::default()
    };
    let r = run(&Scenario::winter_with_grid_event(config)).unwrap();
    assert_eq!(r.appliance_kwh, 0.0);
    assert!(!r.appliance_ran());
    assert_eq!(r.s2_resources, 5, "one fewer resource to describe");
    assert!(r.saving_eur() > 0.0);
}

#[test]
fn a_box_with_no_planner_still_washes_up() {
    // The house is never worse off when the planner is gone. A shiftable
    // appliance is the one device whose whole instruction is "start", so a box
    // with no plan that simply never sends it leaves the household with dirty
    // dishes and — because the comparison ran the machine — a *negative* saving.
    //
    // The fallback is the behaviour every appliance timer has always had: start
    // it when the sun is out, and no later than the last moment that still
    // finishes inside the window the household gave.
    let scenario = Scenario {
        dishwasher: Some((time::Duration::hours(9), time::Duration::hours(23))),
        ..Scenario::summer_without_a_planner(HouseholdConfig::default())
    };
    let r = run(&scenario).unwrap();
    assert!(r.appliance_ran(), "the wash happens with no planner at all");
    assert!(r.appliance_kwh > 1.0, "{:.2} kWh", r.appliance_kwh);
    // …and it waits for the sun rather than starting the moment it is allowed,
    // which is the whole difference between a fallback and a timer.
    assert!(
        r.appliance_shift_minutes > 0,
        "started {} minutes after it was allowed to",
        r.appliance_shift_minutes
    );
}

#[test]
fn the_tight_evening_plans_against_three_futures_and_the_slack_night_does_not() {
    // Planning against three futures costs five to seven times the solve, so something
    // has to decide which days are worth it. The trigger is a property of the
    // charging session rather than of the plan — a plan that has looked only at
    // the median cannot know it is at risk, which is measured in
    // `EvSession::tightness` and is why the obvious trigger was not used.
    //
    // A structural zero on the evening the mechanism exists for would mean it
    // decides nothing; a large number on the ordinary night would mean it costs
    // on every day and buys on few.
    let tight = run(&Scenario {
        adaptive_risk: true,
        ..Scenario::winter_evening_deadline(HouseholdConfig::default())
    })
    .unwrap();
    assert!(
        tight.risk_re_solves > 0,
        "the evening a car arrives as the reduction starts is the day this is for"
    );

    let slack = run(&Scenario {
        adaptive_risk: true,
        ..Scenario::winter_with_grid_event(HouseholdConfig::default())
    })
    .unwrap();
    assert!(
        slack.risk_re_solves < tight.risk_re_solves / 2,
        "a night with fourteen hours to take twenty kilowatt-hours is not: \
         {} against {}",
        slack.risk_re_solves,
        tight.risk_re_solves
    );

    // …and it is off by default, because four weathers on two days support a
    // sign and not a change to what a household's box does.
    let off = run(&Scenario::winter_evening_deadline(
        HouseholdConfig::default(),
    ))
    .unwrap();
    assert_eq!(off.risk_re_solves, 0);
}

#[test]
fn a_days_forecast_scores_are_one_episode_and_never_claim_calibration() {
    // Forecast error is correlated across a day, so ninety-six slots of one
    // Tuesday are one draw wearing ninety-six hats. The day scores its own
    // forecasts and it may not call the result calibration — R22, and the reason
    // `Calibration` counts episodes as well as samples.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert!(
        r.pv_forecast.samples > 20,
        "a winter day's worth of lit quarter hours"
    );
    assert_eq!(r.pv_forecast.episodes, 1);
    assert!(
        !r.pv_forecast.is_well_calibrated(),
        "no single day may report itself calibrated, whatever its coverage"
    );
    assert!(
        !r.load_forecast.is_well_calibrated(),
        "nor the load forecast"
    );
}

#[test]
fn a_forty_two_c_community_moves_the_day_and_the_baseline_is_in_it_too() {
    // The other half of § 42c, and the one that was missing for four versions:
    // `hems-grid::sharing` could *settle* an allocation and the planner had no
    // term that valued one, so a household could belong to a community and its
    // box would never once move a kilowatt-hour to catch the neighbours' roof.
    //
    // Three things have to hold at once, and only running the day can show them.
    // **Both days move**, and that is the whole of why the comparison is still a
    // comparison. § 42c Abs. 4 Nr. 1 obliges a network operator to make sharing
    // possible from 1 June 2026, and every reference day is dated before it — so
    // a community on the January day would settle an allocation nobody would
    // perform, which is what this test used to do and report (D179). The shift
    // is whole weeks, so the weekday and therefore the load profile's day type
    // survive it, and the member outside a community is measured on the same
    // Thursday as the member inside one.
    let lawful =
        Scenario::winter_with_grid_event(HouseholdConfig::default()).on_a_day_sharing_reaches();
    assert!(
        hems_grid::sharing::applies_on(lawful.date),
        "the day a § 42c comparison runs on has to be one § 42c reaches"
    );
    let plain = run(&lawful).unwrap();
    let mut with_community = lawful.clone();
    with_community.community = Some(hemsd::CommunityMembership::mehrfamilienhaus(
        with_community.config.pv.map_or(Power::ZERO, |pv| pv.kwp) * 3.0,
    ));
    let shared = run(&with_community).unwrap();

    // And a community on a day the rule does not reach is refused rather than
    // settled quietly — the assertion that would have caught this.
    let mut too_early = Scenario::winter_with_grid_event(HouseholdConfig::default());
    too_early.community = with_community.community;
    assert!(
        run(&too_early).is_err(),
        "a § 42c allocation on {} is one no network operator would perform",
        too_early.date
    );

    // 1. The module is *reached*. A structural zero here is the failure mode
    //    this workspace keeps finding in itself — a rule implemented, cited,
    //    tested and called by nothing.
    assert!(
        shared.shared_kwh > 1.0,
        "the community allocated {:.2} kWh — is anything calling it?",
        shared.shared_kwh
    );
    assert_eq!(
        plain.shared_kwh, 0.0,
        "and nothing where there is no community"
    );

    // 2. The credit is real money and it is on its own line, so a household can
    //    see what membership bought rather than having it hidden in the bill.
    assert!(
        shared.cost.sharing_eur < -0.5,
        "credit of {:.2} € against {:.1} kWh allocated",
        shared.cost.sharing_eur,
        shared.shared_kwh
    );

    // 3. **The baseline is in the same community.** A household joins one and
    //    then does nothing about it; the Aufteilungsschlüssel allocates it
    //    anyway. If the baseline were left outside, the saving would be the
    //    value of the *membership* rather than of the shifting — the same
    //    asymmetry as measuring against a household that ignored the network
    //    operator, and a much more flattering one.
    assert!(
        shared.baseline.total() < plain.baseline.total() - 0.1,
        "the unmanaged member is allocated too: {:.2} € against {:.2} €",
        shared.baseline.total(),
        plain.baseline.total()
    );

    // 4. And what the *planner* adds on top of the membership: it moves flexible
    //    load into the quarter hours the community is generating, so it is
    //    allocated more of the same roof than the member who did nothing. Both
    //    households are in the same community under the same
    //    Aufteilungsschlüssel, so this is the shifting and nothing else.
    //
    //    Measured on the **allocation** rather than on the saving. It was
    //    `shared.saving_eur() > plain.saving_eur()`, which is a difference of
    //    two differences — each of them two ~30 € winter days — and on a January
    //    day it came out at three cents. Three cents between two thirty-euro
    //    numbers is not a mechanism check, it is noise with a sign; the same
    //    day states the mechanism as 12,9 kWh against 3,9.
    assert!(
        shared.shared_kwh > shared.baseline_shared_kwh * 1.5,
        "the planner catches more of the community's roof than the member who did \
         nothing: {:.2} kWh against {:.2} kWh",
        shared.shared_kwh,
        shared.baseline_shared_kwh
    );
    assert!(
        shared.cost.sharing_eur < shared.baseline.sharing_eur,
        "and it is worth more money for it: {:.2} € credited against {:.2} €",
        shared.cost.sharing_eur,
        shared.baseline.sharing_eur
    );
    // The unmanaged member is allocated something rather than nothing, which is
    // what makes the comparison above a comparison.
    assert!(
        shared.baseline_shared_kwh > 0.0,
        "a member that does nothing is still allocated its share"
    );
    // `plain` is the same day outside any community, and it is what says the
    // whole of the difference above comes from the membership.
    assert_eq!(plain.baseline_shared_kwh, 0.0);
}

#[test]
fn the_day_the_household_would_be_asked_about_survives_the_process() {
    // `[A1 7.3]` keeps a § 14a control event for **two years**, and the house is
    // never worse off when the cloud is gone. Put together, those mean
    // the record has to exist on the box: one that only exists once it has been
    // uploaded is an intention with a network dependency, and the day a network
    // operator asks about is exactly the day the link was down.
    //
    // Counting the evidence and keeping it are different things, and only the
    // second one survives a restart. This is the test that tells them apart.
    let r = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert!(
        !r.evidence.is_empty(),
        "a day with a reduction has a record of it"
    );
    assert_eq!(
        r.evidence.len(),
        r.control_events + r.failsafe_events,
        "the record carries every event the counts were taken from"
    );
    assert_eq!(
        r.evidence.iter().map(|e| e.samples.len()).sum::<usize>(),
        r.evidence_samples,
        "and every sample of the compliance trace [A1 7.2] asks for"
    );

    let path = std::env::temp_dir().join(format!("hems-day-{}.sqlite", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut store = hemsd::store::Store::open(&path).unwrap();
        for quarter in &r.quarter_hours {
            store
                .put_quarter_hour(
                    &hemsd::store::Recorded {
                        registers: *quarter,
                        production: r.production_kwh_by_slot.get(&quarter.slot).copied(),
                    },
                    r.quarter_hours[0].slot.start(),
                )
                .unwrap();
        }
        for event in &r.evidence {
            store.put_control_event(event).unwrap();
        }
    }

    // A second `Store`, as a second process would open it.
    let store = hemsd::store::Store::open(&path).unwrap();
    let kept = store.control_events().unwrap();
    assert_eq!(kept.len(), r.evidence.len());
    assert_eq!(
        kept.iter().map(|s| s.event.clone()).collect::<Vec<_>>(),
        r.evidence,
        "the record read back is the record that was written, to the last sample"
    );
    assert_eq!(
        store.quarter_hours().unwrap().len(),
        96,
        "and the day's own registers, which is what MiSpeL and § 42c settle from"
    );

    // Nothing has been forwarded, so everything is still owed to the fleet.
    assert_eq!(
        store.backlog().unwrap(),
        hemsd::store::Backlog {
            events: r.evidence.len(),
            quarter_hours: 96,
            outbound: 0,
        },
        "a box that has not reached the fleet has a backlog, not a gap"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_day_is_never_reported_across_a_network_in_the_clear() {
    // What crosses this link is what the household consumed and when nobody was
    // in. Loopback is how the demo works; anything else has to be TLS, and it is
    // refused rather than warned about — a warning on a box nobody watches is a
    // warning nobody reads.
    for allowed in [
        "http://127.0.0.1:8080/v1/days",
        "http://localhost:8080/v1/days",
        "http://[::1]:8080/v1/days",
        "https://obsd.example/v1/days",
    ] {
        assert!(
            hemsd::report::is_confidential(allowed).is_ok(),
            "{allowed} should be allowed"
        );
    }
    for refused in [
        "http://obsd.example/v1/days",
        "http://10.0.0.5:8080/v1/days",
        "http://192.168.1.7/v1/days",
    ] {
        assert!(
            hemsd::report::is_confidential(refused).is_err(),
            "{refused} sends a household's day in the clear"
        );
    }
}

#[test]
fn a_fleet_url_without_a_port_is_not_an_error() {
    // `TcpStream::connect` wants an explicit port and an HTTP client does not:
    // `https://obsd.example/v1/days` is the ordinary way anybody would write it.
    assert!(hemsd::report::is_confidential("https://obsd.example/v1/days").is_ok());
}

#[test]
fn a_fleet_that_is_down_costs_a_delay_and_not_a_day() {
    // `hemsd simulate --report-to` used to be one `POST`, and a failure printed
    // a line to stderr. The day was gone — the box had already discarded what it
    // was built from — so the one link between the edge and the fleet lost data
    // whenever `obsd` restarted. G3 says the house is never worse off when the
    // cloud is gone, and a report that only survives a working WAN does not meet
    // it. This is the property that replaces that.
    let path = std::env::temp_dir().join(format!(
        "hems-outbound-{}-{}.sqlite",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&path);

    let now = time::OffsetDateTime::now_utc();
    {
        let mut store = hemsd::store::Store::open(&path).unwrap();
        store
            .queue_event(
                "haus-1:2026-01-14",
                hems_events::SITE_DAY_REPORTED,
                br#"{"specversion":"1.0"}"#,
                now,
            )
            .unwrap();
    }

    // The process restarts — a reboot, an update, a power cut — and the day is
    // still owed.
    let mut store = hemsd::store::Store::open(&path).unwrap();
    let pending = store.pending_outbound(10).unwrap();
    assert_eq!(
        pending.len(),
        1,
        "the day outlived the process that made it"
    );
    assert_eq!(pending[0].event_id, "haus-1:2026-01-14");
    assert_eq!(
        store.backlog().unwrap().outbound,
        1,
        "and it is visible as a backlog rather than as nothing at all — a fleet \
         link that has been down for longer than anybody noticed is invisible in \
         every other KPI, because the household was managed correctly throughout"
    );

    // What is stored is the body, never a signed request: the signature covers a
    // timestamp `obsd` refuses after five minutes, so one made when the row was
    // written would be stale by the time the box came back.
    assert_eq!(pending[0].body, br#"{"specversion":"1.0"}"#);

    store.mark_sent(&[pending[0].id], now).unwrap();
    assert!(store.pending_outbound(10).unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_box_closes_its_day_from_what_it_wrote_down() {
    // The half of the edge→fleet link that had never existed: `obsd` was fed
    // only by `hemsd simulate`, so it had never seen a household. This is the
    // shape of what a box now queues at the end of a Berlin calendar day —
    // built by reading back the rows the control loop wrote, so a restart at
    // half past eleven still reports the whole day (D116).
    let path = std::env::temp_dir().join(format!(
        "hems-close-day-{}-{}.sqlite",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&path);

    let day = time::macros::date!(2026 - 01 - 15);
    let midnight = metering::calendar::day_start_utc(day);
    {
        let store = hemsd::store::Store::open(&path).unwrap();
        for i in 0..96 {
            let quarter = hems_grid::mispel::QuarterHour {
                slot: hems_core::prelude::Slot::containing(
                    midnight + time::Duration::minutes(15 * i),
                ),
                grid_draw: rust_decimal::Decimal::new(15, 2),
                grid_feed_in: rust_decimal::Decimal::new(5, 2),
                device_consumption: rust_decimal::Decimal::ZERO,
                // The storage system, giving back something quite unlike the
                // sun — so a report that read `Z2E¼` as production would fail
                // here rather than quietly halving a household's roof (D124).
                device_generation: rust_decimal::Decimal::new(90, 2),
                storage_consumption: None,
                storage_generation: None,
                anzulegender_wert: rust_decimal::Decimal::new(786, 2),
                spot_price: rust_decimal::Decimal::new(1250, 2),
            };
            store
                .put_quarter_hour(
                    &hemsd::store::Recorded {
                        registers: quarter,
                        production: Some(rust_decimal::Decimal::new(10, 2)),
                    },
                    midnight,
                )
                .unwrap();
        }
    }

    // A fresh process, as after a restart, reading the day back out.
    let store = hemsd::store::Store::open(&path).unwrap();
    let kpis = hemsd::runtime::day::kpis(
        &store,
        "reference-household",
        day,
        hemsd::runtime::day::Unplanned::watching(),
        hemsd::runtime::day::Clipping::default(),
        hemsd::runtime::day::FeedIn::default(),
        &hemsd::runtime::day::Scored::default(),
    )
    .unwrap()
    .expect("ninety-six registers is a day");

    assert_eq!(kpis.site, "reference-household");
    assert_eq!(kpis.date, day);
    assert!((kpis.imported_kwh - 14.4).abs() < 1e-9, "96 × 0,15 kWh");
    assert!((kpis.exported_kwh - 4.8).abs() < 1e-9);
    assert!((kpis.produced_kwh - 9.6).abs() < 1e-9);
    assert!(
        kpis.self_sufficiency > 0.0 && kpis.self_sufficiency < 1.0,
        "a house that both drew and produced: {}",
        kpis.self_sufficiency
    );

    assert_eq!(
        kpis.economics, None,
        "a box meters; a baseline is a counterfactual and five of the six cost \
         terms are modelled"
    );
    assert_eq!(kpis.forecast, None, "and it did not score its own bands");
    assert!(
        !kpis.is_measurable(),
        "so a fleet counts it apart rather than averaging it in as a day that \
         saved nothing"
    );
    assert!(kpis.respected_the_grid, "no event said otherwise");
    assert_eq!(kpis.control_events, 0);

    // And it travels as the same signed CloudEvent the simulator's day does.
    let event = hems_events::Event::new(
        hems_events::SITE_DAY_REPORTED,
        "hems://sites/reference-household".to_owned(),
        format!("reference-household:{day}"),
        midnight,
        &kpis,
    );
    let body = event.to_bytes().expect("a day serialises");
    let back = hems_events::Event::<hems_core::report::DayKpis>::parse(
        &body,
        hems_events::SITE_DAY_REPORTED,
    )
    .expect("and `obsd` can read it back");
    assert_eq!(back.data, kpis, "through the type both sides share");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_carbon_price_moves_load_towards_the_clean_hours() {
    // Two objective terms the solver has read since the beginning and **nothing
    // could switch on**: `Objective` appeared nowhere under `services/`, so a
    // household that wanted its load in the hours the grid is clean had no way
    // to ask. And even switched on, the carbon term could not have worked: the
    // Energy-Charts intensity parser had no consumer, the poller dropped what it
    // fetched, and `SlotPrice::co2_g_per_kwh` was hard-coded `None` — so the
    // objective saw a flat annual constant, which makes a carbon price
    // algebraically identical to an autarky premium.
    //
    // With a real intensity curve it is a different signal, and this is the
    // claim: it moves the household's imports towards the clean hours.
    let plain = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();

    let mut priced = Scenario::winter_with_grid_event(HouseholdConfig::default());
    // Far above any real carbon price, because what is being tested is the
    // *mechanism*: at 55 €/t the term is worth about two ct/kWh and competes
    // with wear and comfort, and a test that asserted on that margin would be
    // measuring the tie-break rather than the signal.
    priced.objective = priced.objective.with_carbon_price(2.0);
    let priced = run(&priced).unwrap();

    // The intensity behind what each household imported. Not the day's average
    // — that is a property of the grid and no plan can move it — but the
    // emissions of the electricity this household actually drew.
    let intensity = |r: &hemsd::DayResult| r.imported_co2_kg / r.imported_kwh.max(1e-9) * 1000.0;
    assert!(
        intensity(&priced) < intensity(&plain) - 1.0,
        "pricing carbon should buy cleaner kilowatt-hours: {:.0} g/kWh against {:.0}",
        intensity(&priced),
        intensity(&plain)
    );

    // And the KPI is not structurally zero, which is the failure this workspace
    // keeps finding in itself: a term that is priced and whose effect nothing
    // reports could stop being applied with no day noticing.
    assert!(
        plain.imported_co2_kg > 1.0,
        "a winter day importing 50 kWh emits something: {:.2} kg",
        plain.imported_co2_kg
    );

    // The guard is outside the objective, so no weight a household chooses can
    // buy its way past a network operator's reduction.
    assert!(priced.grid_event_respected);
}

#[test]
fn an_autarky_premium_imports_less_and_one_day_cannot_price_it() {
    // The self-sufficiency dial, and the honest limit of what a reference day
    // can say about it.
    //
    // The **mechanism** is testable on one day: paying to avoid the grid
    // imports less.
    let plain = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    let mut autarky = Scenario::winter_with_grid_event(HouseholdConfig::default());
    autarky.objective = autarky.objective.with_autarky_premium(0.30);
    let autarky = run(&autarky).unwrap();

    assert!(
        autarky.imported_kwh < plain.imported_kwh - 0.05,
        "an autarky premium should import less: {:.2} kWh against {:.2}",
        autarky.imported_kwh,
        plain.imported_kwh
    );
    assert!(autarky.grid_event_respected);

    // What this day deliberately does **not** assert is what the premium cost,
    // and the reason is D59's: the plan is optimised against a forecast and
    // measured against a realisation, so a different plan meets a different day.
    // Measured here the premium comes out **€1,46 better** on the ledger —
    // because it discharged the store harder (`stored_eur` €0,00 → €0,91) and
    // let the house run cooler (`discomfort_eur` €0,22 → €0,57), and on this one
    // weather that happened to pay: the bill still fell further, €17,10 against
    // €19,86 on 45,3 kWh of import against 52,6.
    //
    // That is not a saving, it is a single draw: the same trap the hedge fell
    // into, where one realisation pays a premium every time and makes its claim
    // never. What a preference is *worth* needs the multi-weather sweep, which
    // is `hemsd risk`, and asserting a sign here would be pinning noise.
    let _ = autarky.saving_eur();
}

#[test]
fn the_day_counts_the_hours_ss_51_eeg_took_the_remuneration_in() {
    // § 51 EEG is applied per slot inside the price stack — the anzulegender
    // Wert goes to zero in a negative quarter hour — and until this KPI existed
    // **no day reported whether it had ever bound**. The rule could have stopped
    // being applied and every reference figure would have moved without anything
    // naming the cause.
    //
    // It also caught a documented number being wrong: the summer curve was
    // described in two places as having four negative quarter hours, and it has
    // twelve — three whole hours from eleven to two.
    let summer = run(&Scenario::summer_surplus(HouseholdConfig::default())).unwrap();
    assert_eq!(
        summer.para51_hours, 12,
        "three hours of negative prices is twelve quarter hours"
    );

    // And the winter day has none, which is what makes the pair a check rather
    // than a constant: a figure that is the same on every day is not measuring
    // the day.
    let winter = run(&Scenario::winter_with_grid_event(HouseholdConfig::default())).unwrap();
    assert_eq!(winter.para51_hours, 0);

    // The other half of the same argument: the carbon KPI is a property of what
    // the household *drew*, so a winter day that imports fifty kilowatt-hours
    // emits far more than a summer day that runs off its own roof.
    assert!(
        winter.imported_co2_kg > summer.imported_co2_kg * 5.0,
        "a winter import bill is a winter carbon bill: {:.2} kg against {:.2}",
        winter.imported_co2_kg,
        summer.imported_co2_kg
    );
}

/// The landing page prints the winter day's report. It has to be **that** report.
///
/// A terminal transcript on a marketing page is a number printed where nobody
/// compares it: it is copied once, and every change to the physics, the tariff or
/// the planner moves it silently. This session's own irradiance and free-heat
/// work moved every line of it and nothing failed.
///
/// So the page's block is parsed and held to the day it claims to be. `hemsd` is
/// `publish = false`, which is what makes reaching outside the crate for the
/// template legitimate — a published crate could not package it.
///
/// A line the report does not have, or has differently, fails here rather than on
/// somebody's screenshot.
#[test]
fn the_landing_page_prints_the_day_it_says_it_does() {
    const PAGE: &str = include_str!("../../../site/templates/index.html");
    // The README shows the *same* day, and for four versions nothing held it to
    // anything: it had drifted to a bill of €21,03 where the day reported
    // €19,88, and to a saving of €2,14 where the day said €2,18. A transcript on
    // a landing page and a transcript in a README are the same artefact with the
    // same failure mode, and there is no reason to guard one and not the other.
    const README: &str = include_str!("../../../README.md");
    const OPENING: &str = "2026-01-15 — with a § 14a reduction";

    let scenario = Scenario::winter_with_grid_event(HouseholdConfig::default());
    let day = run(&scenario).unwrap();
    let report = hemsd::render::day(&scenario, &day);

    for (what, page, terminator) in [
        ("the landing page", PAGE, "</code></pre>"),
        ("README.md", README, "\n```"),
    ] {
        let Some(start) = page.find(OPENING) else {
            panic!("{what} no longer shows a winter day, or shows another one");
        };
        let block = &page[start
            ..start
                + page[start..]
                    .find(terminator)
                    .unwrap_or_else(|| panic!("{what} has an unclosed transcript block"))];
        check_block(what, block, &report);
    }
}

/// Hold one transcript block to the report the binary would print.
fn check_block(what: &str, block: &str, report: &str) {
    {
        let mut checked = 0usize;
        for line in block.lines().skip(1) {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            let (label, value) = split_report_line(line);
            let Some(actual) = report
                .lines()
                .map(str::trim_end)
                .find_map(|l| (split_report_line(l).0 == label).then(|| split_report_line(l).1))
            else {
                panic!("{what} shows `{label}`, which this day no longer reports");
            };
            assert_eq!(
                value, actual,
                "{what} says `{label}` is `{value}`; the day says `{actual}`"
            );
            checked += 1;
        }
        assert!(
            checked >= 20,
            "only {checked} lines were compared in {what} — the block has changed shape"
        );
    }
}

/// A report line is a label and a value with two or more spaces between them.
fn split_report_line(line: &str) -> (&str, &str) {
    match line.trim().split_once("  ") {
        Some((label, value)) => (label.trim(), value.trim()),
        None => (line.trim(), ""),
    }
}

/// The unmanaged household's tank is **the tank this household has**, held where
/// **this household's thermostat** would hold it.
///
/// Two defects in one place, and both were invisible for the same reason: the
/// reference household's configured values happened to equal the constants
/// standing in for them. The baseline built a tank of its own — `litres × c ×
/// 15 K`, on `TankSim::new`'s default coefficient of 3,0 and its default 45 W
/// standing loss — and the reference span really is 60 − 45, the coefficient
/// really is 3,0 and the loss really is 45 W. When the literal equals the
/// configuration, no test can tell one from the other. And it commanded the
/// heater unconditionally, so it reheated to `t_max_c` — the highest **safe**
/// temperature, a scald bound an installer sets so nobody is burnt — where the
/// household asks for `t_set_c` (D183).
///
/// So this asserts by **moving the configuration**, which is the only thing a
/// constant cannot follow.
#[test]
fn the_baseline_household_heats_its_water_the_way_this_household_asked() {
    let tank_of = |cop: f64, t_set_c: f64| {
        let mut config = HouseholdConfig::default();
        let dhw = config
            .dhw
            .as_mut()
            .expect("the reference household has a tank");
        dhw.cop = cop;
        dhw.t_set_c = t_set_c;
        run(&Scenario::winter_with_grid_event(config)).unwrap()
    };

    // 1. The **set point** reaches the baseline. A household that keeps its water
    //    at 50 °C is cheaper to be than one that keeps it at 58 °C, and a
    //    baseline that ignored the setting would price the two identically — so
    //    the manager would be credited with the difference between two
    //    households rather than with a decision.
    let cool = tank_of(3.0, 50.0);
    let warm = tank_of(3.0, 58.0);
    // The **bill**, not the total, and the distinction is the point rather than a
    // convenience. `stored_eur` charges a household for ending with less in its
    // stores than it opened with, and the baseline tank opens at half its usable
    // heat whatever the thermostat is set to — so the 50 °C household drains
    // toward its set point and is charged €1,19 for doing so where the 58 °C one
    // is charged €0,73. That artefact is worth more than the €0,36 of electricity
    // the set point actually costs, and with the battery in the same clamped
    // quantity since D195 it is large enough to invert the total.
    //
    // It is *fair* — both households open in the same state, so it cancels in a
    // saving — but it is not the instrument for this claim. What the set point
    // reaches is the meter: a household that keeps its water at 58 °C buys more
    // electricity than one that keeps it at 50 °C, and nothing about store
    // accounting can mask that.
    assert!(
        warm.baseline.energy_eur > cool.baseline.energy_eur + 0.01,
        "a baseline held at 58 °C should buy more electricity than one held at \
         50 °C: {:.2} € against {:.2} €",
        warm.baseline.energy_eur,
        cool.baseline.energy_eur
    );

    // 2. …and so does the **coefficient of performance**. An immersion heater
    //    buys three kilowatt-hours where a hot-water heat pump buys one, and a
    //    baseline hard-coded to 3,0 would tell a household with the first that
    //    its hot water costs a third of what it pays.
    let immersion = tank_of(1.0, 55.0);
    let heat_pump = tank_of(3.0, 55.0);
    assert!(
        immersion.baseline.total() > heat_pump.baseline.total() + 0.05,
        "an immersion heater's baseline should cost more than a hot-water heat \
         pump's: {:.2} € against {:.2} €",
        immersion.baseline.total(),
        heat_pump.baseline.total()
    );

    // 3. And the *managed* side moves with it too, which is what says both
    //    households are the same appliance. A saving is a difference between two
    //    runs of one house; if only one side followed the configuration, the
    //    difference would be measuring the configuration instead.
    assert!(
        immersion.cost.total() > heat_pump.cost.total() + 0.05,
        "the plan has to pay the same physics: {:.2} € against {:.2} €",
        immersion.cost.total(),
        heat_pump.cost.total()
    );
}

/// A box on its **first evening** — hour one of every real installation.
///
/// Every other day here warms up with six weeks of metering, so nothing
/// exercised the code that runs when a box knows nothing: no load profile, no
/// correction its roof has earned, no session history. That is not an edge case,
/// it is what every household gets on the day the installer leaves, and it lasts
/// until the profile fills (D187).
///
/// What a cold box owes a household is **not a good plan** — it cannot have one
/// — but a lawful one, a delivered one, and an honest account of how little it
/// knows. Those are the assertions; the euro figure is the *finding*, and it is
/// recorded in R31 rather than asserted, because it is currently negative.
#[test]
fn a_box_on_its_first_evening_is_lawful_and_honest_about_what_it_knows() {
    let warm = Scenario::winter_with_grid_event(HouseholdConfig::default());
    let mut cold = warm.clone();
    cold.warm_up_days = 0;

    let warm_day = run(&warm).unwrap();
    let day = run(&cold).unwrap();

    // 1. It plans at all. A box that refused until it had history would leave a
    //    household on the fallback arbiter for its first six weeks, which is
    //    worse than a plan made from one meter reading.
    assert_eq!(
        day.minutes_without_a_plan, 0,
        "a cold box has to plan from its first reading, not from its first month"
    );

    // 2. It is lawful. None of this depends on learning: the guard derives the
    //    § 14a ceiling and the § 9 EEG cap from the site and the session, and a
    //    box that knows nothing about its household still knows the law.
    assert!(
        day.grid_event_respected,
        "a cold box respected no § 14a reduction"
    );
    assert!(
        day.peak_feed_in_kw <= day.feed_in_ceiling_kw.unwrap_or(f64::INFINITY) + 1e-6,
        "a cold box fed in {:.2} kW over a ceiling of {:?}",
        day.peak_feed_in_kw,
        day.feed_in_ceiling_kw
    );

    // 3. It delivers the service. The car reaches its target and nobody has a
    //    cold shower — the two soft terms a bad forecast is most likely to spend.
    assert!(
        day.cost.unserved_eur < 0.01,
        "a cold box left {:.2} € of service undelivered",
        day.cost.unserved_eur
    );
    assert!(
        (day.ev_charged_kwh - warm_day.ev_charged_kwh).abs() < 0.5,
        "the car has to be charged whatever the box knows: {:.1} kWh cold against \
         {:.1} kWh warm",
        day.ev_charged_kwh,
        warm_day.ev_charged_kwh
    );

    // 4. And it is **honest about knowing nothing**. This is the one that would
    //    otherwise rot: a cold box whose roof correction read like a learned one,
    //    or whose forecast scored as well as a warm box's, would be a box
    //    reporting confidence it has not earned — R28's shape, one layer up.
    assert!(
        (day.roof_correction - 1.0).abs() < 1e-9,
        "a roof nobody has metered cannot have earned a correction: {:.3}",
        day.roof_correction
    );
    assert!(
        day.pv_forecast.crps > warm_day.pv_forecast.crps,
        "a box that has never seen this roof forecast it as well as one that has: \
         {:.0} W cold against {:.0} W warm",
        day.pv_forecast.crps,
        warm_day.pv_forecast.crps
    );

    // 5. What six weeks of metering is worth, which is the number this day
    //    exists to produce. It is large, and it is currently a *loss* — see R31.
    assert!(
        warm_day.saving_eur() > day.saving_eur() + 1.0,
        "six weeks of learning should be worth more than a euro a day on this \
         household: {:.2} € warm against {:.2} € cold",
        warm_day.saving_eur(),
        day.saving_eur()
    );
}

/// The R31 figure the notes quote, printed rather than asserted.
///
/// `cargo test -p hemsd --test simulated_days -- --ignored --nocapture`. It is
/// not an assertion because the useful properties of a cold box are in
/// `a_box_on_its_first_evening_is_lawful_and_honest_about_what_it_knows`; this
/// exists so the number in the notes has somewhere to come from.
#[test]
#[ignore = "prints the R31 figure the notes quote; not an assertion"]
fn the_cold_box_figure_the_notes_quote() {
    let mut cold = Scenario::winter_with_grid_event(HouseholdConfig::default());
    cold.warm_up_days = 0;
    let d = run(&cold).unwrap();
    println!(
        "COLD saved {:.2} € (cost {:.2}, baseline {:.2})",
        d.saving_eur(),
        d.cost.total(),
        d.baseline.total()
    );
}

/// The day's bill is what a **two-register meter** would bill, not what netting
/// the quarter hour would.
///
/// A bidirectional meter integrates the instantaneous flow into the import
/// register or the export register as the sign falls. A quarter hour in which a
/// house draws for seven minutes and feeds back for seven registers *both*, and
/// is billed for both at two different prices. Netting it first makes that
/// quarter hour free. `hemsd` accumulates tick by tick with the two directions
/// priced apart, on **both** sides of the comparison.
///
/// # What this pins, and why it moved off the winter day
///
/// It used to assert that the shortcut moved the winter day's *bill saving* by
/// about a third — arXiv:2510.25373's mechanism, measured here at €0,93. That
/// figure turned out to be an artefact of the batteryless baseline (D195): the
/// household netting forgave was the unmanaged one, whose thermostat heat pump
/// cycled under a producing roof with nothing to absorb the swing. Give that
/// household the battery it actually owns and the swings go into the store
/// instead of across the meter, and the shortcut is worth under a cent on the
/// January day and at most four on any reference day. That is a true fact about
/// houses with batteries rather than a weakened test, and it is worth recording:
/// a published comparison of netting conventions is measuring the *baseline's*
/// storage as much as the convention.
///
/// So the guard is now on the **mechanism** rather than on a difference between
/// two households, which is both more direct and more sensitive. The autumn day
/// is the one where the managed household still reverses inside a quarter hour —
/// a shoulder-season roof swinging either side of the house's own draw — so its
/// tick-priced bill must be strictly dearer than its netted one. Price a
/// register instead of a tick and the two collapse onto each other, which is
/// exactly what this fails on.
#[test]
fn the_bill_is_what_a_two_register_meter_would_bill() {
    let r = run(&Scenario::autumn_without_a_planner(
        HouseholdConfig::default(),
    ))
    .unwrap();

    // The managed household reverses inside the quarter hour on a shoulder-season
    // day, so the two ways of pricing it cannot agree. They agree only if
    // something has started pricing a register.
    let forgiven = r.cost.energy_eur - r.energy_eur_netted;
    assert!(
        forgiven > 0.01,
        "netting this day's quarter hours changes the managed household's bill by only \
         {forgiven:.4} € (tick-priced {:.4} €, netted {:.4} €) — either the household \
         stopped reversing inside a quarter hour, or something is now pricing a register \
         instead of a tick",
        r.cost.energy_eur,
        r.energy_eur_netted
    );
    // Netting can only ever forgive: it cancels opposing flows that were bought
    // at the import price and sold at the lower export one. A negative here on
    // either household would mean the arithmetic has a sign error.
    assert!(
        r.cost.energy_eur >= r.energy_eur_netted - 1e-9,
        "netting made the managed household's bill larger: {:.4} € against {:.4} €",
        r.cost.energy_eur,
        r.energy_eur_netted
    );
    assert!(
        r.baseline.energy_eur >= r.baseline_energy_eur_netted - 1e-9,
        "netting made the unmanaged household's bill larger: {:.4} € against {:.4} €",
        r.baseline.energy_eur,
        r.baseline_energy_eur_netted
    );
}

/// The unmanaged household owns **the same battery**, and runs it.
///
/// For five versions it did not, and that single omission decided most of every
/// saving this project quoted: the winter day's bill saving was €2,99 against an
/// idle store and is €0,74 against the same store on its factory controller. The
/// difference was never the planner's — it is the import/export spread on
/// everything a battery cycles whether or not anybody is optimising, and nobody
/// removes a battery to go back to an unmanaged house.
///
/// The pin is the **wear**, because wear is throughput and throughput is the one
/// thing an idle store cannot have. Delete the baseline's battery and this goes
/// to zero; leave it in and both households pay for the life they spend, which is
/// what makes the remaining difference a difference of decisions rather than of
/// equipment (D195).
#[test]
fn the_unmanaged_household_runs_the_battery_it_owns() {
    for (name, scenario) in [
        (
            "winter",
            Scenario::winter_with_grid_event(HouseholdConfig::default()),
        ),
        (
            "summer",
            Scenario::summer_without_a_planner(HouseholdConfig::default()),
        ),
    ] {
        let r = run(&scenario).unwrap();
        assert!(
            r.baseline.wear_eur > 0.01,
            "{name}: the unmanaged household spent {:.3} € of battery life — a store that \
             costs nothing to run is a store nobody is running, and the saving beside it is \
             the value of owning a battery rather than of managing one",
            r.baseline.wear_eur
        );
        // Same pack, same wear rate, both cycling: the two figures belong to the
        // same order of magnitude. A baseline wearing the battery ten times
        // harder than the plan would mean the greedy rule is being charged for
        // something other than self-consumption.
        assert!(
            r.baseline.wear_eur < r.cost.wear_eur * 5.0 + 0.10,
            "{name}: the unmanaged battery spent {:.3} € against the plan's {:.3} €",
            r.baseline.wear_eur,
            r.cost.wear_eur
        );
        // And the manager still has to be worth something once the hardware is
        // the same on both sides — that is the whole claim, now measured against
        // a household that is not handicapped.
        assert!(
            r.saving_eur() > 0.5,
            "{name}: saved only {:.2} € against a household with the same equipment",
            r.saving_eur()
        );
    }
}
