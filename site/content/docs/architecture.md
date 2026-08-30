+++
title = "Architecture"
description = "Three control planes at three cadences, and a fixed order of authority between them."
weight = 2
+++

## Three planes

A house is decided at three speeds, and confusing them is how energy managers go
wrong.

| Plane | Cadence | Authority | Where |
|---|---|---|---|
| **Guard** | every tick | absolute | `hems-grid` + `hems-realtime::guard` |
| **Arbiter** | about a second | inside the guard's bounds | `hems-realtime::arbiter` |
| **Planner** | 15-minute slots, re-planned every 15 minutes or on event | advisory | `hems-optimizer` |

### The guard

`[BK6-22-300 Anlage 1 Ziff. 4.6 S. 3]` requires that a network operator's
reduction takes precedence over market-driven control whenever it is the
stricter of the two. An optimiser cannot be trusted with that, because an
optimiser's job is to trade things off and this is not tradeable.

So it lives in a plane of its own, applied last, as an **intersection of
intervals**. Every layer may only narrow; nothing below the guard can widen what
it decided. That makes "the grid limit was respected" a property of the
structure rather than a code path somebody has to remember to write — and it is
checked as one, over a thousand randomised households, measurements, plans and
user overrides.

#### Two kinds of limit, and the difference matters

A **per-asset** limit binds one device: its own ratings, its state of charge, the
fuses on the path from it to the connection.

A **shared** limit binds a set of them together: the § 14a ceiling on the
netzwirksamer Leistungsbezug of every controllable device, the main fuse *in both
directions*, each sub-distribution board, the 4,6 kVA of unbalanced load
VDE-AR-N 4100 allows, and the § 9 EEG cap on what may leave the connection point.
These are the ones that are easy to get wrong, because bounding each asset by
the shared limit *individually* looks correct and is not: 11 kW of wallbox, 8 kW
of heat pump and 5 kW of battery each fit under a 24 kW connection, and burn it
together — and, the other way round, a roof allowed 5,88 kW and a battery allowed
5,88 kW put 11,76 kW through a limit of 5,88.

hems shares each of them with the same weighted max-min allocator, and
intersects the results. Because a per-asset minimum of two allocations can only
be smaller than either, respecting several shared limits at once is automatic
rather than a case analysis.

The two are also spent differently. A § 14a ceiling is spent only by the
controllable devices — the surplus from the roof raises it, exactly as
`[A1 2.3]` says, and so does a battery discharging into them, because energy that
came out of a store never crossed the connection point. A fuse is spent by
*everything* behind it, uncontrollable load included. The same allocator,
different budgets.

Lending a discharge needs care, and the care is the design. The objection is
real — the same tick that spends a discharge may be about to reverse it — so the
guard lends only what is **measured**, what this tick is **about to ask for
anyway**, and what the store can **sustain for a whole control period**, and it
pins the lender's ceiling at zero in exchange. The battery may go on discharging
or it may stop; it may not turn into a load while the others spend the headroom
it created.

The two grid limits are also enforced with deliberately different conservatism.
A § 14a reduction is a *control instruction* with a five-minute response
presumption, so the guard may never be over it even for a tick, and a silent
device is assumed to be drawing its nameplate. The § 9 EEG cap is a *settlement*
limit read off quarter-hour registers, so only metered consumption counts as
headroom — a nameplate guess is safe in one direction and would be nonsense in
the other.

And the Schieflast reaches fewer devices than it looks like it should. VDE-AR-N
4100 Abschnitt 5.5.2 covers only equipment that can feed in or store —
generation, storage, charge points — so a heat pump, a hot-water tank and the
household's own load are outside it. Counting everything would put a kettle on L1
against the budget of the one device the manager can actually move.

#### Missing measurements are not zeroes

A controllable device the drivers cannot hear from is assumed to be drawing its
nameplate power. Assuming nothing is the tempting default and it is exactly
wrong: the guard would hand the silent device's share to everybody else while it
kept drawing, and the site would pass a network operator's limit with nothing in
the log to say why. The verdict says which devices had to be assumed, so a
compliance figure computed from an assumption never looks like one computed from
a meter.

The one exception is generation: a silent inverter is assumed to be producing
*nothing*, because assuming otherwise would raise every budget on the site — the
one direction a guard may never guess in.

#### The reserve is a promise, so the guard keeps it

A battery's backup reserve, its floor and its ceiling are enforced here as well
as in the planner. A plan that respects a reserve is not enough: the arbiter
tracks surplus and corrects inside the slot, and five minutes is long enough to
spend an evening's worth of backup on a cheap quarter-hour.

### The arbiter

A plan made at 12:00 for the quarter hour starting at 12:15 cannot know that a
cloud will pass at 12:19. The arbiter runs four steps once a second — desire,
guard, smooth, explain — and because step two is an intersection and step three
only moves a value towards its previous one *inside* that interval, no later step
can undo an earlier bound.

**It follows the plan's energy, not its setpoint.** A slot target says "put
2,4 kWh into the battery during this quarter hour"; the arbiter divides what is
left of that by what is left of the slot and asks for the result, inside the
direction the plan committed to. That survives the cloud at 12:19, three minutes
lost to a network operator's reduction, and a driver that took a while to answer.
A literal setpoint survives none of them.

**And when there is no plan at all, it keeps the house running anyway.** A cold
start, a stale plan, a solver that timed out — the fallback is what every home
battery has always done: cover the house from the roof and the store rather than
from the grid, in both directions. Absorbing surplus and stopping there was the
earlier behaviour, and it meant an offline box bought the evening peak at the
retail price with a full battery sitting behind the meter. `just demo offline`
runs a whole June day that way: 100 % self-sufficiency, 3,0 kWh imported, no
planner at all.

**And "no plan" does not mean "zero" for every device.** An inverter, a heat pump
and a hot-water tank all answer a request for *less*; an energy manager only ever
limits them, and each has controls of its own. So an absent instruction means "no
limit" — the inverter runs at its maximum power point, the heat pump and the tank
run their own thermostats. Reading it as "off", which is right for a battery and
a charge point, lets a January house go cold and hands out cold showers in June,
while reporting a saving for both: energy nobody used is energy nobody bought.

**A plan has to be younger than the arbiter's tolerance for one.** The arbiter
drops a plan older than `max_plan_age`, because a stale plan was computed against
prices and forecasts that have moved on and following it looks deliberate while
being nothing of the kind. That means the planner has to re-solve *faster* than
the tolerance: at thirty-minute re-planning against a twenty-minute tolerance the
house runs on the fallback for ten minutes in every thirty, nothing fails, and
the day costs €1,50 more. `DayResult::minutes_without_a_plan` is what makes that
visible, and a test pins it at zero.

#### One conductor or three

A three-phase wallbox cannot charge below 6 A on every conductor, which at 230 V
is 4,14 kW. On one conductor the same 6 A is 1,38 kW. So a household with 2 kW of
surplus either charges its car or does not, depending entirely on a contactor.

The policy has the three ingredients every working implementation has —
hysteresis on the way up, a confirmation window before acting, and a dwell time
after — plus one that is easy to get wrong: **the hysteresis is relative to the
mode, not to the threshold.** Applied symmetrically it creates a band in which a
charge point already running on three conductors is told to drop to one, where it
can deliver less; a plan asking for precisely the three-phase minimum, which is
an ordinary thing for a semi-continuous variable to land on, switched the wallbox
down to a mode that could only give it 3,7 kW.

Three things this is careful *not* to do. It does not read the ceiling the guard
computed *after* the Schieflast rule, because that bound exists only because the
device is single-phase and sits just below the power a three-phase session needs
to start — a charge point that switched down could never find a reason to switch
back up. It does not read the guard's ceiling for the mode the wallbox is
*leaving*: the tick that switches one up otherwise carries a single-phase ceiling
of 3,68 kW into a three-phase decision, where anything below 4,14 kW is no
current at all, so the wallbox is commanded zero, the surplus jumps, and the
policy switches it straight back — fifty-one contactor operations in a June day.
The guard therefore applies the mode-dependent bounds a second time, for the mode
about to be commanded. And the planner is never offered the single-phase range at
all: left on it becomes a continuous power dial, and a plan wanting exactly 2 kW
of leftover surplus reaches for one conductor and pays the onboard charger's
overhead for the privilege.

The allocation that feeds it is computed twice, on purpose, and the difference is
one number. What the arbiter **commands** uses the minimum of the conductor count
the charge point is actually in, so a share it could not use is shed rather than
handed over. What the conductor **policy** reads uses the lowest minimum the
wiring can reach, so a session shed for failing a three-phase minimum can still
argue its way down to one conductor.

Measured: with a 10 kWh battery and a working planner it is worth a cent or two a
day, because a planner can duty-cycle a quarter hour and reach the same average.
With the planner off it puts **2,5 kWh a day into the car** that would otherwise
have been exported, for **three** contactor operations.

### The planner

A receding-horizon mixed-integer linear program over 48 hours of quarter-hour
slots. It prices battery wear, respects § 14a and § 9 EEG as hard constraints
**per slot** — a ninety-minute reduction is not a forty-eight-hour one — plans
the building and the hot-water tank as the stores they are, values what is left
in all three at the end of the horizon, and hands back a plan with an envelope
per asset: how much freedom it is giving away, rather than leaving the arbiter to
guess.

It is advisory by construction. The guard re-derives every limit from live
measurements a second at a time, so a plan made against a forecast that turned
out wrong is corrected rather than obeyed.

Each slot also carries what a kilowatt-hour is worth **to each device** — the
shadow price of that store's own state equation — which is what the guard's
allocator weights a reduction by. See [the planner](@/docs/optimizer.md) and
[forecasting](@/docs/forecasting.md).

## Why everything is sans-I/O

The guard, the arbiter, the planner, the price stack, the solar model and the
EEBUS state machine all take time as a parameter. None of them opens a socket or
reads a clock.

That is not purism. It is what makes a January day with a network operator
reduction, a control box that stops talking and a car that has to be full by
seven o'clock into a **unit test that runs in milliseconds** — and it means the
same code runs on a gateway box, in a simulation and in a regression suite
without a line of difference.

`just purity` fails the build if a domain crate reaches for a clock, the
filesystem, the network or `unsafe`.

## One sign convention

Every power and energy value uses the load convention: positive is power flowing
*into* the thing being measured.

| Thing | Positive | Negative |
|---|---|---|
| Grid connection | import | export |
| Photovoltaic array | — | production |
| Battery | charging | discharging |
| Wallbox | charging the car | discharging it |
| Hot-water tank | heating | — |

The cost is that production is negative. What it buys is one invariant that holds
everywhere and is testable: **the grid connection power equals the sum of the
assets behind it**. A residual that is not near zero means a meter is missing or
mis-signed, and the arbiter reports it rather than optimising against a fiction.
