+++
title = "Tariffs and prices"
description = "A German bill is a stack, and every layer moves for its own reasons — the five published day-ahead sources, § 51 EEG, and the Modul advisor that answers a question no supplier will."
weight = 5
+++

Since **1 October 2025** the German day-ahead auction clears in quarter hours, and
since **§ 41a EnWG** every supplier has to offer a tariff that follows it. That
makes “what does a kilowatt-hour cost at 17:15 on Thursday?” a question with an
answer — and the answer is the input the [planner](@/docs/optimizer.md) spends
most of its objective on.

`hems-tariff` is that answer. Like every domain crate it does **no I/O**: the
parsers are `&str` in, a series out, and the fetching lives in
[`tariffd`](@/docs/services.md#tariffd-the-price-stack).

## The bill is a stack

Every layer moves for its own reasons and on its own clock.

| Layer | Moves with | Where it comes from |
|---|---|---|
| **Energy** | the market, every 15 minutes | § 41a EnWG, the EPEX day-ahead auction |
| **Network charge** | the time of day, if Modul 3 is chosen | BK8-22/010-A and the operator's price sheet |
| **Levies and taxes** | the calendar year | StromStG, KWKG, § 19 StromNEV, Offshore-Netzumlage, Konzessionsabgabe |
| **Value added tax** | rarely, and on all of the above | 19 % |
| **Feed-in** | the support regime, and the sign of the spot price | EEG §§ 19, 51, 53 |

The layers stay **separate all the way through** and are summed only at the edge.
The optimiser wants one number per direction per slot; the household wants to see
the stack; and the [Modul advisor](#the-modul-advisor) has to take it apart again
to answer “would Modul 2 be cheaper for us?”. A design that adds them up early
can serve exactly one of those three.

<pre class="mermaid">
flowchart LR
  subgraph pub["published, fetched by tariffd"]
    E["day-ahead energy<br/>ct/kWh, per quarter hour"]
  end
  subgraph conf["configured from the price sheet"]
    N["network working price<br/>+ Modul 3 level"]
    L["levies and taxes"]
  end
  E --> S["SlotPrice"]
  N --> S
  L --> S
  S --> IMP["import_ct, gross<br/>what the household pays"]
  S --> EXP["export_ct<br/>what it earns — zero when<br/>the spot price was negative"]
  S --> SH["shared_ct<br/>a § 42c allocation"]
  IMP --> P["the planner"]
  EXP --> P
  SH --> P
</pre>

### Money is exact, control is not

Prices and costs are `rust_decimal::Decimal` in **cents per kilowatt-hour**.
Optimisation runs on `f64`, because a solver has no use for exact arithmetic —
but anything a household is shown or billed comes back through `Decimal`, and the
conversion happens **once**, at `SlotPrice::import_f64`. A settlement that went
through a `double` is one nobody can reproduce.

## The five sources, and what is a decision in each

`hems-tariff::source` parses what ENTSO-E, SMARD, aWATTar, Tibber and
Energy-Charts actually publish. Each is a pure function tested against a
**captured response**, on a machine with no network, including the two days a
year with 92 and 100 quarter hours.

Three things in them are decisions rather than plumbing:

- **The factor of ten lives in one place.** €/MWh and ct/kWh differ by ten, and a
  conversion written at five call sites is wrong at one of them within a year.
- **An hourly price expands, it is never averaged.** A source that still publishes
  hours becomes four identical quarter hours. Averaging four quarter hours into
  one destroys exactly the structure the planner exists to exploit.
- **A position is resolved as an instant, not an index.** “Position 27 of the
  day” is not a time until somebody says which day, and on the March and October
  days it is not the same quarter hour as position 27 of an ordinary one.

And one that costs real money: Tibber publishes a **gross consumer** price.
`PriceBasis` refuses to let it be used as a wholesale one, because adding the
levies on top of a price that already contains them roughly doubles a
household's modelled bill — and a planner optimising against a doubled bill makes
confident, expensive, wrong decisions.

## § 51 EEG: negative prices earn nothing

§ 51 Abs. 1 reduces the **anzulegender Wert** to zero in a quarter hour with a
negative spot price. Both remuneration schemes are computed from it — § 53 Abs. 1
for the Einspeisevergütung, Anlage 1 zu § 23a Nr. 1 for the Marktprämie — so the
rule zeroes the tariff *and* the premium.

The rule turns on the sign of the **market** price, not on what the household
pays, so the raw spot price is kept beside the retail one. A household in
Direktvermarktung priced at `spot + Prämie` that still books the premium in a
negative hour is being paid in exactly the hours the statute is paying it to
stop.

Whether the rule reaches a plant at all is a **date**, and it is deliberately not
the same date that lifts the § 9 EEG cap — see
[the grid rules](@/docs/grid-rules.md#ss-51-one-meter-two-rules-two-clocks).

## Modul 3: time-variable network charges

A network charge that changes with the clock is the second dynamic layer, and it
moves for entirely different reasons from the first: the market prices *energy*,
Modul 3 prices *the network at times it is busy*. The two are routinely out of
phase, and a planner that only sees one of them will move load into an hour that
is cheap on the wrong axis.

Availability is narrow and worth knowing before building on it: only together
with Modul 1, only at a location without registrierende Leistungsmessung, and
only with an intelligent metering system. Windows and price levels are fixed per
calendar year for the whole network area, must be billed in at least two
quarters, and must appear in the preliminary price sheet by 15 October of the
preceding year.

There is **no machine-readable national format** for any of it — a PDF and an
Excel sheet per network operator — so the installer transcribes the operator's
calendar into `[tariff.modul3]` and `hemsd run --check` refuses a box whose
calendar breaks the [Anwendungshilfe](@/docs/grid-rules.md#modul-3-time-variable-network-charges).
Window membership is decided on **local wall-clock time**, because that is how
the price sheet is written.

It is worth planning against for a reason the wholesale curve cannot match: the
windows are fixed **a year in advance**. A household learns tomorrow's spot price
each afternoon and learns its Hochtarif in the preliminary price sheet of the
preceding October, so the one flexibility it can schedule around with certainty
is this one.

## The Modul advisor

Modul 1, 2 and 3 are three different ways of paying the network, and a household
may only pick one. Nobody selling a tariff can be relied on to run that
comparison in the customer's favour. A household's own energy manager can, and it
is holding the only data that answers it.

`compare_moduls` prices a household's **own quarter-hourly history** under all
three:

```console
  Modul 2 pays above                     2417 kWh/a
  …on this day it would have         -3.97 € on the energy
```

It reports a **threshold in kilowatt-hours a year**, not a projected saving, and
that is the whole design. One day is not a year: multiplying a January Thursday
with a car on the cable by 365 tells every household with an electric vehicle
that it is losing four figures. A threshold is a statement the data supports —
*above this annual consumption, Modul 2 is the cheaper contract* — and the
household already knows which side of it they are on.

## § 42c: the neighbours' roof is a cheap block, not a free one

A household in an energy-sharing community pays the community's **energy
component** for the kilowatt-hours the Aufteilungsschlüssel allocated, and its
supplier's for the rest. `SlotPrice::shared_ct` is the same stack with the
community's energy price substituted, so the two are directly comparable.

The result is less dramatic than the phrase “free solar from the neighbours”
suggests, and the arithmetic is the point: a community selling at **12 ct/kWh
net** still delivers a **32,5 ct** kilowatt-hour against the supplier's
**47,9** — a third off. Network charges, the Stromsteuer, the Konzessionsabgabe
and 19 % VAT do not care where the electron came from, because it crossed the
public grid to get here.

`SlotPrice::shared_saving_f64` floors at **zero**, and never goes negative. That
keeps the planner's sharing term convex; the reasoning is in
[the planner](@/docs/optimizer.md#ss-42c-the-neighbours-roof-as-a-cheap-block-of-import).

## Two queries a household actually asks

Most of what a household wants from a price curve is not an optimisation.

```rust
let stack = PriceStack::resolve(&tariff, horizon);

// "When should the car charge?" — answerable without a solver.
let cheapest = stack.cheapest_slots(8);

// "When does feeding in earn nothing?" (§ 51 EEG)
let free = stack.zero_export_slots();

// The number that decides whether grid-charging a battery pays at all:
// the spread has to cover the round trip and the wear before anything is left.
let spread = stack.spread_ct();
```

## Where the prices come from at runtime

`tariffd` fetches the published curves, reconciles a curve that arrives twice
under a **written trust order**, and holds two days each way so a WAN outage
never costs a plan. That is on [the fleet page](@/docs/services.md#tariffd-the-price-stack);
what matters here is that none of it is in this crate, and that a price stack
which merely *stops short* of the horizon is still allowed — a horizon can
outrun the last published auction, and being indifferent about when to act out
there is exactly the state of knowledge.
