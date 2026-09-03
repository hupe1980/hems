+++
title = "The planner"
description = "A receding-horizon mixed-integer linear program that prices battery wear instead of hiding it."
weight = 6
+++

A plan is made every quarter of an hour, covers the next twenty-four, and only
its **first slot** is ever executed. Everything after that slot exists to price
the consequences of it.

<pre class="mermaid">
flowchart LR
  I["prices · forecasts<br/>site state · grid limits"] --> S["solve the horizon<br/>MILP"]
  S --> E["execute slot 0<br/>as a target + an envelope"]
  S --> T["slots 1…95<br/>discarded"]
  E --> A["the arbiter,<br/>once a second"]
  A --> M["measurements"]
  M --> I
</pre>

That is what makes several decisions on this page turn out the way they do: a
[minimum runtime](#a-compressor-s-minimum-runtime-needs-the-compressor-s-own-state)
that says nothing about slot 0 is enforced on no day, and a
[commitment horizon](#the-commitment-horizon-the-tail-does-not-need-quarter-hours)
may coarsen the tail without coarsening the decision.

## The formulation

One set of variables per quarter-hour slot of a 24-hour horizon, all in watts and
all non-negative: grid import and export, battery charging and discharging, the
energy in the battery, charging power at the charge point, the energy in the car,
the hot-water heater and the heat in its tank, and production thrown away. Plus,
per shiftable appliance, one binary per **feasible** start.

```text
g_in − g_out = load + b_ch − b_dis + ev + hp + dhw + app − (pv − curtail)
                                                               (energy balance)
e[k]    = e[k−1] + Δ(η_ch·b_ch − b_dis/η_dis)                  (battery state)
ev_e[k] = ev_e[k−1] + Δ·η·ev                                   (car state)
w[k]    = w[k−1] + Δ·cop·dhw − loss − draw[k] + short[k]       (tank state)
ev_on·min_charge ≤ ev ≤ ev_on·max_charge, ev_on ∈ {0,1}        (6 A or nothing)
Σ_k start[i][k] + not_run[i] = 1,  start ∈ {0,1}               (one wash, once)
app[k] = Σ_i Σ_j start[i][j]·programme_i[k − j]                (its own shape)
b_ch + ev + hp − b_dis + curtail + dhw + app ≤ ceiling[k] + (pv − load)
                                                    while pv > load
Σ SteuVE − b_dis                      ≤ ceiling[k]  otherwise   (§ 14a)
g_out ≤ feed-in ceiling[k]                                     (§ 9 EEG, LPP)
g_out ≤ pv − curtail + b_dis                                   (no invented export)
g_in ≤ M_in·(1 − z[k]),  g_out ≤ M_out·z[k],  z ∈ {0,1}         (one direction,
                                     only where export price > import price       at a time)
```

There is deliberately **no mirror** of the export bound. "Imported power has to
go into the house or a store" is implied by it and the energy balance, so it
constrains nothing — and it is always tight whenever the household is not
exporting, which gives it a free non-negative dual that absorbs the energy
balance's own. With it in the model every shadow price below is meaningless: an
ordinary 20 ct hour came back at 5 986 €/kWh.

`Σ SteuVE` is the devices a network operator may actually reduce, not every
device the planner models: a heat pump group below 4,2 kW is not a steuerbare
Verbrauchseinrichtung. That is a fact about a site's nameplates and its
commissioning dates, which `hems-grid` answers and the planner is told — see
[the § 14a section](#the-ss-14a-constraint-is-on-the-netzwirksamer-leistungsbezug-per-slot).

minimising, in euros throughout,

```text
Σ Δ·( (price_in + carbon_price·intensity + autarky_premium)·g_in
      − price_out·g_out
      + wear·(b_ch + b_dis)
      + curtailment_penalty·curtail
      + discomfort_price·(kelvin outside the comfort band) )
  + shortfall_price·(hot water not delivered)
  + unmet_charge_price·(charge not delivered by the deadline)
  + Σ_i unrun_price_i·not_run[i]
  − terminal value of what is left in the battery, the building and the tank
```

## Fifteen decisions worth explaining

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
receding-horizon controller it repeats at every re-plan. hems values what is
left at the mean import price over the horizon, discounted by what it costs to
get back out.

### You cannot import and export at the same instant

`g_in` and `g_out` are two non-negative variables whose *difference* the energy
balance fixes, so adding the same watt to both is always feasible. The objective
charges `import − export` for that watt, which is the whole of the defence — and
only a defence while importing costs more than exporting earns.

Two ordinary German tariffs invert that. A deeply negative day-ahead quarter hour
pushes the import price below zero while [§ 51 EEG](@/docs/tariffs.md) holds the
export price at zero, and a legacy feed-in tariff — a 2010 roof at 39 ct/kWh — is
above retail in every hour of the year. There the export bound keeps the model
*bounded* and cannot make it *right*: its mirror is algebraically itself, so
there is no inequality left to add, and one connection point running one way at a
time is a disjunction.

So hems declares a direction binary, big-Ms taken from what the household could
draw and what the roof and the battery could deliver, **only in the slots where
the price could pay for the round trip** — no slots at all on a modern feed-in
tariff, which is the economy the MILP-HEMS literature uses.

### The § 14a constraint is on the netzwirksamer Leistungsbezug, per slot

Not on consumption. The surplus a roof is producing raises the ceiling exactly as
`[A1 2.3]` says it does, so a household with photovoltaics keeps charging through
a reduction — lawfully. The same arithmetic runs in the guard once a second
against measurements; here it runs against the forecast.

Four details, and each is easy to get backwards.

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

`Σ SteuVE` is the devices the ceiling **actually binds**, and the planner cannot
work that out for itself. A device is a steuerbare Verbrauchseinrichtung only if
it passes the 4,2 kW of `[A1 2.4.1]` — individually for a charge point and a
battery, summed per Fallgruppe for heat pumps — which is a fact about a site's
nameplates and its commissioning dates. `hems-optimizer` takes no site: it sees a
battery model, a charging session and a thermal model, none of which carries a
Fallgruppe. Assuming all three are controllable is harmless while a roof is
producing, where a device that *spends* the surplus and a device the ceiling
*binds* carry the same `+1` on the same row — and it is wrong on a winter
evening, where the row has no surplus term at all and a 3 kW heat pump no network
operator may reduce was charged against a ceiling it is not under. The household
was made colder than the Festlegung asks for, in exactly the hours a reduction
happens. `hems-grid::classify_at` is what answers it, `hemsd` asks, and
`PlanningLimits::steuve_devices` is how the answer reaches the plan. The guard
never had this problem: it classifies the real site every tick.

And the ceiling is read **per slot**. A reduction has a duration — `[LPC-909]`
sends one with the limit, and the failsafe releases after its own minimum — and
stretching today's ninety minutes across a whole horizon plans the
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
rather than branched in every slot after the car has left, which on a 96-slot
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

Measured on a September day with the planner off — the shoulder season, where the
surplus sits in the 1,4 – 4,1 kW band for hours: a switchable charge point puts
**13,1 kWh** into the car against **0,2** for a fixed three-phase one, for one
contactor operation. Midsummer measures nothing, because the roof spends the
middle of the day above the 4,14 kW three conductors need.

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

### A dishwasher is placed, not spread

The obvious model for a shiftable load — energy the planner may move between
slots — is wrong in the way that matters. A dishwasher's programme is *shaped*:
two kilowatts while it heats, two hundred watts while it washes, two kilowatts
again to dry. A planner allowed to smear that over six hours schedules seven
hundred watts of dishwasher into every sunny slot, which no dishwasher will carry
out, and the household's day arrives with the machine still full.

So the decision is a single binary per feasible start, and the programme follows
it exactly. Binaries are declared **only where a start would fit** — after the
household's earliest, finishing before its deadline and inside the horizon — so a
two-hour programme in an eight-hour window leaves a few dozen places to go rather
than a hundred and ninety.

It is also the one device in the model with **no ceiling to give**. A controller
that pauses a running dishwasher has not shed a kilowatt, it has left the dishes
dirty — so the appliance declares `SCHEDULE` and not `LIMIT_CONSUMPTION`, the
one-second arbiter never touches it, and its power reaches the guard as a
*measurement* inside the household's own load. In the § 14a line above it is on
the side that **spends** the surplus, not the side the ceiling bounds: a white
good is in none of the four Fallgruppen of `[A1 2.4.1]`.

Not running it is soft, at €2 — a household that asked for a wash in a window too
tight for it is owed a plan that says "not this one" rather than no plan at all —
and the report charges it, on both sides.

And with **no plan at all**, the box still starts it: when the measured surplus
covers its first step, and no later than the last start that finishes inside the
window. A box that waits for a plan that is not coming leaves the household with
dirty dishes and a worse day than a €10 appliance timer would have given them.

### The forecast is a distribution, and the plan may be priced against it

The forecast publishes a band. A deterministic planner throws two thirds of it
away and optimises against a median that will not happen.

`Risk` reads it as **three futures** instead — Swanson's rule on the P10, P50 and
P90, weighted 0,3 / 0,4 / 0,3, so the scenario set costs nothing to produce
beyond the band that is already there. They are paired **comonotone in the
household's misfortune**: the pessimistic future is dull *and* hungry, because a
household's bad day is the correlated one, and sampling the two independently
would put most of the probability on the bland middle.

Only the **first slot's** controllable decisions are shared across the futures.
That is the slot the arbiter is about to commit, and a plan with three answers for
the next quarter hour is three plans and a coin; everything after it is
**recourse**, which is what makes hedging affordable rather than a tax on every
sunny day. The grid and the state variables are deliberately not tied —
production differs between the futures by kilowatts in that first slot.

The tail is Rockafellar–Uryasev, exactly:

```text
minimise (1 − λ)·Σ p_s·cost_s  +  λ·( ζ + 1/(1−α)·Σ p_s·tail_s )
subject to  tail_s ≥ cost_s − ζ ,  tail_s ≥ 0
```

**And the evaluation that can falsify it ships with it.** A single simulated day
pays a hedge's premium every time and makes its claim never, so measured once,
insurance is always a pure loss. `hemsd risk` runs the day under several weathers
under each policy:

```console
$ just risk deadline 20
  policy                mean     worst      best    unserved     solve
  one median           2.81€     1.78€     3.55€       0.07€      103s
  three futures        2.96€     1.76€     3.92€       0.01€      517s
  …and the tail        2.89€     1.67€     3.82€       0.01€      481s
  only when at risk     2.92€     1.67€     3.93€       0.01€      467s

  band               covered      CRPS    episodes
  production             80%      178W          20
  household load         81%       21W          20

  a 10–90 band should cover 80 %. It does, over enough days to say so.
```

Over twenty seeded weathers on each of two days, scenarios **pay where a service
is at risk** — three futures beat the median on the mean, €2,96 against €2,81,
and take the undelivered charge from €0,07 to €0,01. They **cost €1,04 a day
where nothing is at risk**. And **no** policy improves the worst day: on the
ordinary day the hedge takes it from €1,18 to €0,32. So the default is one
median.

Every one of those figures moved when the sweep grew from four weathers to
twenty, which is the argument for owning the sweep rather than a footnote to it.

A calibrated band is a *precondition* for planning against scenarios, not an
improvement to it. Scored only where there is something to forecast — a band of
nothing against an outcome of nothing is midnight, not a forecast that came true
— and with each tail calibrated against its own outcomes, the bands cover 80 % on
the January day and 75 % on the June one over twenty weathers each. So these
figures are about the machinery rather than about the input.

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

### A compressor's minimum runtime needs the compressor's own state

A non-modulating heat pump has minimum on- and off-times, and they are stated the
way every unit-commitment model states them:

```text
on[j] ≥ on[k] − on[k−1]      for j ∈ (k, k + L)     once started, stay on
on[j] ≤ 1 − on[k−1] + on[k]  for j ∈ (k, k + ℓ)     once stopped, stay off
```

Every row there constrains a **transition**, so every row needs the slot before
it — and none of them says anything about slot 0. Slot 0 is the only slot a
receding horizon ever executes.

So a box could start the compressor, commit that quarter hour, re-plan against a
model with no memory of it, and stop it again. For ever, at every re-plan, with
every individual plan feasible and no property violated anywhere. The minimum
runtime was stated in every plan and enforced on no day.

The plan is therefore given `CompressorState` — whether the unit is running, and
for how long — so `on[−1]` is a constant and the opening slots its minimum still
owes are pinned rather than branched. Anchoring the first transition also shrinks
the search: a 96-slot heating day went from **5,0 s to about 1,0 s**, because a
free first transition is a branch that propagates the length of the horizon.

The rows stay the pairwise ones above. Rajan and Takriti's turn-on/turn-off
inequalities describe the convex hull of this polytope, so the relaxation is
integral and the solver stops branching on it — built, and measured over five
96-slot heating days at **6,9 s against 4,9 s**. The tighter relaxation is real,
and the `2n` extra columns and `2n` extra rows cost more than it returns at a
household's size, where the horizon is a hundred slots rather than a utility's
thousands.

### The commitment horizon: the tail does not need quarter hours

Ninety-six binaries in each of ninety-six re-plans is a genuine mixed-integer
problem, and the reference winter day on a single-speed compressor took
**13:19** of solver time against nine seconds for the same day on a modulating
unit. Three numerical attacks on that — the convex hull above, a warm start from
the previous plan, pinning the slots the compressor's own history had already
decided — each bought a fraction and none of them closed it.

What closes it is structural, and it comes from the same unit-commitment
literature: a receding horizon executes the **first** slot and throws the rest
away. The tail exists to price that slot's consequences, and a consequence
measured to the hour is the same consequence measured to the quarter hour,
because the building fabric's time constant is days. So the compressor is decided
slot by slot over a fine head — two hours by default — and **once per clock
hour** after it.

Three things make that sound rather than merely fast:

* it is a **restriction** of the feasible set and never a relaxation, so nothing
  the model states — the minimum runtime, the § 14a ceiling, the comfort band —
  can be escaped by coarsening it;
* the **continuous** power stays per slot, so a committed hour is still free to
  modulate between the unit's floor and its rating. A block fixes only *whether*
  the compressor runs;
* the blocks are anchored to the **local clock**, not to where the fine head
  happens to end, so they sit on the boundary the tariff already steps at and do
  not slide by one slot at every re-plan.

Measured on the reference winter day, back to back on the same machine:
**13:19 → 2:12**, with the cost of the day identical to the cent — €22,57 either
way — and one *fewer* compressor start.
Where a block costs something is a price that moves faster than the block itself,
and `CommitmentHorizon::fine()` decides every slot on its own for anyone who
needs that, or wants the model every other figure here was measured against.

### The inputs have to describe the horizon

Every input to a plan is indexed by slot, and a slot an input does not reach is
the dangerous case. A forecast is the worst of them: a missing band read as
**zero** says the roof is dark *and* the house is empty. Wrong in both directions at once, so the plan defers every flexible
kilowatt-hour into hours it believes cost nothing, and the day arrives to find
them ordinary. Nothing logged it, nothing failed, and the plan looked like any
other.

Both forecasts must now cover the horizon, and a price stack must be the
horizon's own hours — it carries its own slot, so that is one comparison per
slot. A stack that merely *stops short* is still allowed and still filled with a
flat default: a horizon can outrun the last published day-ahead auction, and
being indifferent about when to act out there is exactly the state of knowledge.

## Solvers

`good_lp` behind a feature flag. The default is **microlp**, pure Rust, so
`cargo test` needs no C++ toolchain and the crate cross-compiles to a gateway box
without a build environment on it. **HiGHS** is markedly faster on a full
96-slot horizon and is what a production box should be built with; CI runs the
test suite against both.

The model has binaries, so it also carries a **relative gap** (0,5 % by default)
and a **time budget** (10 s), honoured by HiGHS. A household plan is re-made
every quarter of an hour against forecasts that are wrong by far more than half a
per cent; spending a minute to prove the last fraction of it is spending a minute on
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

## § 42c: the neighbours' roof as a cheap block of import

Since 1 June 2026 a German household may share renewable electricity with its
neighbours over the public grid. What is shared is an **allocation**: each
quarter hour the community's generation is divided among its members by an
Aufteilungsschlüssel, and each member's share is billed at the community's price
instead of their supplier's.

Settling that after the fact is bookkeeping. The interesting half is that a
member should **move its flexible load into the quarter hours the community is
generating** — and that is a planner question.

The allocation is capped at what the member actually drew, so a slot costs

```text
shared_price · min(share, g⁺)  +  import_price · (g⁺ − min(share, g⁺))
```

which is a *cheap block first*. That function is **convex exactly while the
community is the cheaper of the two**, and a convex cheap-block price is one
bounded column and one row in the linear program rather than a binary:

```text
g_shared ≤ share[k]     the Aufteilungsschlüssel's own offer
g_shared ≤ g⁺[k]        a member cannot be allocated what it did not draw
```

with the discount on the objective. The two bounds together say
`g_shared = min(share, g⁺)` at the optimum, without anybody writing a `min`.

Where a community charges *more* than the supplier the same function is concave,
and an optional discount would let the plan believe it could decline an
allocation it cannot — the key applies whatever anybody prefers. So the discount
floors at zero and the plan claims no advantage rather than inventing one. The
column is not declared at all outside a community, because even a column pinned
to zero reorders the model, and the reference winter day came back a cent
different for one.

Two things are easy to get wrong here and both cost real money in the report.

**Only the energy component changes.** § 42c does not exempt the Netzentgelt, the
Stromsteuer, the Konzessionsabgabe or 19 % of value added tax, because the
electricity reaches the member over the public grid. A community selling at
12 ct/kWh net delivers a **32,5 ct** kilowatt-hour where the supplier delivers
**47,9** — a third off, which is real money and is nothing like the ninety per
cent "free solar from the neighbours" implies.

**The baseline joins the same community.** A household signs up and then does
nothing about it; the key allocates it anyway. On the reference winter day that
membership is worth **€0,88** to a household with no energy manager. What the
planner adds on top — moving the dishwasher from +75 minutes to +15 and the heat
pump into the neighbours' daylight — is **€0,19**, and the day settles
**14,5 kWh** through the community from its own quarter-hour registers. Reporting
the €0,88 as a planner saving would be the same asymmetry as measuring against a
household that ignored the network operator.

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
relaxation. So the model is solved twice.

<pre class="mermaid">
flowchart LR
  P1["<b>pass 1</b> — MILP<br/>microlp or HiGHS<br/>relative gap · time budget"] --> D["the discrete decisions:<br/>the charge point's on/off,<br/>a single-speed compressor's"]
  D --> P2["<b>pass 2</b> — LP with those pinned<br/>always Clarabel<br/>objective scaled ×10⁵"]
  P1 --> PLAN["the plan the arbiter executes"]
  P2 --> DU["duals: a price per asset,<br/>and the § 14a ceiling's own"]
  DU --> W["the guard's allocation weights"]
</pre>
 The second solve pins the discrete
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
short; **nothing at all** on the same household with a battery, because the store
lends the controllable devices all the headroom `[A1 2.3]` allows and the ceiling
stops binding. A limit that costs a household nothing is a limit nobody should be
compensated for. Aggregators price both at "30 % of nominal", because nobody
publishes the alternative.

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
20 ct hour comes back at 5 986 €/kWh. (That the row constrains nothing is also
why it could not fix the round trip it looks like it forbids — see
[the direction binary](#you-cannot-import-and-export-at-the-same-instant).)

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
production, comfort given up, and the service it decided not to deliver — for the
plan *and* for the baseline. The invariant is that **every term of the objective
is a term of the report**: comparing energy bills alone credits the optimiser for
a cycle it has already paid for in cell life, for a degree of cold it accepted,
and for a charging session it abandoned. On the reference winter day the
difference is €3,41 of bill against €2,10 of actual saving. The larger number is
the one every other system quotes.

The ledger is closed on the stores as well. A period that ends with an emptier
battery than it began with has spent something it started with, and that is
charged at what it would cost to put back — the same number the terminal value
already puts in the objective, on the other side of the ledger. The mirror case
is deliberately *not* credited: the baseline has no battery to store anything in
and could never earn it, so a saving figure may understate itself and may not
flatter itself.

The **car** is the exception and has an entry of its own, because both households
own the same one. Charge pushed into it *past* what the household asked for is a
kilowatt-hour nobody buys later, so it is credited on both sides. That reverses a
claim worth stating plainly: respecting the household's own Ladelimit costs
money on the ledger, and what it buys — lithium life — is a cost the ledger does
not price. Saying the limit paid for itself was an artefact of valuing energy in
a car at nothing.

That baseline has to deliver the **same service** or it is not a comparison: the
car still reaches its target, the house is still warm and the shower is still
hot. What it lacks is the *decisions* — no battery, a charge point that starts
the moment the car is plugged in, and a heat pump and a hot-water tank on
ordinary thermostats, stepped through the same two-mass building model the plan
is solved against. A baseline that priced a household with no car and no heating
at all would credit the optimiser for energy it never had to buy.

It also lives under the **same law**: a house with no energy management system
cannot be addressed as one `[A1 4.4.b]`, so during a reduction its Steuerbox
turns each device down on its own `[A1 4.4.a]` and may take none of them below
`[A1 4.5.1]`, and its roof is capped by § 9 EEG like any other. And it pays for
the service it fails to deliver, on the same terms — an unmanaged wallbox on a car
plugged in an hour before it leaves falls short too.
