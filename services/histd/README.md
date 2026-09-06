# histd

The fleet's record of what every box did, for
[hems](https://github.com/hupe1980/hems).

Two records with two different owners and two different reasons to exist, and
keeping them apart is most of the design.

| Record | Whose question | What it is |
|---|---|---|
| **Evidence** | the network operator's | `[A1 7.2]` says what a § 14a control event is documented with — the ceiling, when it arrived, what was done about it, a trace of what the connection point drew — and `[A1 7.3]` says it is kept for **two years** |
| **Settlement** | the household's invoice | the quarter-hour meter registers MiSpeL's Abgrenzung and § 42c's allocation are computed from |

Three things fall out of that split:

- **Retention is a column**, so “what will you still have in eighteen months” is a
  query rather than an argument.
- **Quantities are exact decimal strings**, never floats. A settlement that went
  through a `double` is a settlement nobody can reproduce.
- **Reads open their own connection.** A household's export is 370 ms of SQLite,
  and behind one lock a box's evidence write waits 2,7 s for it.

## Three exports, authorised differently

| | |
|---|---|
| `POST /v1/sites/{site}/quarter-hours` | a box writes its own registers |
| `POST /v1/sites/{site}/events` | a box writes its own control events |
| `GET /v1/sites/{site}/nachweis` | the **network operator's** Nachweis |
| `GET /v1/sites/{site}/mispel?year=&month=` | the MiSpeL settlement, `[MiSpeL A1 4.2]` / `[A2 4.2]` |
| `GET /v1/sites/{site}/export` | the **household's** Data Act Article 4 export |

A box's credential reaches its own site and no other. An operator's reaches every
household's § 14a evidence and **none** of their Data Act exports: a Nachweis is
the record of what the operator itself commanded and what the connection point
drew, and it is theirs to check — while Article 4 is a right of the *user*, and a
fleet token is not a household.

The **MiSpeL settlement** sits with the export rather than with the Nachweis. It
is the household's own levy privilege (§ 21 EnFG from 01.10.2026) and the flows
behind it — how full the store was, when it was charged from the grid, what the
roof earned — and the Festlegung has the *Anlagenbetreiber* produce and submit
it. So the same credential that cannot read one cannot read the other, and it is
off the `/mcp` surface for the same reason.

Three things about it are the design rather than the plumbing. The **option is
declared per site** (`[mispel.haus-1]`) and an undeclared site is refused, because
Ausschließlichkeit, Abgrenzung and Pauschal each produce a *different* Nachweis
from the same registers and a guessed one is arithmetically perfect and about
somebody else's installation. Ausschließlichkeit settles nothing and is still
**checked** against those registers: it is a claim that no quarter hour shows
grid draw and storage charging at once, `(1)¼` is what measures that, and the
export reports the figure and names any quarter hour that breaks it. The **period comes from the calendar**, not from the
caller: the arithmetic needs exactly one calendar month and cannot check that it
got one, and a boundary at a fixed `+01:00` would lose an hour every March and
double one every October. And the **denominator is on the document** —
`quarter_hours_expected`, `quarter_hours_present`, `complete` — because a quarter
hour the box could not price gets no register, so a month legitimately has gaps
and a settlement over part of one under-reports every quantity in it while
looking whole.

Until it existed the whole formula set had **no production caller at all**: the
box wrote the registers, this service kept them for two years, and nothing ever
settled them.

## Where this runs, and why it is not on the box

The edge is a **single** daemon, so a gateway runs `hemsd` and nothing else, and
the box's own copy of these records lives in *its* embedded stores behind a
store-and-forward outbox. This daemon is the **fleet** side: everybody's two
years, queryable, which is what a Nachweis and a Data Act export are asked for at
scale.

## Why SQLite today

`bundled`, so it needs no server and no system library — which means every query
here is exercised against a **real** database in `cargo test` rather than against
a mock, and `just ci` stays clone-and-run. The schema is written in `mako`'s
layout, so moving to a Postgres-plus-Iceberg tier is a second migration directory
rather than a rewrite.

## License

MIT OR Apache-2.0
