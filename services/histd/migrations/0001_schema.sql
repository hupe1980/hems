-- histd — the fleet's copy of the § 14a record, across every site.
--
-- Three tables, and the shape of each is decided by the question it has to
-- answer rather than by what was convenient to write.
--
-- `quarter_hour` is the **settlement** record: the meter registers MiSpeL's
-- Abgrenzung (BK 618-25-02) and § 42c's allocation are computed from. Every
-- quantity is a decimal *string*, never a float — a settlement that went through
-- an `f64` is a settlement that cannot be reproduced, and `rust_decimal`'s own
-- `Display` is the exact form the rest of the workspace already travels in (P3).
--
-- `control_event` and `compliance_sample` are the **evidence** record of
-- [A1 7.2]: what the network operator asked for, when, what was done about it,
-- and what the connection point actually drew while it lasted. [A1 7.3] says it
-- is kept for two years, which is why `expires_at` is a column rather than a
-- policy somebody remembers to apply.
--

-- There is no `PRAGMA` in this file, and that is not an omission. A migration
-- runs inside a transaction, and `journal_mode = WAL` cannot be set inside one;
-- and `foreign_keys` and `busy_timeout` are per *connection* rather than per
-- database, so setting them here would configure the connection that created the
-- schema and no other — which works until the second time the process starts.
-- All three are in `Store::migrate`, applied on every open.

-- One site's quarter-hour meter registers.
CREATE TABLE IF NOT EXISTS quarter_hour (
    site_id            TEXT    NOT NULL,
    -- The slot's start, as Unix seconds. An instant, not a local string: a
    -- household's own day boundary is `metering`'s question and storing a
    -- wall-clock string here would ask it twice, differently, twice a year.
    slot_start         INTEGER NOT NULL,
    grid_draw_kwh      TEXT    NOT NULL,
    grid_feed_in_kwh   TEXT    NOT NULL,
    device_consumption_kwh TEXT NOT NULL,
    device_generation_kwh  TEXT NOT NULL,
    anzulegender_wert_ct   TEXT NOT NULL,
    spot_price_ct          TEXT NOT NULL,
    -- Bitemporality, the one thing `meterstore` owns that a local store still
    -- needs: a register may be restated, and a settlement rerun has to be able
    -- to ask what was known on the day rather than what is known now.
    recorded_at        INTEGER NOT NULL,
    PRIMARY KEY (site_id, slot_start)
) STRICT;
-- No index on `(site_id, slot_start)`: the primary key already is one, and a
-- second copy of it would be paid for on every write and read by nothing.

-- One § 14a control event, [A1 7.2].
--
-- **A document plus projections.** `document` is the whole `ControlEvent` as
-- `serde` JSON and is the only thing the event is ever *reconstructed* from; the
-- columns beside it exist to be queried, filtered and indexed on, and are
-- derived from the same value in the same statement.
--
-- A column per field with the enums written through `format!("{:?}")` cannot be
-- read back: `Debug` is not a serialisation, nothing promises it round trips,
-- and renaming a variant would silently change what two years of evidence say.
CREATE TABLE IF NOT EXISTS control_event (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    site_id         TEXT    NOT NULL,
    -- The event itself, `serde` JSON, **without** its samples: those are the
    -- table below, so that each fact has one home and a trace of ten thousand
    -- rows is not re-parsed to answer "how many".
    document        TEXT    NOT NULL,
    -- Projections. Everything below is derivable from `document` and is stored
    -- because a query needs it: the rule to tell an operator's instruction from
    -- the box restraining itself, the instants to window on, the powers and the
    -- flag so a Nachweis does not parse two years of JSON to find one breach.
    rule            TEXT    NOT NULL,
    received_at     INTEGER NOT NULL,
    released_at     INTEGER,
    first_ceiling_w REAL    NOT NULL,
    strictest_ceiling_w REAL NOT NULL,
    minimum_power_w REAL    NOT NULL,
    below_minimum   INTEGER NOT NULL,
    -- Two years from the day it closed, [A1 7.3]. A column rather than a policy:
    -- a retention rule nobody can query is a retention rule nobody can prove.
    expires_at      INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS control_event_by_site
    ON control_event (site_id, received_at);
CREATE INDEX IF NOT EXISTS control_event_by_expiry
    ON control_event (expires_at);

-- The minute-resolution trace of what the connection point drew, [A1 7.2].
CREATE TABLE IF NOT EXISTS compliance_sample (
    event_id     INTEGER NOT NULL REFERENCES control_event(id) ON DELETE CASCADE,
    at           INTEGER NOT NULL,
    netzwirksam_w REAL   NOT NULL,
    ceiling_w    REAL    NOT NULL,
    PRIMARY KEY (event_id, at)
) STRICT;
