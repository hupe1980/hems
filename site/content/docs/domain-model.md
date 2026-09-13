+++
title = "The domain model"
description = "One sign convention, a quarter-hour grid that survives both DST transitions, an electrical tree, and commands that cannot exist without a reason."
weight = 3
+++

`hems-core` is the vocabulary every other crate speaks. It holds **facts and no
rules**: what a site is made of, what a quarter hour is, what a measurement and
a command are. Whether a heat pump is a *steuerbare Verbrauchseinrichtung* under
§ 14a is a rule and lives in [`hems-grid`](@/docs/grid-rules.md); what a
kilowatt-hour costs is [`hems-tariff`](@/docs/tariffs.md); what to do about
either is [`hems-optimizer`](@/docs/optimizer.md) and `hems-realtime`.

The regulatory vocabulary the workspace shares — `Fallgruppe`, the Bundesland
calendar, the market identifiers — comes from
[`metering`](https://github.com/hupe1980/metering), so hems and the metering
layer can never disagree about what a quarter hour or a Fallgruppe is.

## One sign convention

Every power and energy value uses the **load convention**: positive is power
flowing *into* the thing being measured.

| Thing | Positive | Negative |
|---|---|---|
| Grid connection | import (Netzbezug) | export (Einspeisung) |
| Photovoltaic array | standby draw at night | production |
| Battery | charging | discharging |
| Wallbox | charging the car | discharging it (V2H / V2G) |
| Heat pump, hot-water tank, household load | consumption | — |

The cost is that production is negative, which reads oddly the first time. What
it buys is one invariant that holds everywhere and is testable:

```text
grid connection power  ==  Σ (power of every asset behind it)
```

`Site::balance_residual` is that equation. A residual that is not near zero means
a meter is missing or mis-signed, and the arbiter reports it rather than
optimising against a fiction — the failure that otherwise shows up as a house
that keeps buying electricity it cannot account for.

## The quarter-hour grid, and the two days a year that are not 96 slots

Fifteen minutes is the settlement grain of the German market and, since the SDAC
move to 15-minute market time units on **1 October 2025**, also the grain of the
day-ahead price. So it is the grain the planner works in.

`Slot` is identified by its **UTC** start, and that is deliberate. Every offset
Europe/Berlin has ever used is a whole number of hours, so a quarter-hour
boundary in UTC is also one in local time and slot arithmetic stays plain UTC
arithmetic across both transitions. Two questions genuinely need the time zone:

- which **local day** a slot belongs to — a day has **92, 96 or 100** slots;
- the **wall-clock** time a slot starts at, which is what a § 14a Modul 3 window
  or a comfort schedule is written in.

Both go through `metering::calendar`. On the long October day the repeated hour
is inside the same Modul 3 window twice; on the short March day the skipped hour
is inside none. Both are what the price sheet means.

## Assets, and what an asset is not

An `Asset` is a thing behind the connection point with an identity, a rating and
a set of capabilities. The interesting ones are not the obvious ones.

| Asset | The part that is easy to get wrong |
|---|---|
| `Battery` | asymmetric charge and discharge ratings, a **backup reserve** that is a promise, and round-trip efficiency that belongs in the *fill rate* rather than in the power |
| `Evse` | a charge point is off, or at **at least 6 A per conductor** (IEC 61851). There is nothing in between, and a plan that trickles 500 W into it delivers nothing |
| `HeatPump` | how it is controlled — a ceiling, or one of the four SG Ready contact states — is a different fact from what it draws |
| `DhwTank` | a store, not a second thermal model: three hundred litres between 45 and 60 °C are five kilowatt-hours of heat that can be bought hours before they are used. All five facts are the **installation's** — volume, heater, coefficient (1,0 for an immersion heater, 3,0 for a hot-water heat pump), standing loss, and the three temperatures. `t_set_c` is the one that is easy to misread: it bounds nothing the plan does, and it is where the household's own thermostat would sit with no manager at all — so it is what the *baseline* is held at, and therefore what the saving is measured against |
| `PvArray` | its § 9 EEG status is a **declaration** about the installation, not an inference from its nameplate |
| `FlexibleLoad` | `LoadKind::Shiftable` **carries** the `Programme` it will run, quarter hour by quarter hour |

That last one is the clearest case of a fact the type system can make
unrepresentable. A dishwasher's programme is *shaped* — two kilowatts to heat,
two hundred watts to wash, two kilowatts again to dry. An appliance that
announces flexibility and cannot say what shape it takes has told a manager
nothing it can act on, so the shape is not optional.

### Wiring and mode are different questions

`PhaseConnection` is what a device is **wired to**. `PhaseMode` is what it is
**using right now**. Answering the second with the first is how an 11 kW wallbox
drawing symmetrically on three conductors acquires the 4,6 kVA unbalance limit
that only applies to a single-phase device — and loses two thirds of its
charging power to a rule that was never about it.

## The electrical tree

A house is not a single busbar. A wallbox in the garage sits behind a
sub-distribution board with its own cable and its own fuse, and everything behind
that board is bounded by it — independently of, and usually well below, the main
connection.

<pre class="mermaid">
flowchart TD
  R["Grid connection<br/>63 A · both directions"] --> SB1["Sub-board: garage<br/>32 A"]
  R --> SB2["Sub-board: utility room<br/>25 A"]
  R --> H["Household load<br/>uncontrollable"]
  SB1 --> EV["Wallbox 11 kW"]
  SB2 --> HP["Heat pump 8 kW"]
  SB2 --> DHW["Hot-water tank 2 kW"]
  R --> BAT["Battery 5 kW"]
  R --> PV["Roof 9,8 kWp"]
</pre>

`Circuits` form a tree rooted at the grid connection, and the guard narrows every
asset's interval by **every** limit on its path to the root. That is the same
mechanism the § 14a and § 9 EEG limits use — they are simply limits that sit at
the root. Load management that only knows the main fuse either trips the
sub-board or leaves capacity unused, and there is no third option.

## Envelopes: every layer narrows, nobody widens

Every layer of hems hands down an **interval** rather than a number.

<pre class="mermaid">
flowchart LR
  A["Physically possible<br/>−5 … 11 kW"] --> B["Guard: fuses, ratings,<br/>reserve, § 14a, § 9 EEG"]
  B --> C["Plan: what it wants<br/>this quarter hour"]
  C --> D["Arbiter: a point<br/>inside what is left"]
  D --> E["Device: what it will<br/>actually accept"]
</pre>

The idea is OpenEMS's — a scheduler that hands each controller an interval of
possible solutions that later controllers can only shrink — made explicit. An
`Envelope` has a `floor` and a `ceiling`, both of which may be either sign, and
an **empty** interval is a real outcome rather than a bug: two rules that cannot
both be satisfied. `Envelope::resolve` keeps the stricter *ceiling*, because
exceeding a grid limit is worse than falling short of a floor.

This is what makes “the grid limit was respected” an intersection rather than a
code path somebody has to remember to write — see
[Architecture](@/docs/architecture.md#the-guard).

## A command cannot exist without a reason

“Why did my wallbox stop at 17:04?” is the single most common question a home
energy manager has to answer — to the customer, to the installer, and to the
network operator, who may ask an operator to show that a § 14a reduction was
actually carried out `[BK6-22-300 A1 7.2]`.

So `Setpoint::new` is the only constructor and it takes a `Reason`. There is no
way to build a command that cannot say where it came from, and a `Reason` carries
an `Authority`:

| Authority | Set by | Example |
|---|---|---|
| `Guard` | a grid rule or a safety limit | `GuardRule::Lpc` — a § 14a reduction |
| `Realtime` | the arbiter, inside the guard's bounds | tracking the plan's energy through a cloud |
| `Plan` | the planner | this slot's target for the battery |
| `User` | the household | “charge now”, a holiday mode |

`Authority` is what makes “the grid limit wins” a **checked property** rather
than a convention: the ordering is a fact about the type, and the property test
that asserts it over a thousand random households reads it rather than
re-deriving it.

`Command` is deliberately a small closed set — an active power, a consumption
ceiling, a production ceiling, a charging current, a phase count, an operation
mode, on/off. A ceiling is not a target: the asset is free to use less, which is
what every § 14a and § 9 EEG limit actually says.

### Non-finite values stop at the boundary

The unit constructors are infallible and cheap, and `debug_assert!` finiteness.
The gate that matters is where a number becomes an **action**: `Setpoint::new`
refuses a non-finite command, so a NaN out of a broken driver or a degenerate
solve can never reach a device.

## The building is a store, and it is discretised exactly

`hems-core::thermal` carries a two-mass RC model of the house: an air node with a
time constant of about thirteen minutes, and a fabric node with one measured in
days. The fabric usually stores several times what the household battery does,
and it is free.

It is stepped by an **exact zero-order hold** — the coefficients come from a
matrix exponential — rather than by explicit Euler. At a quarter-hour step the
explicit scheme gets the fast eigenvalue's *sign* wrong and the input gain 64 %
too large, and is only conditionally stable. The exact step is a contraction at
any step size, carries no discretisation error under the assumption a
quarter-hour plan already makes, and is still linear in the heat input — which is
what keeps the planner a linear program. [The planner](@/docs/optimizer.md#the-building-is-discretised-exactly-not-by-explicit-euler)
has the numbers.

The same `Rc2Discrete` serves the planner, the rule-based baseline it compares
itself against, and the simulator that answers it. One model, and no way for the
plan and the house to disagree about physics for numerical reasons.

### The heat nobody paid for

The model carries two more numbers: an effective **solar aperture** and a
constant **internal gain** — the sun through the glazing, and the people, the
cooking and everything electric that ends up warm. Both are identified from the
household's own record along with the fabric.

They are first-order, not a refinement. The average archetype loses
`(21 − T_out) / 6` kW, which is 2,7 kW at 5 °C outdoors; a clear March noon puts
about 550 W/m² on a vertical south plane, and 4,5 m² of aperture turns that into
2,5 kW, with the internal gains another 0,45. **On the days a heating plan is
most worth making, the free heat exceeds the demand**, and in January it is about
half of it. A plan without it heats a room the sun is already warming and pays
the comfort slack for the overshoot — the opposite of the way a conservative
omission would be wrong.

It costs the programme nothing: the state equation is already linear in the heat
input, and free heat is a *known constant* per slot rather than a decision, so it
is an offset on the right-hand side.

## Everything takes time as a parameter

No function in `hems-core` reads a clock, opens a socket, spawns a task or
contains `unsafe`. `just purity` fails the build if it — or any of the other nine
domain crates — reaches for one. That is what makes a whole winter day, DST
transition included, a unit test that runs in milliseconds.

```rust
use hems_core::prelude::*;

let limit = Setpoint::new(
    AssetId::new("wallbox-garage")?,
    Command::ConsumptionCeiling(Power::from_kw(4.2)),
    Reason::guard(GuardRule::Lpc),
    time::macros::datetime!(2026-01-15 17:04:00 UTC),
)?;

assert_eq!(limit.authority(), Authority::Guard);
```
