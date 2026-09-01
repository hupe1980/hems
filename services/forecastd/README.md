# forecastd

What the sky is going to do, for [hems](https://github.com/hupe1980/hems).

`hems-forecast::weather` parses ICON-D2 out of Open-Meteo and
`hems-forecast::solar` turns irradiance into what a given array would make. Both
are pure functions. This is the process that fetches, caches and serves them.

## It serves weather, not a forecast

The distinction is the whole architecture. A weather model knows about the sky and
knows nothing about the tree that shades the east string, the chimney, or the
fact that this roof has not been cleaned since 2023.

Turning modelled irradiance into a *forecast* is `hems-forecast::residual`'s job
and it happens **on the box**, from that box's own metering — because the
correction is a property of one roof and cannot be learned centrally without the
meter that sees it. A fleet service that shipped finished production forecasts
would be a fleet service that had to know every roof.

| | |
|---|---|
| `GET /v1/weather/{location}` | irradiance and temperature, quarter-hourly |
| `GET /v1/production/{location}?kwp=9.8` | the *geometric* model applied to a nameplate — a starting point, not a forecast |
| `GET /livez`, `/readyz`, `/version` | as every daemon here |

**Unauthenticated on purpose**: irradiance over a location is public weather.

## Why the cache is the same shape as `tariffd`'s

For the same reason: a box with no WAN still has to plan. A weather run is good
for hours, so an outage that lasts one is invisible — and readiness is computed
from how much of the horizon is **covered** rather than from when the last request
returned.

## License

MIT OR Apache-2.0
