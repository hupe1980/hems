+++
title = "The fleet"
description = "The services around one box: day-ahead prices, weather, two years of § 14a evidence, enrolment, signed releases, and a fleet view that counts breaches."
weight = 11
+++

The edge is **one** process. The § 14a failsafe is a sixty-second heartbeat and a
two-hour minimum, and an IPC hop inside that path buys nothing — so a gateway
runs `hemsd` and nothing else, with its stores embedded. Everything else in this
page is cloud.

`hemsd run` is that process today: a household, a tariff and a driver set from
TOML, one task per driver holding its own socket, the guard and the arbiter
deciding against real measurements, and a receding-horizon plan every five
minutes against the two arrows coming *into* it below.

Neither of those is a trust anchor. A box that cannot reach `tariffd` or
`forecastd` keeps the house safe and lawful and loses the plan — a cost in euros
rather than in compliance — and it says which of the two is missing on its own
readiness probe rather than looking like a box that is planning badly.

<pre class="mermaid">
flowchart TB
  subgraph box["the household"]
    H["<b>hemsd</b><br/>guard · arbiter · planner<br/>its own two years of evidence<br/>what its roof has learned<br/>+ an outbox"]
  end
  subgraph fleet["the fleet"]
    T["<b>tariffd</b><br/>five day-ahead sources"]
    F["<b>forecastd</b><br/>ICON-D2 via Open-Meteo"]
    HI["<b>histd</b><br/>evidence + registers<br/>Nachweis · Data Act export"]
    FL["<b>fleetd</b><br/>enrolment · config · releases"]
    O["<b>obsd</b><br/>the fleet view"]
    A["<b>agentd</b><br/>advisory only"]
  end
  T -- "prices" --> H
  F -- "the sky" --> H
  H -- "evidence, from its own outbox" --> HI
  FL -- "signed config + manifests" --> H
  H -- "a signed CloudEvent per day" --> O
  O -. "what a population says" .-> A
  NO["network operator"] -. "Nachweis" .-> HI
  HH["the household"] -. "Data Act Art. 4 export" .-> HI
</pre>

**Nothing here is a trust anchor.** The § 14a limit comes off a wire from a
Steuerbox and the guard enforces it locally; prices and weather only ever make a
plan *better*; and an update — like the box's own configuration — is verified
against a key the box was **built** with rather than against a hostname. The
house is never worse off when the cloud is gone.

## One shell, and two decisions in it

`hems-service` owns the forty lines every daemon has — configuration, logging, a
health surface, a bounded shutdown — and owns nothing about energy. Seven daemons
written six times means those forty lines diverge in the direction that costs
most, because the one that is wrong is the one whose readiness probe lies.

**Live and ready are different questions.** An orchestrator restarts a process
that fails `livez` and merely stops routing to one that fails `readyz`. A daemon
whose upstream price source is down is not *broken*, and restarting it does not
bring ENTSO-E back. Answering the second with the first is the mistake that makes
a fleet oscillate.

And `livez` has to be able to **fail**. It could not: nothing in the workspace
ever cleared the flag, so every daemon answered 200 as long as its HTTP server
was answering — including one whose control loop had panicked half an hour
earlier. That is the shape of liveness probe that is worse than none, because
the orchestrator told to restart on it never does and the daemon looks healthy
throughout. So a task the process cannot do its job without is spawned as
*vital*: if it panics or is cancelled, `/livez` fails and the process is one to
restart. On the box that is the control loop — a `hemsd` with no guard, no
arbiter and no evidence record is not a running energy manager whatever it will
tell you over HTTP. Ending because a shutdown was asked for is not a fault:
failing liveness during a drain has an orchestrator send `SIGKILL` in the middle
of it.

**Readiness names every dependency and when it was last good**, so the first
click in an incident is also the last:

```console
$ curl -s localhost:8080/readyz
{"ready":true,"probes":{"collector":{"ready":true,"last_good":"2026-09-01T01:17:43Z"}}}
```

Every daemon answers `/livez`, `/readyz` and `/version`, takes a `--config` and a
set of `HEMS_<NAME>_*` environment variables, and **none of them needs the others
to start**.

## `tariffd` — the price stack

Three jobs. **Fetch** each configured source on a schedule, with a backoff that
does not turn one outage into a denial of service against somebody's free API.
**Reconcile** what arrives. **Serve** the box's question, and say honestly how
much of the horizon is covered — so a household with no prices plans against a
flat default *knowingly*.

A curve arrives twice and the two disagree, so the reconciliation is **written
down**: a more trusted source always wins, then a finer publication, then the
later observation. “Last write wins” gets the first one wrong, and a Tibber curve
arriving after an ENTSO-E one would overwrite it.

| | |
|---|---|
| Endpoints | `/v1/prices?from=…&slots=96`, `/v1/prices/coverage`, `/v1/carbon`, `/v1/modul3/{netzbetreiber}` |
| Authentication | **none, on purpose** — a day-ahead curve and a published price sheet are nobody's household data |
| Agents | `/mcp`, when switched on |
| Holds | two days each way, plus the curated Modul 3 catalogue |

It also serves the grid's **carbon intensity**, and that route exists because
the chain behind it was dead end to end: the Energy-Charts parser had no
consumer, the fetch loop dropped the series it had just fetched, the price
stack's own intensity field was hard-coded absent, and the planner's carbon term
could therefore only ever see a flat annual constant — which makes a carbon
price algebraically the same thing as an autarky premium, because every hour is
equally dirty. Two dials, one behaviour.

The reason to have it is that intensity and price **disagree**. A still winter
evening is dear *and* dirty and both signals point the same way; a windy night
is cheap and only moderately clean, while a sunny midday is cheap *and* clean.
So a household that prices carbon moves flexible load out of the cheap night and
into the cheap middle of the day, which no price signal on its own would ask
for — and in the evening peak, where the two agree, the dial correctly changes
nothing.

It also carries the **curated Modul 3 catalogue**: one transcription of each
network operator's Zählzeitdefinition per year — the identical shape a single
box takes as `[tariff.modul3]` — validated against the BDEW Anwendungshilfe at
start-up. A calendar that breaks a rule refuses the daemon, because a fleet
serving windows nobody may sell prices a whole Netzgebiet against a tariff
nobody may be billed on.

The fetching is behind an `Upstream` trait. In production it is `reqwest`; in
every test it is a table of captured responses, so the whole daemon — schedule,
backoff, reconciliation, readiness, the HTTP surface — is covered without a
network, in milliseconds, on a machine that is offline. A test that needs the
internet is a test that is skipped.

## `forecastd` — the sky, never a finished forecast

The distinction is the whole architecture. A weather model knows about the sky
and knows nothing about the tree that shades the east string, the chimney, or the
fact that this roof has not been cleaned since 2023.

So what crosses the wire is **irradiance and temperature**. Turning that into a
production forecast is the residual model's job and it happens **on the box**,
from that box's own metering, because the correction is a property of one roof
and cannot be learned centrally without the meter that sees it. A fleet service
that shipped finished production forecasts would be a fleet service that had to
know every roof.

| | |
|---|---|
| Endpoints | `/v1/weather/{location}`, `/v1/production/{location}?kwp=9.8` |
| Authentication | **none, on purpose** — irradiance over a location is public weather |
| Agents | `/mcp`, when switched on |
| Source | ICON-D2 through Open-Meteo, at quarter-hour resolution |

The cache is the same shape as `tariffd`'s and for the same reason: a weather run
is good for hours, so an outage that lasts one is invisible, and readiness is
computed from how much of the horizon is *covered* rather than from when the last
request returned.

## `histd` — two records, two owners

| Record | Whose | Why |
|---|---|---|
| **Evidence** | the network operator's question | `[A1 7.2]` says what a control event is documented with; `[A1 7.3]` says two years |
| **Settlement** | the household's invoice | the quarter-hour registers MiSpeL's Abgrenzung and § 42c's allocation are computed from |

Keeping them apart is most of the design, and three things fall out of it:

- **Retention is a column**, so “what will you still have in eighteen months” is a
  query rather than an argument.
- **Quantities are exact decimal strings**, never floats. A settlement that went
  through a `double` is one nobody can reproduce.
- **Reads open their own connection.** A household's export is 370 ms of SQLite,
  and behind one lock a box's evidence write waits 2,7 s for it.

| | |
|---|---|
| Endpoints | `/v1/sites/{site}/quarter-hours`, `/v1/sites/{site}/events`, `/v1/sites/{site}/nachweis`, `/v1/sites/{site}/export`, `/v1/sites/{site}/mispel` |
| Authentication | per site, and a box may write only **its own** |
| Agents | `/mcp`, authorising each caller as itself — see below |

The three exports are authorised differently on purpose. A **Nachweis** is the
record of what the network operator itself commanded and what the connection
point drew, and it is theirs to check. The **export** is the household's data
under Data Act Article 4 — everything the product generated, including when the
shower ran and which fortnight nobody was in. Article 4 is a right of the *user*,
and a fleet token is not a household.

### The MiSpeL settlement

The third, and until recently the arithmetic had no caller at all: the box wrote
the registers, this service kept them for two years, and **nothing ever settled
them** — the whole formula set of Anlage 1's (1)–(33) and Anlage 2's (P1)–(P15)
was reachable only from a unit test, so it could have been wrong in every
release without a single day noticing. From 01.10.2026 that document is what a
household's levy privileges (§ 21 EnFG) and its EEG support depend on.

Three things about it are the design rather than the plumbing:

- **The option is declared per site, and an undeclared site is refused.** The
  Festlegung has the household choose between Ausschließlichkeit, Abgrenzung and
  Pauschal, and each produces a *different* Nachweis from the same registers —
  so one computed under a guessed Basisfall is arithmetically perfect and about
  somebody else's installation. Ausschließlichkeit settles nothing and is still
  **checked**: it is a claim that no quarter hour shows grid draw and storage
  charging at the same time, the Festlegung's own `(1)¼ = MIN[Z1NB¼ ; Z2V¼]` is
  what measures it, and the export reports that figure and names the quarter
  hours that break it. A Nachweis that is only a declaration is the one Nachweis
  nobody can check.
- **The period comes from the calendar, not from the caller.** The arithmetic
  needs exactly one calendar month and cannot check that it got one, so the
  route takes a year and a month and builds the window itself. A boundary at a
  fixed `+01:00` would lose an hour every March and double one every October —
  on the two months a settlement is most likely to be queried.
- **The denominator is on the document.** A quarter hour the box could not price
  gets no register at all, so a month legitimately has gaps, and a settlement
  summed over 2 800 of a month's 2 980 quarter hours under-reports every
  quantity in it while looking exactly like a whole one. `quarter_hours_expected`,
  `quarter_hours_present` and `complete` sit beside the figures, because a number
  whose denominator is invisible cannot fail.

It is the **household's** document rather than the operator's, so it sits beside
the Data Act export and off the agent surface: the Festlegung has the
Anlagenbetreiber produce and submit it, and a § 14a operator credential that
could read every household's storage economics is a reach nothing granted it.

SQLite (bundled) is what is here now, because it needs no server and no system
library: every query is exercised against a *real* database in `cargo test`
rather than against a mock, and `just ci` stays clone-and-run. The schema is
written in `mako`'s layout, so the move to a Postgres-plus-Iceberg tier is a
second migration directory rather than a rewrite.

## `fleetd` — enrolment, configuration, releases

**Enrolment.** A box arrives holding an enrolment secret an installer put on it
and leaves holding a long-lived credential of its own. The secret is
**single-use**: a second attempt with the same one is refused, because an
enrolment secret that still works after the box is in the field is a credential
sitting in an installer's notes.

Both facts are in an embedded store rather than in memory, and each is written
**before** it is acted on. A credential exists in exactly two places — the box
and the fleet — so a `fleetd` that forgot one on restart would leave a household
presenting a token nothing recognises, unable to enrol again because its secret
is spent. What is *not* stored is which sites exist and what they should run:
that is the operator's intent and it lives in the configuration file.

**Configuration.** Versioned, and the box reports which version it is *running*.
That is the half usually missing: a fleet that can only push cannot answer “how
many of my boxes actually took the change”, which is the question asked the
morning after a rollout.

**Releases.** `fleetd` publishes a manifest and an Ed25519 signature over it, and
never holds a signing key. See [Security](@/docs/security.md) for why that
sentence is the whole argument.

| | |
|---|---|
| Endpoints | `/v1/enrol`, `/v1/config`, `/v1/config/running`, `/v1/releases/{component}` |
| Roster | `/v1/fleet`, and an **operator's** token — it says which households exist, which build each is on and which are unreachable right now, and it answers within that operator's tenant |

## `obsd` — the fleet view

Three questions, and they are not the same shape.

| Question | Shape | Why it matters |
|---|---|---|
| “How are we doing?” | an **average** — saving, self-sufficiency, forecast scores | an average over enough days is the only honest way to quote any of them, and each has the denominator its own question needs |
| “Who is broken?” | a **count** | one household in ten thousand that failed to respect a reduction is an incident with a name and a date; “99,99 % compliance” reads as success |
| “Is the forecast honest?” | **neither** | one day's coverage figure is a coin toss reported to three significant figures, and only the merge of many days can be compared with the 80 % the band promises |

The averages do **not** share a denominator, and that is the second thing worth
saying. A *saving* is a counterfactual: it needs a baseline, only a simulator can
re-run a household as an unmanaged one, and a day the planner was shown the
weather is an upper bound rather than a result — so it is a mean over the days
that have one. *Self-sufficiency* is three meter readings, and every box has
them. Sharing the two meant a fleet of real households published nought while
every box in it was reporting a perfectly good figure.

So the summary carries breaches as a **list of sites** and never as a rate — and a
box reports its scores rather than judging them. The same shape covers the two
seam numbers: the days a box spent without a plan, and the days a device could
not hold a command the arbiter gave it, are **named days** rather than fleet
percentages. A wallbox refusing a third of its commands is one household with one
installation problem, and an average puts it at half a per cent.

`obsd` holds a **window, not a history**. The record is `histd`'s; what lives here
is derived, bounded and rebuildable, and losing it costs a dashboard rather than
a Nachweis.

There are **two** statutory limits on a household's connection point, and the
fleet lists them apart. § 14a arrives as an instruction and leaves a record, so a
breach can be read back out of the store; § 9 Abs. 2 EEG applies to the plant by
force of law and leaves nothing, so the box measures the instantaneous
Einspeiseleistung against the ceiling the guard derives on every tick and reports
the worst excess of the day. `over_feed_in_ceiling` sits beside `breached` rather
than mixed into it: different rules, answered by different parties.

### A day only arrives signed

```console
$ just fleet-demo

  reported to http://127.0.0.1:8080/v1/days — HTTP 202

$ curl -s -H "Authorization: Bearer tok-demo" localhost:8080/v1/fleet | jq '{sites, saving_eur, breached}'
{
  "sites": 1,
  "saving_eur": 2.092545122909012,
  "breached": []
}
```

`saving_eur` is the reference winter day's own €2,14, which is the point: the
fleet view is fed by the same number the day prints, through a type both sides
share, so a renamed field is a compile error rather than a dashboard reading zero
for six weeks.

### A box reports what it metered, and no money at all

`hemsd run` closes each Berlin calendar day and queues a report. It carries the
energies the connection point saw and the whole § 14a compliance record — and
**no `economics` and no `forecast`**, because a box cannot produce either
honestly:

- a **baseline** is what the day would have cost with no energy manager, and the
  only way to get one is to re-run the day as an unmanaged house. A box has what
  happened;
- five of the six **cost** terms are modelled rather than metered — battery wear,
  curtailment, discomfort, energy borrowed from the stores, charge past what was
  asked for — so a box filling in the sixth would publish a figure that read as
  complete and understated itself;
- a **forecast score** merges as an episode, so an unmeasured coverage reported
  as `0.0` would drag the fleet's calibration toward zero while looking like a
  contribution.

So the summary counts `unmeasurable_days` beside `foresight_days`, and a fleet of
real boxes reports **no** saving rather than a saving of zero. The two exclusions
are counted apart because they mean different things: a foresight day is an upper
bound nobody can reach, and a day without a baseline is a household.

The day travels over **TLS** as a **signed CloudEvent**, and the two are different
guarantees the box needs both of: the signature says the report is the one this
box sent and has not been edited; TLS says nobody read it on the way. Plain
`http` is allowed only to a loopback address and refused anywhere else.

It is queued in the box's own store before it is sent, so a fleet that is down
costs a delay rather than a day — the same store-and-forward the evidence uses.
Three things follow from that, and they are the interesting part:

- **The signature is made at the attempt, never stored.** Standard Webhooks
  signs `id . timestamp . body` and a receiver refuses a timestamp outside five
  minutes, so a signature made when the row was written is worthless by the time
  a box back from an overnight outage sends it. What is kept is the body.
- **The message id is the site and the date**, so a box that recomputes
  yesterday is *correcting* one report rather than adding a second — and `obsd`
  deduplicates on the same string the signature covers.
- **A refusal is not a retry.** A `5xx`, a `429` or a refused connection is the
  fleet being unavailable and the day is worth keeping. Any other `4xx` is
  `obsd` having read the document and refused it, and asking again changes
  nothing — so the row leaves the backlog carrying the refusal, rather than the
  box asking the same rejected question every five minutes for ever.

That endpoint holds the list of households that did *not* respect a network
operator's reduction, so an unauthenticated write to it can put a compliant site
on that list or take a breach off it. The signature covers the message id, the
timestamp and the exact bytes, so a captured request cannot be replayed or
edited — and an `obsd` with no secret configured refuses everything rather than
accepting everything.

Each box signs with **its own** key, and a report whose claimed site is not that
key's site is refused. A shared secret would authenticate the bytes and say
nothing about the sender, which for this endpoint means any box could write a
§ 14a breach onto a household that is not its own.

## Every one of them answers an agent too

Each fleet service mounts a read-only Model Context Protocol surface on the port
it already binds, and `agentd` is the advisory plane that reads them. Both are on
their own page — see [Agents](@/docs/agents.md).

## A store beats a passthrough

`tariffd` holds two days each way, `forecastd` keeps the last good run, and both
compute readiness from what they still **cover** rather than from when the last
request returned. A WAN outage shorter than the horizon therefore costs the
household nothing at all.

The storage half of the same idea is on the box. `[A1 7.3]` keeps a control event
for two years, so `hemsd` holds its **own** copy and forwards second: what the
fleet has not acknowledged is an outbox that grows, not a gap. Two things travel
on that loop — the `[A1 7.2]` control events, and the quarter-hour **registers**
MiSpeL's Abgrenzung and § 42c's allocation are computed from — and a drain that
sent one and silently kept the other would look exactly like a box with nothing
to say, which is why a test asserts both arrive. A record that
exists only once it has been uploaded is an intention with a network dependency —
and the day a network operator asks about is the day the link was down.

The drain is slow and batched for the same reason: what is being forwarded is
already safe, so urgency buys nothing but load on a service the household does
not own, and a box back from a month offline must not send a month at once.
Forwarded is never *deleted* — the two years are the household's, and pruning
follows the retention window rather than an acknowledgement. The sweep runs where
the record is written, a few times a day, rather than on a timer with its own
idea of when: two years is `[A1 7.3]`'s floor and keeping more is holding a
household's control history for no reason anybody asked for.

## Running the fleet locally

```console
$ just fleet-demo                     # or, spelled out:

$ cat > obsd.toml <<'TOML'
[webhook_secrets]
reference-household = ["env:HEMS_OBSD_SECRET_REFERENCE_HOUSEHOLD"]
TOML
$ HEMS_OBSD_SECRET_REFERENCE_HOUSEHOLD=whsec_demo cargo run -p obsd -- --config obsd.toml &
$ HEMS_OBSD_SECRET=whsec_demo \
    cargo run -p hemsd -- simulate --day winter --report-to http://127.0.0.1:8080
```

Notice what the secret looks like in that file — the *reference* is configured
and the value is not. That mechanism is on the [Security](@/docs/security.md)
page.
