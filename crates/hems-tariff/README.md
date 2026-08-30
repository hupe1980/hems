# hems-tariff

What a kilowatt-hour costs a German household, quarter hour by quarter hour —
for [hems](https://github.com/hupe1980/hems).

A bill is a stack and every layer moves for its own reasons: the energy price
with the market (every 15 minutes since 01.10.2025), the network charge with the
time of day if Modul 3 is chosen, levies and taxes with the calendar year, and
feed-in with the support regime and whether the price went negative.

- 🧱 **The layers stay separate** all the way through and are only summed at the
  edge, because the optimiser wants one number per direction and the household
  wants to see the stack.
- 💯 **Money is exact.** `rust_decimal` in cents per kilowatt-hour; `f64` only
  where a solver needs it, converted once.
- ⚡ **§ 51 EEG** turns on the sign of the *market* price, not of what the
  household pays — so the raw spot price is kept as well as the retail one.
- 🧮 **A Modul advisor** prices a household's own quarter-hourly history under
  Modul 1, 2 and 3 and computes the Modul 2 break-even. Nobody selling a tariff
  can be relied on to run that comparison in the customer's favour; a household's
  own energy manager can, and it is holding the only data that answers it. It
  reports a **threshold in kilowatt-hours a year**, not a projected saving: one
  day is not a year, and multiplying a January Thursday with a car on the cable
  by 365 tells every household with an electric vehicle that it is losing four
  figures.
- 📥 **Parsers for what the five sources actually publish** — ENTSO-E A44 XML,
  SMARD, aWATTar, Tibber and Energy-Charts — as pure functions: a `&str` in, a
  quarter-hourly series out, so every one is tested against a captured response
  on a machine with no network. Three things in them are decisions rather than
  plumbing: the €/MWh↔ct/kWh factor of ten lives in exactly one place; an hourly
  price **expands** to four quarter hours and is never averaged into one; and a
  position is resolved as an **instant**, so the 92-quarter-hour day in March and
  the 100-quarter-hour day in October both come out right. Tibber's *gross*
  consumer price carries a `PriceBasis` that refuses to be used as a wholesale
  one — adding the levies on top of a price that already contains them costs a
  household about double.

## License

MIT OR Apache-2.0
