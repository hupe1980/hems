+++
title = "The planner"
description = "A receding-horizon mixed-integer linear program that prices battery wear instead of hiding it."
weight = 4
+++

## The formulation

One set of variables per quarter-hour slot of a 48-hour horizon, all in watts and
all non-negative: grid import and export, battery charging and discharging, the
energy in the battery, charging power at the charge point, the energy in the car,
the hot-water heater and the heat in its tank, and production thrown away.

```text
g_in − g_out = load + b_ch − b_dis + ev + hp + dhw − (pv − curtail)
                                                               (energy balance)
e[k]    = e[k−1] + Δ(η_ch·b_ch − b_dis/η_dis)                  (battery state)
ev_e[k] = ev_e[k−1] + Δ·η·ev                                   (car state)
w[k]    = w[k−1] + Δ·cop·dhw − loss − draw[k] + short[k]       (tank state)
ev_on·min_charge ≤ ev ≤ ev_on·max_charge, ev_on ∈ {0,1}        (6 A or nothing)
b_ch + ev + hp − b_dis + curtail + dhw ≤ ceiling[k] + (pv − load)
                                                    while pv > load
b_ch + ev + hp − b_dis                ≤ ceiling[k]  otherwise   (§ 14a)
g_out ≤ feed-in ceiling[k]                                     (§ 9 EEG, LPP)
g_out ≤ pv − curtail + b_dis                                   (you can only
g_in  ≤ load + b_ch + ev + hp + dhw                             export what you
                                                                actually have)
```

minimising, in euros throughout,

```text
Σ Δ·( (price_in + carbon_price·intensity + autarky_premium)·g_in
      − price_out·g_out
      + wear·(b_ch + b_dis)
      + curtailment_penalty·curtail
      + discomfort_price·(kelvin outside the comfort band) )
  + shortfall_price·(hot water not delivered)
  + unmet_charge_price·(charge not delivered by the deadline)
  − terminal value of what is left in the battery, the building and the tank
```

## Nine decisions worth explaining

### Battery wear is a cost, not a constraint

Most planners leave it out, and a cost-only receding-horizon controller will
cycle a battery for any spread at all. Measured against a physically grounded
degradation model, the damage can exceed the energy saving by up to an order of
magnitude ([arXiv 2606.16051](https://arxiv.org/abs/2606.16051)).

hems charges throughput in euros per kilowatt-hour, half on each leg so a full
cycle pays it once. A reasonable value is the pack price divided by the warranted
throughput — about 8 ct/kWh for a €4 000 pack warranted for 2,4 MWh per kWh of
capacity. `just demo-all` shows the same day with the term set to zero.

### Stored energy has value after the horizon ends

Without a terminal value the plan empties the battery into its own last slots,
because energy that survives the horizon is worth nothing to a model that stops
there. That is an artefact of where the horizon happens to stop, and in a
receding-horizon controller it repeats every five minutes. hems values what is
left at the mean import price over the horizon, discounted by what it costs to
get back out.

### You cannot import and export at the same instant

Stating it as two physical bounds — exported power comes from the roof or the
battery, imported power goes into the house or a store — keeps the model a pure
linear program, which matters on a gateway box. Without them the model is
*unbounded* wherever export earns more than import costs, and it will invent an
infinite round trip through a meter.

### The § 14a constraint is on the netzwirksamer Leistungsbezug, per slot

Not on consumption. The surplus a roof is producing raises the ceiling exactly as
`[A1 2.3]` says it does, so a household with photovoltaics keeps charging through
a reduction — lawfully. The same arithmetic runs in the guard once a second
against measurements; here it runs against the forecast.

Three details, and each is easy to get backwards.

Curtailed production is on the *left*-hand side, not subtracted from the
surplus: throwing energy away cannot buy headroom. A hot-water tank is on the
left too, because it spends the surplus before the controllable devices see it —
and it is **not** itself a steuerbare Verbrauchseinrichtung, since `[A1 2.4.1]`
lists four Fallgruppen and a water heater is in none of them. A Heizstab that is
the heat pump's Zusatzheizung is another matter, and it is already counted there.

The battery's **discharge** is on the left with a minus, because energy that came
out of a store never crossed the connection point. Charging and discharging
appear with opposite signs, so no round trip through the battery can manufacture
headroom — and the plan stops refusing to charge a car under a teatime reduction
while a full battery sits behind the meter.

And the ceiling is read **per slot**. A reduction has a duration — `[LPC-909]`
sends one with the limit, and the failsafe releases after its own minimum — and
stretching today's ninety minutes across a forty-eight-hour horizon plans the
house under a limit that lapsed before teatime. It costs money in both
directions: the plan charges the car at three in the morning as though the
network operator were still asking for something, and it never sees the next one
coming when one is announced ahead. The same shape carries an *anticipated*
window, which is what the operator's monthly list of control actions per postcode
`[A1 8.4]` is.

### A charge point is off, or above 6 A

The cheapest way to put a modest amount of energy into a car, if nothing says
otherwise, is to trickle a few hundred watts across many hours. Below the 6 A of
IEC 61851 a wallbox is not charging slowly — it is idle — so that plan delivers
nothing at all, and nobody finds out until the morning.

So the charge point is **semi-continuous**: one binary per slot, off or between
its minimum and its maximum. It is the only binary a car needs, and it is pinned
rather than branched in every slot after the car has left, which on a 192-slot
horizon is most of them.

### The charging deadline is soft, and priced

A hard deadline returns **no plan at all** when it cannot be met, and a failed
solve leaves the arbiter on the fallback — which may charge the car less than the
best achievable schedule would have. "I could not do all of it" is a better
answer than "I could not do any of it".

So the target carries a slack priced at €5 per kilowatt-hour: far above any
electricity price, so the deadline is lexicographic in practice and the plan
gives up a kilowatt-hour of charge only when there is genuinely no way to deliver
it — and *finite*, so the answer is always a schedule. `Solved::unmet_charge` is
how much it had to give up, which is the thing a household needs to know before
the morning rather than after it.

The comfort band and the charging deadline are both soft, which is most of the
reason a "fallback ladder" of progressively simpler solvers is not needed.

### The conductor count is not the planner's decision

A charge point that can drop to one conductor has two operating ranges — 4,14 to
11 kW on three, 1,38 to 3,68 kW on one — and they are mutually exclusive. Modelling
both takes a second binary per slot plus a variable per mode to carry the
different efficiencies, and it lets the plan schedule a session under a ceiling
too tight for three conductors.

It was built, measured, and **removed**. On the reference winter day it was worth
one cent and cost four times the solve time — ten seconds to forty for a
simulated day — because the extra integrality lands exactly in the slots a § 14a
event makes hard. The conductor count belongs to the arbiter instead: it sees the
measured surplus rather than a forecast, decides in a pure function that costs
nothing, and can change its mind inside a slot.

What the planner gives up is the ability to schedule a charging session under a
limit too tight for three conductors. That is a real case, and the answer to it
is the soft charging deadline above rather than making every solve four times
slower.

Measured on a June day with the planner off: a switchable charge point puts
**2,5 kWh** into the car that a fixed three-phase one exports, for **three**
contactor operations.

### Hot water is a store, and a cold shower has a price

Three hundred litres between 45 and 60 °C hold about five kilowatt-hours of heat,
and a hot-water heat pump puts it there at a coefficient of performance near
three — so the whole store is under two kilowatt-hours of electricity that can be
bought hours before it is used. It is the cheapest flexibility in most German
houses and the one nobody plans with.

It is modelled as a **linear store**: heat above the lowest acceptable
temperature, with a leak and a draw. Not a second RC model, because what a
household cares about is whether there is hot water rather than the temperature
profile inside the cylinder — and because that is S2's own view of a tank (a fill
level, a fill rate and a leakage, `FRBC`), so it maps onto `hems-flex` without
translation.

The draw is a **soft** constraint, priced at €3 per kilowatt-hour of heat, for
exactly the reason the charging deadline is: a household that starts the day with
a cold tank cannot have a hot shower at seven whatever the plan says, and "this
schedule, and it is two kilowatt-hours short" is a better answer than "no
schedule exists".

### The building is discretised exactly, not by explicit Euler

The building's own inertia is usually several times the household battery, and
free — which makes the two-mass RC model the largest store in the plan. It is
also the one most easily got wrong, because the air node's time constant is about
thirteen minutes and the planner's step is fifteen.

Stepping it with explicit Euler at that step size does two things at once. The
fast eigenvalue comes out **negative** — `−0,16` where the exact value is `+0,31`
— so the planned indoor temperature *rings* after every change of heat input,
which is exactly the slots in which an on/off heat pump's minimum runtime is
decided and the comfort slack is priced. And the input gain is **64 % too large**:
one kilowatt held for a slot raises the air by 0,254 K, not by `Δt/C_air` =
0,417 K, because the air is shedding heat into the fabric while it warms. A
planner that believes heating works two thirds better than it does under-heats.
It is also only *conditionally* stable, and nothing checks the condition: drop
the air capacity from 0,6 to 0,3 kWh/K — a flat rather than a house — and the
planned temperature diverges outright.

So `hems-core::thermal` discretises the model **exactly**, by a zero-order hold:
the heat input and the outdoor temperature are constant across a slot, which is
precisely what a quarter-hour plan asserts about them, and under that assumption
the step carries no discretisation error at all. The coefficients come from a
matrix exponential, so they are a contraction at any step size — the scheme
cannot ring and cannot diverge. It is still linear in the heat input, which is
what keeps the planner a linear program.

The same discretisation serves the planner, the rule-based baseline it compares
itself against, and the simulator that answers it. One model, and no way for the
plan and the house to disagree about physics for numerical reasons.

### Every preference is a price, so the objective has one unit

An enum of goals — cost, carbon, self-sufficiency — is a unit error wearing a
design: switching to "carbon" replaces the energy price with grams of CO₂ and
leaves battery wear, curtailment and comfort in euros, so the plan minimises a
sum of two currencies. It behaves, because 400 g/kWh and €0,30/kWh are numbers
of a similar size. That is not a reason.

So a carbon price (€/kg) and an autarky premium (€/kWh imported) *add* to the
energy price rather than replacing it, and every term is comparable with every
other. Setting one to zero switches that concern off; setting it high makes it
lexicographic in practice, without the machinery of a lexicographic solve.

## Solvers

`good_lp` behind a feature flag. The default is **microlp**, pure Rust, so
`cargo test` needs no C++ toolchain and the crate cross-compiles to a gateway box
without a build environment on it. **HiGHS** is markedly faster on a full
192-slot horizon and is what a production box should be built with; CI runs the
test suite against both.

The model has binaries, so it also carries a **relative gap** (0,5 % by default)
and a **time budget** (10 s), honoured by HiGHS. A household plan is re-made
every five minutes against forecasts that are wrong by far more than half a per
cent; spending a minute to prove the last fraction of it is spending a minute on
nothing. When the budget runs out the best plan found so far is used, because a
late plan is worse than a slightly suboptimal one — the arbiter falls back to
self-consumption while it waits.

The budget costs something that is worth naming: it is measured against the
**wall clock**, so the answer depends on how busy the machine was. Two runs of
the same day on the same inputs can differ, which is exactly what "replay the day
and compare" needs not to happen. That is how it was found — the determinism test
passed on its own and failed under a parallel test run. So a box in the field
keeps the budget, and anything that has to be *reproducible* sets it to zero and
waits. The relative gap is unaffected: it is a property of the search, not of the
clock.

## What the plan carries

Per slot: a target and an **envelope** for each asset — how much freedom the
planner is giving away, rather than leaving the arbiter to guess — and the
marginal value of a kilowatt-hour **to that asset**, which is what weights a
§ 14a allocation when a limit arrives and there is not enough to go round.

## What a kilowatt-hour is worth, per asset

"A reduction takes power from where it is worth least" needs a value **per
device**. One marginal value per slot is the same number for everything in it,
and a weighted allocator handed it weights nothing.

A mixed-integer program has no duals: its value function is not convex, and
whatever a branch-and-bound solver reports at its final node is a dual of some
relaxation. So the model is solved twice. The second solve pins the discrete
decisions at what the first chose — the charge point's on/off, the
non-modulating heat pump's — and the linear program that remains has duals that
are the marginal values *conditional on the decisions the plan made*. That is the
right conditioning: the arbiter is not going to re-open them either.

| Row | Its dual is |
|---|---|
| the energy balance | what a kilowatt-hour delivered anywhere costs the plan |
| the battery's state equation | what a kilowatt-hour held in the store is worth |
| the car's | near the tariff with a night ahead; near €5/kWh when the departure is close and the plan is short |
| the tank's, the building's air node | the same, in electricity, so the coefficient of performance is already in them |
| **the § 14a ceiling** | what a kilowatt-hour of **relief** from the network operator would be worth |

The last one answers a different question from the rest — not what a household
*holds* but what a *limit costs it* — and it is the number this market does not
have. €3,93/kWh on a household with no store whose car will otherwise leave
short; €0,16 on the same household with a battery, because the store lends the
controllable devices the headroom `[A1 2.3]` allows. Aggregators price both at
"30 % of nominal", because nobody publishes the alternative.

The dual pass always runs on [Clarabel](https://clarabel.org), whichever backend
solved the mixed-integer problem. Clarabel is pure Rust, so this costs no C++
toolchain — and, more to the point, it leaves **no backend split**: a plan does
not mean different things on two builds of the same software.

### Two things a model has to get right for any of this to mean anything

**No redundant row may be left in it.** "Power cannot be imported and exported at
once" reads like two statements, and the second — imported power has to go into
the house or a store — is *implied* by the first and the energy balance. It
constrains nothing, which is why such a row survives; and it is tight whenever
the household is not exporting, which is most of the time. An always-tight
redundant row has a free dual that absorbs the energy balance's along with it, so
the price of a kilowatt-hour is not inaccurate but **indeterminate**: an ordinary
20 ct hour comes back at 5 986 €/kWh.

**Conditioning that a simplex method hides.** Watts and euros put a variable at
10³ and its objective coefficient at 10⁻⁵ — a condition number near 10⁸. Simplex
only compares coefficients with each other and never notices; against an
interior-point solver's tolerances the objective is nearly flat. The dual pass
scales the objective by 10⁵ and divides the duals back out, which is exact rather
than a tuning knob.

### What the weights currently decide

Nothing, on the reference household, and that is measured rather than assumed:
`--uniform-weights` prints identical numbers. Three reasons, all correct
behaviour — the planner has usually already solved the split under the same
ceiling; when it has not, the charge point's indivisible 6 A minimum eats most of
a 4,2 kW budget; and the hardware quantises what is left to zero-or-6 A.

The mechanism is better founded than the slot price and costs one linear solve.
It decides something as soon as a household has a third and fourth controllable
device with a continuous range — white goods, a buffer tank, a second car.

The plan also reports what it expects to cost *and* what the same horizon would
cost without any of it, so a saving is a subtraction rather than a claim.

It reports that cost **term by term** — energy, battery life, curtailed
production, comfort given up — for the plan *and* for the baseline. Comparing
energy bills alone puts the saving back exactly where the wear term exists to
stop it being: a number that credits the optimiser for a cycle it has already
paid for in cell life and for a degree of cold it decided to accept. On the
reference winter day the difference is €3,42 of bill against €2,23 of actual
saving. The larger number is the one every other system quotes.

The ledger is closed on the stores as well. A period that ends with an emptier
battery than it began with has spent something it started with, and that is
charged at what it would cost to put back — the same number the terminal value
already puts in the objective, on the other side of the ledger. The mirror case
is deliberately *not* credited: the baseline has no battery to store anything in
and could never earn it, so a saving figure may understate itself and may not
flatter itself.

That baseline has to deliver the **same service** or it is not a comparison: the
car still reaches its target, the house is still warm and the shower is still
hot. What it lacks is the *decisions* — no battery, a charge point that starts
the moment the car is plugged in, and a heat pump and a hot-water tank on
ordinary thermostats, stepped through the same two-mass building model the plan
is solved against. A baseline that priced a household with no car and no heating
at all would credit the optimiser for energy it never had to buy.
