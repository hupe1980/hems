+++
title = "Devices and drivers"
description = "What a wanted power becomes on real hardware — amperes, contact states, contactors — and the sans-I/O driver contract that keeps a protocol from becoming a second control plane."
weight = 9
+++

The control planes decide in **watts**, because watts are what the physics and
the regulation are written in. Almost no device does. Between the arbiter's
decision and the wire there are therefore two crates, and they answer different
questions:

| Crate | Question |
|---|---|
| `hems-device` | *what will this device accept?* — amperes, contact states, phase counts, and what it will **actually** take if asked |
| `hems-drv` | *how do I say it on this protocol?* — SunSpec over Modbus TCP, EEBUS LPC |

Both are sans-I/O. `hemsd` owns the socket.

## What a device will actually take

Emitting active power to everything is the obvious first implementation, and it
leaves most of a German household undriveable.

| Device | Speaks | The trap |
|---|---|---|
| Charge point | **amperes per conductor** | below the 6 A of IEC 61851 it does not charge slowly — it charges *nothing* |
| Heat pump (SG Ready) | **one of three contact states** | the states are not ordered by power |
| Heat pump (EEBUS) | a **ceiling** | it has its own thermostat under that ceiling |
| Inverter | a **ceiling**, never a target | curtailment is a bound, and the maximum power point is the device's business |
| Hot-water tank | **on or off** | one power, no range |
| Battery | a signed **power** | asymmetric charge and discharge ratings |

### `realisable`: what the device will hold, not what it was told

Some devices are **semi-continuous**: off, or somewhere between a minimum and a
maximum, with nothing in between. Asking a three-phase charge point for 3,7 kW is
asking for 5,3 A, and it answers by charging nothing at all.

Every layer above has to know that, and until `realisable` existed none of them
did: the arbiter commanded the value, the energy tracker counted it as delivered,
the plan fell behind by exactly that much and compensated in the next slot, and
the only place the truth appeared was the meter.

A request below the minimum resolves to **zero**, not up to the minimum. Rounding
*up* is the tempting choice — a semi-continuous device can deliver a fractional
average by running at its minimum for part of a slot — and it is wrong twice
over. The planner's own semi-continuous constraint already guarantees it never
*asks* for a fraction, so the only requests below the minimum are the two where
more power is the wrong answer: the tail of a slot whose energy has already been
delivered, and a guard that has cut the device below what it can start on. On the
reference winter day, rounding those up bought **2,2 kWh** of electricity nobody
wanted, at the evening price.

`realisable` takes the **guard's envelope**, not a bare number. For an
indivisible device, “what may it take” and “what can it hold” are one question,
and answering them in sequence is wrong whichever order you pick: narrowing a
resolved value puts it back between the device's steps, and resolving after
narrowing can round *up* past the ceiling the guard just imposed — which under a
§ 14a budget is an exceedance rather than a rounding error.

### `single_speed`: one operating point is a different shape

A hot-water tank is on at its rating or off. That is not the same shape as a
charge point, which is off or anywhere in a **range**, and the difference decides
how the arbiter tracks a slot's energy.

A device with a range can hold the average power the slot still needs, so asking
for it is right. A device with one operating point cannot: asking for the average
means asking for **nothing** until the average happens to reach half the rating,
by which time the device has to run flat out for the rest of the slot and any
interruption leaves the energy undelivered. So a single-speed device is run
**early**, at its rating while the slot still owes energy — which is what a
thermostat behind a relay does anyway. Without it, the § 9 EEG reference day
emptied the tank.

## SG Ready, and the state that is not a reduction

Two dry contacts, **three** states — not four. Version 1.1 of the BWP interface
specification dropped the old “start command” state, and an implementation that
still sends four is talking to a device that no longer listens.

| State | SG1 | SG2 | Meaning |
|---|---|---|---|
| 1 | 1 | 0 or 1 | Power **limitation** — not necessarily off |
| 2 | 0 | 0 | Normal operation |
| 3 | 0 | 1 | Boost — store surplus as heat |

State 1 is the interesting one, and it is routinely read backwards. The
specification recommends manufacturers implement it as the § 14a minimum —
4,2 kW up to an 11 kW connection, 40 % above that, the same two numbers as
`[BK6-22-300 A1 4.5.1]`. That is a **guaranteed minimum**, not a reduction. A
4 kW heat pump on a 30 kW connection is guaranteed 12 kW it cannot use, so its
state 1 is its full rating — **higher** than the half-load a modulating unit
draws in state 2.

hems encodes that rather than assuming the states are ordered, and picks a state
by comparing the wanted power to fractions of the unit's **own** rating. It is
also the coarsest interface in the workspace, and a bridge to an installed base
rather than a destination: from 1 July 2027 a heat pump funded under the BEG
needs an interoperable digital interface in a Code-of-Conduct format — EEBUS per
VDE-AR-E 2829-6.

## The driver contract

A driver is the only part of the workspace that knows a protocol. It is also the
part most likely to be written by somebody who has never read the rest of it,
against a device that behaves badly, in a hurry. So the contract is the narrowest
one here: **bytes and a clock in, events and bytes out**.

<pre class="mermaid">
sequenceDiagram
  participant S as socket (hemsd)
  participant D as Driver (sans-I/O)
  participant R as Registry
  participant G as Guard
  S->>D: on_bytes(&[u8], now)
  Note over D: or on_timeout(now)<br/>when poll_deadline passes
  D-->>R: poll_event() → Measured / GridLimit / CommandOutcome
  D-->>S: poll_transmit() → bytes to send
  R->>G: SiteState + GridLimits
  G-->>R: setpoints
  R->>D: command(&Command, now)
</pre>

Nothing in the trait blocks and nothing allocates a runtime. A driver that needs
to wait says so with `poll_deadline` and is called back.

Three things follow, and the third is the one that matters:

- **A whole day is a unit test.** The § 14a failsafe is a sixty-second heartbeat
  and a two-hour minimum. A driver that read a clock could only be tested by
  *waiting*; one that takes time as a parameter makes “the Steuerbox goes quiet at
  17:04 and comes back at 19:11” an ordinary assertion.
- **A device that misbehaves is reproducible.** Partial frames, a register that
  stops updating, a peer that answers late — all of them are a byte slice and a
  timestamp.
- **The guard cannot be lied to by accident.** A driver reports what it *read*; it
  does not decide what the site may do. A driver that computed its own limit
  would be a second control plane nobody audited.

### Two kinds of driver, one trait

A **device** driver speaks to something the household owns and reports
`Measured`. A **grid** driver speaks to something the network operator owns and
reports `GridLimit` — and accepts **nothing**, because a household does not
command its own reduction.

They are one trait because `hemsd` runs one loop, and because the difference is
in what a driver *emits*. Which of the two it is comes from
`DriverCapabilities`, declared once and checked at registration.

### Quality is the driver's to set

A driver is the only thing in the workspace that knows whether the number it
holds came off the wire this second or is the last one it saw before the device
went quiet. No layer above can recover that distinction, because both arrive as
the same `f64`.

### Available power is declared, not assumed

A curtailed inverter asked what it is producing answers with **what the manager
already commanded**. Read that alone and a controller never lifts its own
curtailment: it asks for 5 kW, reads 5 kW, and concludes the roof is doing its
best.

So drivers declare `reports_available_power`, and a household is entitled to know
which of the two its box is running on. Where it is false the fallback is the
nameplate — optimistic, and self-correcting on the next tick.

## The registry: four mismatches that are loud at startup

Something has to own a *set* of drivers, give each one its bytes, and fold what
they say into the two things the control planes read — `SiteState`, what the
house is doing, and `GridLimits`, what the operator is asking for. That is
`hemsd`'s registry, and it lives there rather than in `hems-drv` because it is
the layer where a socket becomes legitimate.

Registration is a **check**, not a formality. A declaration nothing validates is a
comment with a type, so `Registry::register` refuses four mismatches that would
otherwise be discovered months later:

| Refused | Otherwise presents as |
|---|---|
| a driver for an asset the site does not have | a device that is simply never commanded |
| two drivers for one asset | two sources of truth about one meter |
| a controllable asset whose driver cannot command | a device the arbiter talks to all day and never moves |
| a § 14a site with no driver that reports grid limits | a household that believes it is participating and would never hear a reduction |

Each of those is silent at runtime and loud at startup, which is the right way
round.

## `modbus` — SunSpec over Modbus TCP

Inverters, meters and batteries, over the one protocol that needs no membership,
no registration and no certificate. Most inverters sold in Germany speak it, and
it is where a box that manages a real house starts.

The **register maps are not ours**. SunSpec is a thousand pages of model
definitions, and the [`sunspec`](https://crates.io/crates/sunspec) crate carries
them as generated types with `Model::parse` — a pure function from a register
block to a typed struct. What is ours is the part a specification cannot give
you: the framing, the walk that finds the models on a *particular* device (where
model 103 lives differs between firmware versions of the same inverter), and the
honesty about what the protocol cannot say.

What a device **is** is decided by which models it publishes rather than by
configuration: a device carrying model 103 is an inverter whatever a TOML file
calls it, and the mismatch is worth finding at discovery rather than in a
measurement that reads plausibly and means something else.

| Models | What it is |
|---|---|
| 101 / 102 / 103, 701 | inverter |
| 201–204 | meter |
| 802 | battery |

Model **701** is the one worth naming: `ThrotPct` is how much throttling is in
effect, so `W / (1 − ThrotPct)` recovers what the array would deliver
unthrottled. A device that publishes it earns `reports_available_power`; one that
does not, says so.

## `eebus` — the § 14a side

The **Controllable System** of *Limitation of Power Consumption*: the role a
household energy manager plays toward the network operator's Steuerbox. The
operator's box is the *Energy Guard*; it writes an active-power limit, sends a
heartbeat every sixty seconds, and if it stops, the household restrains itself to
a pre-agreed failsafe value until a minimum period has run.

**The protocol logic is not ours, and that is the point.** The five-state
limitation machine, the 120-second heartbeat timeout, the 2–24 hour
`FailsafeDurationMinimum`, the rule that an expired duration deactivates a limit
— all of it lives in the [`eebus`](https://crates.io/crates/eebus) crate,
sans-I/O, exercised against the use-case specification. This driver is a
*translation*, and `hems_grid::LpcState` is **derived** from `eebus`'s rather
than tracked alongside it. Two implementations of a certifiable state machine
disagree, and the one that is wrong is whichever the certification lab is not
looking at. The state machine itself is on
[the grid rules page](@/docs/grid-rules.md#the-eebus-limitation-machine).

`eebus` measures in a monotonic duration since the system started; hems works in
wall-clock instants, because a § 14a evidence record is a statement about
calendar time. That conversion is the whole of the conversion, and it is
one-directional: a driver is given wall-clock instants and never asks what time
it is.

A whole LPC day runs in virtual time — a reduction, its own expiry, heartbeat
loss, the failsafe and the release. An operator's limit and a household
restraining itself because nobody is talking to it are reported as **different
events**, because they are different things in the evidence record of
`[A1 7.2]`.

*Not yet:* the SHIP session and the SPINE datagrams that carry a write from a
real Steuerbox.

## One crate, protocols behind features

All of `hems-drv` together is about two thousand lines. `hems-grid` alone is five
thousand and `hems-device` is eight hundred as a *single* crate, so three crates
for this would be ceremony — and the standing rule in this workspace is that
machinery has to be earned. The **trait** is earned, by two implementors of
genuinely different shapes; a crate each is not.

The isolation a crate each would buy is bought by `optional = true` instead: a box
built with `--features modbus` never compiles, audits or ships the EEBUS stack.
What one crate adds is that the feature matrix lives in one manifest.
