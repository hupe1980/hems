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
  S->>D: on_link(Up, now)
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

### The one fact bytes cannot carry

A driver holds state that belongs to a *session* rather than to a device: half a
Modbus frame that has arrived and is not yet whole, a request waiting for its
answer, a SPINE peer it has discovered. A reconnect invalidates every one of
them — and nothing in a stream of bytes says so, because the first bytes of the
new socket look exactly like the continuation of the old one. A leftover
half-frame then makes the whole stream decode at an offset, and every reading
after it is plausible and wrong.

So the layer that owns the socket says: `on_link(LinkState, now)`. It is the one
thing only that layer knows.

For EEBUS it is also where **discovery** begins — SPINE learns who is on the
other end by asking — and it is deliberately *not* a reason to fall to the
failsafe. `[LPC-911]` times the failsafe by the heartbeat and by nothing else, so
a WLAN glitch a reconnect repairs inside two minutes costs the household nothing.
A driver that restrained a house for a lost packet would be obeying a rule nobody
wrote.

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

## The registry: five mismatches that are loud at startup

Something has to own a *set* of drivers, give each one its bytes, and fold what
they say into the two things the control planes read — `SiteState`, what the
house is doing, and `GridLimits`, what the operator is asking for. That is
`hemsd`'s registry, and it lives there rather than in `hems-drv` because it is
the layer where a socket becomes legitimate.

Registration is a **check**, not a formality. A declaration nothing validates is a
comment with a type, so registration refuses five mismatches that would otherwise
be discovered months later:

| Refused | Otherwise presents as |
|---|---|
| no drivers at all | a box that keeps the house safe by assuming every controllable device is at its nameplate, for ever |
| a driver for an asset the site does not have | a device that is simply never commanded |
| two drivers for one asset | two sources of truth about one meter |
| a controllable asset whose driver cannot command | a device the arbiter talks to all day and never moves |
| a § 14a site with no driver that reports grid limits | a household that believes it is participating and would never hear a reduction |

Each of those is silent at runtime and loud at startup, which is the right way
round — and `hemsd run --check` is all five without opening a socket, which is
what an installer runs before leaving.

The first two are different mistakes with the same symptom, and they are reported
apart on purpose: one is a household commissioned wrongly, the other is a box
nobody has commissioned yet, and telling an installer the first when they have
done the second sends them looking in the wrong place.

### What is *not* refused, and why

A controllable asset with **no driver at all** is a different fact from the five
above, and it is named rather than rejected. The five are declarations
contradicting themselves; this one is a box part-way through commissioning, or a
household that owns a device hems has no driver for yet. Refusing would make the
site model a list of what is *wired* rather than a list of what is *there*.

It is not nothing either — the arbiter decides a setpoint for each of them every
second and has nowhere to send it — so it is logged once at start-up and carried
on `/v1/status` as `undriven`. A fact that is equally true every second belongs
on a screen, not in a log: a partly commissioned box that warned per asset per
tick would write eighty-six thousand identical lines a day and bury the one real
fault in the middle of them.

### And the loop around it

The registry holds the drivers; something has to hold the **sockets**. That is
one `tokio` task each: connect, read until the driver's own deadline, write
whatever it produced, reconnect with a bounded backoff — for ever, because a
household gateway box is not a request that can fail. An inverter that is off
overnight, a wallbox on a switched socket and a Wi-Fi bridge somebody unplugged
all come back, and a task that gave up on the third attempt would leave the guard
assuming a nameplate for the rest of the year.

Two details in it are written down because they were got wrong first. A deadline
already in the past means *wake now*, not a seventy-year sleep — which is what
converting a negative duration to an unsigned one produces, with no symptom but a
device that is never polled again. And a device that stops answering has to stop
being **believed**: its reading ages out, the box reports it as silent, and the
guard goes back to the conservative assumption. That is safe, it is expensive,
and the household is entitled to know which device is costing them.

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

### The bytes are SPINE datagrams

The driver owns a **SPINE engine** as well as the state machine, so what
`on_bytes` and `poll_transmit` take and give is one SPINE datagram as JSON —
which is the whole payload of a SHIP data frame. That is not a convention chosen
for convenience; it is where the specification puts the boundary, and putting the
driver's boundary in the same place has two consequences worth having.

A network operator's Energy Guard discovering the box, binding to its
`LoadControl` feature, sending its heartbeat and writing 4,2 kW is then an
ordinary integration test with **no socket in it** — both ends are the real
engines, and a message either side refuses to encode simply does not arrive. And
`hemsd` is left with TCP, TLS, a WebSocket and a handshake, and no protocol logic
at all, which is the only arrangement in which there is exactly one copy of the
§ 14a state machine in the product.

### The session under it

`hemsd` opens the socket: TCP, TLS 1.2 with mutual authentication, a WebSocket
upgrade and the SHIP handshake. The household **listens** — the Energy Guard is
the network operator's box and it is the side that dials — and accepts one
session at a time, because a Controllable System has exactly one Energy Guard.

The box's key lives in its own database, and that is the commissioning story
rather than a storage detail. **The SKI follows the key**: it is what an
installer reads off a screen and gives to the metering point operator, and field
reports make that exchange the single most common § 14a commissioning failure
there is. A box that generated a fresh key on every boot would make it fail again
on every boot. The trust store is kept with it, so a household does not re-pair
its Steuerbox after a power cut.

An unapproved peer still completes TLS — it has to, so its SKI can be shown to
somebody — and is held short of the data phase. That is the whole of SHIP's trust
model, and it is what stops anyone on the household's network reducing the house.

## One crate, protocols behind features

All of `hems-drv` together is about two thousand lines. `hems-grid` alone is five
thousand and `hems-device` is eight hundred as a *single* crate, so three crates
for this would be ceremony — and the standing rule in this workspace is that
machinery has to be earned. The **trait** is earned, by two implementors of
genuinely different shapes; a crate each is not.

The isolation a crate each would buy is bought by `optional = true` instead: a box
built with `--features modbus` never compiles, audits or ships the EEBUS stack.
What one crate adds is that the feature matrix lives in one manifest.
