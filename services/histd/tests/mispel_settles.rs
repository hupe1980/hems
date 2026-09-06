//! The MiSpeL settlement, over registers the way a box actually writes them.
//!
//! This is the seam that did not exist: the box wrote quarter-hour registers,
//! `histd` kept them for the two years of `[A1 7.3]`, and **nothing ever
//! settled them**. The whole formula set of `hems-grid::mispel` — Anlage 1's
//! (1)–(33) and Anlage 2's (P1)–(P15) — was reachable only from a unit test, so
//! it could have been wrong in every release and no test would have moved.
//!
//! From 01.10.2026 that document is what a household's levy privileges
//! (§ 21 EnFG) and its EEG support depend on, so the failure mode is not an
//! unused function: it is a household that cannot produce the Nachweis it is
//! asked for.

use hems_grid::mispel::{Basisfall, QuarterHour};
use histd::Store;
use histd::config::MispelSettings;
use histd::export::mispel;
use rust_decimal::Decimal;
use time::macros::date;

/// A month of registers as the control loop writes them: a household drawing
/// from the grid, feeding a little back, and cycling its store.
///
/// The four register names are the Festlegung's own — `Z1NB¼`, `Z1NE¼`,
/// `Z2V¼`, `Z2E¼` — and the values are deliberately all different, so a
/// settlement that read one for another fails here rather than reconciling.
fn month_of_registers(store: &Store, site: &str, year: i32, month: u8, quarters: usize) {
    let first = time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), 1)
        .expect("a real month");
    let start = metering::calendar::day_start_utc(first);
    let now = start;
    for i in 0..quarters {
        let slot =
            hems_core::prelude::Slot::containing(start + time::Duration::minutes(15 * i as i64));
        store
            .put_quarter_hour(
                site,
                &QuarterHour {
                    // 0,580 kWh drawn, 0,003 fed back, and a store that took
                    // 0,200 and gave 0,150 — a perfectly ordinary quarter hour.
                    grid_draw: Decimal::new(580, 3),
                    grid_feed_in: Decimal::new(3, 3),
                    device_consumption: Decimal::new(200, 3),
                    device_generation: Decimal::new(150, 3),
                    anzulegender_wert: Decimal::new(786, 2),
                    spot_price: Decimal::new(1250, 2),
                    ..QuarterHour::empty(slot)
                },
                now,
            )
            .expect("a register the box wrote");
    }
}

#[test]
fn a_full_month_settles_and_says_so() {
    let store = Store::in_memory().unwrap();
    // October 2026 in the Europe/Berlin calendar is 31 days with one **long**
    // day — the clocks go back on the 25th — so it holds 2 980 quarter hours
    // rather than 31 × 96 = 2 976. Getting that wrong is the whole reason the
    // window comes from `metering::calendar` and not from arithmetic on days.
    let expected = 31 * 96 + 4;
    month_of_registers(&store, "haus-1", 2026, 10, expected);

    let doc = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A1,
        }),
        2026,
        Some(10),
    )
    .expect("a month of registers settles");

    assert_eq!(doc["settled"], true);
    assert_eq!(
        doc["quarter_hours_expected"], expected,
        "the long October day has a hundred quarter hours, not ninety-six"
    );
    assert_eq!(doc["quarter_hours_present"], expected);
    assert_eq!(doc["complete"], true, "every quarter hour is on record");
    assert_eq!(doc["rule_set"], "BK 618-25-02, Arbeitsstand 05.08.2026");

    // The arithmetic ran and produced the Festlegung's own figures rather than
    // an empty object — the structural-zero check this workspace keeps needing.
    let a = &doc["figures"]["abgrenzung"];
    assert!(
        a["grid_draw"].as_str().is_some(),
        "the numbered figures are the domain type's own: {a}"
    );
    let relief: f64 = doc["figures"]["levy_relief_share"]
        .as_str()
        .expect("the one number a household wants")
        .parse()
        .unwrap();
    assert!(
        (0.0..=1.0).contains(&relief),
        "a share of the month's grid draw: {relief}"
    );
}

#[test]
fn a_month_with_gaps_is_settled_and_is_not_called_complete() {
    // The hazard this guard exists for. A quarter hour the box could not price
    // gets **no register** (deliberately — a register carries two prices, and a
    // zero in either is the figure that says § 51 EEG switched support off), so
    // a month legitimately has gaps. A settlement summed over part of a month
    // under-reports every quantity in it while looking exactly like a complete
    // one, and it is a legal document.
    let store = Store::in_memory().unwrap();
    month_of_registers(&store, "haus-1", 2026, 11, 2_000);

    let doc = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A1,
        }),
        2026,
        Some(11),
    )
    .expect("a partial month still computes");

    assert_eq!(doc["quarter_hours_present"], 2_000);
    assert_eq!(doc["quarter_hours_expected"], 30 * 96);
    assert_eq!(
        doc["complete"], false,
        "the denominator has to be visible, or a partial settlement reads as a whole one"
    );
}

#[test]
fn the_option_decides_which_period_may_be_asked_for() {
    // The Abgrenzungsoption settles per calendar **month** and the
    // Pauschaloption per calendar **year**, and the arithmetic says it cannot
    // check that for you. So the caller does not get to choose: asking for the
    // wrong period is refused rather than silently summed over the wrong ∑M.
    let store = Store::in_memory().unwrap();
    month_of_registers(&store, "haus-1", 2026, 10, 96);

    let abgrenzung = MispelSettings::Abgrenzung {
        basisfall: Basisfall::A1,
    };
    assert!(
        mispel(&store, "haus-1", Some(abgrenzung), 2026, None).is_err(),
        "a month is required"
    );

    let pauschal = MispelSettings::Pauschal {
        fall: hems_grid::mispel::PauschalFall::P1,
        solar_kwp: 9.8,
        storage_kwh: 10.0,
    };
    assert!(
        mispel(&store, "haus-1", Some(pauschal), 2026, Some(10)).is_err(),
        "a month must be omitted"
    );
    let year = mispel(&store, "haus-1", Some(pauschal), 2026, None)
        .expect("the Pauschaloption settles a year");
    assert_eq!(year["figures"]["option"], "pauschal");
    assert_eq!(
        year["quarter_hours_expected"],
        365 * 96,
        "2026 is not a leap year, and the two clock changes cancel over a year"
    );
}

#[test]
fn a_site_that_declared_nothing_is_refused_rather_than_guessed_at() {
    // Every option produces a different Nachweis from the same registers, so a
    // default would be a household settled under an installation it does not
    // have — arithmetically perfect and about somebody else.
    let store = Store::in_memory().unwrap();
    month_of_registers(&store, "haus-1", 2026, 10, 96);
    let err =
        mispel(&store, "haus-1", None, 2026, Some(10)).expect_err("no declaration, no settlement");
    assert!(
        matches!(err, histd::export::MispelExportError::Undeclared { .. }),
        "{err}"
    );
}

/// Exclusivity settles nothing and is **checked** rather than taken on trust.
///
/// A store that is never charged from the grid has no grey energy to separate,
/// so there is nothing to settle — but "nothing to settle" is not the same as
/// "nothing to say". The claim is a statement about the registers, `(1)¼ =
/// MIN[Z1NB¼ ; Z2V¼]` is what measures it, and the registers are right there.
/// Returning the declaration alone made the one option whose Nachweis is a
/// promise the one option nothing verified.
#[test]
fn exclusivity_settles_nothing_and_proves_it_from_the_registers() {
    let store = Store::in_memory().unwrap();
    // A month whose every quarter hour draws 0,580 kWh from the grid *and*
    // puts 0,200 into the store: the fixture is an ordinary household, and for
    // this option it is a month of broken claim.
    month_of_registers(&store, "haus-1", 2026, 10, 31 * 96 + 4);
    let doc = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Ausschliesslichkeit),
        2026,
        Some(10),
    )
    .expect("exclusivity always answers");
    assert_eq!(doc["settled"], false);
    assert_eq!(doc["option"], "ausschliesslichkeit");
    assert_eq!(
        doc["held"], false,
        "0,200 kWh into the store every quarter hour, under grid draw"
    );
    assert!(
        !doc["breaches"].as_array().expect("a list").is_empty(),
        "a household needs the quarter hour, not only a total"
    );
    assert!(
        doc["notice"]
            .as_str()
            .is_some_and(|s| s.contains("does not cover this period"))
    );
}

/// …and a household that kept the promise is told so, with the figure.
#[test]
fn exclusivity_held_reports_zero_rather_than_silence() {
    let store = Store::in_memory().unwrap();
    let first =
        time::Date::from_calendar_date(2026, time::Month::October, 1).expect("a real month");
    let start = metering::calendar::day_start_utc(first);
    for i in 0..(31 * 96 + 4) {
        let slot = hems_core::prelude::Slot::containing(start + time::Duration::minutes(15 * i));
        store
            .put_quarter_hour(
                "haus-2",
                &QuarterHour {
                    // The meter runs, and the store is idle while it does.
                    grid_draw: Decimal::new(580, 3),
                    grid_feed_in: Decimal::ZERO,
                    device_consumption: Decimal::ZERO,
                    device_generation: Decimal::new(150, 3),
                    anzulegender_wert: Decimal::new(786, 2),
                    spot_price: Decimal::new(1250, 2),
                    ..QuarterHour::empty(slot)
                },
                start,
            )
            .expect("a register the box wrote");
    }
    let doc = mispel(
        &store,
        "haus-2",
        Some(MispelSettings::Ausschliesslichkeit),
        2026,
        Some(10),
    )
    .expect("exclusivity always answers");
    assert_eq!(doc["held"], true);
    assert_eq!(doc["gleichzeitiger_netzbezug_kwh"], "0");
    assert_eq!(doc["complete"], true);
    assert!(doc["breaches"].as_array().expect("a list").is_empty());
}

#[test]
fn a_settlement_covers_only_the_month_it_names() {
    // The window is half-open and comes from the Berlin calendar, so the
    // register at 00:00 on the first of the next month belongs to that month.
    let store = Store::in_memory().unwrap();
    month_of_registers(&store, "haus-1", 2026, 10, 31 * 96 + 4);
    month_of_registers(&store, "haus-1", 2026, 11, 96);

    let october = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A1,
        }),
        2026,
        Some(10),
    )
    .unwrap();
    assert_eq!(
        october["quarter_hours_present"],
        31 * 96 + 4,
        "November's registers are November's"
    );
    let _ = date!(2026 - 10 - 25);
}
