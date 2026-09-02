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

-- What the box has learned about its own house.
--
-- Two models, and neither can be shipped from a factory: the multiplicative
-- corrector that turns a geometric roof model into a forecast of *this* roof —
-- the tree that shades the east string, the datasheet that was optimistic, the
-- dust — and this household's own quarter hours by day type. A fortnight of
-- observations is what makes a forecast worth having, and a box that forgot
-- them on every reboot would start from a factory roof and refuse to plan until
-- it had seen a quarter hour of its own load.
--
-- One row per model, keyed by name, holding the model as JSON. JSON rather than
-- columns because the *shape* is `hems-forecast`'s and belongs to it: a schema
-- here would be a second definition of a structure this crate does not own, and
-- the one that is wrong would be whichever nobody is testing. What this table
-- promises is only that the bytes come back, and `Store::learned` refuses to
-- deserialise them into a shape that has moved on rather than guessing.
--
-- No `forwarded_at`: this is the box's own state and not a record anybody is
-- owed. It is derived from the two years above and can be rebuilt from them.
CREATE TABLE IF NOT EXISTS learned (
    name        TEXT    NOT NULL PRIMARY KEY,
    model       TEXT    NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;

-- The box's EEBUS identity and the peers it trusts.
--
-- Both have to survive a restart, and for different reasons.
--
-- The **identity** is a private key, and the SKI derived from it is what an
-- installer reads off a screen and gives to the metering point operator. Field
-- reports make that exchange the single most common § 14a commissioning
-- failure, and a box that generated a fresh key on every boot would make it
-- fail again on every boot. Stored as PEM, and never logged.
--
-- The **trust store** is the whole of SHIP's trust model: a peer whose SKI is
-- not in it may complete TLS — it has to, so its SKI can be shown to a user —
-- and is held short of the data phase. Forgetting it on a reboot means a
-- household re-pairing its Steuerbox after a power cut.
--
-- One row, because a box is one node. The `id` column is a constant so the
-- table cannot grow a second identity nobody meant to create.
CREATE TABLE IF NOT EXISTS eebus_identity (
    id            INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    ship_id       TEXT    NOT NULL,
    key_pem       TEXT    NOT NULL,
    -- The trusted peers as JSON, which is `eebus::runtime::TrustStore`'s own
    -- form. A schema here would be a second definition of a structure this
    -- crate does not own.
    trusted       TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
) STRICT;

-- ── Outbound events ─────────────────────────────────────────────────────────
--
-- A CloudEvent the box has produced and the fleet has not yet taken. The day
-- report is the first of them.
--
-- The same argument as `control_event.forwarded_at`, applied to something the
-- box does not otherwise keep: a day report that existed only as an in-flight
-- POST was lost whenever `obsd` blinked, and the box had already thrown away
-- the day it was built from. G3 says the house is never worse off when the
-- cloud is gone, and a report that only survives a working WAN does not meet it.
--
-- What is stored is the **body**, not the signed request. A Standard Webhooks
-- signature covers `id . timestamp . body` and a receiver refuses a timestamp
-- outside five minutes, so a signature made when the row was written is
-- worthless by the time a box back from an outage sends it. The signature is
-- therefore made at each attempt, over the bytes kept here.
CREATE TABLE IF NOT EXISTS outbound_event (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The CloudEvent `id`, which is also the `webhook-id` a receiver
    -- deduplicates on. UNIQUE because a re-sent day is a *correction* of the
    -- same message and not a second one: `hemsd` derives it from the site and
    -- the date for exactly that reason, and a queue that held two rows with one
    -- id would send the same day twice and call it two.
    event_id     TEXT    NOT NULL UNIQUE,
    -- The CloudEvents `type`, so a drain can route without parsing the body.
    event_type   TEXT    NOT NULL,
    -- The exact bytes the signature will cover.
    body         BLOB    NOT NULL,
    created_at   TEXT    NOT NULL,
    -- NULL until the fleet has taken it.
    forwarded_at TEXT,
    -- How many attempts have been made, and why the last one failed. Kept on
    -- the row rather than in a log because "which of my reports is stuck, and
    -- on what" is a question asked days later.
    attempts     INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT
) STRICT;

CREATE INDEX IF NOT EXISTS outbound_event_pending
    ON outbound_event (created_at) WHERE forwarded_at IS NULL;
