+++
title = "hems"
description = "The open-source home energy management platform for the German market: § 14a EnWG, § 9 EEG, MiSpeL and Modul 3 as executable, cited rules; a guard the optimiser cannot argue with; EEBUS and S2 native; written in Rust."
+++

# The house is a power station now

<p class="lead">hems is the open-source home energy management platform for the
German market. Grid rules that are executable and cited, a guard the optimiser
cannot argue with, and everything that matters is sans-I/O.</p>

A household with a roof, a battery, a car, a heat pump and a hot-water tank has
legal obligations. Since 2024 the network operator may turn its controllable
devices down (§ 14a EnWG). Since 2025 a new photovoltaic system may feed in only
60 % of its installed power until an intelligent meter arrives (§ 9 EEG), every
supplier offers a tariff that changes every quarter hour (§ 41a EnWG), and
nothing is earned while the price is negative (§ 51 EEG). From 2026 storage has
to account for where its energy came from (MiSpeL), and neighbours may share
electricity (§ 42c EnWG).

Most energy managers treat that as paperwork. hems treats it as computation.

<div class="cta">

[Get started](@/docs/getting-started.md) [Source on GitHub](https://github.com/hupe1980/hems)

</div>

<div class="cards">
<div class="card"><h3>Cited, not claimed</h3><p>Every rule names the document it comes from — <code>[A1 4.5.2]</code>, <code>[LPC-031]</code> — and a build guard checks that the document is one you can actually obtain.</p></div>
<div class="card"><h3>Proven, not promised</h3><p>“The network operator’s limit wins” is a property test over a thousand random households, not a code path someone remembered to write. The main fuse in both directions, every sub-board and the 4,6 kVA Schieflast limit go through the same mechanism.</p></div>
<div class="card"><h3>Honest about money</h3><p>Battery wear is in the objective, and so are carbon and comfort — all of them as prices, so the terms can be added up. And the saving is <em>reported</em> with all of them too: the bill alone is always the flattering number.</p></div>
<div class="card"><h3>Measured where the rule measures it</h3><p>§ 14a limits what the controllable devices draw <em>from the grid</em>, so a discharging battery lends them headroom. § 9 EEG limits what leaves the <em>connection point</em>, so the house’s own consumption is headroom too. Applying either per device is the obvious implementation, and it is wrong in euros.</p></div>
<div class="card"><h3>Works without the cloud</h3><p>Guard, arbiter, planner and solar model take time as a parameter. A winter day with a grid event is a unit test that runs in milliseconds — and a device the manager can only <em>limit</em> is handed back to its own thermostat rather than held at zero.</p></div>
<div class="card"><h3>A price per device, not per hour</h3><p>The plan carries the shadow price of each store's own state equation — what the household loses if <em>this</em> device is held back — so "a reduction takes power from where it is worth least" is a decision rather than a sentence. It also prices the § 14a ceiling itself: what relief from a network operator is worth to <em>this</em> household, €3,93/kWh with a car about to leave short and €0,16 with a battery to lend it headroom. Aggregators price both at "30&nbsp;% of nominal".</p></div>
<div class="card"><h3>The forecast is allowed to be wrong</h3><p>The simulated day runs a seeded realisation the planner is never shown — a cloud at 12:19, a milder afternoon, a roof that delivers 92&nbsp;% of its datasheet — and the planner gets only what six weeks of the box's own metering could have taught it. Every day scores its own forecasts beside the money. <code>--perfect-foresight</code> gives the counterfactual, and on the winter day it is worth <strong>60&nbsp;% of the saving</strong>.</p></div>
</div>

## One day, end to end

```console
$ cargo run -p hemsd -- simulate --day winter

  2026-01-15 — with a § 14a reduction

  produced                                  8.4 kWh
  charged into the car                     21.7 kWh
  heat pump                                26.2 kWh
  hot water                                 3.1 kWh
  self-sufficiency                             13 %

  roof, as the box learned it        90 % of the model
  production forecast, CRPS          65 W (93 % covered)
  load forecast, CRPS                18 W (85 % covered)

  electricity bill                          20.47 €
  battery life spent                         0.63 €
  comfort given up                           0.22 €
  cost of the day                           21.51 €
  without optimisation                      23.74 €
  saved                                      2.23 €
  …of it on the bill                         3.42 €

  § 14a limit in force                       90 min
  …covered by the store                     1.5 kWh
  control events recorded            1 (93 samples)
  slowest reaction                   0 s, commanded
  minutes without a plan                          0
  limit respected throughout                    yes

  described in S2                       5 resources
  relief from § 14a was worth            0.16 €/kWh
```

Six lines in that table are not in anybody else's:

| Line | What it means |
|---|---|
| **without optimisation** | the same day delivering the **same service** — car charged, house warm, shower hot — with no battery, a wallbox that starts on plug-in and ordinary thermostats, against the **same weather** down to the cloud at 12:19. A saving computed any other way flatters itself. |
| **saved / …of it on the bill** | €2,23 against €3,42. The saving counts the battery life and the comfort the plan actually spent; the bill is the flattering number every other system quotes. |
| **…covered by the store** | `[A1 2.3]` in one number — kilowatt-hours the battery lent the controllable devices during the reduction, which never crossed the connection point and which the Festlegung therefore does not count. |
| **slowest reaction** | whether the household had to be *commanded* into the reduction or was **already below** it. Both satisfy `[A1 4.2]`; a record that cannot tell them apart reports a compliant quiet house as one that took minutes to react. |
| **minutes without a plan** | the arbiter drops a plan older than its tolerance. A planner re-solving more slowly than that leaves the house on the fallback for part of every cycle, silently, at about €1,50 a day. |
| **relief from § 14a was worth** | the shadow price of the network operator's own ceiling: what a kilowatt-hour of relief is worth to *this* household. Sixteen cents here, because the store lends the headroom `[A1 2.3]` allows; €3,93 on the same evening in a house with no store whose car would otherwise leave short. |

And the three forecast lines are the evidence for the three money lines. *90 % of
the model* is the box having found, from six weeks of its own metering, that this
roof delivers a tenth less than its geometry says — nothing told it. *CRPS* is
the standard probabilistic-forecast score, in watts. A day that scored zero is a
day the planner was shown the answer, and its saving is an upper bound nobody in
a real house can reach; the same day with `--perfect-foresight` saves €5,52.

## Where to start

| | |
|---|---|
| [Getting started](@/docs/getting-started.md) | clone it, run the checks, watch a January day with a § 14a reduction |
| [Architecture](@/docs/architecture.md) | three control planes, three cadences, one order of authority |
| [The grid rules](@/docs/grid-rules.md) | § 14a, § 9 EEG, Modul 3, MiSpeL, § 42c — as code, with the citation for every number |
| [The planner](@/docs/optimizer.md) | the receding-horizon MILP, and what a kilowatt-hour is worth per device |
| [Forecasting](@/docs/forecasting.md) | what the box believes about tomorrow, and what being wrong costs |
| [Flexibility](@/docs/flexibility.md) | S2 / EN 50491-12-2 as the internal model |
