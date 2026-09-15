+++
title = "Flexibility"
description = "S2 (EN 50491-12-2) as the internal model: a device describes what it can do, not what it is for."
weight = 8
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

<pre class="mermaid">
flowchart TB
  SITE["a household's assets"] --> DS["describe_site"]
  DS --> FRBC["<b>FRBC</b><br/>battery · hot-water tank<br/>a car with a departure"]
  DS --> PEBC["<b>PEBC</b><br/>charge point with no car<br/>inverter curtailment"]
  DS --> OMBC["<b>OMBC</b><br/>SG Ready heat pump"]
  DS --> PPBC["<b>PPBC</b><br/>dishwasher"]
  DS --> GAP["what it <i>cannot</i> express<br/><i>counted, and printed</i>"]
  FRBC --> MSG["every message a Resource<br/>Manager would send"]
  PEBC --> MSG
  OMBC --> MSG
  PPBC --> MSG
  MSG --> INSTR["an Instruction comes back:<br/>a mode ID and a factor in [0,1]"]
  INSTR --> WATTS["watts — only with the<br/>description that was sent"]
</pre>

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
appliance that announces flexibility and cannot say what shape it takes has told
a manager nothing it can act on — and `is_interruptible` is `false`, because a dishwasher
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
control type is merely not `NotControllable` produces a figure that goes up when a
device is added and never notices that no description was ever written for it — so
a device can sit inside the count for versions with nothing to send.

## Describing is half of it

A device that says what it can do and then ignores the answer has made S2's
argument and declined the consequence. The other half is `hems_flex::session` —
the Resource Manager's whole side of a conversation:

1. the RM speaks first, and its `Handshake` is the one that must list versions
   (S2 makes that mandatory for the RM and optional for the CEM, because the RM
   is the constrained side and the manager is the one that adapts);
2. the CEM answers, picks a version from that list, and gets the
   `ResourceManagerDetails`;
3. it selects a control type; the `SystemDescription` follows;
4. from then on the RM owes statuses — a `PowerMeasurement`, an `ActuatorStatus`
   or `Status` saying which operation mode it is in and how far into it, a
   `StorageStatus` for anything with a fill level — and the CEM may instruct.

**Every message is acknowledged and an instruction is answered twice.** The
`ReceptionStatus` is about the wire; the `InstructionStatusUpdate` is about the
household, and they are different questions — a tank told to heat while its own
thermostat holds it off has received the message perfectly and is not going to
carry it out. `INVALID_CONTENT` is how a Resource Manager says *that actuator is
not mine* to a manager that has confused two devices, and a session that answered
`OK` to everything would let a CEM believe it was driving something.

**It has no socket.** Messages in, messages out, and `now` is a parameter — the
same contract every protocol core in this workspace holds to. A whole
negotiation, a CEM that picks a control type nobody offered, an instruction for
somebody else's actuator: each is an assertion rather than a WebSocket and a
sleep.

**And it does not obey.** A decoded instruction becomes an *event*, and what
happens to it is the arbiter's decision. A Customer Energy Manager is one more
voice with an opinion about a device, and it ranks below the guard exactly as the
planner does — a session that wrote setpoints straight to hardware would be a
second control plane with no § 14a precedence in it.

One session is **one resource**, because that is what the standard says:
`ResourceManagerDetails` identifies a single resource and the CEM selects a
single control type for it, so a household is several sessions rather than one
multiplexed connection. `sessions_for` builds them from the site, pairing each
description with the ratings its statuses are a fraction of.

## And it runs on a socket

The session is sans-I/O; `hemsd` is where one is allowed to exist. Turning on
`[s2] enabled` puts the surface on the listener the box already binds:

```text
ws://<box>/s2/<asset>
```

One connection per resource, and **the path is what names it**. No S2 message
carries a resource identifier — that is the direct consequence of one RM per
connection — so something outside the protocol has to say which resource a socket
is about. A listening port per resource is the same information expressed as a
firewall rule, and it changes whenever a household buys a battery; the asset
identifier is the one already in the household's own configuration.

What crosses the seam is still a desire. An instruction becomes a request the
arbiter ranks [above the box's own plan and below the
household](@/docs/architecture.md), and the guard narrows it afterwards like
everything else — so a manager cannot ask its way past a § 14a ceiling.

**A manager that stops talking stops deciding.** Every request expires after one
market interval, so a crashed process, a cut cable or a lapsed certificate cannot
hold a household at whatever it last said — the § 14a failsafe's own failure with
the ownership reversed.

**And the second answer arrives late, on purpose.** An instruction is answered
twice, and the interesting one is the second: a network operator's reduction
arrives *after* the `ACCEPTED` has gone out, so the box sends an
`InstructionStatusUpdate` of `ABORTED` when the guard takes an instruction back,
once per instruction. It is the answer an aggregator settles on, and the same
fact reaches the household in the sentence that explains a bill: *your manager
asked for this and your network operator would not allow it.*

**A manager presents a credential of its own, and the surface is off until you
turn it on.** This route sits behind the same gate as every other one the box
adds. What a manager presents is not the household's own token but one issued to
it by name —

```console
$ curl -sX PUT -H "Authorization: Bearer $HEMS_TOKEN" \
    localhost:8080/v1/managers/aggregator-nord
{"name":"aggregator-nord","token":"…"}
```

— listed at `GET /v1/managers` and withdrawn at `DELETE`, which stops it working
on the next request. It is the shape a Steuerbox has on the § 14a side, for the
same reason: a household that can see *that* something is driving its battery but
not *what* cannot revoke it. The token is shown once; naming the same manager
again is how it is rotated.

What bounds the worst case is the ranking: everything a connected manager does
goes through the guard, so the § 14a, § 9 EEG, fuse and device-rating properties
hold whoever is on the socket, and a person who presses *pause* still wins. What
is left is money, spent badly, by somebody the household gave a credential to.

Two control types are acknowledged and not yet carried out — an `OMBC` SG Ready
mode and a `PPBC` programme start — because the arbiter decides a *power* per
asset and those are decided elsewhere. They are counted rather than dropped
quietly, which is how a gap says out loud that somebody wants it.

## Standing on a library, and being checked by it

The data model comes from [`s2-kit`](https://crates.io/crates/s2-kit), which
proves its 36 messages and 41 component types against the standard's own JSON
schemas in its own CI. Writing our own would be a second opinion about a wire
format, which is the one thing a standard exists to prevent.

What it adds over generated types alone is a **rule-numbered semantic
validator** — everything JSON Schema cannot say, with the clause behind each rule
quoted. The interop test drives a real Customer Energy Manager session with that
validator on, so every message this box sends is checked against the catalogue
and a violation names the rule it broke.

That is worth more than it sounds. A hand-written test peer only ever proves the
far end did not crash; a peer that tracks session state refuses things the
standard forbids — a control-type message arriving before the acknowledgement
that accepted the selection, say (`S2-STATE-001`), which is the kind of ordering
error nothing else in a test suite can see.

### What a validator cannot check

A rule catalogue checks **messages**. It cannot check a *session* — the order
things happen in, what is owed when, what a resource does with an instruction
after answering it — and a session is where both ends are hand-written.

Three session rules this surface keeps, none of which is visible in any single
message:

- **An instruction is a schedule.** `execution_time` means "when to start; in
  the past means as soon as possible", so one for later waits, in time order.
  The box answers `Accepted` on receipt and `Started` when it begins. The queue
  is bounded at two days of quarter hours, a refusal past that is told to the
  manager rather than dropped, and a closed session clears it.
- **`RevokeObject` withdraws one that has not run.** One already carried out is
  not revocable; the honest answer for that is the `Aborted` the guard's own
  override path sends.
- **`NO_SELECTION` is a manager letting go** — a state rather than a capability,
  and how an aggregator finishes a dispatch window. The hold is released at
  once, because a manager's instruction ranks above the box's own plan. The
  connection stays up: a manager that is not driving may still want the
  measurements, and may select again without reconnecting.

None of the three can be proved from inside. That is R32's whole argument, and
it is why what this surface needs next is not a feature but a stranger on the
far end of the socket.

Every message this crate produces is round-tripped through JSON in its own tests
— **by value**, not by message type — which checks the whole of it against the
standard's schema for the price of one assertion. That needs `serde_json`'s
`float_roundtrip` feature, which is not one of its defaults: without it, reading
a float back is a fast approximation and a battery's fill rate does not survive
the trip.
