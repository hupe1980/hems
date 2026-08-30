# hems-optimizer

The planner of [hems](https://github.com/hupe1980/hems): a receding-horizon
mixed-integer linear program over quarter-hour slots that **prices battery wear
instead of hiding it**.

A cost-only planner cycles a battery for a spread that does not cover the damage;
measured, the hidden degradation can exceed the energy saving by an order of
magnitude. hems charges throughput in euros per kilowatt-hour, half on each leg,
so a full cycle pays it once.

- 💶 **One currency.** Carbon (€/kg), self-sufficiency (€/kWh imported), comfort
  (€ per kelvin-hour) and curtailment are *prices*, so the terms can honestly be
  added up. An objective that swapped the energy price for grams of CO₂ while
  leaving wear in euros would be minimising a sum of two currencies.
- 🔌 **A charge point is off, or above 6 A.** Semi-continuous, because below the
  minimum of IEC 61851 a wallbox is not charging slowly — it is idle, and a plan
  that trickles 500 W into it delivers nothing.
- 🏠 **The building is a battery.** A 2R2C thermal model with a priced comfort
  band and its own terminal value; the fabric usually stores several times what
  the household battery does, and it is free.
- 🚿 **So is the hot water.** A linear store — heat above the lowest acceptable
  temperature, with a leak and a draw, which is S2's own view of a tank — and a
  cold shower is priced rather than forbidden, for the same reason the charging
  deadline is.
- ⚖️ **Grid rules are hard constraints, per slot** — § 14a on the netzwirksamer
  Leistungsbezug (a discharging store raises it; a ninety-minute reduction is not
  a forty-eight-hour one), § 9 EEG on feed-in, the connection on import.
- 📉 **An honest baseline.** The plan reports its cost *and* what the same
  horizon costs delivering the same service with none of the decisions, so a
  saving is a subtraction rather than a claim.
- 💰 **A shadow price per asset, and one for the network operator's limit.** A
  mixed-integer program has no duals, so the model is solved a second time with
  the discrete decisions pinned; the dual of each store's own state equation *is*
  what a kilowatt-hour held there is worth to this household, with the departure
  time, the comfort band, the wear and tomorrow's reduction already in it. That
  is what weights the guard's allocation — one marginal value per *slot* weights
  nothing. The dual of the § 14a ceiling comes with it, and it is the honest
  answer to "what is your flexibility worth": €3,93/kWh on a household whose car
  will otherwise leave short, €0,16 on the same one with a battery to lend it
  headroom.
- ⚙️ `good_lp` behind `microlp` (default, pure Rust — no C++ toolchain) or
  `highs` (production), with a relative gap and a time budget. The **dual pass
  always runs on Clarabel**, which is also pure Rust, so there is no backend
  split: a plan does not mean different things on two builds of the same
  software.

Two things a model has to get right for a dual to mean anything, and neither can
fail a test of the primal. **No redundant row may be left in it**: `g⁺ ≤ load +
b⁺ + ev + h + w` is implied by the energy balance, so it constrains nothing — and
it is tight whenever the household is not exporting, which gives it a free dual
that absorbs the energy balance's and makes the price of a kilowatt-hour
*indeterminate*. And **conditioning**: watts against euros is a 10⁸ condition
number, which simplex never notices because it only compares coefficients with
each other, and which an interior-point method faithfully returns as noise.

## License

MIT OR Apache-2.0
