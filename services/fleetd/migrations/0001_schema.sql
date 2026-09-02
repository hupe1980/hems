-- What survives a restart of `fleetd`.
--
-- The configured half of the fleet — which sites exist, their enrolment secrets,
-- the configuration each should run and the signature over it — is *not* here.
-- That is declared in the daemon's own configuration, which is the record of
-- what an operator intends, and duplicating it into a database gives two answers
-- to one question.
--
-- What is here is the half only the running fleet knows: the credential a box
-- was issued, and what it has said since. Both are facts about the world rather
-- than about the operator's intent, and neither can be re-derived — a lost token
-- is a fleet of boxes holding a credential nothing recognises.

-- One row per box that has enrolled.
--
-- The row *is* the single-use property: `site` is the primary key, so a second
-- enrolment attempt collides with the first rather than being refused by a map
-- that a restart emptied.
CREATE TABLE IF NOT EXISTS enrolment (
    site        TEXT    PRIMARY KEY,
    -- The credential the box presents from now on.
    token       TEXT    NOT NULL UNIQUE,
    enrolled_at TEXT    NOT NULL
) STRICT;

-- What each box last said it is running.
--
-- Separate from `enrolment` because it changes on every report and the
-- enrolment never changes at all. A row appears only once a box has reported:
-- "has not said yet" and "is on version zero" are different facts, and a
-- default would collapse them.
CREATE TABLE IF NOT EXISTS running (
    site            TEXT    PRIMARY KEY REFERENCES enrolment(site) ON DELETE CASCADE,
    running_version TEXT    NOT NULL,
    last_seen       TEXT    NOT NULL
) STRICT;

-- Answering "which of my boxes have gone quiet" without reading every row.
CREATE INDEX IF NOT EXISTS running_by_last_seen ON running (last_seen);
