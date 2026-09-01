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

## Two exports, authorised differently

| | |
|---|---|
| `POST /v1/sites/{site}/quarter-hours` | a box writes its own registers |
| `POST /v1/sites/{site}/events` | a box writes its own control events |
| `GET /v1/sites/{site}/nachweis` | the **network operator's** Nachweis |
| `GET /v1/sites/{site}/export` | the **household's** Data Act Article 4 export |

A box's credential reaches its own site and no other. An operator's reaches every
household's § 14a evidence and **none** of their Data Act exports: a Nachweis is
the record of what the operator itself commanded and what the connection point
drew, and it is theirs to check — while Article 4 is a right of the *user*, and a
fleet token is not a household.

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
