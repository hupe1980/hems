# tariffd

The price service of [hems](https://github.com/hupe1980/hems): fetch, reconcile,
serve.

`hems-tariff::source` parses what the five published sources publish and
`hems-tariff::cache` decides what to do when two of them disagree. Both are pure
functions and neither has ever made a request. This is the process that does.

## Three jobs

**Fetch.** Ask each configured source on a schedule, and keep asking after a
failure with a backoff that does not turn one outage into a denial of service
against somebody else's free API.

**Reconcile.** A curve arrives twice and the two disagree, so the order is
**written down**: a more trusted source always wins, then a finer publication,
then the later observation. “Last write wins” gets the first one wrong, and a
Tibber curve arriving after an ENTSO-E one would overwrite it.

**Serve.** Answer the box's question — *what do you have for this horizon* — and
say honestly how much of it is covered, so a household with no prices plans
against a flat default **knowingly** rather than being told nothing is wrong.

| | |
|---|---|
| `GET /v1/prices?from=…&slots=96` | the resolved quarter-hourly curve |
| `GET /v1/prices/coverage` | how much of a window it can price |
| `GET /livez`, `/readyz`, `/version` | as every daemon here |

**Unauthenticated on purpose**: a day-ahead curve is a published auction result.
That is a decision rather than an oversight, which is why it is written down.

## The fetching is behind a trait

`Upstream` is the seam. In production it is `Http`, which is `reqwest`. In every
test it is a table of captured responses, so the whole daemon — schedule,
backoff, reconciliation, readiness, the HTTP surface — is covered without a
network, in milliseconds, on a machine that is offline. A test that needs the
internet is a test that is skipped.

## A store beats a passthrough

It holds two days each way and computes readiness from what it still **covers**
rather than from when the last request returned, so a WAN outage shorter than the
horizon costs the household nothing at all.

## Configuration

A `--config` file and `HEMS_TARIFFD_*` in the environment, which wins. Every
credential may be written as `env:NAME` or `file:/run/secrets/x` rather than as
itself, and an unresolvable reference **stops the daemon** rather than being taken
literally — a `tariffd` sending the literal string `env:ENTSOE_TOKEN` as a
security token looks exactly like one whose source has started rejecting it.

## License

MIT OR Apache-2.0
