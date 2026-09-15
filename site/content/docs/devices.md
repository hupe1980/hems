+++
title = "Devices and drivers"
description = "What a wanted power becomes on real hardware — amperes, contact states, contactors — and the sans-I/O driver contract behind EEBUS, SunSpec and Modbus."
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

Every layer above has to know that, which is what `realisable` is for. Without
it the arbiter commands the value, the energy tracker counts it as delivered, the
plan falls behind by exactly that much and compensates in the next slot — and the
only place the truth appears is the meter.

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

### And so is whether silence means anything

The registry drops a reading older than ten seconds, and asks the household to be
told which device has gone quiet. That is the right question of a **polled**
driver: a SunSpec inverter reads every second, so a reading older than a few of
those means the device stopped answering while its socket stayed open, and only
the timestamp can say so.

It is the wrong question of a driver whose values are **notified on change**.
EEBUS MDT, MRT and MOT deliver a value when it moves, and a hot-water tank
holding 52 °C and a room holding 21 °C move for hours at a time — which is the
protocol working. Judging them the same way dropped both from the site's state
ten seconds after every reading, so the tank and the building were in the plan
only in the moments just after they changed. Which is the opposite of when a plan
needs them.

Notified-versus-polled is not the distinction, which is worth saying because it
is the one to reach for. *Every* EEBUS scenario here is subscription-driven — the
use-case specifications ask an actor to subscribe, and name polling only as the
fallback for a subscription that was refused. What separates them is whether the
notification comes on a **clock**, and across all fifty-seven descriptors the
things on a clock are the heartbeats and nothing else. So the declaration is held
to the specification: a driver that gains a use case with a heartbeat — and stops
being one whose silence is meaningless — fails the build rather than a
household.

So drivers declare `reports_on_change`, and what such a driver owes instead is a
**link** — which it reports on its own initiative, because a SHIP session that
goes away takes the driver's link with it. What that gives up is the device that
keeps its socket open and stops updating; where the peer stamps its readings, the
driver puts *that* instant on the measurement and the age is meaningful again.

A peer's timestamp is believed within two minutes ahead and an hour behind this
box's own clock, and not otherwise. A household device sets its clock from NTP or
from nothing at all, and the second is common — and `Measurement::at` is what the
guard ages a reading by, so a wrong one is a reading refused for ever or trusted
for ever.

### Available power is declared, not assumed

A curtailed inverter asked what it is producing answers with **what the manager
already commanded**. Read that alone and a controller never lifts its own
curtailment: it asks for 5 kW, reads 5 kW, and concludes the roof is doing its
best.

So drivers declare `reports_available_power`, and a household is entitled to know
which of the two its box is running on. Where it is false the fallback is the
nameplate — optimistic, and self-correcting on the next tick.

## The registry: what it refuses before a byte moves

Something has to own a *set* of drivers, give each one its bytes, and fold what
they say into the two things the control planes read — `SiteState`, what the
house is doing, and `GridLimits`, what the operator is asking for. That is
`hemsd`'s registry, and it lives there rather than in `hems-drv` because it is
the layer where a socket becomes legitimate.

Registration is a **check**, not a formality. A declaration nothing validates is a
comment with a type, so registration refuses six mismatches that would otherwise
be discovered months later:

| Refused | Otherwise presents as |
|---|---|
| no drivers at all | a box that keeps the house safe by assuming every controllable device is at its nameplate, for ever |
| a driver for an asset the site does not have | a device that is simply never commanded |
| two drivers that both command one asset | one contactor obeying two managers, and nothing that could say which |
| two drivers that both measure one asset | two sources of truth about one meter |
| a controllable asset no driver can command | a device the arbiter talks to all day and never moves |
| a § 14a site with no driver that reports grid limits | a household that believes it is participating and would never hear a reduction |

Each of those is silent at runtime and loud at startup, which is the right way
round — and `hemsd run --check` is all six without opening a socket, which is
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

### A write answer is not a confirmation

Curtailment in model 123 is a percentage of `WMax`, and the write response
echoes the address and the quantity and **never the values**. So "accepted"
means the frame was well formed and nothing more, and two perfectly ordinary
inverters answer it identically while disobeying:

- one **clips** `WMaxLimPct` to its own minimum step — the plan asked for 2 kW
  and the roof will deliver 3;
- one stores `WMaxLimPct` and leaves `WMaxLim_Ena` at **zero**, which curtails
  nothing at all. The setpoint is there, the limit is not in force, and every
  layer above reports a compliant house.

So the driver reads the registers back and answers from what it finds there.
`confirmed` is what the device actually holds, `accepted` is whether the limit is
in force, and `detail` names which of the two happened. The difference between
commanded and confirmed is where a plan and a house quietly stop agreeing, and
it is now a number rather than a surprise.

A command that is **never answered** — the device went quiet, the link dropped —
is reported as a failure rather than left in flight. Silence here is the
dangerous case: nothing contradicts the ceiling, so it reaches the § 14a evidence
record as one that was issued and obeyed.

The box keeps the last outcome per device and publishes it on `/v1/status` as
`disobedient`, with its own readiness check beside the driver one. The two are
deliberately separate: a **silent** device is a network fault, and a
**disobedient** one is answering perfectly well and not doing what it is told.
They are different ends of the house.

### The walk is bounded, because the device decides where it goes

Each step of the chain walk reads two registers the *device* chose — a model
identifier and a length — and looks for the next header at `address + 2 +
length`. So the device, not the driver, decides how long the walk is. A firmware
that answers a length of **zero** advances it by two registers a step and never
reaches the end-of-chain sentinel; one whose length overflows the address space
pins it at the top and re-reads it for ever.

Neither is dangerous — every step is a Modbus round trip rather than a tight
loop, and a driver that never finishes discovery reports no models, so the guard
falls back to the device's nameplate, which is the safe direction. It is simply a
box that reads one device for ever and never manages it. So there are three ways
the chain ends and only the first is the tidy one: the sentinel, a header with no
body (a model *is* its body), and running past sixty-four models or off the end
of the address space. The largest real device this workspace has met publishes
eleven.

It is the same argument the **scale factor** makes one layer down: `10^exponent`
with an exponent read straight off a wire is an infinity waiting to happen, so an
exponent outside the specification's own −10…10 is treated as no scaling at all.
Both are refusals to invent a number on behalf of hardware nobody here controls.

### The other half of the market: a map, not a model list

SunSpec works because it is a *standard* — the device publishes a model list and
the driver walks it, so nothing is typed into a file and nothing can be typed
wrongly. Most German heat pumps are not that. A Stiebel, Vaillant, Viessmann or
Bosch unit answers Modbus all day and publishes no model list at all: the
register numbers are in a PDF, and every unit's are different.

`modbus::registers` is the driver for that, and what it is mainly for is **one
measurement**. The planner models the building, learns which house it is from the
household's own record, and can start the compressor — and all of it is gated on
an *indoor temperature*, which most installed heat pumps publish in a register
and nowhere else.

`hvac::mrt` carries a room temperature, so a unit that speaks EEBUS needs no
register map. Registers are the path for everything that does not, which is most
of the installed base — and the **alternative** rather than a companion, because
both measure and one asset gets one meter.

A point declares five things and guesses none of them:

| | Why it cannot be inferred |
|---|---|
| **space** — holding or input | separately addressed, and most vendors put sensors in the input space SunSpec never touches; reading the wrong one returns a plausible number from elsewhere in the map |
| **width and word order** | a 32-bit value spans two registers and vendors disagree which comes first. 1,8 kW read the other way round is 117 964 800 W — a household drawing a hundred megawatts, which no bounds check calls impossible the way a negative temperature is impossible |
| **scale** | a temperature is published in tenths of a kelvin as often as in kelvin, and a *negative* scale is how a vendor reporting generation as positive becomes this workspace's load convention |
| **field** | which of `power`, `temperature_c` or `soc` it becomes — a short list on purpose, so a map can only say things the rest of the box already knows how to use |

### What it writes

A map with no `writes` is read-only, and that is the default. A register map
that can write anything is one where a typo in a configuration file starts a
compressor — the consequence is not a bad reading but a heat pump doing
something nobody asked for — so commanding belongs to a protocol that says what
a value *means*.

One command has no such protocol: a **direction**. EEBUS's twelve HVAC use cases
are temperature and system-function measurement and control, and none of them
reverses a refrigerant circuit, so for a reversible heat pump the choice is a
vendor register or a plan that cannot be carried out. A box that decides to
pre-cool and cannot say so is worse than either: the unit runs whichever way its
own thermostat last chose, the meter agrees with the commanded power, and the day
report claims a saving nobody made.

So a household may declare writes, under three rules:

| Rule | What it prevents |
|---|---|
| A separate list, not a flag on a read point | no typo in a point can turn it into a write |
| A write names its command and **enumerates** the values it may take (`heat = 1, cool = 2`) | the driver never computes a number to write, so there is no scale to get wrong |
| One holding register, sixteen bits | the word-order trap, which is the commonest way a map is wrong while still looking plausible |

Continuous setpoints — a consumption ceiling in watts — are deliberately not
writable here. That is where a scale error is dangerous, and where EEBUS LPC and
SunSpec 704/705 already work.

Two answers are reported rather than assumed. A device that **refuses** a write
answers with an exception on a transaction no outstanding read matches, so the
driver tracks its own writes and turns the refusal into a `Command` event. And a
reconnect **forgets** them: a transaction identifier means nothing across a new
socket, and matching a stale one against a fresh reply reports a write that never
happened.

`run --check` closes the household side: a reversible heat pump whose drivers
cannot set a thermal mode is **refused at start-up**. Everything else about that
installation works — the plan decides cooling, the arbiter commands a power, the
driver writes it, the meter agrees — and the compressor runs whichever way it was
already running. In January that is invisible because both agree; in July it
heats the house.

One point per request, too, rather than one read spanning a block. Coalescing is
the obvious optimisation and it is wrong here: a vendor map is sparse, a device
answers a read crossing an unimplemented register with an exception, and the
reply carries no way to say *which* register was the problem — so one bad number
in the file would silently cost every point that shares its block.

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

### What the box publishes, and the fourth feature

A Controllable System announces four features on one entity, and only three of
them are obvious:

| Feature | Role | What it is for |
|---|---|---|
| `LoadControl` | server | the limit itself — what the Energy Guard writes to |
| `DeviceConfiguration` | server | the failsafe value and its minimum duration |
| `DeviceDiagnosis` | server | this box's own operating state |
| `DeviceDiagnosis` | **client** | subscribing to the *guard's* heartbeat |

The fourth is the interesting one, and its absence fails in the worst possible
way. A SPINE subscription runs **client → server** (§ 5.3.6), so a box that
subscribed to the guard's heartbeat from its own *server* feature is refused by
any peer that checks the role — and a Controllable System with no heartbeat then
correctly refuses every limit, for want of one. Discovery succeeds, the bindings
settle, the subscription appears to be requested, and no limit is ever written,
with nothing on the wire to say why. Under § 14a that is an installation that
looks commissioned and silently is not.

The same shape of failure is why the actor cannot be built except through a
builder that ends at `install`: a device that skipped it answers everything and
publishes no limit *description*, so the guard reads an empty list, finds no
`limitId` to write to, and sends nothing.

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

### The car, and an arrival that has no message

What the plan was always missing about a charge point is not power — the arbiter
has commanded amperes since the first day — but *whether there is a car on the
end of it, and how full it is*. `EvSession` has been in the optimiser from the
beginning and was never built on a running box, because nothing reported an
arrival.

`eebus-ev` reports one, and the way it does is the interesting part. **EVCC
scenario 1 has no payload at all**: an `EV` entity *appearing* underneath the
`EVSE` entity is how a car says it is plugged in, and scenario 8 is the entity
going away again. So the box watches the peer's own entity tree — re-reading
discovery every half minute, because a cable going in sends nothing — and EVSOC
then gives the state of charge and the battery's size.

Both of those or neither. A percentage says nothing about how long charging will
take and a capacity says nothing about how much is needed, so a car that
publishes one of them is a car the planner leaves out rather than one it charges
against an invented battery. A car on IEC 61851 has a pilot wire and cannot
answer at all — it is still plugged in, and the plan still has to know that.

**The departure and the target are the household's.** No EEBUS use case carries
either, and that is right: a car that published a departure time would be
publishing a guess about its driver. They come from configuration, and they are
what turn a state of charge into a deadline the planner can price.

There is one thing this seam cannot do, and it is worth knowing rather than
discovering. SPINE keeps a peer's discovery as a **merged document** — a re-send
is allowed to be partial, so every consumer would otherwise reimplement the merge
— which means a shorter reply cannot remove what an earlier one added. An `EV`
entity that has gone away is still in the tree. Taking it out needs a datagram
that says `delete`, which a charge point has to choose to send. So an arrival is
visible on every peer and a departure only on one that deletes, or when the
session restarts — and the household's own departure time is what ends the
session in the meantime.

### The heat pump: the lever, and the state it is aimed at

Three use cases on one session, because SHIP grants one connection per peer pair
and they are three parts of one decision.

**OHPCF** is the lever, and it is the only one in the whole set that can ask an
appliance to consume *more*. Everything else on the grid side is a ceiling, and a
ceiling an appliance is already under changes nothing — so a plan that has worked
out the house will be cheaper if the compressor runs at eleven, while the roof is
exporting, had no way to say so. The compressor announces that it *could* run,
what it would draw, how long it must run once started and how long it must then
rest; the last two are the planner's minimum-runtime rows, arriving from the
machine rather than from a configuration file.

It needs a **binding**, and that is the part a manager built on monitoring use
cases does not expect. A subscription buys the right to be *told*; only a binding
buys the right to write. Without one the compressor answers every start
`BindingRequired` before its own state machine ever sees it — an offer the box can
locate, report, and never take up.

**MRT** is the state the lever is aimed at: the air temperature of each room the
unit monitors. A device that watches four announces the use case four times, and
the driver reports the mean — the model has a single air node, because `Rc2`
describes a *dwelling*, and picking the first room would have the planner heat the
house to keep one bedroom in band. The mean is over the rooms that have **spoken**,
not the ones that exist: folding a zero in for a sensor that has not reported is a
house the planner thinks is freezing.

**MOT** is the weather at this building rather than a forecast for the grid
square. Planning still needs the forecast — the future cannot be measured — but
the *fit* is better off with the thermometer, and the difference is several
degrees on the days it matters. Which the fit would otherwise attribute to the
fabric, since heat loss is exactly what the two are told apart by.

### The hot-water tank, and the direction hems had never gone

Everything above is a network operator reaching *this* box. A hot-water circuit
is the other way round: a device on the household's own network, which the box
dials. `eebus-dhw` is that driver, and it does two things — **MDT**, the
temperature the tank actually reached, and **CDSF**, asking for a one-time hot
water loading.

One number, and it is the one that kept a whole feature out of every real plan.
The optimiser has modelled a hot-water store since it was written: a linear tank
with a heater, a coefficient of performance, a standing loss and a price on a
cold shower. It was never *given* one on a running box, because
`DhwModel::stored_now` — the heat in the tank right now — is read off a
thermometer and nothing reported one.

So the tank enters the plan **only where a driver measured it**. An unmeasured
one is absent from the problem and from the asset names, not guessed at: a
store's state of charge is not a thing to assume, and the guess is wrong in the
expensive direction — a tank guessed full is one nobody heats overnight, and the
household finds out in the shower.

And for a long time the number went one way only. A tank is a **controllable**
asset, so a household whose only tank driver was this one was refused at
start-up: the box could read a store it had no way to move, and the plan
scheduled heat nothing could carry the decision to. CDSF scenario 2 is that
decision — a one-time loading is the button in the bathroom, pressed over the
wire, and scenario 3 gives it back when a cloud arrives.

It is deliberately **not** a setpoint. `cdt` writes one, and a setpoint is a
number the circuit's own controller may decline to act on — and one written into
an operation mode the circuit is not in is applied, acknowledged, and changes
nothing. The planner decides a *power* per slot from a comfort band it already
holds as a constraint, and a loading is what that maps onto.

Two refusals are worth naming, because both are silent otherwise:

- A reading the circuit marks `outOfRange` or `error` is **not** a temperature.
  `[MDT-005]` says an appliance SHALL ignore it, and the dangerous reading is
  never a wild one — a sensor stuck at 5 °C is perfectly plausible, and it would
  have the plan heat a full tank all night at the day's worst price.
- A reconnect drops the descriptions with the peer. An address that comes back
  may be a different circuit, and MDT Table 7 permits `degC`, `degF` and `K`, so
  resolving a new circuit's values against an old one's meaning is forty degrees
  wrong exactly where it matters.

The box does not write a setpoint, and that is deliberate. Asking for a
temperature is CDT, and CDT has a trap in it: a setpoint written into an
operation mode the circuit is not in is applied, acknowledged, and changes
nothing. A box that only wrote setpoints would report success and heat no water.
Reading is what makes that visible at all, so it comes first — and the plan moves
the tank by curtailing its heater, which it can already do.

### The session under it

`hemsd` opens the socket: TCP, TLS 1.2 with mutual authentication, a WebSocket
upgrade and the SHIP handshake. For § 14a the household **listens** — the Energy
Guard is the network operator's box and it is the side that dials — and accepts
one session at a time, because a Controllable System has exactly one Energy
Guard. For a device on the household's own network it is the box that dials,
reconnecting for ever with a bounded backoff, because a heat pump on a switched
socket is not a request that can fail. The datagram pump above the handshake is
the same one either way, which is what keeps there being one copy of it.

Both directions share **one identity**. The SKI follows the key, so a box that
dialled under a second key would be two devices on its own network — one of which
an installer has never been shown. And the peer's SKI is required rather than
optional when dialling: TLS proves a SKI rather than taking its word, and a box
that dialled whatever answered on the address would take a tank temperature from
anything on the network that offered one.

**And the box announces itself**, because the side that listens is the side that
has to be findable. `_ship._tcp` with the SHIP TXT record set (SHIP § 6): the
SHIP ID, the WebSocket path and the SKI. Without it a Steuerbox has to be given
an address by hand, which is the one thing a network operator's box does not
have, and a certification lab asks to see the record set anyway.

The announcement is **withdrawn for the length of a session**. SHIP asks a node
to stop announcing while it cannot accept another connection, and this box
cannot: one Energy Guard means a second peer that found it and dialled would be
refused, so announcing through a session is advertising a refusal. Only routable
addresses go into the record — a `169.254.x.x` from a DHCP that never answered
tells a peer to dial somewhere it cannot reach, which is worse than saying
nothing. And a box that *cannot* announce still starts: no multicast on the
network, or a container without host networking, does not stop a session opened
to the box's address, and trading a working § 14a installation for a missing
convenience is the wrong way round.

The box's key lives in its own database, and that is the commissioning story
rather than a storage detail. **The SKI follows the key**: it is what an
installer reads off a screen and gives to the metering point operator, and field
reports make that exchange the single most common § 14a commissioning failure
there is. A box that generated a fresh key on every boot would make it fail again
on every boot. The trust store is kept with it, so a household does not re-pair
its Steuerbox after a power cut.

**And a SKI can be approved while the box is running.** That is the other half of
the commissioning story: an unapproved peer is *held pending*, not refused, so an
installer who reads the SKI off the Steuerbox can `POST` it to `/v1/pairing` and
the handshake that was already waiting completes. What it replaces is a static
list in a configuration file, edited and then restarted — on a box whose restart
costs its § 14a session, its plan and its place in the control period. A mistyped
SKI is refused at the point somebody can still fix it, rather than stored as a
peer that will never connect with nothing anywhere to say why.

Revoking one deliberately does not tear the session down. The § 14a session is
how a reduction arrives, and dropping it the instant somebody revokes a SKI would
take the household out of contact with its network operator on a keystroke; it
simply cannot reconnect, which is what revocation means.

An unapproved peer still completes TLS — it has to, so its SKI can be shown to
somebody — and is held short of the data phase. That is the whole of SHIP's trust
model, and it is what stops anyone on the household's network reducing the house.

The handshake also settles a **SHIP version**, and the box logs which one. It is
not a detail: 1.0.1 is the certification minimum and 1.1 is what carries
`accessMethods.id`, the field a peer would be dialled back with — so an installer
reading a log after a reconnect that did not happen is owed the fact rather than
left to infer it.

## One crate, protocols behind features

All of `hems-drv` together is about two thousand lines. `hems-grid` alone is five
thousand and `hems-device` is eight hundred as a *single* crate, so three crates
for this would be ceremony — and the standing rule in this workspace is that
machinery has to be earned. The **trait** is earned, by two implementors of
genuinely different shapes; a crate each is not.

The isolation a crate each would buy is bought by `optional = true` instead: a box
built with `--features modbus` never compiles, audits or ships the EEBUS stack.
What one crate adds is that the feature matrix lives in one manifest.
