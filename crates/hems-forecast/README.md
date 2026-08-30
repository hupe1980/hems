# hems-forecast

Forecasts, and how much to trust them — for
[hems](https://github.com/hupe1980/hems).

A photovoltaic forecast has two halves that fail differently. **The geometry is
exact, deterministic and free**: where the sun stands over a given roof at a
given minute needs no weather service and no internet, and it is the same next
January as it was last January. The weather is neither. And a third thing fails
differently again — **the roof itself**, which delivers less than its datasheet
for reasons nobody wrote down: a tree that shades the east string until ten, a
chimney, three years of pollen, a module tolerance.

So hems computes the geometry itself — solar position, clear-sky irradiance,
transposition onto the module plane, cell-temperature derating, inverter
clipping — treats a cloud forecast as a multiplier on top, and then **learns the
rest from the meter**. That last step is the difference between a photovoltaic
*model* and a photovoltaic *forecast*, and it is what a box can do that a weather
service cannot.

```text
  roof, as the box learned it        90 % of the model
  production forecast, CRPS          65 W (93 % covered)
```

- ☀️ **Solar geometry and a physical array model**, no service required.
- 🔍 **An online residual corrector** — multiplicative, bucketed by local hour,
  exponentially weighted over about a fortnight of observations — that finds what
  this roof actually delivers, and whose *measured dispersion* becomes the width
  of the band.
- 📈 **Load profiles by day type** from the household's own quarter hours, with
  empirical quantiles widened for a thin cell: the 10th percentile of five
  observations is the smallest of the five, which is systematically inside the
  true one, and a planner told the household is more predictable than it is
  spends a battery on the difference.
- 🚗 **Charging sessions by weekday** — when the car comes home, when it leaves,
  how empty. Deliberately not central: the arrival is taken *late*, the departure
  *early* and the energy *high*, because a plan is a commitment and each end is
  chosen to make it cheap to be wrong about.
- 🏠 **RC identification** of the building from its own indoor/outdoor/heat
  record, by a pattern search on the one-step error under the exact
  zero-order-hold step the planner uses — and a refusal where the record has no
  excitation in it, because a week in which the heating never changed says
  nothing about the response to heating.
- 🪫 **Naive fallbacks** — seasonal-naive and persistence, with a band that
  widens over the horizon — for the first morning and the Tuesday the WAN went
  down. Refusing to forecast is worse than forecasting badly.
- 🎯 **Calibration metrics** — pinball, coverage, bias, MAE and **CRPS**, in the
  units the forecasting literature compares models in — because a forecast nobody
  scores is a number nobody should act on, and because a score of zero means the
  forecaster was shown the answer.
- 🚫 **A model with no evidence looks like one.** An untrained corrector is the
  identity with a wide band; a weekday with fewer than three observed sessions
  produces no forecast at all.
- 🧊 **No I/O, no clock.** Every model is a pure function of a record, so a
  simulated season of forecasting is a unit test.

## License

MIT OR Apache-2.0
