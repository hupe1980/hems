+++
title = "Forecasting, and being wrong"
description = "What the box believes about tomorrow, how it learned it, and what it costs to be wrong about it — the number no other energy manager publishes."
weight = 5
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
| January, § 14a reduction, 20 kWh of charging to place | **€2,09** | €5,25 | 60 % |
| January evening, car arrives *as* the reduction starts | **€2,51** | €4,93 | 49 % |
| June, more sun than the house can use | **€8,61** | €8,91 | 3 % |
| May, § 9 EEG cap, no car | **€1,31** | €1,53 | 14 % |

The shape of that table is a result rather than noise. Where the surplus lasts
all day the plan has slack and being wrong costs nothing. Where a large charging
session has to be placed into the cheap hours **around** a network operator's
reduction, more than half the headline saving was knowledge nobody has.

Any energy manager quoting a saving without saying which of the two it measured
is quoting the second one.

## The models

Everything is a pure function of a record. Nothing here reads a clock, opens a
socket or holds a model file, which is what lets a whole simulated season of
forecasting run as a unit test.

| Module | Predicts | From |
|---|---|---|
| `solar` | what the roof would produce under a clear sky | geometry, tilt, azimuth, the inverter's limit |
| `residual` | what it *will* produce | the same roof's own history against that model |
| `load` | the household's uncontrolled draw | its own quarter hours, by day type |
| `session` | when the car comes home and how empty | its own charging sessions, by weekday |
| `building` | which house this is | indoor and outdoor temperature against the heat put in |
| `naive` | any of them, badly, with nothing | the last day, or the last hour |
| `metrics` | nothing — it scores the rest | pinball, coverage, bias, CRPS |

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

## What this measurement is not

Two limits, stated here rather than discovered by somebody else.

**A calibration figure from one day is not a calibration figure**, and the type
now says so. Forecast error is correlated across a day, so a single realisation
lands mostly inside or mostly outside its own band — ninety-six slots of one
Tuesday are one draw wearing ninety-six hats. `Calibration` therefore carries an
**episode** count as well as a sample count, `is_well_calibrated` asks for twenty
*days*, and a test pins that the reference day cannot claim to be one whatever its
coverage. The days themselves belong in `obsd`.

`hemsd backtest --day summer --days 20` is what produces the days: the same day
under twenty seeded weathers, each an episode, merged. It says the bands are the
width they claim to be — 80 % coverage on the January day, 75 % on the June one,
against a nominal 80 %.

**A score whose denominator is the night cannot fail.** The number that used to
stand here was 93 % against a nominal 80 %, and it was recorded as a defect in the
band. It was arithmetic about how long a January night is: the score counted every
quarter hour of the day, and in a dark one the model forecasts nothing and nothing
happens — `0 [0 … 0]` against `0`, trivially inside its own band and trivially
zero loss. Sixty-four such pairs in a denominator of ninety-six put a floor of
67 % under the coverage figure however wrong the forecast was. Scored only where
there was something to forecast, the same day covers 81 %.

The test that should have caught it asserted the defect — *"every quarter hour of
the day should have been scored"*. That is why the day report now prints
`81 % of 32 lit` rather than `93 % covered`: how much of a day a score is about
belongs on the line with the score.

**The width of a band is a separate question from its middle.** Once the night
came out, the June day *was* over-wide — and for a reason the old figure had
hidden: sixty-six of its sixty-seven daylight quarter hours were sitting exactly
on a constant width floor. A band that is the same ±12 % all day is not an
uncertainty estimate. Each hour bucket now carries one multiplier per tail, moved
by its own outcomes so that a tenth of them fall outside each side whatever shape
the residual distribution has — adaptive conformal inference, two multiplications
per observation — and the band comes out asymmetric, which is right: a roof can
fall a long way below the clear-sky model and cannot rise far above it. The June
day's CRPS fell from 84 W to 67 W and the capped day's from 137 W to 27 W, with
no saving figure moving, because a deterministic plan reads only the median.

**The box's history is generated by the same process the day is.** Six weeks of
metering, produced by the same simulator, means the forecasts are scored against
a world whose statistics they were fitted to. That is the friendliest possible
test — and it still leaves 60 % of the winter saving on the table. A real box
faces a distribution that shifts: a season, a new tenant, a roof that gets
cleaned. The field number is worse than this one and never better, which is the
safe direction for a claim, and the reason any figure a customer is ever shown
should come from a back-test the model was not fitted to.
