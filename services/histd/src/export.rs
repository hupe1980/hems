//! The Data Act export, and the § 14a Nachweis.
//!
//! Two exports with two different readers, and they are not the same document.
//!
//! **The household's** — Regulation (EU) 2023/2854 Article 4 gives a user the
//! right to the data their connected product generates, "in a comprehensive,
//! structured, commonly used and machine-readable format", and free of charge.
//! So it is JSON, it is everything, and it is one request.
//!
//! **The network operator's** — `[A1 7.2]` asks a narrower question about a
//! wider window: what did you command, when, and what did the connection point
//! draw while it lasted. It is one event at a time and it carries the whole
//! minute-resolution trace, because an operator checking a reduction is checking
//! the trace.
//!
//! # Why both are generated rather than stored
//!
//! A stored export is a third copy that drifts. Both of these are a query over
//! the same two tables, and the tables are the record.
//!
//! # And why neither of them writes SQL
//!
//! Both go through [`Store`]'s own reads and serialise the domain
//! types, so a Nachweis is the record rather than a second rendering of it.
//! Assembling the document column by column would make it a second rendering,
//! and the two would agree until one of them was changed.

use hems_grid::mispel::QuarterHour;
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::store::{Store, StoreError};

/// Everything one site has on record, for a household exercising Article 4.
///
/// # Errors
/// [`StoreError::Sql`], or [`StoreError::NotReadable`] for a stored event this
/// build cannot parse.
pub fn data_act(store: &Store, site: &str) -> Result<Value, StoreError> {
    let quarters = store.quarter_hours(site, None, None)?;
    Ok(json!({
        "site": site,
        "produced_at": OffsetDateTime::now_utc().unix_timestamp(),
        "notice": "Regulation (EU) 2023/2854 Article 4: the data this product generated, \
                   in full, machine-readable, and free of charge.",
        "quarter_hours": quarters.iter().map(quarter_hour).collect::<Vec<_>>(),
        "control_events": events(store, site, None, None)?,
    }))
}

/// One site's § 14a evidence over a window, for a network operator.
///
/// # Errors
/// As [`data_act`].
pub fn nachweis(
    store: &Store,
    site: &str,
    from: Option<OffsetDateTime>,
    to: Option<OffsetDateTime>,
) -> Result<Value, StoreError> {
    Ok(json!({
        "site": site,
        "basis": "BK6-22-300 Anlage 1, Ziffer 7.2",
        "retention": "two years from the end of each event, Ziffer 7.3",
        "produced_at": OffsetDateTime::now_utc().unix_timestamp(),
        "events": events(store, site, from, to)?,
    }))
}

/// The events over a window, each as the document that was stored.
fn events(
    store: &Store,
    site: &str,
    from: Option<OffsetDateTime>,
    to: Option<OffsetDateTime>,
) -> Result<Vec<Value>, StoreError> {
    store
        .control_events(site, from, to)?
        .into_iter()
        .map(|stored| {
            // `ControlEvent`'s own `serde` form, so the document an operator is
            // handed is the one the box wrote — instants as RFC 3339, powers in
            // watts, every ceiling in sequence — plus the two facts the store
            // knows and the event does not.
            let mut value =
                serde_json::to_value(&stored.event).map_err(|e| StoreError::NotReadable {
                    id: stored.id,
                    detail: e.to_string(),
                })?;
            if let Some(map) = value.as_object_mut() {
                map.insert("id".into(), json!(stored.id));
                map.insert(
                    "expires_at".into(),
                    json!(stored.expires_at.unix_timestamp()),
                );
                // Derived, and in the document because an operator reading a
                // Nachweis is asking exactly these two questions and should not
                // have to fold a list of ceilings to answer them.
                map.insert(
                    "strictest_ceiling_w".into(),
                    json!(stored.event.strictest_ceiling().get()),
                );
                map.insert("below_minimum".into(), json!(stored.event.below_minimum()));
            }
            Ok(value)
        })
        .collect()
}

/// One quarter hour's registers.
///
/// Decimal **strings**, not numbers. A JSON number is a `double` to every reader
/// that has ever parsed one, and a settlement quantity that has been through one
/// is a settlement quantity nobody can reproduce (P3).
fn quarter_hour(q: &QuarterHour) -> Value {
    json!({
        "slot_start": q.slot.start().unix_timestamp(),
        "grid_draw_kwh": q.grid_draw.to_string(),
        "grid_feed_in_kwh": q.grid_feed_in.to_string(),
        "device_consumption_kwh": q.device_consumption.to_string(),
        "device_generation_kwh": q.device_generation.to_string(),
        "anzulegender_wert_ct": q.anzulegender_wert.to_string(),
        "spot_price_ct": q.spot_price.to_string(),
    })
}
