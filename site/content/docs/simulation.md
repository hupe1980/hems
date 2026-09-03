+++
title = "Simulation and evaluation"
description = "Seven reference days, hardware simulators that all know how to say no, a seeded weather the planner never sees, and the multi-day sweeps that can falsify what a single day claims."
weight = 10
+++

Nothing in hems talks to household hardware yet, so `hems-sim` stands in for
every device. That makes the simulator the thing every figure on this site rests
on — which is why the design question it answers is not “does it run?” but **can
it fail?**

## The reference household

Every reference day is the same house unless the day says otherwise: a common
German single-family home in 2026.

| | |
|---|---|
| Roof | 9,8 kWp of modules on an 8 kW inverter |
| Battery | 10 kWh, 5 kW both ways, 10 % kept back for a power cut, wear priced at 8 ct/kWh of throughput |
| Charge point | 11 kW, able to drop to one conductor; the household's own Ladelimit at 75 % |
| Heat pump | 5 kW electrical, modulating, comfort band 20–23 °C |
| Hot water | 300 litres on a 0,5 kW hot-water heat pump |
| Dishwasher | a ninety-minute programme: heat, wash, heat to dry |
| Connection | 35 A main fuse |
| § 9 EEG | an intelligent metering system since 2024, **and the 60 % cap still on** |

That last row is the ordinary German household of 2026 and the two halves are
deliberately different answers. The meter has been in for years, so § 51 EEG has
been taking the negative quarter hours since the start of 2025 — and the § 9
Abs. 2 cap is still on, because *that* one lifts only after the network
operator's first successful Ansteuerbarkeit test, which is a different event on a
different clock. `--imsys` is that test happening.

## Seven days

```console
$ just demo-all
```

| Day | What it shows | Saved |
|---|---|---|
| `winter` | a reduction from 17:00 to 18:30, a car that must be full by seven, and a dishwasher the plan holds back 75 minutes | €2,09 |
| `summer` | more production than the house can use, and four quarter hours of negative prices | €8,61 |
| `deadline` | a car that arrives *as the reduction starts* with three hours to take 13 kWh under the household's own 10,5 kW minimum, shared with a heat pump | €2,51 |
| `shared` | the same evening on a household with **no store**, owed 7,56 kW rather than 10,5, and a reduction that arrives at 17:07 rather than on the re-planning grid | €1,28 |
| `offline` | **the planner switched off** — what the box does on its own | €7,94 |
| `autumn` | a September day, planner off, the surplus in the band only one conductor can use | €2,72 |
| `capped` | a clear May day on a 20 kWp roof, the § 9 EEG cap binding at 12,06 of 12,00 kW | €1,31 |

`autumn` is also the only one of the seven where the seam between the arbiter and
the wiring shows: a switching wallbox spends the afternoon being asked for power
that falls between what three conductors and one can hold, and
`hems_device::realisable` answers each of those with zero — correctly, and
silently. The day reports **19 ticks and 0,18 kWh** of it against nought on the
other six, and its own test pins that, which is what keeps the number from being
structurally zero.

Two of those are chosen rather than obvious. `capped` is in **May**, not June,
because the cap is a fraction of *direct-current* power and how close a roof gets
to it is decided by cell temperature. `shared` removes the store rather than
adding a device, because a household with no battery is the one where a § 14a
ceiling actually binds — and that is where the shadow price of relief stops being
zero.

### And the comparisons

Six of these run as part of `just demo-all`; the last two are their own recipes,
because each costs minutes rather than seconds.

| Flag | What it isolates |
|---|---|
| `--perfect-foresight` | the January day with the future known: €5,25 against the €2,09 an honest forecast earns |
| `--wear-eur-per-kwh 0` | a cost-only optimiser: 18,7 kWh of battery throughput instead of 15,5 |
| `--no-phase-switching` | on the autumn day, 0,2 kWh into the car against 13,1 — and a car 4,8 kWh short |
| `--imsys` | the § 9 EEG cap lifted: one cent to the managed household, twelve to the unmanaged one |
| `--uniform-weights` | every asset given the same allocation weight, which is what one marginal value per slot amounts to |
| `--sharing` | inside a § 42c community: 14,5 kWh allocated, and the baseline joins the same community |
| `--heat-pump-on-off` | a single-speed compressor — the only unit a minimum runtime constrains |
| `--risk` | one future or three, and how much of the objective sits on the worst of them |

## Every simulator knows how to say no

A simulator that agrees with its controller flatters it and hides exactly the
bugs worth finding. So each one has at least one refusal:

| Simulator | Its refusal |
|---|---|
| `EvseSim` | below 6 A per conductor it charges **nothing**, and the contactor costs a session while the vehicle re-negotiates |
| `BatterySim` | reports what it **took**, not what it was told, and has a standing loss — one left alone is not full a week later |
| `PvSim` | follows a curtailment command in a second or two rather than instantly |
| `BuildingSim` | a thermostat that refuses to keep heating a house that is already warm |
| `TankSim` | runs out of hot water |
| `ApplianceSim` | **ignores a stop** — a programme interrupted halfway is not one that resumes |
| `CompressorSim` | refuses a stop its minimum runtime has not earned, and counts its starts |
| `SteuerboxSim` | emits EEBUS limitation events on a script, including going quiet |

Two of those found real defects. Without the heat pump's own thermostat, a
manager that stopped planning cooked the reference house to **64 °C** and
reported it as a saving — energy nobody used is energy nobody bought. And the
compressor is the clearest case in the workspace: a minimum runtime can be
implemented, documented, cited and unit-tested and still be enforced on **no
day**, because its rows need the slot before them and a receding horizon executes
only the slot that has none. A constraint no simulator can be watched breaking is
one no simulator can be watched obeying.

## The day that happens is not the day that was forecast

This is the part that decides whether any saving figure means anything.

<pre class="mermaid">
flowchart LR
  SEED["seed + instant"] --> R["Realisation<br/>4 octaves of value noise:<br/>~4 h, 1 h, 15 min, 4 min"]
  R --> SIM["the day the house lives<br/>hems-sim"]
  HIST["six weeks of the box's<br/>own metering"] --> FC["what the planner is told<br/>hems-forecast"]
  FC --> PLAN["plan"]
  PLAN --> SIM
  SIM --> SCORE["CRPS, coverage,<br/>and the money"]
  FC --> SCORE
</pre>

`weather::Realisation` is a seeded process: four octaves of correlated noise on
the cloud cover at about four hours, one hour, a quarter hour and four minutes —
a front, a haze, a cumulus field, one cloud — plus a diurnal temperature shape
with a slow error, a load multiplier and a hot-water draw that varies.

The noise is **correlated rather than white**, deliberately. White noise averages
out inside a quarter hour, so a planner working in quarter hours never sees it
and the arbiter has nothing to catch up on. The fastest octave is the one that
earns its keep, because it moves *below* the planner's own grain.

And it is a pure function of `(seed, instant)` — no generator state, no iteration
order to depend on — so the day still replays to the last euro cent on any
machine under any number of threads. Determinism was never the thing that had to
go; being *told the answer* was.

The simulated roof delivers **92 %** of what its geometry says: soiling, a little
shading, mismatch, an ordinary German roof after three years. Nothing tells the
model, and the residual corrector has to find it — which it does, reporting 90 %.
A figure sitting at exactly 100 % would mean the corrector is not being fed.

## A test whose job is to fail if the simulator gets too good

Three of the workspace's guards are worth naming because of what they guard
against:

- one asserts the reference day's forecasts were **wrong** — a day the planner
  cannot be surprised by measures a planner that was shown the answer, and a test
  that only checks “the day saves money” passes either way;
- one runs the day's own quarter-hour registers through the § 42c allocation;
- one checks that every asset the arbiter commands can be **described in S2**.

A rule module can be implemented, cited, tested and reached by nothing at all,
and no property test catches that: a property is a statement about code that
runs.

## One day cannot answer some questions

Two of them, and both have a command.

### Is the forecast band the width it claims to be?

Forecast error is correlated across a day, so ninety-six quarter hours of one
Tuesday are close to **one draw**: a day's coverage figure is a coin toss
reported to three significant figures.

```console
$ just backtest summer 20
```

Twenty seeded weathers, each one episode, their scores merged. It is the only
thing in the workspace that can say whether the band the planner hedges against
is the width it claims — 80 % coverage on the January day, 75 % on the June one,
against a nominal 80 %. `Calibration` carries an **episode** count beside its
sample count and `is_well_calibrated` asks for twenty *days*, so a single day
that happened to land inside its band cannot report itself calibrated.

### What is a hedge worth?

A single realisation pays a hedge's premium every time and makes its claim never,
so measured once, insurance is always a pure loss.

```console
$ just risk deadline 20

  policy                mean     worst      best    unserved     solve
  one median           2.81€     1.78€     3.55€       0.07€      103s
  three futures        2.96€     1.76€     3.92€       0.01€      517s
  …and the tail        2.89€     1.67€     3.82€       0.01€      481s
  only when at risk     2.92€     1.67€     3.93€       0.01€      467s
```

Scenarios **pay where a service is at risk** and **cost about a euro a day where
nothing is**, and no policy improves the worst day. So the default is one median
— and the sweep that says so ships with the feature it evaluates. Every one of
those figures moved when the sweep grew from four weathers to twenty, which is
the argument for owning the sweep rather than footnoting it.

## What the reference days are not

Two limits, stated here rather than discovered by somebody else.

**The box's history is generated by the same process the day is.** Six weeks of
metering, produced by the same simulator, means the forecasts are scored against
a world whose statistics they were fitted to. That is the friendliest possible
test — and it still leaves 60 % of the winter saving on the table. A real box
faces a distribution that shifts: a season, a new tenant, a roof that gets
cleaned. The field number is worse than this one and never better, which is the
safe direction for a claim.

**A reference day is calibrated.** Each one is tuned to isolate one mechanism, so
adding a device to a tuned scenario makes it measure two things at once and
attribute the sum to one. That is why `offline` and `autumn` leave the dishwasher
unloaded: they are controlled experiments on the *surplus*, and an appliance that
eats a kilowatt-hour of exactly that surplus would make both days unreadable.

## Reproducibility has a price, and it is named

The solver's time budget is measured against the **wall clock**, so the answer
depends on how busy the machine was. Two runs of the same day on the same inputs
can differ — which is exactly what “replay the day and compare” needs not to
happen. That is how it was found: the determinism test passed on its own and
failed under a parallel test run.

So a box in the field keeps the budget, and anything that has to be
**reproducible** sets it to zero and waits. The relative gap is unaffected: it is
a property of the search, not of the clock.
