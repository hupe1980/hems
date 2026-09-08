+++
title = "Forecasting, and being wrong"
description = "What the box believes about tomorrow, how it learned it, and what it costs to be wrong about it — the number no other energy manager publishes."
weight = 7
+++

## A forecast a controller can be judged against

A saving figure is a statement about a **controller**. A controller judged
against a forecast it was handed as fact is not being judged at all — and a
simulator whose forecast *is* the series it is about to run cannot tell a good
planner from one that was shown the answer. Every number it produces is an upper
bound no box in a real house can reach, and the mechanism built to absorb
forecast error — the arbiter tracking the plan's energy rather than its setpoint
— is never once exercised, because the error is identically zero.

So hems separates the two.

## The day that happens, and the day that was forecast

The simulator runs a seeded **realisation**; the planner is given only what a box
could actually have known.

| The planner is told | Where it comes from |
|---|---|
| production | the geometric model, corrected by what *this* roof has actually been delivering, with the band its own measured dispersion earns |
| household load | this household's own quarter hours, by day type, with empirical quantiles |
| the car | its own charging sessions by weekday — until the cable goes in and it becomes a fact |
| outdoor temperature | the diurnal shape, without the day's own error |
| hot water | the household's usual draw, not this morning's |

| The house does | |
|---|---|
| production | clear sky × (1 − *this day's* cloud) × the 92 % this roof actually delivers |
| household load | the profile × this day's noise |
| outdoor temperature | the shape plus a slow error |
| hot water | this morning's actual shower |

The noise is **correlated**, over four octaves at about four hours, one hour, a
quarter hour and four minutes — a front, a haze, a cumulus field, one cloud.
White noise would be the wrong error entirely: it averages out inside a quarter
hour, so a planner working in quarter hours never sees it and the arbiter has
nothing to catch up on. The fastest octave is the one that earns its keep,
because it moves *below* the planner's own grain.

And all of it is a pure function of `(seed, instant)` — no generator state, no
iteration order to depend on — so the day still replays to the last euro cent.
Determinism was never the thing that had to go; being *told the answer* was.

<pre class="mermaid">
flowchart LR
  G["solar geometry<br/>position · clear-sky · Erbs + HDKR<br/><i>exact, free, no service</i>"] --> M
  W["cloud cover<br/>ICON-D2 via forecastd"] --> M["clear sky × (1 − cloud)"]
  M --> C["<b>residual corrector</b><br/>multiplicative · by local hour<br/>~a fortnight, exponentially weighted"]
  H["this roof's own meter"] --> C
  C --> F["a median and a band<br/>whose width is the measured dispersion"]
  F --> P["the planner"]
  F --> S["CRPS · coverage,<br/>scored beside the money"]
</pre>

The three halves fail differently, which is why they are separated. **The
geometry is exact, deterministic and free** — where the sun stands over a given
roof at a given minute needs no weather service and is the same next January as
it was last. The weather is neither. And the **roof itself** delivers less than
its datasheet for reasons nobody wrote down.

## The soiling nobody mentions

The simulated roof delivers **92 %** of what its geometry says. Soiling, a little
shading, mismatch, modules that were never quite at their datasheet: an ordinary
German roof after three years. Nothing tells the model, and that is the point —
the residual corrector has to find it, exactly as it would in the field.

```console
  roof, as the box learned it        90 % of the model
```

A figure sitting at exactly 100 % would mean the corrector is not being fed. The
correction is **multiplicative** (the error scales with the irradiance; an
additive correction learned at noon would invent 400 W of production at
midnight), **bucketed by local hour** (that is where the shade is), and
**exponentially weighted** over about a fortnight of observations, so a roof that
has just been cleaned is not held to last summer.

The width of the band is the dispersion the corrector has actually measured — not
a constant somebody chose — with a floor under it, because a roof that has
behaved identically for ten days has not made the weather deterministic.

## Scoring the forecast, beside the money

Every day prints what its forecasts were worth:

```console
  production forecast, CRPS          192 W (81 % of 32 lit)
  load forecast, CRPS                18 W (85 % covered)
```

**CRPS** is the continuous ranked probability score — the number the forecasting
literature compares models on, in the unit of the quantity, so a claim about
these forecasts can be put beside a published one. The percentage is how often
the outcome landed inside the 10–90 band, which should be near 80 — and `of 32
lit` is how many quarter hours it is a percentage *of*. A production score is
about the part of the day the sun was up; the other sixty-four slots of a January
day are a band of nothing against an outcome of nothing, which is midnight rather
than a forecast that came true.

A day whose CRPS is zero is a day the planner was shown the answer, so there is a
test whose only purpose is to **fail if the simulator gets too good**: the
reference day's forecasts must score above zero, the residual corrector must find
the 8 % the roof is down by, and knowing the future must be worth at least a euro
more than not knowing it. A test that only checks "the day saves money" passes
either way.

## What being wrong costs

```console
$ cargo run -p hemsd -- simulate --day winter --perfect-foresight
```

| Day | Saved | Saved, knowing the future | The premium |
|---|---|---|---|
| January, § 14a reduction, 20 kWh of charging to place | **€1,94** | €4,55 | 57 % |
| January evening, car arrives *as* the reduction starts | **€2,57** | €4,89 | 47 % |
| June, more sun than the house can use | **€8,84** | €9,07 | 3 % |
| May, § 9 EEG cap, no car | **€0,89** | €0,70 | **−27 %** |

The shape of that table is a result rather than noise. Where the surplus lasts
all day the plan has slack and being wrong costs nothing. Where a large charging
session has to be placed into the cheap hours **around** a network operator's
reduction, more than half the headline saving was knowledge nobody has.

The last row is *negative*, and that is reported rather than smoothed. Nothing on
the capped day turns on knowing the weather: it is a statutory ceiling and a
store that either absorbs the clipping or does not, so both plans make the same
decisions and what is left is one realisation's worth of noise, which can fall
either way.

Any energy manager quoting a saving without saying which of the two it measured
is quoting the second one.

## The models

Everything is a pure function of a record. Nothing here reads a clock, opens a
socket or holds a model file, which is what lets a whole simulated season of
forecasting run as a unit test. On a real box the two models that learn — the
residual corrector and the load profile — are taught on the quarter-hour
boundary and kept in `hemsd`'s own store, so a reboot does not cost a fortnight
of them.

| Module | Predicts | From |
|---|---|---|
| `solar` | what any plane on the house receives, and what the roof makes of it | geometry, a global horizontal irradiance — the clear-sky model or `forecastd`'s — split into beam and diffuse by **Erbs** and transposed by **HDKR**, this roof's own tilt and azimuth from the configuration, and the inverter's limit |
| `residual` | what it *will* produce | the same roof's own history against that model |
| `load` | the household's uncontrolled draw | its own quarter hours, by day type |
| `session` | when the car comes home and how empty | its own charging sessions, by weekday |
| `building` | which house this is — its fabric, the sun it lets in and the heat its occupants make | indoor and outdoor temperature and the irradiance on its windows, against the heat put in, starting from the archetype the installer picked |
| `naive` | any of them, badly, with almost nothing | one reading, on a box that has no profile yet |
| `metrics` | nothing — it scores the rest | pinball, coverage, bias, CRPS |

Two of them start from something the installer typed rather than from a constant,
and the distinction matters differently in each. The roof's **azimuth** is not
something `residual` can learn its way out of: the corrector is a multiplicative
level per hour of the local day, bounded, and learned separately per season, so
it absorbs soiling and the tree in front of the east string in a fortnight and
takes seasons to absorb a wrong compass bearing. The **building** archetype is
only a prior, and `building::identify` replaces it with a fit from the house's own
thermometer as soon as it has one — but the fabric capacity spans a factor of five
across the archetypes and decides whether pre-heating into a cheap hour pays at
all, so the weeks before the fit are not free.

The fit has six parameters, not four. Two of them are the heat the house gets for
**nothing**: an effective *solar aperture*, and the waste heat of the people and
appliances inside. They are not a refinement. The average German single-family
archetype loses about 2,7 kW at 5 °C outdoors; a clear March noon on a
south-facing 4,5 m² aperture is 2,5 kW of it, and the internal gains another 0,45
— so on exactly the days a heating plan is worth making, the free heat *exceeds
the demand*. A model without it runs the compressor into a room the sun is
already warming, and a fit without it has nowhere to put the sun but the
insulation, so it reports a better-insulated house in June than in December.

The aperture is driven by the irradiance on the **vertical** plane the windows
are in, which is why `facade_azimuth_deg` is configuration: at 52° north a
vertical south plane sees 1,6 times the horizontal irradiance at a December noon
and half of it at a June one, so an aperture fitted against the horizontal would
be a different number in every season. And the fit **will not walk a parameter
the record cannot constrain** — a fortnight with no daylight in it still
identifies the fabric and leaves the aperture where the prior put it, because a
free parameter that the data says nothing about does not stay where it started:
it absorbs whatever else the model gets wrong.

Three of them are asymmetric on purpose:

**A session forecast is not central.** The arrival is taken at the *late*
quantile, the departure at the *early* one and the energy at the *high* one. A
plan is a commitment the arbiter has to be able to keep and a shortfall is priced
at €5/kWh, so the forecast is not trying to be right on average — it is trying to
make the plan that follows it cheap to be wrong about.

**A small sample is widened, not trusted.** The empirical 10th percentile of five
observations is the smallest of the five, which is systematically *inside* the
true one — and that direction is the dangerous one, because a planner told the
household is more predictable than it is spends a battery on the difference. A
Sunday backed by three observed Sundays produced a band the outcome fell inside
41 % of the time against the 80 % it promises. The observed half-width is
inflated by `√((n+1)/(n−1))`, which is large where history is thin and vanishes
as it grows, and never narrows a band.

**A model with no evidence looks like one.** An untrained corrector returns the
identity ratio and a wide prior band. A weekday with fewer than three observed
charging sessions returns **no** forecast at all, and the planner then reserves
nothing rather than reserving the evening's cheap hours for a car that may not
come.

**A cell with no history says so by widening.** The profile keys its cells on
`(day type, quarter hour)`, and a box's first Saturday has none — nor does its
first public holiday, nor any quarter hour it has not been metered through. An
empty cell borrows the same quarter hour from the day types the household *has*
been seen on, widened by half because a Saturday is not a Monday; a quarter hour
seen on no day at all falls back to the household's own level, widened twice.
Answering zero would be the one thing a forecast must never do — say *the house
will use nothing, and I am sure* — and a plan given that defers every flexible
kilowatt-hour into hours it believes are free.

Only a profile that has learned nothing at all has nothing to say, and the box
does not ask one: it uses persistence off its own meter on its first morning, and
refuses to plan if it cannot read its own connection point.

## What this measurement is not

Two limits, stated here rather than discovered by somebody else.

**A calibration figure from one day is not a calibration figure**, and the type
now says so. Forecast error is correlated across a day, so a single realisation
lands mostly inside or mostly outside its own band — ninety-six slots of one
Tuesday are one draw wearing ninety-six hats. `Calibration` therefore carries an
**episode** count as well as a sample count, `is_well_calibrated` asks for twenty
*days*, and a test pins that the reference day cannot claim to be one whatever its
coverage. The days themselves belong in `obsd`.

`hemsd backtest --day summer --days 20` — `just backtest summer 20`, and see
[simulation](@/docs/simulation.md#one-day-cannot-answer-some-questions) — is what
produces the days: the same day
under twenty seeded weathers, each an episode, merged. It says the bands are the
width they claim to be — 80 % coverage on the January day, 75 % on the June one,
against a nominal 80 %.

**A score whose denominator is the night cannot fail.** Counting every quarter
hour of a January day puts a floor of 67 % under any coverage figure, however
wrong the forecast was: in a dark quarter hour the model forecasts nothing and
nothing happens — `0 [0 … 0]` against `0`, trivially inside its own band and
trivially zero loss — and sixty-four of the day's ninety-six slots are like that.

So a production score is computed only where there was something to forecast, and
the report prints **how much of the day it is about** on the same line:
`81 % of 32 lit` rather than a bare percentage. A denominator that cannot fail
belongs beside the number it flatters.

**The width of a band is a separate question from its middle.** A band that is the
same ±12 % all day is a constant, not an uncertainty estimate, and a coverage
figure computed over the night hides that it is one.

So each hour bucket carries **one multiplier per tail**, moved by its own outcomes
so that a tenth of them fall outside each side whatever shape the residual
distribution has — adaptive conformal inference, two multiplications per
observation. The band comes out **asymmetric**, which is right: a roof can fall a
long way below the clear-sky model and cannot rise far above it. It is worth 67 W
of CRPS on the June day and 27 W on the capped one, and it moves no saving figure
at all, because a deterministic plan reads only the median.

**The box's history is generated by the same process the day is.** Six weeks of
metering, produced by the same simulator, means the forecasts are scored against
a world whose statistics they were fitted to. That is the friendliest possible
test — and it still leaves 60 % of the winter saving on the table. A real box
faces a distribution that shifts: a season, a new tenant, a roof that gets
cleaned. The field number is worse than this one and never better, which is the
safe direction for a claim, and the reason any figure a customer is ever shown
should come from a back-test the model was not fitted to.
