# hems-flex

**The household's flexibility, in the language Europe agreed on.**

[S2](https://s2standard.org) — **EN 50491-12-2** — is the interface between a
Customer Energy Manager and the Resource Managers in a building. Its central
idea is what makes it different from every protocol that came before:

> A device describes **what it can do**, not what it is for.

A battery, a hot water tank and a parked car are all *storage with a fill level*.
Once they say so in the same words, an energy manager can plan all three without
knowing what any of them is — and a device that arrives next year works without a
driver being written for it. EEBUS, by contrast, organises around named use
cases, which is why it needs a new one for each new thing a device might do.

hems plans in S2's terms internally and speaks EEBUS where the German grid
requires it. This crate is the first half.

The wire types come from [`s2energy`](https://crates.io/crates/s2energy),
generated from the official schema by the standard's own authors (TNO /
Flexiblepower). Writing our own would be a second opinion about a wire format,
which is the one thing a standard exists to prevent.

## Which control type an asset belongs to

| Control type | For | hems assets |
|---|---|---|
| **FRBC** Fill Rate Based Control | a fill level and a rate | battery, hot water tank, a car with a departure time |
| **PEBC** Power Envelope Based Control | a bound is all that is needed | charge point, inverter curtailment, a heat pump that takes a ceiling |
| **OMBC** Operation Mode Based Control | discrete states | SG Ready heat pump, interruptible load |
| **PPBC** Power Profile Based Control | a fixed sequence started in a window | washing machine, dishwasher, tumble dryer |
| **DDBC** Demand Driven Based Control | actuators serving a reported demand | — (no driver reports a heat demand yet) |

The choice follows from **what the energy manager needs to be able to say**, not
from the device class. A charge point with no departure time is a power envelope;
the same charge point with a car that must be full by seven is *storage*, with a
level, a rate and a target. Describing it as an envelope throws all three away.
That is why [`control_type_for`] takes the situation, not only the hardware.

## What the descriptions get right

Three details that are easy to get wrong and expensive to get wrong:

- **Round-trip losses live in the fill rate, not the power.** 5 kW into a 95 %
  battery stores 4,75 kWh per hour. A manager planning on the electrical figure
  believes the battery is full a quarter of an hour early.
- **Both battery modes start at idle.** An `operation_mode_factor` of zero stops
  whichever mode is active, so a manager that changes its mind never has to
  switch mode first — and never overshoots.
- **A charge point's envelope floor is its minimum current, not zero.** Below
  6 A a wallbox cannot operate at all; an envelope that says 0 invites a manager
  to allocate 2 kW and wonder why nothing charges.

- **A shiftable appliance is not interruptible**, and says so. A dishwasher
  stopped halfway is not one that resumes; it is one somebody has to restart with
  the dishes still dirty. `is_interruptible: false` is the whole content of the
  control type, and a manager told otherwise will pause one to shed a kilowatt.

A fifth, which only shows up on the second connection: **the identifiers are
derived, not generated.** An instruction names an operation mode by ID, so a
Resource Manager that re-mints its IDs on every reconnect invalidates every
description the manager cached, and a manager replaying a ten-minute-old plan
addresses modes that no longer exist. hems derives them (UUIDv5) from the
asset's own identity, so a restart changes nothing — every description, the SG
Ready heat pump's included, which was the one that generated them until the fifth
audit found it.

And one that carries real money: `consequence_type` is `DEFER` for a wallbox and
`VANISH` for an inverter. Curtailed sunlight does not come back later. That single
field tells a manager it may throttle a car freely and must think twice about PV.

## Reading an instruction back

An instruction is not a command: it names an operation mode by ID and gives a
factor in `[0, 1]`. Turning that into watts needs the description that was sent,
which is why every function here takes one. A mode we cannot name is a mode whose
power range we do not know, so it is refused rather than guessed.

```rust,ignore
let description = describe_battery(&battery, now);
// … send description.system to the CEM, receive an instruction …
let power = battery_power(&description, &instruction, &battery)?;
```

## A whole site in one call

```rust,ignore
let described = describe_site(&site, &DescribeContext::new(now, horizon_end, &modes)
    .with_ev_session(EvStorage { stored, capacity, efficiency }));

for message in described.messages() { connection.send(message).await?; }
```

`SiteDescription` groups what it built by control type and — this is the part
that matters — keeps a separate list of what it **could not** build.

## Written, and *reached*

A flexibility model nothing imports is documentation, not a feature, and no
property test catches a module with no caller. So the reference day calls
`describe_site` every run and reports both numbers
(`described in S2: 6 resources` — battery, charge point with a car on it, heat
pump, hot-water tank, dishwasher, roof), and a test pins them.

Counting *descriptions built* rather than assets whose control type is not
`NotControllable` is the whole point. The second number goes up when a device is
added and never notices that no `describe_*` was ever written for it, which is
how the hot-water tank sat inside it for four versions with nothing to send. A
gap now says so out loud: `6 resources, 1 it cannot express`.

## License

Apache-2.0 OR MIT.
