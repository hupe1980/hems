+++
title = "Getting started"
description = "Clone it, run the checks, and watch a household get through a January day with a § 14a reduction."
weight = 1
+++

## Requirements

Rust **1.94** (pinned in `rust-toolchain.toml`) and [`just`](https://just.systems).

Nothing else. The default solver is pure Rust, so there is no C++ toolchain and
no system library to install, and every test runs on a machine with no network.

```console
$ git clone https://github.com/hupe1980/hems && cd hems
$ just            # lists every recipe
```

## Run the checks

```console
$ just ci
```

The same list, in the same order, that CI runs:

| Step | What it is |
|---|---|
| `fmt-check` | `cargo fmt --all --check` |
| `lint` | Clippy, **warnings as errors**, every target and every feature |
| `purity` | fails if a domain crate reaches for a clock, the filesystem, the network or `unsafe` |
| `test` | the whole suite, all features |
| `guards` | `cargo xtask check-all` — citations, the event catalogue, manifests, wire forms |
| `deny` | licences and advisories |
| `doc` | rustdoc with warnings as errors |

`guards` is the unusual one. It resolves **328 regulatory citations** across five
document families against an index of primary sources and fails the build if one
names a document the index does not carry; and it checks that each of **121**
quantities, instants and dates says how it travels on the wire.

## Watch a day

```console
$ cargo run -p hemsd -- simulate --day winter
```

One simulated January day through the whole control stack — guard, arbiter,
planner, evidence recorder — against simulated hardware and a seeded weather the
planner never sees. It runs in seconds and prints
[the report on the front page](/).

```console
$ just demo-all
```

Seven days, and six comparisons run against them. The days:

| Day | What it shows | Saved |
|---|---|---|
| `winter` | a network operator reduction from 17:00 to 18:30, a car that must be full by seven, and a dishwasher the plan holds back half an hour | €2,09 |
| `summer` | more production than the house can use, and four quarter hours of negative prices | €8,61 |
| `deadline` | a car that arrives *as the reduction starts* and has three hours to take 13 kWh under the household's own 10,5 kW minimum, shared with a heat pump | €2,51 |
| `shared` | the same evening on a household with **no store**, owed 7,56 kW rather than 10,5, and a reduction that arrives at 17:07 rather than on the re-planning grid | €1,28 |
| `offline` | **the planner switched off** — what the box does on its own | €7,94 |
| `autumn` | a September day, planner off, the surplus in the band only one conductor can use | €2,72 |
| `capped` | a clear May day on a 20 kWp roof, with the § 9 EEG 60 % cap binding at 12,06 of 12,00 kW — the 60 W over is the inverter's own settling time, not a decision | €1,31 |

What each comparison isolates, and why a reference day is built the way it is,
are on [simulation and evaluation](@/docs/simulation.md).

Three more commands answer questions a single day cannot:

```console
$ just backtest summer 20      # is the forecast band the width it claims to be?
$ just risk deadline 20        # what does planning against three futures buy?
$ just demo-on-off             # a single-speed compressor, the only unit a
                               # minimum runtime constrains
```

Each is minutes rather than seconds, which is why none of them is in CI.

### Keeping the day

```console
$ cargo run -p hemsd -- simulate --day winter --store ./box.db
```

`--store` keeps the day's § 14a evidence and quarter-hour registers in the box's
own database — the two years `[A1 7.3]` asks for, written *before* anything is
reported anywhere, with an outbox column so what the fleet has not acknowledged
is a backlog rather than a gap.

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

| Crate | What it is | Page |
|---|---|---|
| `hems-core` | the domain model: assets, circuits, the slot grid, setpoints that name a reason | [Domain model](@/docs/domain-model.md) |
| `hems-grid` | § 14a, § 9 EEG, Modul 3, MiSpeL, § 42c, the LPC machine, the evidence record | [Grid rules](@/docs/grid-rules.md) |
| `hems-tariff` | the price stack, five day-ahead parsers, the Modul advisor | [Tariffs](@/docs/tariffs.md) |
| `hems-forecast` | solar geometry, the residual corrector, load and session models, the metrics | [Forecasting](@/docs/forecasting.md) |
| `hems-optimizer` | the receding-horizon MILP and the shadow prices | [The planner](@/docs/optimizer.md) |
| `hems-realtime` | the guard plane, the allocator, the one-second arbiter | [Architecture](@/docs/architecture.md) |
| `hems-device`, `hems-drv` | what a wanted power becomes on hardware, and the driver contract | [Devices and drivers](@/docs/devices.md) |
| `hems-flex` | the household's flexibility in S2 / EN 50491-12-2 | [Flexibility](@/docs/flexibility.md) |
| `hems-sim` | deterministic hardware and a seeded weather | [Simulation](@/docs/simulation.md) |
| `hems-service` | the shell the daemons share | [The fleet](@/docs/services.md) |

## Run the fleet

Five daemons sit around the box. Each is a binary with a `--config` and a set of
`HEMS_<NAME>_*` environment variables, and **none of them needs the others to
start**.

```console
$ just fleet-demo
```

That builds `obsd` and `hemsd`, starts the fleet view on loopback, runs the
winter day, reports it as a **signed** CloudEvent, and then asks the fleet view
what it now knows:

```console
$ curl -s -H "Authorization: Bearer tok-demo" localhost:8080/v1/fleet | jq '{sites, saving_eur, breached}'
{
  "sites": 1,
  "saving_eur": 2.092545122909012,
  "breached": []
}
```

`saving_eur` is the reference winter day's own €2,09, which is the point: the
fleet view is fed by the same number the day prints, through a type both sides
share, so a renamed field is a compile error rather than a dashboard reading zero
for six weeks.

What each daemon owns, which of them are authenticated and which are open on
purpose, and why a day only ever arrives signed is on [the
fleet](@/docs/services.md).

## Where the rules come from

Every regulatory number carries the document and clause it comes from —
`[BK6-22-300 A1 4.5.2]`, `[LPC-031]` — and the citation guard resolves all 328 of
them against an index of primary sources.

The documents themselves are third-party copyrighted publications and are **not
redistributed** here; the index records the retrieval URL of each, so a working
copy can be rebuilt from public sources.

## Where to go next

- [Architecture](@/docs/architecture.md) — three planes, three cadences, one
  order of authority. Read this before anything else.
- [Grid rules](@/docs/grid-rules.md) — what § 14a, § 9 EEG, MiSpeL and § 42c
  actually require, with the citation for every number.
- [The planner](@/docs/optimizer.md) — the formulation, and the fourteen
  decisions in it that are worth explaining.
