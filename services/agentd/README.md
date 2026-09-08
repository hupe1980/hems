# agentd

The advisory plane for [hems](https://github.com/hupe1980/hems).

Every other crate answers a question about **one** thing: is this setpoint inside
the guard's bound, does this quarter hour settle, was this § 14a reduction
respected. Those answers are exact, and they are the ones that decide what a
household draws.

None of them answers the question an operator has in the morning, which is about
a **population**: of forty § 14a breaches this week, does one cause account for
most of them; of a fleet's days, how many stand behind the saving on the
dashboard. Those are correlations across many exact answers.

## It proposes, and it cannot act

Two things make that structural rather than a promise:

- **`Advice` is a leaf type.** No method returns a setpoint, a plan or an
  override, nothing in the workspace consumes one, and there is no route here
  that writes. A reviewer checks the guarantee by looking at what `Advice` can
  become, which is nothing.
- **The authority cannot widen.** A specialist runs under one derived by
  `Authority::attenuate`; it holds `hems.record.read` and `hems.fleet.read` and
  nothing else — in particular not `hems.export.read`, because the Data Act
  Article 4 export is a right of the *user*, and an advisory agent is not one.

## The specialists

| Name | What it notices |
|---|---|
| `compliance-triage` | whether most of a week's § 14a breaches were on boxes that also spent time with no plan — the intersection of two of `obsd`'s lists on `(site, date)`. Below-minimum commands grouped by **date**: one command reaching many households is one network operator's mistake. Roofs over the § 9 EEG ceiling grouped by **site**: that ceiling is a fixed fraction of installed capacity and does not move, so a plant crossing it repeatedly is misconfigured rather than unlucky |
| `saving-provenance` | that the saving rests on two modelled days while a hundred and eighty from real boxes are excluded; that most days on record were run with the weather known in advance; that the coverage figure is short of the twenty independent days a calibration needs |

Findings are ranked by a **quantity** — households, minutes, days — never by an
invented severity. Households are counted and never averaged. Two findings in
different units are grouped rather than compared.

## A cadence, not a subscription

Both specialists answer a question about a population over a window, so a
per-event trigger would be one review per reported day: ten thousand a day on a
fleet of ten thousand households, each reaching very nearly the previous answer.

Instead one `Summary` is read from `obsd`'s `/v1/fleet` every few hours and
handed to **every** specialist — so two findings an operator reads together are
about one set of days. That summary is the run's input and is labelled untrusted,
because it crossed a socket; the run is journaled, so a finding replays to the
summary it was drawn from rather than to whatever `obsd` says now.

| | |
|---|---|
| `GET /v1/advice` | the queue: what each specialist last concluded, when, over how many days, and the run that produced it |
| `/mcp` | the same queue for an agent — `list_advice`, `list_specialists` |

Both ask for `hems.fleet.read` **by name**: a box's own token reaches its own
household and must not reach a list of the households that failed to respect a
network operator's reduction.

## Try it

```console
$ just agent-demo
```

## Configuration

`agentd.example.toml` is the annotated starting point, and a test parses it — so
an example that has drifted from the struct it documents fails the build.
