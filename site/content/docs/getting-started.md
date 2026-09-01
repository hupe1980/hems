+++
title = "Getting started"
description = "Clone it, run the checks, watch a household get through a January day with a § 14a reduction."
weight = 1
+++

## Requirements

Rust 1.94 (pinned in `rust-toolchain.toml`) and [`just`](https://just.systems).
Nothing else: the default solver is pure Rust, so there is no C++ toolchain and
no system library to install.

## Run everything

```console
$ git clone https://github.com/hupe1980/hems && cd hems
$ just ci
```

`just ci` runs formatting, Clippy with warnings as errors, the purity check, the
whole test suite, the workspace guards, `cargo-deny` and the documentation
build — the same list, in the same order, that CI runs.

## Watch a day

```console
$ just demo-all
```

Seven days and six comparisons:

| Day | What it shows | Saved |
|---|---|---|
| `winter` | a network operator reduction from 17:00 to 18:30, a car that must be full by seven, and a dishwasher the plan holds back half an hour | €2,09 |
| `summer` | more production than the house can use, and four quarter hours of negative prices | €8,61 |
| `deadline` | a car that arrives *as the reduction starts* and has three hours to take 13 kWh under the household's own 10,5 kW minimum, shared with a heat pump | €2,51 |
| `shared` | the same evening on a household with **no store**, owed 7,56 kW rather than 10,5, and a reduction that arrives at 17:07 rather than on the re-planning grid | €1,28 |
| `offline` | **the planner switched off** — what the box does on its own | €7,94 |
| `autumn` | a September day, planner off, the surplus in the band only one conductor can use | €2,72 |
| `capped` | a clear May day on a 20 kWp roof, with the § 9 EEG 60 % cap binding at 12,06 of 12,00 kW — the 60 W over is the inverter's own settling time, not a decision | €1,31 |

…plus eight comparisons:

| Flag | What it isolates |
|---|---|
| `--perfect-foresight` | the January day with the future known in advance: €5,25 against the €2,09 an honest forecast earns |
| `--wear-eur-per-kwh 0` | what a cost-only optimiser does to a battery — 18,7 kWh of throughput instead of 15,5 |
| `--no-phase-switching` | on the autumn day: 0,2 kWh into the car against 13,1, and a car 4,8 kWh short of where it had to be |
| `--risk` | one future or three, and how much of the objective sits on the worst of them. `just risk` runs the multi-weather sweep that says what each is worth — a single day pays a hedge's premium every time and makes its claim never |
| `--imsys` | the § 9 EEG cap lifted on both households — one cent to the managed one, twelve to the unmanaged one |
| `--uniform-weights` | every asset given the same allocation weight, which is what a single marginal value per slot amounts to |
| `--sharing` | the household inside a § 42c energy-sharing community: 14,5 kWh allocated, the flexible load moved into the neighbours' daylight — and the baseline in the same community, so the figure is the value of shifting rather than of joining |
| `--heat-pump-on-off` | a single-speed compressor rather than a modulating one — the only unit whose cycling is scheduled, so the only one a minimum runtime constrains. Slower on purpose: a binary per slot per re-plan, committed per clock hour beyond a two-hour head |

`--store <path>` keeps the day's § 14a evidence and quarter-hour registers in the
box's own database — the two years `[A1 7.3]` asks for, written *before* anything
is reported anywhere, with an outbox column so what the fleet has not
acknowledged is a backlog rather than a gap. A record that exists only once it
has been uploaded is an intention with a network dependency.

`just backtest summer 20` asks the other multi-day question: is the forecast band
the width it claims to be? Twenty seeded weathers, each one episode, their scores
merged — because forecast error is correlated across a day, so one day's coverage
figure is a coin toss reported to three significant figures.

## Run the fleet

Five daemons sit around the box, and each is a binary with a `--config` and a set
of `HEMS_<NAME>_*` environment variables. None of them needs the others to start.

```console
$ just fleet-demo                     # or, spelled out:

$ cat > obsd.toml <<'EOF'
webhook_secrets = ["env:HEMS_OBSD_WEBHOOK_SECRET"]
EOF
$ HEMS_OBSD_WEBHOOK_SECRET=whsec_demo cargo run -p obsd -- --config obsd.toml &
$ HEMS_OBSD_SECRET=whsec_demo \
    cargo run -p hemsd -- simulate --day winter --report-to http://127.0.0.1:8080
  …
  reported to http://127.0.0.1:8080/v1/days — HTTP 202

$ curl -s localhost:8080/v1/fleet | jq '{sites, saving_eur, breached}'
{
  "sites": 1,
  "saving_eur": 2.092545122909012,
  "breached": []
}

$ curl -s localhost:8080/readyz
{"ready":true,"probes":{"collector":{"ready":true,"last_good":"2026-09-01T01:17:43Z"}}}
```

`saving_eur` is the reference winter day's own €2,09, which is the point: the
fleet view is fed by the same number the day prints, through a type both sides
share, so a renamed field is a compile error rather than a dashboard reading zero
for six weeks.

The day travels over **TLS** as a **signed CloudEvent**, and `obsd` refuses an
unsigned one. The two are different guarantees and the box needs both: the
signature says the report is the one this box sent and has not been edited, TLS
says nobody read it on the way. Plain `http` is allowed only to a loopback
address — which is what the demo above is — and refused anywhere else.

That endpoint holds the list of households that did *not* respect a network
operator's reduction, so an unauthenticated write to it can put a compliant site
on that list or take a breach off it. The signature covers the message id, the
timestamp and the exact bytes, so a captured request cannot be replayed,
re-attributed or edited — and an `obsd` with no secret configured refuses
everything rather than accepting everything, because the deployment where
somebody forgot it is the one nobody would notice.

Notice what the secret looks like in that file. Any credential in this workspace
— an ENTSO-E token, a site's enrolment secret, this one — may be written as
`env:NAME` or `file:/run/secrets/x` instead of as itself, and an unresolvable
reference stops the daemon rather than being taken literally. A credential in a
configuration file is a credential in an image, in a backup, and eventually in a
repository.

| Service | Give it | It answers |
|---|---|---|
| `tariffd` | an endpoint per price source, with its token | `/v1/prices?from=…&slots=96` and how much of that window it can price. Open on purpose: a day-ahead curve is a published auction result |
| `forecastd` | a list of locations | `/v1/weather/{location}` and `/v1/production/{location}?kwp=9.8`. Open on purpose: irradiance over a location is public weather |
| `histd` | a database path, one token per site, and the operators' | `/v1/sites/{site}/nachweis` for a network operator, `/v1/sites/{site}/export` for the household — and a box may write only its own site |
| `fleetd` | a site with an enrolment secret, and **signed** configuration and releases — it holds signatures and never a signing key | `/v1/enrol`, `/v1/config`, `/v1/releases/{component}` |
| `obsd` | the secret boxes sign their reports with, and an operator token to read | `/v1/fleet`, and `/v1/sites/{site}` |

Two of the five carry household data and are authenticated per site: a box's
credential reaches **its own** record and no other, an operator's reaches every
household's § 14a evidence and none of their Data Act exports — Article 4 is a
right of the *user*, and a fleet token is not a household. The other two serve
published auction results and public weather and are open on purpose, which is
written down so the difference reads as a decision rather than an oversight.

Every one of them answers `/livez`, `/readyz` and `/version`. `readyz` names each
dependency, whether it is passing, and **when it was last good** — so a `tariffd`
whose upstream is down tells you which upstream and since when, and stays up,
because restarting it would not bring ENTSO-E back.

## Use one crate

The domain crates are independent and none of them does any I/O.

```rust
use hems_grid::para14a::{ControlMode, SteuVe, minimum_power};
use hems_core::prelude::{AssetId, Fallgruppe, Power};

let devices = [
    SteuVe { assets: vec![AssetId::new("wallbox")?], fallgruppe: Fallgruppe::Ladepunkt,     power: Power::from_kw(11.0) },
    SteuVe { assets: vec![AssetId::new("battery")?], fallgruppe: Fallgruppe::Stromspeicher, power: Power::from_kw(5.0)  },
];

// 4,2 kW + (2 − 1) × 0,8 × 4,2 kW = 7,56 kW
assert!((minimum_power(&devices, ControlMode::Ems).kw() - 7.56).abs() < 1e-9);
```

## The regulatory documents

Every regulatory number carries the document and clause it comes from —
`[BK6-22-300 A1 4.5.2]`, `[LPC-031]` — and `cargo xtask check-citations` resolves
all 328 of them against an index of primary sources, failing the build if one
names a document the index does not carry.

`cargo xtask check-wire` does the same for the quantities: every `Decimal`,
instant and date says how it travels, because the impl each inherits accepts a
JSON number that has already been through an `f64` — or writes a date as
`[2024, 1]` — and the Cargo feature that would fix it is global to a build
graph.

The documents themselves are third-party copyrighted publications and are not
redistributed here; the index records the retrieval URL of each, so a working
copy can be rebuilt from public sources.
