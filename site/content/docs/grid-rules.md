+++
title = "The grid rules"
description = "§ 14a EnWG, the EEBUS limitation state machine, § 9 EEG, Modul 3, MiSpeL and § 42c — as code, with the citation for every number."
weight = 4
+++

Every rule in `hems-grid` names the document it comes from. `[A1 4.5.2]` is
Anlage 1 to BNetzA decision BK6-22-300; `[LPC-031]` is the EEBUS *Limitation of
Power Consumption* use-case specification. A rule without a citation is a bug,
and `cargo xtask check-citations` fails the build if a cited document is not one
the reader can obtain.

Several of them are not implemented here at all. The § 14a minimum power, the
netzwirksamer Leistungsbezug, the Modul 3 conformance rules, the VDE-AR-N 4100
Unsymmetrieleistung and the allocation identity § 42b and § 42c settle on belong
to [`metering`](https://github.com/hupe1980/metering), which owns *quantities*
where hems owns *control*. Two enumerations of one regulation are two things
that can disagree about it.

## § 14a EnWG — which devices, and how far down

Three questions, three functions.

### Is this a steuerbare Verbrauchseinrichtung?

`[A1 2.4.1]` names four Fallgruppen: a non-public charge point, a heat pump
heater *including its auxiliary and emergency heaters*, space cooling, and
electricity storage with respect to charging. Each above 4,2 kW.

The trap is `[A1 2.4.2]`: heat pumps and cooling are summed **per Fallgruppe**
behind one connection before the threshold is applied, and if the sum passes it
the whole group counts as a *single* controllable device. Two 3 kW heat pumps are
one 6 kW device, not two devices below the threshold — a distinction worth
4,2 kW of minimum power. Charge points and storage are counted individually.

### Under which regime?

`[A1 10]` is four transitional regimes at once. An old device on the old reduced
network fee keeps it until 31.12.2028 and moves to the new rules on 01.01.2029.
A night-storage heater keeps the old rule with no end date at all. An old device
that never had a reduction is simply outside the Festlegung. And any of them may
switch voluntarily at any time — the network operator cannot refuse, and there
is no way back.

### How far down may it be turned?

```text
P_min = base + (n − 1) × GZF(n) × 4,2 kW

base = max(0,4 × ΣP_Wärmepumpe ; 0,4 × ΣP_Raumkühlung)  if a heat-pump or
                                                          cooling group above
                                                          11 kW is included
     = 4,2 kW                                             otherwise
```

with the Gleichzeitigkeitsfaktor 0,80 / 0,75 / 0,70 / 0,65 / 0,60 / 0,55 / 0,50
for two to eight devices and 0,45 from nine on.

Note the `max`, not a sum: heating and cooling do not run at full power at the
same time, and adding them would hand out a minimum the network never has to
grant.

## The netzwirksamer Leistungsbezug — why a roof lifts the ceiling

`[A1 2.3]` defines the limit as applying not to what the controllable devices
draw, but to **the part of the grid draw they cause**. While the photovoltaic
system produces more than the rest of the house needs, its surplus covers the
wallbox, and the wallbox can keep charging above the operator's limit without a
single watt of it being netzwirksam.

That is the economic case for owning an energy manager under § 14a, and hems
computes it in both places it matters: in the planner against the forecast, and
in the guard once a second against measurements.

```text
budget = ceiling + max(0, production − other load)
```

<pre class="mermaid">
flowchart LR
  PV["roof<br/>4,0 kW"] --> N{"surplus?"}
  HH["household load<br/>1,2 kW"] --> N
  N -- "2,8 kW spare" --> B["budget for the<br/>controllable devices"]
  BAT["battery discharging<br/>2,6 kW, sustainable"] --> B
  C["§ 14a ceiling<br/>4,2 kW"] --> B
  B --> R["9,6 kW the wallbox and the<br/>heat pump may share, lawfully"]
</pre>

Electrons carry no labels, so the split between controllable and other load has
to be chosen — and the Festlegung does not choose. It defines the quantity as
*"derjenige Anteil … der zeitgleich durch eine oder mehrere steuerbare
Verbrauchseinrichtungen verursacht wird"* and stops there, so which part of the
remaining grid draw the controllable devices caused is an apportionment nobody
performed for us. hems therefore takes a **Verursachungsregel** rather than
picking one silently:

| Convention | Reading | Effect |
|---|---|---|
| `SteuVeZuletzt` | generation covers the uncontrollable load first | `min(grid draw, controllable draw)` — never understates, and the default |
| `Anteilig` | generation is shared pro rata | lower whenever a roof is producing |

Both are defensible. The default can never overstate the headroom, so a guard
built on it errs early rather than late; a household whose network operator has
agreed the pro-rata reading in its Technische Mindestanforderungen can say so,
and nobody else should.

### A discharging battery is production too

`[A1 2.3]` measures what the controllable devices draw *from the grid*, so
energy that came out of a store never crossed the connection point and never
counted. A household with a full battery may therefore charge its car under a
teatime reduction, and reading the ceiling as a limit on *consumption* instead
leaves the car short on the one evening a battery exists for.

A discharge is not a roof, though: the same tick that spends it may be about to
reverse it. So the guard lends only what survives three tests — it is
**measured**, this tick is **about to ask for it too**, and the store can
**sustain it for a whole control period** — and pins the lender's ceiling at zero
in exchange. The battery may go on discharging or it may stop; what it may not
do is turn into a load while the others spend the headroom it created.

On the reference January evening that is 5,4 kWh of headroom, and the difference
between a car that leaves full and one that does not.

Two further readings go the conservative way, and both are places where the
tempting default is the unsafe one:

- a controllable device the drivers cannot hear from is assumed to be drawing
  its **nameplate** power, never nothing — otherwise the guard hands its share
  of the budget to the others while it keeps charging;
- a silent *inverter*, by contrast, is assumed to be producing nothing, because
  the alternative would raise every budget on the site.

The verdict records which devices had to be assumed, so a compliance figure
computed from an assumption never looks like one computed from a meter.

A nameplate assumption is safe in **one** direction only, and that is worth
saying separately. Guessing that a silent device is drawing its rated power keeps
the § 14a budget small, which is the right way to be wrong. Reusing the same
guess as headroom for *feeding in* turns it inside out — a house with three quiet
devices would be told it may produce nineteen kilowatts above the § 9 EEG cap
because it is probably using them — so the export side counts only what a meter
actually saw.

### And a small device is not a controllable one

A device is a steuerbare Verbrauchseinrichtung only if it passes the 4,2 kW of
`[A1 2.4.1]` — individually for a charge point and a battery, **summed per
Fallgruppe** for heat pumps, so two 3 kW heat pumps behind one connection are one
6 kW controllable device. A 3 kW heat pump on its own, or a 0,5 kW battery, is
ordinary load: a network operator may not reduce it, and it never appears against
the ceiling. What it does instead is *spend the surplus*, like the hot-water tank
and the dishwasher.

That is a fact about a site's nameplates and its commissioning dates, and getting
it wrong is expensive in both directions. Counting a device in when it is out
leaves the household **colder than the Festlegung asks for** on the winter
evenings a reduction actually happens; counting it out when it is in **overstates
the surplus** and therefore understates the netzwirksamer Leistungsbezug, which
is the one direction a compliance record may never be wrong in.

So there is exactly one function that answers it — `hems_grid::classify_at`, from
the site's own assets and the day — and everything asks it: the guard every tick,
the planner through
[`PlanningLimits::steuve_devices`](@/docs/optimizer.md#the-ss-14a-constraint-is-on-the-netzwirksamer-leistungsbezug-per-slot),
and the evidence record when it writes down what crossed the connection point.

## Where the § 14a limit is not the only shared one

A § 14a ceiling is spent only by the controllable devices. The main fuse **in
both directions**, each sub-distribution board, the 4,6 kVA of unbalanced load
VDE-AR-N 4100 allows, and the § 9 EEG feed-in cap are spent by **everything**
behind them, uncontrollable load included.

Each goes through one weighted max-min allocator and the results intersect —
the mechanism is in [Architecture](@/docs/architecture.md#two-kinds-of-limit-and-the-difference-matters).
What matters here is that a limit spent by a *set* of devices has to be shared
between them: bounding each asset by it individually lets a roof allowed 5,88 kW
and a battery allowed 5,88 kW put 11,76 kW through a limit of 5,88.

## Which devices the symmetry rule reaches

VDE-AR-N 4100 Abschnitt 5.5.2 caps the Unsymmetrieleistung of a customer
installation at 4,6 kVA — the same limit as 20 A per Außenleiter, and derived
from EN 50160's 2 % cap on voltage unbalance.

It does **not** apply to everything behind the connection, which is the reading
that looks obvious. The VDE FNN Hinweis is explicit: *"Die Anforderungen zum
symmetrischen Betrieb gelten nur für Geräte die elektrische Energie einspeisen
oder speichern können, also Erzeugungsanlagen, Speicher, Ladeeinrichtungen für
Elektrofahrzeuge."* Generation, storage and charging equipment; not a heat pump,
not a hot-water tank, not the household's own single-phase load.

Summing everything would put a kettle on L1 against the budget of the one device
the manager can actually move. A limit applied more widely than the rule is still
applied wrongly, and here the household pays for it in charging power.

One caveat. The limit is stated in **kVA** and hems computes it from **active
power**, because that is what a household driver reports — so it understates the
unbalance exactly when the grid has asked an inverter for reactive support under
VDE-AR-N 4105. A meter that reports apparent power per conductor is used as it
stands.

## The EEBUS limitation machine

A § 14a limit reaches the energy manager over EEBUS, from the FNN Steuerbox.
Five states, three interacting timers, and one rule that is routinely
implemented backwards.

| State | Meaning |
|---|---|
| `init` | Just restarted; limited by the failsafe value |
| `unlimited/controlled` | In contact, no limit active |
| `limited` | A limit from the Energy Guard is in force |
| `failsafe` | The heartbeat stopped; the failsafe value applies |
| `unlimited/autonomous` | Out of contact long enough that the failsafe was released |

<pre class="mermaid">
stateDiagram-v2
  [*] --> init : restart
  init --> unlimited_controlled : heartbeat resumes,<br/>no limit in force
  init --> limited : heartbeat resumes,<br/>a limit is in force
  unlimited_controlled --> limited : Energy Guard writes a limit
  limited --> unlimited_controlled : limit released, or its<br/>duration expires [LPC-909]
  unlimited_controlled --> failsafe : heartbeat missed for 120 s
  limited --> failsafe : heartbeat missed for 120 s
  failsafe --> limited : contact returns,<br/>a limit is in force
  failsafe --> unlimited_controlled : contact returns
  failsafe --> unlimited_autonomous : FailsafeDurationMinimum<br/>expires [LPC-922]
  unlimited_autonomous --> unlimited_controlled : contact returns
  note right of init : a restart comes back limited, not free
  note right of unlimited_autonomous : a broken control box must not<br/>block a household for ever
</pre>

- The heartbeat goes both ways at least every 60 s (`[LPC-005]`, `[LPC-006]`).
- Missing it for 120 s means the Energy Guard is gone.
- A restart comes back **limited**, not free (`[LPC-901/1]`) — a heat pump that
  reboots during a grid emergency must not return at full power.
- The **Failsafe Duration Minimum** (2–24 h, `[LPC-022]`) is *not* a safety timer
  that keeps the device down. It is a release valve: once it expires with the
  heartbeat still missing, the device goes **unlimited** (`[LPC-922]`), because a
  broken control box must not block a household for ever.

That last one is what an implementation written from intuition gets wrong, and
it has a test named after it. The machine itself lives in the
[`eebus`](https://crates.io/crates/eebus) crate and hems *derives* its state from
it rather than tracking one alongside — see [devices and
drivers](@/docs/devices.md#eebus-the-ss-14a-side).

`init` and `failsafe` are also not § 14a events. The limit in force there is the
device's own preconfigured value, applied because nothing is talking to it — the
manager restraining itself, not an operator reducing the house. The evidence
record counts the two apart, because reporting the second as the first tells a
household the operator intervened on a day when nobody did.

## § 9 EEG — two caps with different references

Two limits apply to feed-in and they are fractions of different things:

- the **60 % cap** of the Solarspitzengesetz (in force 25.02.2025) is a fraction
  of *installed* capacity, and applies until an intelligent metering system with
  a control device is in operation;
- the EEBUS **feed-in factor** (`[MGCP-011]`) is a fraction of the *cumulated
  nominal AC power* of the inverters: `P_feed-in ≤ factor × Σ P_PV,AC,nom`.

Those differ on almost every real installation, because inverters are routinely
undersized against the modules. Taking the smaller of the two results is the only
reading that satisfies both.

Both are measured at the **connection point**, which is where the statute
measures them, so what the house is using is headroom: a household drawing 4 kW
may produce 4 kW above the cap and still feed in exactly the cap. Applied per inverter instead, a
house consuming 3 kW would have its roof curtailed as though it consumed nothing
and throw away exactly those 3 kW every sunny hour.

The two grid limits are also enforced with deliberately different conservatism,
because they are enforced by different machinery. A § 14a reduction is a *control
instruction* with a five-minute response presumption `[A1 4.2]`, so the guard may
never be over it even for a tick. The 60 % cap is a *settlement* limit read off
quarter-hour meter registers, so the quantity to control is the average over the
quarter hour — and curtailing a roof as though the car were not charging throws
away real kilowatt-hours to avoid a one-second transient nobody meters.

`just demo capped` runs a clear **May** day on a 20 kWp roof with a small store —
May, not June, because the cap is a fraction of *direct-current* power and how
close a roof gets to that fraction is decided by cell temperature — and prints
the quarter-hour feed-in peak against the ceiling. The cap binds, at 12,06 of
12,00 kW for four quarter hours around solar noon — the 60 W over is the
simulated inverter's own settling time at a one-minute control period, not a
decision — and the household loses
**0,2 kWh** of export.

That is far less than "60 %" sounds, and the arithmetic is worth stating: a
German roof's clear-day peak is only about two thirds of its direct-current
rating once system losses, soiling and a 50 °C cell are taken off, so the 60 %
line clips the top tenth of the peak on the clearest days of the year — and a
household with a store, a tank and a heat pump *absorbs* most of that rather than
throwing it away.

What it *costs* needs a baseline that is capped too, because § 9 EEG does not ask
whether there is an energy manager behind the meter: lifting the cap moves the
managed household's own cost by a cent and the unmanaged one's by twelve.

The three things that decide the figure are worth naming, because getting any of
them wrong inflates it several-fold: the month (a 50 °C cell in June keeps a roof
barely above the 60 % line), whether the planner is being shown the weather in
advance, and whether the roof is modelled at its datasheet or at what a roof
three years old actually delivers. With the cap lifted there is no curtailment at
all.

An **LPP** session is a third source and a different story: it is a network
operator asking for something right now. The three are reported apart, because
telling a household that a network operator intervened when in fact the statute
simply applied is a different claim about the world.

What lifts the 60 % cap is a **technical fact**, not a commercial one: an
intelligent metering system with a control device in operation *and* the network
operator's first successful Ansteuerbarkeit test — § 9 Abs. 2 waits for both — or
the output sold on the market with the Fernsteuerbarkeit § 10b EEG demands. "The
contract is signed" and "the control path works" are different days, and on the
days between them the cap still applies. The default is that nothing has lifted
it: the cap staying on costs a household some feed-in, lifting it wrongly means
feeding in above a statutory limit, and only one of those is the operator's
problem.

### The scope of the cap is three conditions and two of them are not sizes

Reading § 9 Abs. 2 as *"2 to 100 kWp, from 25.02.2025"* is the obvious summary and
every part of it is slightly wrong — in both directions.

There is **no lower bound**. § 9 Abs. 2 S. 1 Nr. 3 reaches every system below
25 kW that draws the Einspeisevergütung; the 2 kW everybody quotes is S. 4, which
exempts *Steckersolargeräte* — and only those, and only with at most 800 VA of
inverter, behind a Letztverbraucher's Entnahmestelle. Read as a size class it
exempts every small roof array from a cap the statute puts on it, so it is a
declared fact about the installation rather than an inference from a nameplate.

The **upper bound is exclusive**: from 100 kW up, Nr. 1 demands remote
reducibility and no percentage at all.

And the date is a **window**. § 100 Abs. 3b disapplies the cap to systems
commissioned between 01.01.2023 and 24.02.2025, so "before the Solarspitzengesetz"
does not mean "uncapped": a system from 2019 still carries the obligation of the
EEG version applicable to it, which it met either with equipment the operator can
reduce it with — no static ceiling — or by limiting itself to **70 %**
(§ 100 Abs. 3 S. 2 Nr. 2). Which of the two is, again, a declaration.

### § 51 — one meter, two rules, two clocks

§ 51 Abs. 1 reduces the **anzulegender Wert** to zero in a quarter hour with a
negative spot price. Both remuneration schemes are computed from the anzulegender
Wert — § 53 Abs. 1 for the Einspeisevergütung, Anlage 1 zu § 23a Nr. 1 for the
Marktprämie — so the rule zeroes the tariff *and* the premium. A household in
Direktvermarktung priced at `spot + Prämie` in a negative hour is being told it
still earns the premium in exactly the hours the statute is paying it to stop.

Whether the rule reaches a plant at all is a **date**. § 51 Abs. 2 Nr. 1 exempts
anything below 100 kW for every period *before the end of the calendar year in
which it is fitted with an intelligent metering system* — so a meter fitted in
March keeps the negative hours of that whole year — and Nr. 2 exempts anything
below 2 kW until a Bundesnetzagentur Festlegung that has not been made.

That date is deliberately **not** the same fact as the one that lifts the 60 %
cap. § 9 waits for the Ansteuerbarkeit test; § 51 asks only when the meter went
in. The ordinary German § 14a household of 2026 is exactly the gap between them:
an intelligent metering system in for years, so the negative quarter hours earn
nothing, and the 60 % cap still on because nobody has run the test.

## The evidence record — and what "acted on it" means

`[A1 7.2]` asks the operator of a controllable device to show, for an individual
case and in a way the network operator can follow, that a commanded reduction was
carried out; `[A1 7.3]` keeps the records for two years. hems writes one
append-only record per control event at the moment the guard acts: every ceiling
commanded and when, the minimum the customer was owed against each,
a minute-resolution trace of the netzwirksamer Leistungsbezug, and how long it
took to take effect.

That last one has a trap in it. `[A1 4.2 S. 5]` requires the reduction to be acted
on without delay, and it is tempting to record "acted on" as *the first setpoint
issued*. But a reduction to 4,2 kW on a house drawing 2,1 kW requires **nothing
to be sent to anything**: the manager is inside the limit from the instant it
arrives, and there is no setpoint because there is no change to make. Waiting for
the next thing that happens to move and calling that latency reports eight
minutes on a household that was compliant from the first second — which a network
operator reads as a breach.

So a reduction takes effect when the household is *inside* it, and the record
says which of the two happened:

```console
  slowest reaction                   0 s, commanded
  slowest reaction                   0 s, already below
```

Both satisfy `[A1 4.2]`. They are different facts, and an operator asking "what
did you do?" is owed "nothing, we were at 2,1 kW" rather than a blank.

## What a reduction is worth to a household

The § 14a ceiling is a constraint in the planner, so it has a **shadow price**:
what one kilowatt-hour of relief from it would be worth to this household,
computed from its own plan rather than assumed.

```console
  relief from § 14a was worth            3.93 €/kWh
```

That is a house with no store whose car will otherwise leave short. The *same*
reduction on the same household with a 10 kWh battery is worth **nothing at
all**, because the store lends the controllable devices the headroom `[A1 2.3]`
allows and the ceiling stops binding — and a limit that costs a household nothing
is a limit nobody should be compensated for. It is `[A1 2.3]` measured in money, and it is what
a § 41e Aggregatorvertrag offer or an OpenADR bid should be priced from —
aggregators currently price both households at "30 % of nominal".

## Modul 3 — time-variable network charges

Available only together with Modul 1, only at a location without registrierende
Leistungsmessung, and only with an intelligent metering system. Windows and price
levels are fixed per calendar year for the whole network area, must be billed in
at least two quarters, and must appear in the preliminary price sheet by 15
October of the preceding year.

There is no machine-readable national format for any of it — a PDF and an Excel
sheet per network operator — so whoever commissions the box **transcribes** the
operator's calendar into its configuration, records where it came from, and the
box refuses to start unless it conforms:

```console
$ hemsd run --check --config /etc/hemsd/hemsd.toml
📅 Modul 3 `NB-14A-3-2026` for 2026 conforms — HT 17:00–20:00, NT 22:00–06:00, billed in Q1, Q4
   transcribed from https://www.example-netz.de/preisblatt-2026.pdf#modul3
```

Transcribing is not inventing, and the refusal is what keeps the difference. The
seven rules are checked before a byte moves — three levels that are all
**reachable**, a day with no gaps, at least two hours of Hochtarif on every day
class, windows identical all year, at least two billed quarters, one calendar
year of validity, and the three preconditions of § 1 — and a `source` is required,
because when a household queries a bill the first question is which document said
so.

The reachability rule is the one worth stating on its own, because its failure is
silent. A Niedertarif band written as a single wrapping window leaves that level
**declared and unreachable**: the register appears, the day is still fully covered
by the fallback, every other rule passes, and the calendar reads as a conforming
three-level tariff while the household is billed for a module whose cheap hours
it can never be in.

Window membership is decided on **local wall-clock time**, because that is how
the price sheet is written. On the long October day the repeated hour is inside
the same window twice, and on the short March day the skipped hour is inside
none. Both are what the document means.

## MiSpeL — which kilowatt-hour was green

From **1 October 2026** a household battery that has ever been charged from the
grid, or a bidirectional charge point, keeps its levy privileges and its EEG
support only if the energy through it is separated: which quantity was green,
which was grey, quarter hour by quarter hour, summed over a calendar month. Get
it wrong and the household loses the privilege on its grid draw, the market
premium on its feed-in, or both.

`hems-grid::mispel` implements the two options that need arithmetic, formula by
numbered formula, in exact decimals.

**Abgrenzungsoption** (Anlage 1, formulas (1)–(33)) follows the electrons. The
whole thing turns on one `MIN` applied *per quarter hour*:

```text
(1)¼ = MIN[ Z1NB¼ ; Z2V¼ ]     grid electricity that went into the store
(2)¼ = MIN[ Z1NE¼ ; Z2E¼ ]     feed-in that came out of it
```

That is the **Speichervorrang** — the legally willed priority that attributes
grid draw to the store before anything else. Summing the month first and taking
the minimum afterwards is the mistake that turns a perfectly ordinary day into
kilowatt-hours of arbitrage, and it is the one a test pins.

From there: the round-trip efficiency is *measured* where a battery is alone on
the meter and *presumed* at 85 % wherever a charge point makes measurement
impossible; energy a car brought home from somebody else's charge point
(**Fremdtankstrom**) is taken back out before anything is settled or supported;
and storage losses are privilegeable only where the storage system's own meter
can see them.

**Pauschaloption** (Anlage 2, formulas (P1)–(P15)) is open to solar up to
30 kWp and draws two flat lines through the year instead: 500 kWh per kilowatt
installed is supportable, an indifference band above it is neither, and anything
above *that* — in quarter hours where the spot price was not negative — is
settleable. The band widens as the store shrinks against the roof, because a
small store leaves less room for the arbitrage the band exists to keep out.

The Festlegung is an Arbeitsstand of 05.08.2026, so every result carries the
`RuleSet` that produced it and the day that rule set starts to apply. A Nachweis
that cannot say which rules made it is not evidence of anything.

## § 42c EnWG — sharing electricity with the neighbours

Since **1 June 2026** final customers inside one distribution network's
Bilanzierungsgebiet may use renewable electricity together over the public grid.
What is shared is an *allocation*, not physics: each quarter hour, the
community's generation is divided among its consumers by an
**Aufteilungsschlüssel** agreed in writing, and each member's share is billed at
the community's price instead of their supplier's.

A static key and a dynamic one are two different contracts, not one rule and its
improvement. Applying the key once and capping each member at what they used
leaves generation on whoever happened to be away: a member allocated 3 kWh who
consumed 1 kWh cannot take the other two, and those two are simply not shared.
Re-offering them to whoever still has unmet consumption shares more — but it is a
*different allocation*, and Abs. 3 Nr. 2 makes the Aufteilungsschlüssel a written
agreement between the parties.

So `hems-grid::sharing` takes an `Aufteilung`. `Statisch` applies the key once
and reports the remainder as unallocated — generation that went to the public
grid and is settled the ordinary way. `Dynamisch` cascades, and is the default
because it is what an energy-sharing community is *for*. Choosing between them
silently would be choosing one of two contracts on the household's behalf.

Each pass is `metering::allocation::allocate`, so `Σ shared + unallocated`
equals the generation exactly, whichever key applies: shares are cut to a
millionth of a kilowatt-hour before anything is subtracted, and the residual is
a difference rather than an accumulation. A settlement that loses a millionth
loses it into nobody's account, and the loss is invisible until somebody
reconciles a year.

### And the plan acts on it

Settling an allocation after the fact is half of § 42c. The other half is that a
household in a community should **move its flexible load into the quarter hours
the community is generating**, which is the whole behavioural reason to join one.

What § 42c changes is *which energy price applies to the kilowatt-hours the
Aufteilungsschlüssel allocated*, and the allocation is capped at what the member
actually drew. So a slot costs

```text
shared_price · min(share, g_in)  +  import_price · (g_in − min(share, g_in))
```

— a **cheap block first**, which is convex exactly while the community is the
cheaper of the two. A convex cheap-block price needs one bounded column and one
row rather than a binary: the allocation is at most the share the key offers, at
most what the household drew, and it comes off the bill.

Where a community is *dearer* than the supplier the same function is concave, and
a model that treated the discount as optional would let the plan believe it could
decline an allocation it cannot — the key applies whatever anybody prefers. So
the discount floors at zero and the plan claims no advantage rather than
inventing one.

The price lives in the tariff, because a price is: § 42c replaces the supplier's
**energy component** and nothing else, since the electricity reaches the member
over the public grid. On the reference winter day a community selling at
12 ct/kWh net still delivers a **32,5 ct** kilowatt-hour against the supplier's
**47,9** — a third off, and not the ninety per cent that "free solar from the
neighbours" suggests. Network charges, the Stromsteuer, the Konzessionsabgabe and
19 % value added tax do not care where the electron came from.

`hemsd simulate --day winter --sharing` runs the comparison, and the **baseline
is in the same community**: a household joins one and then does nothing about it,
and the key allocates it anyway. Leaving the baseline outside would report the
value of the *membership* as though the planner had produced it. On the reference
winter day membership alone is worth **€0,88** to a household that does nothing,
and the planner's shifting adds **€0,19** on top; the day settles **14,5 kWh**
through the community, from its own quarter-hour registers and through the same
allocation a Nachweis would use.

Whether a delivery point may take part at all is a metering question
(Zählerstandsgangmessung *or* quarter-hourly registrierende Leistungsmessung,
§ 42c Abs. 1), and it lives in `metering::sharing`. hems consumes that decision
rather than restating it.
