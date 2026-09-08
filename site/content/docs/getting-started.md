+++
title = "Getting started"
description = "Clone it, run the checks, and watch a household get through a January day with a § 14a reduction."
weight = 1
+++

## Requirements

Rust **1.94** (pinned in `rust-toolchain.toml`) and [`just`](https://just.systems)
to build and run. The default solver is pure Rust, so there is no C++ toolchain
and no system library to install, and no test reaches the internet.

`just test` also needs a **container runtime** — Docker, Podman or Colima. The
fleet daemons deploy on PostgreSQL, so their queries are tested against
PostgreSQL: a service checked on a different engine is a service whose SQL is
checked by nothing. The suite starts one container per test binary and gives each
test a database of its own; `just db` starts one to develop against, and
`HEMS_TEST_POSTGRES` points the suite at a server that is already running
instead. There is deliberately no way to *skip* — a test that passed quietly when
it could not reach a database would report green for a query nobody ran.

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

`guards` is the unusual one. It resolves **405 regulatory citations** across five
document families against an index of primary sources and fails the build if one
names a document the index does not carry; and it checks that each of **122**
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
| `winter` | a network operator reduction from 17:00 to 18:30, a car that must be full by seven, and a dishwasher the plan holds back half an hour | €1,94 |
| `summer` | more production than the house can use, and **twelve** quarter hours of negative prices — three whole hours of § 51 EEG | €8,84 |
| `deadline` | a car that arrives *as the reduction starts* and has three hours to take 13 kWh under the household's own 10,5 kW minimum, shared with a heat pump | €2,57 |
| `shared` | the same evening on a household with **no store**, owed 7,56 kW rather than 10,5, and a reduction that arrives at 17:07 rather than on the re-planning grid | €1,37 |
| `offline` | **the planner switched off** — what the box does on its own | €7,99 |
| `autumn` | a September day, planner off, the surplus in the band only one conductor can use | €2,84 |
| `capped` | a clear May day on a 20 kWp roof, with the § 9 EEG 60 % cap binding at 12,01 of 12,00 kW, and the report saying in its own line whether the ceiling was respected | €0,89 |

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
$ cargo run -p hemsd -- simulate --day winter --store ./box.redb
```

`--store` keeps the day's § 14a evidence and quarter-hour registers in the box's
own embedded store — the two years `[A1 7.3]` asks for, written *before*
anything is reported anywhere, with an outbox so what the fleet has not
acknowledged is a backlog rather than a gap.

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

## Manage a real house

The days above are simulated. To point the box at hardware, describe the house
and the devices in a file:

```console
$ cargo run -p hemsd -- run --check --config services/hemsd/hemsd.example.toml
✅ 8 assets, 3 drivers, 6 resources described in S2

$ cargo run -p hemsd -- run --config /etc/hems/hemsd.toml
```

`services/hemsd/hemsd.example.toml` is the annotated starting point, and it is
parsed by a test — an example that has drifted from the struct it documents fails
the build rather than misleading an installer in a cellar. Every field carries
its unit in its name (`battery_kwh`, `fuse_a`, `comfort_min_c`) and every one has
a default; the `[[drivers]]` list does not, and `run` refuses to start without
it. A box with no drivers measures nothing, so the guard would assume every
controllable device was at its nameplate, for ever.

A size of zero is the absence of the device: `battery_kwh = 0` describes a
household with **no** battery, not a battery of no size — the asset is simply
not built, so nothing decides for it, the S2 description never names it, and a
driver configured for it is refused at start-up.

### Three fields worth ten seconds each

**The roof's geometry.** `pv_tilt_deg` and `pv_azimuth_deg` — 180 is due south,
90 east, 270 west. The azimuth decides *when* the roof produces, and it is the
field nobody thinks to change: an east–west array left on due south produces two
shoulders where the plan expects one midday peak, so the box charges the battery
from a sun that is not there. What the box learns from its own meter is a
multiplicative *level* per hour, so it absorbs soiling in a fortnight and a wrong
compass bearing never.

**Which house it is, thermally.** `[site.building]` takes an archetype —
`average`, `new-build`, `solid-wall`, `apartment` — or the parameters directly:
the four RC ones, plus `solar_aperture_m2` and `internal_gain_kw`, the heat the
house gets for nothing. It is a *prior*: the box fits the real building from the
household's own thermometer once it has watched a few excited days, and the fit
wins. The prior matters for the weeks before that, because the fabric capacity
decides whether pre-heating into a cheap hour pays at all and spans a factor of
five across the classes.

One field there is **not** a prior. `facade_azimuth_deg` — 180 by default, a
south front — says which way the glazing looks, and it is the plane the solar
aperture is measured against both when the box fits it and when the plan uses it.
A vertical south plane at 52° north sees 1,6 times the horizontal irradiance at a
December noon and half of it at a June one, so getting the façade wrong does not
make the model noisier; it makes the aperture a different number in every
season.

**What the paperwork says.** `[site.declared.<asset>]` carries the commissioning
date, the § 14a legacy regime and any exemption; `[site.para9]` the § 9 EEG facts
about the roof. The commissioning date feeds *both* statutes, and they read
silence in opposite directions: § 100 Abs. 3b EEG disapplies the § 9 feed-in cap
entirely to a system commissioned between 01.01.2023 and 24.02.2025, while
`[A1 10.1]` leaves a 2019 heat pump on the old reduced network fee until
31.12.2028. Wrong either way costs money — a roof curtailed every sunny midday
for the life of the installation, or a network operator handed a share of a heat
pump's power it may not reduce. Neither is derivable from a nameplate, so the box
asks, and says what each answer produced where somebody can correct it:

```console
INFO § 14a asset=waermepumpe commissioned=Some(2019-04-01) participation=Legacy { until: 2028-12-31 } controlled=false
INFO § 9 EEG: no statutory feed-in cap applies to this roof commissioned=Some(2024-06-01)
```

`--check` builds the site and the drivers and stops before opening a socket. It
is what an installer runs before leaving, and it refuses the mistakes that are
otherwise silent for months. It also checks the **physics** of the building it
was given: that the model the planner integrates contracts at the quarter-hour
step, and that the heat pump can hold the comfort band at the design outdoor
temperature with its heating rod counted in *at a coefficient of one*, because a
resistive element is not a heat pump. The first is a refusal — parameters it
fails for are parameters whose predicted temperatures diverge. The second is a
warning, because
an undersized heat pump is a comfort problem the plan already prices rather than
a reason to refuse to manage a house.

The refusals are the mistakes that are otherwise silent for months — a driver
for an asset the site does not have, two drivers that both command or both
measure one asset, a controllable device no driver can command, a § 14a household with nothing that could
hear a reduction, a market identifier that fails its own check digit, a thermal
model no house could have, a § 14a regime declared for a device this household
does not own, and a **Modul 3 calendar that breaks the Anwendungshilfe**:

```console
📅 Modul 3 `NB-14A-3-2026` for 2026 conforms — HT 17:00–20:00, NT 22:00–06:00, billed in Q1, Q4
   transcribed from https://www.example-netz.de/preisblatt-2026.pdf#modul3
```

That is the one thing in the file copied by hand out of a PDF, so it is the one
most likely to be a typo — and the failures are quiet. A Hochtarif ten minutes
short of two hours is a tariff nobody may sell; a Niedertarif band written as a
single wrapping window leaves the cheap level **unreachable** while every other
rule passes, and the household pays for a module it can never be in.

It also prints the box's **SKI**:

```console
🔑 SKI  1621 7EDA 71A2 12FD 004A 1864 CFE4 F4CE 3689 D5AD
   give this to the metering point operator, so the Steuerbox trusts it
```

That is the number the whole § 14a link hangs on, and field reports make handing
it over the single most common commissioning failure there is. It follows the
box's key, which lives in the box's own store — so it is the same number after a
reboot, and the pairing is done once.

Once it is running, the box says what it is doing:

```console
$ curl -s localhost:8080/v1/status | jq '{silent, undriven, disobedient, steuve_budget_kw, minutes_without_a_plan, plan_expected_eur}'
```

`silent` is the devices it cannot hear from and `undriven` the controllable ones
nothing speaks for — each of those is a device the guard is being conservative
about, and being conservative costs money. `disobedient` is the third and the
least obvious: a device that is answering perfectly well and not acting on what
it was told, which the box knows because it reads the setpoint back off the
device rather than trusting the acknowledgement. `minutes_without_a_plan` is
`null` until the box has published one, and the readiness probe says why:

```console
$ curl -s localhost:8080/readyz | jq '.probes.planner'
{ "ready": false, "detail": "no day-ahead prices", "last_good": null }
```

## Overruling it

A plan the person paying for the electricity cannot overrule is a plan they pull
the fuse on:

```console
$ curl -sX PUT localhost:8080/v1/overrides/wallbox \
    -H 'content-type: application/json' -d '{"what":"boost","minutes":90}'
{"asset":"wallbox","what":"boost","until":"2026-09-02T06:16:30Z"}

$ curl -sX DELETE localhost:8080/v1/overrides      # back to normal
```

`boost`, `pause` and `away`, per asset. This is the **only write** on the local
API, and it is safe for the same reason everything else is read-only: an
override is a *desire*, which the arbiter reads and the guard then narrows. A
household that presses boost in the middle of a § 14a reduction gets as much as
the reduction allows and not a watt more. An endpoint that set a value on a
device would have gone round the guard.

They expire — four hours by default, a day at most. One that did not would be a
household still paying in July for a boost it pressed in March, and anything
longer than a day is a statement about the house that belongs in the file.

## What it takes to plan

Two services, both optional and neither a trust anchor. Point the box at
`tariffd` and `forecastd` in `[fleet]` and every five minutes it asks what
electricity costs and what the sky will do, models this household's roof
locally, corrects the model with what the roof has actually been delivering,
reads the battery's charge off its own meter, and publishes a plan the arbiter
follows.

It asks for the **sky** rather than for a finished production figure, and the
distinction is the architecture: the correction that turns a geometric model
into a forecast of *this* roof is a property of one address — the tree that
shades the east string, the datasheet that was optimistic, the dust — and only
the box's own meter can teach it.

What it learns is kept in the box's own store, so a reboot does not cost a
fortnight of it. And a box that cannot reach either service keeps the house safe
and lawful and loses the plan, which is a cost in euros rather than in
compliance.

**A store enters the plan only where a driver measured it.** The battery off its
own meter; the hot-water tank over EEBUS MDT; the car when an `EV` entity appears
under the `EVSE`, which is the whole of EVCC scenario 1; and the building from an
**indoor temperature** — the one state a thermal model is integrated from —
which arrives over EEBUS MRT or from a vendor's register map.

An unmeasured one is left out of the *names* the plan may command, not guessed
at: an asset a plan names but does not model gets an envelope pinned at zero, and
the arbiter obeys that as an instruction not to use it.

## Run the fleet

Six daemons sit around the box. Each is a binary with a `--config` and a set of
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

That day came from `hemsd simulate`, which is why it has a saving at all: a
saving needs a baseline, and a baseline is what the day would have cost with no
energy manager — a counterfactual only a simulator can re-run. A day from
`hemsd run` carries the energies and the § 14a record and no money, and the
fleet counts those in `unmeasurable_days` rather than averaging them in as days
that saved nothing.

`saving_eur` is the reference winter day's own €1,94, which is the point: the
fleet view is fed by the same number the day prints, through a type both sides
share, so a renamed field is a compile error rather than a dashboard reading zero
for six weeks.

What each daemon owns, which of them are authenticated and which are open on
purpose, and why a day only ever arrives signed is on [the
fleet](@/docs/services.md).

## Ask the advisory plane

```console
$ just agent-demo
```

`agentd` runs specialists that answer a question about a **population** rather
than about one household — whether one cause accounts for most of a week's § 14a
breaches, what the saving on a dashboard actually rests on. They run on a
replayable journal, so a finding can be re-derived rather than argued about, and
they **propose**: nothing an agent says moves a watt, and that is a property of
the types rather than a policy. See [Agents](@/docs/agents.md).

## Where the rules come from

Every regulatory number carries the document and clause it comes from —
`[BK6-22-300 A1 4.5.2]`, `[LPC-031]` — and the citation guard resolves all 378 of
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
- [Agents](@/docs/agents.md) — the read-only surface every fleet service
  answers on, and the plane that may propose and may not act.
- [Security](@/docs/security.md) — capabilities that narrow under delegation,
  tenancy, and why two of the services are open on purpose.
