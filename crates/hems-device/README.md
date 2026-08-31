# hems-device

What a wanted power becomes on real hardware — for
[hems](https://github.com/hupe1980/hems).

The planner and the arbiter think in watts, because watts are what the physics
and the regulation are written in. **Almost no device does.**

- 🔌 A charge point takes **amperes per conductor**, and refuses anything below
  the 6 A of IEC 61851 by simply not charging — so hems asks for nothing rather
  than for something the device will ignore.
- 🔀 A phase count goes **before** a power, because changing it while current is
  flowing is what damages contactors.
- 🌡️ An SG Ready heat pump takes **one of three contact states**, keyed on
  fractions of the unit's *own* rating. The states are not ordered by power: the
  § 14a recommendation for state 1 is a *guaranteed minimum*, so a small heat
  pump on a large connection has a state 1 that is its full rating — higher than
  the half-load it draws in state 2.
- ☀️ An inverter takes a **ceiling**, never a target.
- 🚿 A **hot-water tank is on or off** — an immersion heater and a small
  hot-water heat pump both have one power, and the only thing their driver
  accepts is a contact state.
- 🎯 And `realisable` says what a device will *actually* take, which is what the
  arbiter commands. A charge point is off or above 6 A per conductor with nothing
  in between; commanding 3,7 kW on three conductors commands nothing at all, and
  until this existed every layer above counted it as delivered.

  It takes the **guard's envelope**, because for an indivisible device "what may
  it take" and "what can it hold" are one question. Answering them in sequence is
  wrong whichever order you pick: narrowing a resolved value puts it back between
  the device's steps, and resolving after narrowing can round *up* past the
  ceiling the guard just imposed — which under a § 14a budget is an exceedance
  rather than a rounding error.

  `single_speed` is the other half. A device with a *range* can hold the average
  power a slot still needs, so the arbiter's energy tracker asks for it. A device
  with one *point* cannot, and a tank that waits until half its rating is due has
  to run flat out for the rest of the slot — so it is run **early** instead, at
  its rating while the slot still owes energy. That is what a thermostat behind a
  relay does, and without it the § 9 EEG reference day emptied the tank.

Emitting active power to all of them is the obvious first implementation, and it
leaves most of a household undriveable. This crate is the translation, and it is
a pure function of the asset and the decision.

## License

MIT OR Apache-2.0
