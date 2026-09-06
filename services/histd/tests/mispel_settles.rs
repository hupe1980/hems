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
async fn month_of_registers(store: &Store, site: &str, year: i32, month: u8, quarters: usize) {
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
            .await
            .expect("a register the box wrote");
    }
}

/// The same month, with the storage system's **own** meter beside `Z2`.
///
/// This is what a box on Basisfall A4 writes: `Z3V¼`/`Z3E¼` are a subset of
/// `Z2V¼`/`Z2E¼` — the store's half of what the store and the charge point did
/// together — and the difference between them is what the charge point did.
async fn month_of_registers_with_a_metered_store(
    store: &Store,
    site: &str,
    year: i32,
    month: u8,
    quarters: usize,
) {
    let first = time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), 1)
        .expect("a real month");
    let start = metering::calendar::day_start_utc(first);
    for i in 0..quarters {
        let slot =
            hems_core::prelude::Slot::containing(start + time::Duration::minutes(15 * i as i64));
        store
            .put_quarter_hour(
                site,
                &QuarterHour {
                    grid_draw: Decimal::new(580, 3),
                    grid_feed_in: Decimal::new(3, 3),
                    device_consumption: Decimal::new(200, 3),
                    device_generation: Decimal::new(150, 3),
                    // Of that pair, the store's own half — the charge point took
                    // the rest.
                    storage_consumption: Some(Decimal::new(120, 3)),
                    storage_generation: Some(Decimal::new(100, 3)),
                    anzulegender_wert: Decimal::new(786, 2),
                    spot_price: Decimal::new(1250, 2),
                    ..QuarterHour::empty(slot)
                },
                start,
            )
            .await
            .expect("a register the box wrote");
    }
}

#[tokio::test]
async fn a_full_month_settles_and_says_so() {
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    // October 2026 in the Europe/Berlin calendar is 31 days with one **long**
    // day — the clocks go back on the 25th — so it holds 2 980 quarter hours
    // rather than 31 × 96 = 2 976. Getting that wrong is the whole reason the
    // window comes from `metering::calendar` and not from arithmetic on days.
    let expected = 31 * 96 + 4;
    month_of_registers(&store, "haus-1", 2026, 10, expected).await;

    let doc = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A1,
        }),
        2026,
        Some(10),
        None,
    )
    .await
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

#[tokio::test]
async fn basisfall_a4_settles_from_the_storage_system_s_own_meter() {
    // The other half of "a rule with no caller is not a feature", and the
    // sharper version of it: A4 *had* a caller. It was a declarable option in
    // `histd.example.toml`, the arithmetic was written and unit-tested, and it
    // could never once have succeeded — `Z3V¼`/`Z3E¼` had no column in either
    // store, so every register read back with no separate meter behind it and
    // `abgrenzung_month` refused the settlement the household had declared.
    //
    // A4 is the case that pays for itself: `[MiSpeL A1 (17)A4]` charges the
    // conversion losses to the store, which A3 cannot see and therefore charges
    // to the household.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    // The same month as A1's, with the store's own half of `Z2` beside it: of
    // the 0,200 kWh the store and the charge point drew, 0,120 was the store,
    // and of the 0,150 they gave back, 0,100 was. The 0,020 difference is the
    // round-trip loss (17)A4 exists to name.
    month_of_registers_with_a_metered_store(&store, "haus-a4", 2026, 10, 2_980).await;

    let doc = mispel(
        &store,
        "haus-a4",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A4,
        }),
        2026,
        Some(10),
        None,
    )
    .await
    .expect("A4 settles once the box's own storage registers reach the fleet");

    assert_eq!(doc["settled"], true);
    assert_eq!(doc["complete"], true);
    let losses = doc["figures"]["abgrenzung"]["storage_losses"]
        .as_str()
        .expect("(17)A4 is the figure A4 exists for");
    assert_eq!(
        losses, "59.600",
        "2 980 quarter hours of 0,020 kWh — and a structural zero here would \
         mean the separate meter never arrived"
    );
}

#[tokio::test]
async fn a_settlement_can_be_reproduced_from_the_registers_it_was_computed_from() {
    // What a corrected register does to a document that has already been sent.
    //
    // A Nachweis is settled and handed over. Weeks later the metering point
    // operator replaces a substitute value with a real reading, and the
    // household is asked why its figures do not match. Two questions follow —
    // *what did we hand over* and *what does the correction change* — and
    // neither is answerable from a record that overwrites. The registers are
    // versioned so that both are.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    let quarters = 2_980;
    month_of_registers(&store, "haus-1", 2026, 10, quarters).await;

    let option = MispelSettings::Abgrenzung {
        basisfall: Basisfall::A1,
    };
    let first = mispel(&store, "haus-1", Some(option), 2026, Some(10), None)
        .await
        .expect("the month settles");
    // The instant the document was produced at: the day after the period, not
    // the wall clock. A test that reached for `now_utc()` would compare the
    // machine's own date with a record dated October 2026 and quietly assert
    // nothing — which is what the first draft of this test did.
    let settled_at = metering::calendar::day_start_utc(date!(2026 - 11 - 01));
    let before = first["figures"]["abgrenzung"]["grid_draw"]
        .as_str()
        .expect("the month's grid draw")
        .to_owned();

    // The correction: one quarter hour's substitute value replaced by a reading
    // twice its size, recorded now rather than on the day.
    let slot = hems_core::prelude::Slot::containing(metering::calendar::day_start_utc(date!(
        2026 - 10 - 01
    )));
    store
        .put_quarter_hour(
            "haus-1",
            &QuarterHour {
                grid_draw: Decimal::new(1_160, 3),
                grid_feed_in: Decimal::new(3, 3),
                device_consumption: Decimal::new(200, 3),
                device_generation: Decimal::new(150, 3),
                anzulegender_wert: Decimal::new(786, 2),
                spot_price: Decimal::new(1250, 2),
                ..QuarterHour::empty(slot)
            },
            settled_at + time::Duration::days(14),
        )
        .await
        .expect("a restated register");

    // Settled again, the correction is in — and the period is still complete,
    // because a restatement is a new version of a quarter hour rather than a
    // second one.
    let corrected = mispel(&store, "haus-1", Some(option), 2026, Some(10), None)
        .await
        .expect("the corrected month settles");
    assert_eq!(corrected["quarter_hours_present"], quarters);
    assert_eq!(corrected["complete"], true);
    let after = corrected["figures"]["abgrenzung"]["grid_draw"]
        .as_str()
        .expect("the month's grid draw");
    assert_ne!(after, before, "the correction has to move the settlement");

    // …and the document that was handed over is still reproducible.
    let reproduced = mispel(
        &store,
        "haus-1",
        Some(option),
        2026,
        Some(10),
        Some(settled_at),
    )
    .await
    .expect("the settlement as it stood");
    assert_eq!(
        reproduced["figures"]["abgrenzung"]["grid_draw"]
            .as_str()
            .expect("the month's grid draw"),
        before,
        "a disputed Nachweis is checked against the registers it was computed \
         from, not against the ones that replaced them"
    );
    assert_eq!(
        reproduced["as_of"],
        settled_at.unix_timestamp(),
        "and the document says which version of the record it is about"
    );
}

#[tokio::test]
async fn basisfall_a4_is_refused_where_the_store_is_not_separately_metered() {
    // The refusal is the point of the nullable column: `NULL` means "no
    // separate meter", never zero. A household declared A4 whose box cannot
    // read its store owes its network operator an error rather than a
    // settlement claiming the battery stood still all month.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    month_of_registers(&store, "haus-1", 2026, 10, 2_980).await;

    let error = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A4,
        }),
        2026,
        Some(10),
        None,
    )
    .await
    .expect_err("A4 without Z3 is not a settlement");
    assert!(
        error.to_string().contains("Z3V"),
        "and it names the registers it wanted: {error}"
    );
}

#[tokio::test]
async fn a_month_with_gaps_is_settled_and_is_not_called_complete() {
    // The hazard this guard exists for. A quarter hour the box could not price
    // gets **no register** (deliberately — a register carries two prices, and a
    // zero in either is the figure that says § 51 EEG switched support off), so
    // a month legitimately has gaps. A settlement summed over part of a month
    // under-reports every quantity in it while looking exactly like a complete
    // one, and it is a legal document.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    month_of_registers(&store, "haus-1", 2026, 11, 2_000).await;

    let doc = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A1,
        }),
        2026,
        Some(11),
        None,
    )
    .await
    .expect("a partial month still computes");

    assert_eq!(doc["quarter_hours_present"], 2_000);
    assert_eq!(doc["quarter_hours_expected"], 30 * 96);
    assert_eq!(
        doc["complete"], false,
        "the denominator has to be visible, or a partial settlement reads as a whole one"
    );
}

#[tokio::test]
async fn the_option_decides_which_period_may_be_asked_for() {
    // The Abgrenzungsoption settles per calendar **month** and the
    // Pauschaloption per calendar **year**, and the arithmetic says it cannot
    // check that for you. So the caller does not get to choose: asking for the
    // wrong period is refused rather than silently summed over the wrong ∑M.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    month_of_registers(&store, "haus-1", 2026, 10, 96).await;

    let abgrenzung = MispelSettings::Abgrenzung {
        basisfall: Basisfall::A1,
    };
    assert!(
        mispel(&store, "haus-1", Some(abgrenzung), 2026, None, None)
            .await
            .is_err(),
        "a month is required"
    );

    let pauschal = MispelSettings::Pauschal {
        fall: hems_grid::mispel::PauschalFall::P1,
        solar_kwp: 9.8,
        storage_kwh: 10.0,
    };
    assert!(
        mispel(&store, "haus-1", Some(pauschal), 2026, Some(10), None)
            .await
            .is_err(),
        "a month must be omitted"
    );
    let year = mispel(&store, "haus-1", Some(pauschal), 2026, None, None)
        .await
        .expect("the Pauschaloption settles a year");
    assert_eq!(year["figures"]["option"], "pauschal");
    assert_eq!(
        year["quarter_hours_expected"],
        365 * 96,
        "2026 is not a leap year, and the two clock changes cancel over a year"
    );
}

#[tokio::test]
async fn a_period_the_festlegung_never_reached_is_refused() {
    // `RuleSet` versions the Festlegung and dates it — `Arbeitsstand 05.08.2026`
    // takes effect on 01.10.2026 — and nothing asked it. A household could have
    // been handed an Abgrenzung for March 2026, arithmetically perfect and about
    // a regime nobody was in. A Nachweis is a legal document; a wrong one is
    // worse than none.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    let abgrenzung = MispelSettings::Abgrenzung {
        basisfall: Basisfall::A1,
    };
    month_of_registers(&store, "haus-1", 2026, 3, 96).await;

    let error = mispel(&store, "haus-1", Some(abgrenzung), 2026, Some(3), None)
        .await
        .expect_err("March 2026 is before the rules");
    assert!(
        error.to_string().contains("2026-10-01"),
        "and it says when they start: {error}"
    );

    // The month they arrive in settles: a straddling period is a partial one,
    // which `complete` already reports.
    month_of_registers(&store, "haus-1", 2026, 10, 96).await;
    assert!(
        mispel(&store, "haus-1", Some(abgrenzung), 2026, Some(10), None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_site_that_declared_nothing_is_refused_rather_than_guessed_at() {
    // Every option produces a different Nachweis from the same registers, so a
    // default would be a household settled under an installation it does not
    // have — arithmetically perfect and about somebody else.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    month_of_registers(&store, "haus-1", 2026, 10, 96).await;
    let err = mispel(&store, "haus-1", None, 2026, Some(10), None)
        .await
        .expect_err("no declaration, no settlement");
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
#[tokio::test]
async fn exclusivity_settles_nothing_and_proves_it_from_the_registers() {
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    // A month whose every quarter hour draws 0,580 kWh from the grid *and*
    // puts 0,200 into the store: the fixture is an ordinary household, and for
    // this option it is a month of broken claim.
    month_of_registers(&store, "haus-1", 2026, 10, 31 * 96 + 4).await;
    let doc = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Ausschliesslichkeit),
        2026,
        Some(10),
        None,
    )
    .await
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
#[tokio::test]
async fn exclusivity_held_reports_zero_rather_than_silence() {
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
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
            .await
            .expect("a register the box wrote");
    }
    let doc = mispel(
        &store,
        "haus-2",
        Some(MispelSettings::Ausschliesslichkeit),
        2026,
        Some(10),
        None,
    )
    .await
    .expect("exclusivity always answers");
    assert_eq!(doc["held"], true);
    assert_eq!(doc["gleichzeitiger_netzbezug_kwh"], "0");
    assert_eq!(doc["complete"], true);
    assert!(doc["breaches"].as_array().expect("a list").is_empty());
}

#[tokio::test]
async fn a_settlement_covers_only_the_month_it_names() {
    // The window is half-open and comes from the Berlin calendar, so the
    // register at 00:00 on the first of the next month belongs to that month.
    let fixture = hems_service::testdb::Postgres::start(histd::store::MIGRATIONS).await;
    let store = Store::new(fixture.db.clone());
    month_of_registers(&store, "haus-1", 2026, 10, 31 * 96 + 4).await;
    month_of_registers(&store, "haus-1", 2026, 11, 96).await;

    let october = mispel(
        &store,
        "haus-1",
        Some(MispelSettings::Abgrenzung {
            basisfall: Basisfall::A1,
        }),
        2026,
        Some(10),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        october["quarter_hours_present"],
        31 * 96 + 4,
        "November's registers are November's"
    );
    let _ = date!(2026 - 10 - 25);
}
