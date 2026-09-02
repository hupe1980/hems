+++
title = "Agents"
description = "Every fleet service answers over the Model Context Protocol, read-only and authorising each caller as itself — and an advisory plane that proposes, and cannot act."
weight = 12
+++

An energy manager produces a great many exact answers: is this setpoint inside
the guard's bound, does this quarter hour settle, was this § 14a reduction
respected. Each is decided by code that can be read and tested, and each decides
what a household draws.

The questions an operator actually asks are a layer above those. *Of forty
breaches this week, does one cause account for most of them? What does the saving
on the dashboard rest on?* Those are correlations across many exact answers, and
this page is about the two things that make them answerable: a surface an agent
can read, and a plane that may propose and may not act.

<pre class="mermaid">
flowchart LR
  subgraph fleet["the fleet services"]
    T["tariffd"]
    F["forecastd"]
    HI["histd"]
    FL["fleetd"]
    O["obsd"]
  end
  MCP{{"<b>/mcp</b><br/>read-only<br/>each call as its own caller"}}
  T --- MCP
  F --- MCP
  HI --- MCP
  FL --- MCP
  O --- MCP
  MCP --> AG["<b>agentd</b><br/>specialists on a<br/>replayable journal"]
  AG -- "proposes" --> OP(["an operator"])
  AG -. "no path exists" .-x HOUSE(["a household's devices"])
</pre>

## The same numbers, and what they mean

Every **fleet** daemon mounts a Model Context Protocol server at `/mcp`, on the
port it already binds, over the state its REST routes already read. Two query paths that can disagree are two answers to one question, so there is
one cache and one summary behind both: `tariffd`'s `get_prices` and its
`/v1/prices` are the same read.

**The box has none**, and that is a decision rather than an unbuilt feature.
Nothing on a household network speaks the protocol; the state an agent would want
already sits in `histd` and `obsd`; and the one write on the box is the
household's own override, which an agent must never reach. A surface that
exposed it would let an agent move a household's energy, and one that did not
would be a read-only copy of `/v1/status`.

What the second surface adds is **prose**. A REST caller has the schema and the
docs; an agent has whatever the tool description says, and these are the numbers
easiest to quote wrongly:

| The number | What a tool has to say about it |
|---|---|
| an absent price slot | nobody published one. Not zero, and not free electricity |
| a negative quarter hour | wholesale. It changes the **feed-in** side under § 51 EEG, and not what consumption costs |
| a § 14a breach | a list with a site and a date. One household in ten thousand is an incident with a name; “99,99 %” is the same fact and reads as success |
| a saving | a mean over the days a saving can be computed from. Days run with the weather known in advance are excluded, because a figure including them is an upper bound no box reaches |
| a coverage figure | not a calibration below twenty independent days. Forecast error is correlated across a day, so ninety-six quarter hours of one Tuesday are close to one draw |

**Every call is authorised as its own caller.** A tool reads the credential on
the request that reached it and resolves it against the same credentials the
REST routes use, so a household's token reads its own site over `/mcp` exactly
as it would over REST, and an operator's reads the households in its tenant.

**Read-only, all of it.** An agent must never be able to move a household's
energy through this surface: that decision belongs to the arbiter, behind the
guard, on the box.

```toml
[mcp]
enabled = true
```

Off unless switched on. A daemon that holds a household's data takes **no** token
of its own here: one shared token would answer every caller as the same
principal, which is the whole thing per-caller authorisation exists to prevent.
One configured with no credentials at all refuses to start rather than refusing
every call identically — a surface that denies everything looks like a caller's
mistake for as long as nobody looks.

`tariffd` and `forecastd` may set a `token`. It is a plain gate in front of an
operator's own upstream quota and carries no authority, because a published
auction result and public weather are nobody's data and there is no household to
authorise for.

## What an agent may hold

A principal carries a set of dotted **capability** patterns and a **site scope**,
and both narrow under delegation — which is what lets an agent hold strictly less
than whoever it acts for. [Security](@/docs/security.md) has the model; what
matters here is the line it draws:

| Capability | An agent |
|---|---|
| `hems.record.read` | **holds it** — the § 14a evidence a Nachweis is built from |
| `hems.fleet.read` | **holds it** — the aggregate, which is what a population question is |
| `hems.export.read` | **never** — the Data Act Article 4 export is when the shower ran and which fortnight nobody was in, and Article 4 is a right of the *user* |
| `hems.record.write` | **never** — and there is no constructor here that could grant it |

## `agentd` — it proposes, and it cannot act

Two specialists, each answering one of those population questions. Both are pure
computation over days the fleet already holds; neither invents a number.

| Specialist | What it notices |
|---|---|
| `compliance-triage` | whether most of a week's § 14a breaches were on boxes that also spent time with no plan — which points at the planner's inputs rather than at the devices that overshot. And below-minimum commands grouped **by date**, because one command reaching many households is one network operator's mistake, not many households' bad luck |
| `saving-provenance` | that the saving rests on three modelled days while forty from real boxes are excluded; that most days on record were run with the weather known in advance; that the coverage figure is sixteen episodes short of being a calibration |

**Advisory by construction, not by policy.** Two things make that a property:

- `Advice` is a **leaf type**. It has no method returning a setpoint, a plan or
  an override, and nothing in the workspace consumes one — so the guarantee is
  checked by looking at what `Advice` can become, which is nothing.
- An agent's authority is derived by `attenuate`, which **refuses to widen**. It
  holds `hems.record.read` and `hems.fleet.read` and nothing else — in
  particular not `hems.export.read`, because the Data Act export is when the
  shower ran and which fortnight nobody was in, and Article 4 is a right of the
  *user*.

**Findings are ranked by a quantity, never by a score.** "High, medium, low"
invents a scale; households, minutes and days are the workspace's own units. Two
findings in different units are grouped rather than compared, because there is no
exchange rate between a household and a minute that this daemon may invent.

The specialists are pure functions. They run on
[agentplane](https://github.com/hupe1980/agentplane) anyway, because what the
runtime provides is not inference: the run, its input and its answer go into an
append-only hash-chained log, and a replay re-executes the logic while reading
each effect back. *"Why did the queue say that in March"* becomes a replay
rather than an argument — and for a pure function the replay is exact.

```toml
# services/agentd/agentd.example.toml — the annotated starting point,
# parsed by a test, so an example that has drifted fails the build.
journal = "/var/lib/agentd/journal.redb"
tenant  = "*"
```

The journal is the **plan of record**, so a path on an ephemeral filesystem is a
log that answers nothing. `tenant` is which operator's households the specialists
may read: `"*"` is every household this deployment knows, which is right for a
single tenant and is a cross-tenant read in any other — so it is written down
rather than being what happens when a field is missing.

```console
$ just agent-demo
```

