-- hemsd — the box's own record of one household.
--
-- The same two records `histd` keeps for the fleet, for a different reason:
-- [A1 7.3] documents a control event for **two years**, and G3 says the house is
-- never worse off when the cloud is gone. A record that exists only once it has
-- been uploaded is an intention with a network dependency, and the day a network
-- operator asks about is the day the link was down.
--
-- Two differences from the fleet's copy, and both follow from D1 — one box, one
-- household, one process:
--
--   * there is no `site_id`. A box is one site, and a column that is the same
--     value in every row is a join key for a join nobody makes.
--   * `forwarded_at` is the **outbox**. NULL means the fleet has not
--     acknowledged this row yet. It is a column rather than a queue table
--     because what is tracked is a property of the row, and a queue that can
--     disagree with the record it points at is a second source of truth.
--
-- Forwarded is not deleted: the two years are the household's, not the fleet's,
-- so pruning follows the retention window and never an acknowledgement.
--
-- There is no `PRAGMA` here. A migration runs inside a transaction and
-- `journal_mode = WAL` cannot be set inside one; `foreign_keys` and
-- `busy_timeout` are per *connection*, so setting them here would configure the
-- connection that created the schema and no other. All three are in
-- `Store::migrate`, applied on every open.

-- The quarter-hour meter registers MiSpeL and § 42c settle from.
--
-- Every quantity is a decimal *string*, never a float: a settlement that went
-- through an `f64` is a settlement nobody can reproduce, and `rust_decimal`'s
-- own `Display` is the exact form the rest of the workspace travels in (P3).
CREATE TABLE IF NOT EXISTS quarter_hour (
    -- The slot's start, as Unix seconds. An instant, not a local string: a
    -- household's own day boundary is `metering`'s question, and a wall-clock
    -- string here would ask it twice, differently, twice a year.
    slot_start             INTEGER NOT NULL PRIMARY KEY,
    grid_draw_kwh          TEXT    NOT NULL,
    grid_feed_in_kwh       TEXT    NOT NULL,
    device_consumption_kwh TEXT    NOT NULL,
    device_generation_kwh  TEXT    NOT NULL,
    anzulegender_wert_ct   TEXT    NOT NULL,
    spot_price_ct          TEXT    NOT NULL,
    -- A register may be restated — a substitute value replaced by a real one, a
    -- correction from the metering point operator — and a settlement rerun has
    -- to be able to ask what was known on the day rather than what is known now.
    recorded_at            INTEGER NOT NULL,
    forwarded_at           INTEGER
) STRICT;

-- Partial, so it indexes only what is *outstanding*: on a box that is keeping up
-- that is a handful of rows out of two years, and on one that has been offline
-- for a week it is exactly the week.
CREATE INDEX IF NOT EXISTS quarter_hour_pending
    ON quarter_hour (slot_start) WHERE forwarded_at IS NULL;

-- One § 14a control event, [A1 7.2].
--
-- **A document plus projections.** `document` is the whole `ControlEvent` as
-- `serde` JSON and is the only thing the event is reconstructed from; the
-- columns beside it are there to be queried and are derived from the same value
-- in the same statement. A column per field with the enums written through
-- `format!("{:?}")` cannot be read back: `Debug` is not a serialisation, nothing
-- promises it round trips, and renaming a variant would silently change what two
-- years of evidence say.
CREATE TABLE IF NOT EXISTS control_event (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The event, **without** its samples: those are the table below, so each
    -- fact has one home and a long trace is not re-parsed to answer "how many".
    document        TEXT    NOT NULL,
    rule            TEXT    NOT NULL,
    received_at     INTEGER NOT NULL,
    released_at     INTEGER,
    first_ceiling_w REAL    NOT NULL,
    strictest_ceiling_w REAL NOT NULL,
    minimum_power_w REAL    NOT NULL,
    below_minimum   INTEGER NOT NULL,
    -- Two years from the day it closed, [A1 7.3]. A column rather than a policy:
    -- a retention rule nobody can query is a retention rule nobody can prove.
    expires_at      INTEGER NOT NULL,
    forwarded_at    INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS control_event_by_expiry ON control_event (expires_at);
CREATE INDEX IF NOT EXISTS control_event_pending
    ON control_event (received_at) WHERE forwarded_at IS NULL;

-- The minute-resolution trace of what the connection point drew, [A1 7.2].
CREATE TABLE IF NOT EXISTS compliance_sample (
    event_id      INTEGER NOT NULL REFERENCES control_event(id) ON DELETE CASCADE,
    at            INTEGER NOT NULL,
    netzwirksam_w REAL    NOT NULL,
    ceiling_w     REAL    NOT NULL,
    PRIMARY KEY (event_id, at)
) STRICT;
