# obsd

What the fleet is actually doing, for
[hems](https://github.com/hupe1980/hems).

Every box reports its day as a `DayKpis`. This service keeps them and answers the
three questions a fleet operator has — and they are **not the same shape**, which
is the design.

| Question | Shape | Why it matters |
|---|---|---|
| “How are we doing?” | an **average** — saving, self-sufficiency, forecast scores | an average over enough days is the only honest way to quote any of them |
| “Who is broken?” | a **count** | one household in ten thousand that failed to respect a § 14a reduction is an incident with a name and a date; the same fact as “99,99 % compliance” reads as success |
| “Is the forecast honest?” | **neither** | forecast error is correlated across a day, so one day's coverage figure is a coin toss reported to three significant figures — only the merge of many days can be compared with the 80 % the band promises |

So the summary carries failures as a **list of sites** and never as a rate, and a
box reports its scores rather than judging them.

## A day only arrives signed

The collector holds the list of households that did *not* respect a network
operator's reduction, so an unauthenticated write to it can put a compliant site
on that list or take a breach off it.

A day therefore reaches it only as a **signed CloudEvent** — Standard Webhooks
over the message id, the timestamp and the exact bytes, so a captured request
cannot be replayed, re-attributed or edited — and over TLS, which is a different
guarantee the box needs as well: the signature says the report is the one this box
sent, TLS says nobody read it on the way. Plain `http` is allowed only to a
loopback address.

An `obsd` with **no secret configured refuses everything** rather than accepting
everything: the deployment where somebody forgot it is the one nobody would
notice.

| | |
|---|---|
| `POST /v1/days` | a box reports a day (signed) |
| `GET /v1/fleet` | the summary, with breaches as a list |
| `GET /v1/sites/{site}` | one household's recent days |

## It holds a window, not a history

The *record* is `histd`'s: the § 14a evidence `[A1 7.3]` keeps for two years and
the quarter-hour registers a settlement is computed from. What lives here is
derived, bounded and rebuildable, and losing it costs a dashboard rather than a
Nachweis.

## Try it

```console
$ just fleet-demo
```

One box reporting a real simulated day into a fleet view on loopback.

## License

MIT OR Apache-2.0
