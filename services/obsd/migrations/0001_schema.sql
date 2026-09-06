-- obsd — every box's day, and what the fleet says about them.
--
-- The fleet view's answers outlive the process that accepted them, and are the
-- same from either replica behind a load balancer (D157). A box reports a day
-- once and does not keep the CloudEvent, so a report this service loses is lost
-- — including the named list of households that did not respect a network
-- operator's reduction, which is the one thing it exists to produce.
--
-- # One row per site per day, and the day itself as a document
--
-- The same shape `histd`'s `control_event` uses, and for the same reason: the
-- document is what a day is *reconstructed* from, and the columns beside it
-- exist to be filtered and indexed on. `DayKpis` has forty-odd fields, most of
-- which no query ever asks about; a column for each would be a schema migration
-- every time a box learns to report one more thing.
--
-- The primary key is `(site, day)` rather than an identity, because a box that
-- re-sends yesterday after a reconnect is **correcting itself** — a fleet that
-- counted the day twice would double one household's saving inside an average.
-- The upsert is that rule, in the one place it cannot be forgotten.

CREATE TABLE IF NOT EXISTS site_day (
    site        TEXT        NOT NULL,
    -- The local day the report is about. A date and not an instant: a household's
    -- day boundary is `metering`'s question and a box has already answered it.
    day         DATE        NOT NULL,
    -- The whole `DayKpis`, as it was reported.
    document    JSONB       NOT NULL,
    -- When this service accepted it, which is what "has this box gone quiet"
    -- is measured from. Distinct from `day`: a box that has been offline for a
    -- week and then forwards its backlog reports seven days at one instant.
    reported_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (site, day)
);

-- The retention sweep. The primary key leads with `site`, so it cannot answer
-- `day < $1` across every household, and without this the daily sweep is a
-- sequential scan of the fleet's whole window to delete one day of it.
--
-- There is deliberately **no** index on `reported_at`. "Which of my boxes have
-- gone quiet" is answered from `last_report`, which is the newest `reported_at`
-- in a window this service is already reading in full for the summary — so an
-- index on it would be paid for on every insert and read by nothing. A schema is
-- subject to the same rule as a module: one with no caller is not a feature.
CREATE INDEX IF NOT EXISTS site_day_by_day ON site_day (day);
