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

<pre class="mermaid">
flowchart TB
  M["measurements<br/>every second"] --> G
  L["§ 14a limit from the Steuerbox<br/>§ 9 EEG cap · fuses · reserve"] --> G
  G["<b>Guard</b><br/>an interval per asset"]
  F["forecasts · prices<br/>the site's own state"] --> P["<b>Planner</b><br/>quarter-hour slots,<br/>receding horizon"]
  P -- "target · envelope · a price per asset" --> A["<b>Arbiter</b><br/>desire → guard → smooth → explain"]
  G -- "the interval nothing may widen" --> A
  A --> S["setpoints, each naming its Reason"]
  S --> D["drivers"]
  D --> M
</pre>

Read the arrows rather than the boxes. The planner's output reaches the arbiter
as **advice**; the guard's output reaches it as a **bound**; and the arbiter's own
step can only pick a point inside what is left.

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
from the grid, in both directions. Absorbing surplus and stopping there leaves an
offline box buying the evening peak at the retail price with a full battery
sitting behind the meter. `just demo offline` runs a whole June day that way:
100 % self-sufficiency, 2,7 kWh imported, no planner at all.

It also knows the word *enough*: the charge point carries the household's own
Ladelimit and the fallback stops there, rather than pushing production into a car
that already has what it was asked for in preference to exporting it.

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

Measured, and the season decides it. With a working planner it is worth a cent or
two a day, because a planner can duty-cycle a quarter hour and reach the same
average. In midsummer it is worth nothing even with the planner off: the roof
spends the middle of the day above the 4,14 kW three conductors need, so the car
fills either way. The German shoulder season is the other nine months — on the
September reference day the surplus sits in the 1,4 – 4,1 kW band all afternoon,
and a switchable wallbox puts **13,1 kWh** into the car against **0,2**, for one
contactor operation.

### The planner

A receding-horizon mixed-integer linear program over 24 hours of quarter-hour
slots. It prices battery wear, respects § 14a and § 9 EEG as hard constraints
**per slot** — a ninety-minute reduction is not an all-day one — plans
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

### …including the drivers

A driver is the part of a system most likely to be written in a hurry, against a
device that behaves badly, by somebody who has not read the rest. So the contract
is the narrowest one in the workspace: **bytes and a clock in, events and bytes
out**. `hemsd` owns the socket.

The § 14a failsafe is a sixty-second heartbeat and a two-hour minimum. A driver
that read a clock could only be tested by waiting; one that takes time as a
parameter makes *"the Steuerbox goes quiet at 17:04 and comes back at 19:11"* an
ordinary assertion — and `hems-drv/eebus` runs a whole day of it in
milliseconds: a reduction, its own expiry, heartbeat loss, the failsafe, and the
release.

Something has to own a *set* of them, and that is `hemsd`'s registry: it gives
each driver its bytes, folds what they say into what the house is doing and what
the operator is asking for, and — before a single byte moves — checks that the
drivers and the site agree about what they are for. Five mismatches are loud at
startup rather than silent for months: a box with no drivers at all, a driver for
an asset that does not exist, two drivers for one asset, a controllable device
whose driver cannot command it, and a § 14a household with nothing that could
hear a reduction. `hemsd run --check` is exactly that, without a socket.

**And around the registry, one task per socket.** It connects, reads until the
driver's own deadline, writes whatever the driver produced, and reconnects with a
bounded backoff for ever — an inverter that is off overnight is not a request
that can fail. The one thing a stream of bytes cannot say is that the socket
under it is a *new* one, so the transport tells the driver: a half-frame left
over from before a drop would make the whole new stream decode at an offset, and
every reading after it would be plausible and wrong.

The drivers themselves, the registry that owns a set of them and what a wanted
power becomes on real hardware are on [devices and
drivers](@/docs/devices.md). Two rules keep a driver from becoming a second
control plane:

- **A driver reports; it does not decide.** What the site may do is the guard's
  decision, made with every asset in view. A grid driver accepts no commands at
  all, because a household does not command its own reduction.
- **The protocol logic is not ours.** The five-state EEBUS limitation machine
  lives in the [`eebus`](https://crates.io/crates/eebus) crate and hems *derives*
  its own state from it rather than tracking one alongside. Two implementations
  of a certifiable state machine disagree, and the one that is wrong is whichever
  the certification lab is not looking at.

  That is why the Controllable System driver owns a **SPINE engine** rather than
  a bare state machine: what its bytes are is a SPINE datagram, which is the
  whole payload of a SHIP data frame, so a network operator's Energy Guard
  discovering the box, binding to its load-control feature and writing 4,2 kW is
  a test with no socket in it. What `hemsd` adds underneath is TLS, a WebSocket
  and a handshake — and no protocol logic at all, which is the only arrangement
  in which there is exactly one copy of the § 14a state machine in the product.

## The box's two outbound questions

A plan needs two things a household cannot measure: what electricity will cost,
and what the sky will do. Both are fetched, and neither is a trust anchor — a box
that cannot reach either keeps the house safe and lawful and loses the plan,
which is a cost in euros rather than in compliance.

The second one is asked in a particular way. `forecastd` will serve a finished
production figure for a named geometry, and the box does **not** ask for it: it
asks for the *sky* and models its own roof. The correction that turns a
geometric model into a forecast of **this** roof — the tree that shades the east
string, the datasheet that was optimistic, the dust on the glass — is a property
of one address, and only that box's own meter can teach it. A route called
`/forecast` is one somebody eventually plans against uncorrected, and asking for
the sky instead makes that mistake unavailable.

What the box learns from its own meter is taught on the quarter-hour boundary —
the only moment at which a quarter hour is *over* — and kept in its own store, so
a reboot does not cost a fortnight of it.

### The plan models what the drivers can report, and names no more

The planner fills in the stores whose state something actually measures: today
the battery, off its own meter. The car, the building and the tank wait on
drivers that report an arrival, an indoor temperature and a tank temperature.

Leaving a store out is not the same as *naming* it. A plan that named the charge
point while modelling no car emits a target of zero watts with an envelope
pinned at zero — and the arbiter obeys that, all day, as an instruction not to
charge. So an asset is named if and only if the problem models it.

The battery is the sharp case in the other direction: a plan built on a guessed
state of charge empties a pack it thought was full. No fresh reading means no
battery in the plan.

## One sign convention

Every power and energy value uses the load convention: positive is power flowing
*into* the thing being measured, so production is negative and **the grid
connection power equals the sum of the assets behind it**. A residual that is not
near zero means a meter is missing or mis-signed, and the arbiter reports it
rather than optimising against a fiction.

The table, and the rest of the vocabulary the whole workspace shares, is on
[the domain model](@/docs/domain-model.md#one-sign-convention).

## The fleet, in one paragraph

Five daemons sit around the box and none of them is a trust anchor. `tariffd`
fetches prices, `forecastd` fetches the sky, `histd` keeps everybody's two years
of § 14a evidence, `fleetd` enrols a box and offers it **signed** configuration
and releases, and `obsd` is the fleet view. They share one shell,
`hems-service`, which owns configuration, logging, a health surface and a
shutdown — and owns nothing about energy.

Two of that shell's decisions are load-bearing. **Live and ready are different
questions**: an orchestrator restarts a process that fails `livez` and merely
stops routing to one that fails `readyz`, so a daemon whose price source is down
must not answer the first with the second. And the readiness body names **every
dependency and when it was last good**, so the first click in an incident is also
the last.

The rest — what each daemon owns, which are authenticated and which are open on
purpose, and why a day only arrives signed — is [the fleet
page](@/docs/services.md).

### The storage half of “never worse off without the cloud”

`[A1 7.3]` keeps a control event for two years, so the box holds its **own** copy
and forwards it second: what the fleet has not acknowledged is an outbox that
grows, not a gap. A record that exists only once it has been uploaded is an
intention with a network dependency, and the day a network operator asks about is
the day the link was down.

That is also the answer to *what runs where*. The edge is **one** process,
`hemsd`, because the § 14a failsafe is a sixty-second heartbeat and a two-hour
minimum and an IPC hop inside that path buys nothing — so the box's stores are
embedded and every other daemon is cloud.
