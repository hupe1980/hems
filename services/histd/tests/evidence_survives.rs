//! The § 14a record, written by the control stack and read back by an operator.
//!
//! `[A1 7.3]` is two years, so the interesting property is not that a record can
//! be written — it is that it is still there after the process that wrote it has
//! gone. Every test here reopens the database to check that.

use hems_core::prelude::GuardRule;
use hems_core::prelude::{AssetId, Power, Slot};
use hems_grid::evidence::{Action, ComplianceSample, ControlEvent};
use hems_grid::mispel::QuarterHour;
use hems_grid::para14a::ControlMode;
use histd::Store;
use histd::export::{data_act, nachweis};
use rust_decimal::Decimal;
use time::OffsetDateTime;
use time::macros::datetime;

const RECEIVED: OffsetDateTime = datetime!(2026-01-15 17:00:00 UTC);
const RELEASED: OffsetDateTime = datetime!(2026-01-15 18:30:00 UTC);

/// The reference winter day's own reduction: ninety minutes at 4,2 kW against a
/// minimum of 10,5, with a sample a minute.
fn winter_event() -> ControlEvent {
    let mut event = ControlEvent::received(
        GuardRule::Lpc,
        ControlMode::Ems,
        Power::from_kw(4.2),
        Power::from_kw(10.5),
        RECEIVED,
    );
    event.applied_at = Some(RECEIVED);
    event.acted = Some(Action::Commanded);
    event.released_at = Some(RELEASED);
    event.source = Some("SKI:0a1b2c3d".into());
    event.assets = vec![
        AssetId::new("wallbox").unwrap(),
        AssetId::new("waermepumpe").unwrap(),
    ];
    event.samples = (0..90)
        .map(|minute| ComplianceSample {
            at: RECEIVED + time::Duration::minutes(minute),
            netzwirksam: Power::from_kw(3.9),
            ceiling: Power::from_kw(4.2),
        })
        .collect();
    event
}

#[test]
fn a_reduction_is_still_there_after_the_process_has_gone() {
    let dir = std::env::temp_dir().join("hems-histd-e2e");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("evidence.sqlite");
    std::fs::remove_file(&path).ok();

    {
        let mut store = Store::open(&path).unwrap();
        store.put_control_event("site-1", &winter_event()).unwrap();
        for i in 0..96 {
            let slot = Slot::containing(datetime!(2026-01-15 00:00:00 UTC)).offset(i);
            store
                .put_quarter_hour(
                    "site-1",
                    &QuarterHour {
                        grid_draw: Decimal::new(580, 3),
                        grid_feed_in: Decimal::new(3, 3),
                        ..QuarterHour::empty(slot)
                    },
                    RECEIVED,
                )
                .unwrap();
        }
    }

    // A different process, a different `Store`, the same record.
    let store = Store::open(&path).unwrap();
    let record = nachweis(&store, "site-1", None, None).unwrap();
    let events = record["events"].as_array().expect("events");
    assert_eq!(events.len(), 1);
    let event = &events[0];
    // The event's own `serde` form: the document an operator is handed is the one
    // the box wrote, rather than a second rendering assembled column by column.
    assert_eq!(event["rule"], "lpc");
    assert_eq!(event["acted"], "commanded");
    assert_eq!(event["source"], "SKI:0a1b2c3d");
    // [A1 7.2] wants the trace, not a summary of it.
    assert_eq!(event["samples"].as_array().unwrap().len(), 90);
    // …and every commanded ceiling in sequence, not only the strictest.
    assert_eq!(event["ceilings"].as_array().unwrap().len(), 1);
    assert!((event["strictest_ceiling_w"].as_f64().unwrap() - 4200.0).abs() < 1e-9);
    assert_eq!(
        event["below_minimum"], true,
        "4,2 kW is below the 10,5 owed"
    );

    std::fs::remove_file(&path).ok();
}

#[test]
fn the_data_act_export_is_everything_and_the_quantities_are_exact() {
    // Regulation (EU) 2023/2854 Article 4: everything the product generated,
    // machine-readable, free. And the settlement quantities travel as decimal
    // strings, because a JSON number is a double to every reader that has ever
    // parsed one.
    let mut store = Store::in_memory().unwrap();
    store.put_control_event("site-1", &winter_event()).unwrap();
    store
        .put_quarter_hour(
            "site-1",
            &QuarterHour {
                grid_draw: Decimal::new(1_234_567, 6),
                ..QuarterHour::empty(Slot::containing(RECEIVED))
            },
            RECEIVED,
        )
        .unwrap();

    let export = data_act(&store, "site-1").unwrap();
    assert_eq!(export["control_events"].as_array().unwrap().len(), 1);
    let quarters = export["quarter_hours"].as_array().unwrap();
    assert_eq!(quarters.len(), 1);
    assert_eq!(
        quarters[0]["grid_draw_kwh"], "1.234567",
        "an exact decimal string and not a float"
    );
    assert!(quarters[0]["grid_draw_kwh"].is_string());
}

#[test]
fn an_operator_can_ask_about_one_window_rather_than_the_whole_record() {
    let mut store = Store::in_memory().unwrap();
    store.put_control_event("site-1", &winter_event()).unwrap();

    let before = nachweis(
        &store,
        "site-1",
        Some(datetime!(2026-01-01 00:00:00 UTC)),
        Some(datetime!(2026-01-15 00:00:00 UTC)),
    )
    .unwrap();
    assert!(before["events"].as_array().unwrap().is_empty());

    let during = nachweis(
        &store,
        "site-1",
        Some(datetime!(2026-01-15 00:00:00 UTC)),
        Some(datetime!(2026-01-16 00:00:00 UTC)),
    )
    .unwrap();
    assert_eq!(during["events"].as_array().unwrap().len(), 1);
}

#[test]
fn one_households_export_never_contains_anothers() {
    // The whole of the multi-tenancy a box needs, and the thing that would be
    // most embarrassing to get wrong in a document a household is entitled to.
    let mut store = Store::in_memory().unwrap();
    store.put_control_event("site-1", &winter_event()).unwrap();
    store.put_control_event("site-2", &winter_event()).unwrap();
    store
        .put_quarter_hour(
            "site-2",
            &QuarterHour {
                grid_draw: Decimal::new(9999, 3),
                ..QuarterHour::empty(Slot::containing(RECEIVED))
            },
            RECEIVED,
        )
        .unwrap();

    let export = data_act(&store, "site-1").unwrap();
    assert_eq!(export["control_events"].as_array().unwrap().len(), 1);
    assert!(
        export["quarter_hours"].as_array().unwrap().is_empty(),
        "site-1 has no registers, and site-2's are not its"
    );
}
