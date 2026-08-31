+++
title = "Flexibility"
description = "S2 (EN 50491-12-2) as the internal model: a device describes what it can do, not what it is for."
weight = 6
+++

## Why a second protocol

hems speaks EEBUS because the German grid requires it: the FNN-Steuerbox sends a
§ 14a limit as an EEBUS `LPC` message, and there is no alternative. But EEBUS
organises around named use cases, and a use case is a description of an *intent*.
`EVSE Commissioning and Configuration`, `Monitoring of Power Consumption`,
`Limitation of Power Consumption` — each is a separate document, a separate
implementation, and a separate thing a device may or may not have.

S2 — **EN 50491-12-2** — asks a different question, and it is the better one:

> A device describes **what it can do**, not what it is for.

A battery, a hot water tank and a parked car are all *storage with a fill level*.
Once they say so in the same words, a planner can plan all three without knowing
what any of them is. A device that arrives next year works with no new driver.

That is not only an argument. The hot-water tank in `hems-optimizer` is a fill
level, a fill rate and a leakage — S2's own view of a tank, and deliberately not
a second RC model — which is why adding it was a variable and a constraint rather
than a device class.

So hems plans in S2's terms internally and translates at the edge. `hems-flex`
is that internal model.

## The five control types

| Control type | For | In a house |
|---|---|---|
| **FRBC** Fill Rate Based Control | a fill level and a rate | battery, hot water tank, a car with a departure time |
| **PEBC** Power Envelope Based Control | a bound is all that is needed | charge point, inverter curtailment |
| **OMBC** Operation Mode Based Control | discrete states | SG Ready heat pump, interruptible load |
| **PPBC** Power Profile Based Control | a fixed sequence started in a window | washing machine, dishwasher, tumble dryer |
| **DDBC** Demand Driven Based Control | actuators serving a reported demand | a heat pump following a heat demand |

## The mapping depends on the situation

The control type follows from **what the manager needs to be able to say**, not
from the device class:

```rust
// No car, or no departure time: a bound is all that is useful.
assert_eq!(control_type_for(&wallbox, false), ControlType::Pebc);

// A car that must be full by seven is storage — a level, a rate and a target.
assert_eq!(control_type_for(&wallbox, true), ControlType::Frbc);
```

Describing that second case as an envelope throws away all three. This is the
distinction S2 draws and a use-case-organised protocol cannot.

A battery also declares **three** roles, not one — producer, consumer *and*
storage. A manager that assumes a single role plans a battery as a load.

And a shiftable appliance carries the *shape* it will draw, not a duration and an
average. A dishwasher takes two kilowatts to heat, two hundred watts to wash and
two kilowatts again to dry; a manager given the average would schedule seven
hundred watts of dishwasher into every sunny slot, which no dishwasher will
carry out. `LoadKind::Shiftable` therefore **carries** its `Programme` — an
appliance that announces flexibility and cannot say what of has told a manager
nothing it can act on — and `is_interruptible` is `false`, because a dishwasher
stopped halfway is not one that resumes, it is one somebody has to restart.

## Three details that are expensive to get wrong

**Round-trip losses belong in the fill rate, not the power.** 5 kW into a 95 %
efficient battery stores 4,75 kWh per hour. A manager planning on the electrical
figure believes the battery is full a quarter of an hour early, and stops
charging into the cheapest hour of the night.

**Both battery modes start at idle.** A factor of zero stops whichever mode is
active, so a manager that changes its mind mid-slot never has to switch mode
first — and never overshoots while it does.

**A charge point's envelope floor is its minimum current.** Below 6 A a wallbox
cannot operate at all. An envelope whose lower bound is 0 invites a manager to
allocate 2 kW and wonder why nothing is charging.

**The identifiers are derived, not generated.** This one only shows up on the
second connection. An instruction names an operation mode by ID, so a Resource
Manager that re-mints its IDs on every reconnect invalidates every description
the manager cached — and a manager replaying a ten-minute-old plan addresses
modes that no longer exist. hems derives them (UUIDv5) from the asset's own
identity, so a restart changes nothing and the crate stays a pure function of
its inputs.

## Deferred is not the same as lost

`consequence_type` is the field that carries real money:

| Asset | Consequence | Meaning |
|---|---|---|
| Charge point | `DEFER` | the car charges later; nothing is lost |
| Inverter | `VANISH` | curtailed sunlight does not come back |

One field tells a manager it may throttle a car freely and must think twice
before curtailing PV. § 9 EEG and § 51 make hems ask for curtailment often
enough that saying so precisely matters.

## Instructions are not commands

An S2 instruction names an operation mode by ID and gives a factor in `[0, 1]`.
Turning that into watts requires the description that was sent — which is why
every decoding function takes one:

```rust
let description = describe_battery(&battery, now);
// … send description.system, receive an instruction …
let power = battery_power(&description, &instruction, &battery)?;
```

An operation mode we never described is a mode whose power range we do not know.
It is refused, not guessed.

## One surprise worth knowing

SG Ready's state 1 is often called the "limited" state. For a small heat pump it
does not limit anything. The § 14a recommendation for state 1 is 4,2 kW (40 % of
the grid connection power above 11 kW) — a *guaranteed minimum*, not a
reduction. A 4 kW heat pump on a 30 kW connection is guaranteed 12 kW it cannot
use, so its state 1 is its full rating: **higher** than the half-load a
modulating unit draws in state 2.

hems encodes this rather than assuming the states are ordered, and picks a state
by comparing the wanted power to fractions of the unit's *own* rating.

## The same wallbox, described two ways

This is the argument for S2 in one function. With a car on it that has a
departure time, a charge point is a **store**: a fill level, a rate, a range —
and a Customer Energy Manager that has never heard of a car plans it with exactly
the code it plans a battery with. With nothing plugged in, a bound is all anybody
can usefully say, and it is an **envelope**.

Two details of the store description are facts about hardware rather than about
the encoding. Its power range starts at the **minimum charging current**, not at
zero, because a charge point below the 6 A of IEC 61851 is not charging slowly —
it is idle, and a manager handed a range from zero will ask for 2 kW on three
conductors and believe a car is charging. And the fill-level range is the whole
battery: the household's own target is a `FillLevelTargetProfile`, which is a
message rather than a description, and folding it into the range would tell a
manager the car physically cannot hold more.

## And it has to be reached, not just written

A module can be implemented, cited, tested and reached by no caller at all, and
no property test catches that — a property is a statement about code that runs. A
flexibility model nothing imports is documentation, not a feature.

So `describe_site` builds **every message a Resource Manager would send** for a
whole household, the reference day calls it every run, and the day reports two
numbers:

```console
  described in S2                       6 resources
```

Six: the battery, the charge point with a car on it, the heat pump, the
hot-water tank, the dishwasher and the roof. Where a description cannot be
built the count is followed by how many — `6 resources, 1 it cannot express` —
and that second number is the one that earns its keep. Counting assets whose
control type is merely not `NotControllable` produces a figure that goes up when
a device is added and never notices that no description was ever written for it,
which is how a hot-water tank can sit inside it with nothing to send.

## Standing on the authors' work

The wire types come from [`s2energy`](https://crates.io/crates/s2energy),
generated from the official JSON schema by TNO and Flexiblepower — the people who
wrote S2. Writing our own would be a second opinion about a wire format, which is
the one thing a standard exists to prevent.
