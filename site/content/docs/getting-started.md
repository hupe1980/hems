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

Seven days and five comparisons:

| Day | What it shows | Saved |
|---|---|---|
| `winter` | a network operator reduction from 17:00 to 18:30, a car that must be full by seven, and a dishwasher the plan holds back half an hour | €2,09 |
| `summer` | more production than the house can use, and four quarter hours of negative prices | €8,61 |
| `deadline` | a car that arrives *as the reduction starts* and has three hours to take 13 kWh under the household's own 10,5 kW minimum, shared with a heat pump | €2,51 |
| `shared` | the same evening on a household with **no store**, owed 7,56 kW rather than 10,5, and a reduction that arrives at 17:07 rather than on the re-planning grid | €1,28 |
| `offline` | **the planner switched off** — what the box does on its own | €7,94 |
| `autumn` | a September day, planner off, the surplus in the band only one conductor can use | €2,72 |
| `capped` | a clear May day on a 20 kWp roof, with the § 9 EEG 60 % cap binding at 12,06 of 12,00 kW — the 60 W over is the inverter's own settling time, not a decision | €1,31 |

…plus six comparisons:

| Flag | What it isolates |
|---|---|
| `--perfect-foresight` | the January day with the future known in advance: €5,25 against the €2,09 an honest forecast earns |
| `--wear-eur-per-kwh 0` | what a cost-only optimiser does to a battery — 18,7 kWh of throughput instead of 15,5 |
| `--no-phase-switching` | on the autumn day: 0,2 kWh into the car against 13,1, and a car 4,8 kWh short of where it had to be |
| `--risk` | one future or three, and how much of the objective sits on the worst of them. `just risk` runs the multi-weather sweep that says what each is worth — a single day pays a hedge's premium every time and makes its claim never |

`just backtest summer 20` asks the other multi-day question: is the forecast band
the width it claims to be? Twenty seeded weathers, each one episode, their scores
merged — because forecast error is correlated across a day, so one day's coverage
figure is a coin toss reported to three significant figures.
| `--imsys` | the § 9 EEG cap lifted on both households — one cent to the managed one, twelve to the unmanaged one |
| `--uniform-weights` | every asset given the same allocation weight, which is what a single marginal value per slot amounts to |
| `--heat-pump-on-off` | a single-speed compressor rather than a modulating one — the only unit whose cycling is scheduled, so the only one a minimum runtime constrains. Slow on purpose: ninety-six extra binaries per re-plan |

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
all 279 of them against an index of primary sources, failing the build if one
names a document the index does not carry.

`cargo xtask check-wire` does the same for the quantities: every `Decimal`,
instant and date says how it travels, because the impl each inherits accepts a
JSON number that has already been through an `f64` — or writes a date as
`[2024, 1]` — and the Cargo feature that would fix it is global to a build
graph.

The documents themselves are third-party copyrighted publications and are not
redistributed here; the index records the retrieval URL of each, so a working
copy can be rebuilt from public sources.
