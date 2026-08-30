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

Six days and five comparisons:

| Day | What it shows |
|---|---|
| `winter` | a network operator reduction from 17:00 to 18:30, and a car that must be full by seven |
| `summer` | more production than the house can use, and four quarter hours of negative prices |
| `deadline` | a car that arrives *as the reduction starts* and has three hours to take 12 kWh under a 4,2 kW ceiling it shares with a heat pump |
| `shared` | the same evening on a household with **no store**, and a reduction that arrives at 17:07 rather than on the re-planning grid |
| `offline` | **the planner switched off** — what the box does on its own |
| `capped` | a clear May day on a 20 kWp roof, with the § 9 EEG 60 % cap binding at 12,00 of 12,00 kW |

…plus five comparisons:

| Flag | What it isolates |
|---|---|
| `--perfect-foresight` | the January day with the future known in advance: €5,52 against the €2,23 an honest forecast earns |
| `--wear-eur-per-kwh 0` | what a cost-only optimiser does to a battery — 18,5 kWh of throughput instead of 15,4 |
| `--no-phase-switching` | a wallbox wired to three fixed conductors exports 5,5 kWh where a switchable one puts 31,0 kWh into the car instead of 28,5 |
| `--imsys` | what an intelligent metering system with a control device is worth to a roof that has not been given one |
| `--uniform-weights` | every asset given the same allocation weight, which is what a single marginal value per slot amounts to |

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
all 244 of them against an index of primary sources, failing the build if one
names a document the index does not carry.

The documents themselves are third-party copyrighted publications and are not
redistributed here; the index records the retrieval URL of each, so a working
copy can be rebuilt from public sources.
