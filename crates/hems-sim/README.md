# hems-sim

Deterministic hardware for [hems](https://github.com/hupe1980/hems) to be tested
against.

A battery with round-trip losses, state-of-charge bounds and the standing loss
that is the reason one left alone is not full a week later; a charge point with a
contactor that takes a tick to change its conductor count and a car whose onboard
charger is measurably less efficient on one of them; an inverter that follows a
curtailment command in a second or two rather than instantly; the same two-mass
building the planner solves against, discretised the same way and fitted with the
thermostat a real one has; a hot-water tank that runs out of hot water; and a
**Steuerbox** that emits EEBUS limitation events on a script.

**Every simulator has at least one way of saying no.** A charge point below 6 A
per conductor charges nothing rather than charging slowly; a battery reports what
it took rather than what it was told; a contactor costs a session while the
vehicle re-negotiates; a heat pump handed back its own controls refuses to keep
heating a house that is already warm — without which a manager that stopped
planning cooked the reference house to 64 °C and reported it as a saving. A simulator that agrees with its controller flatters it and
hides exactly the bugs worth finding.

**And the day is not the day that was forecast.** `weather::Realisation` is a
seeded process — four octaves of value noise on the cloud cover at about four
hours, one hour, a quarter hour and four minutes, a diurnal temperature shape
with a slow error on top, a load multiplier, a hot-water draw that varies — so
the planner has to be wrong before it can be judged. A simulator whose forecast
*is* the series it is about to run cannot tell a good planner from one that was
shown the answer: on the reference winter day, sixty per cent of the saving turns
out to be foresight.

The noise is *correlated* rather than white, deliberately: white noise averages
out inside a quarter hour, so a planner working in quarter hours never sees it.
The fastest octave moves below the planner's own grain, which is where the
arbiter's energy tracking earns its keep.

Every simulator takes `now` as a parameter and steps by a duration, and the
realisation is a pure function of `(seed, instant)` — no generator state, no
iteration order to depend on. So a whole January day with a § 14a reduction, a
control box that stops talking and a car that has to be full by seven runs in
milliseconds and gives the same answer twice, on any machine and under any number
of threads. Without that, a regression in the planner is indistinguishable from
noise and no saving figure can be reproduced.

## License

MIT OR Apache-2.0
