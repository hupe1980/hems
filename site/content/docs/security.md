+++
title = "Security and supply chain"
description = "The box trusts a key it was built with, not a server. Secrets are references, authorisation is per site, and every release ships an SBOM and provenance."
weight = 13
+++

A home energy manager holds a grid connection open, decides when a car charges,
and writes the record a network operator may later ask to see. Three things
follow, and the workspace is built around them rather than adding them later.

## The fleet is not the trust anchor

The Cyber Resilience Act (Regulation (EU) 2024/2847, Annex I Part I § 2(c) and
Part II) requires that a product with digital elements ships security updates and
that their **integrity is protected**. “The fleet server told us to” is not
integrity — it is a trust anchor that moves whenever DNS does.

So the thing a box trusts is a **public key it was built with**.

<pre class="mermaid">
sequenceDiagram
  participant B as hemsd (holds the public key)
  participant F as fleetd (holds signatures, never a key)
  participant A as artefact store
  B->>F: GET /v1/releases/hemsd
  F-->>B: manifest + Ed25519 signature
  Note over B: verify the signature<br/>against the built-in key
  B->>A: fetch the artefact
  Note over B: verify the digest<br/>against the manifest
  Note over B: only then install
</pre>

`fleetd` never holds the signing key. It holds signatures somebody else produced,
so a compromised `fleetd` can serve a manifest **no box will accept**. That is
what makes the fleet server ordinary infrastructure rather than the root of
trust.

### A configuration is the same question

A configuration document decides which assets the site has, what the comfort band
is, and where the box reports. A `fleetd` that could serve an arbitrary one would
be a trust anchor after all, and every sentence above would be a sentence about
the update channel only.

So `SignedConfig` is the release argument applied to the other thing a box pulls,
signed by the **same built-in key** — and `fleetd` holds the signature and never
a signing key there either.

### Verification is sans-I/O

`Release::verify` and `SignedConfig::verify` are pure functions of a manifest, a
signature and a key. Nothing there downloads, unpacks or installs — those are
I/O and they are the caller's. What this owns is the two questions that have a
right answer: *is this manifest from us*, and *is this the artefact it
describes*. “A tampered manifest is refused” is therefore a unit test rather than
a thing somebody tries once against a real server.

## A box has to be able to go back to the factory

The box's store holds the EEBUS identity a network operator's control box
reduces *this* household by, and two years of evidence about what it did. A
gateway that changed hands carrying either would let the previous household's
operator go on controlling the new one. So there is a **factory reset**: every
table in one transaction — a half-reset box has lost its evidence and kept an
identity somebody still trusts — leaving a box whose failsafe falls back to what
its parameter sheet declares.

It **refuses** while the fleet is still owed anything, because unforwarded
evidence exists only on the box and the discovery would come two years later.
The way through for a box that will never see a network again is named for what
it costs, so choosing it is a decision rather than a retry. The same operation is
one of the seven device-level EEBUS conformance procedures, and a test drives it.

## A credential in a configuration file is a credential in a repository

Configuration is read from a file and then the environment, which is right for a
poll interval and wrong for a credential: one in a configuration file is one in
an image, in a backup, and eventually in a repository — and no orchestrator
injects secrets that way.

So the **reference** is configured and the value is not:

```toml
[webhook_secrets]                                    # one key per household
haus-1 = ["env:HEMS_OBSD_SECRET_HAUS_1"]             # from the environment
haus-2 = ["file:/run/secrets/haus-2"]                # from a mounted file
haus-3 = ["whsec_literal"]                           # the secret itself
```

An unresolvable reference is an **error**, never a fallback to the literal. A
deployment signing with the string `file:/run/secrets/webhook` looks exactly like
one whose counterparty has started rejecting it, and it would be found weeks
later.

Every credential in the workspace goes through the same type — an ENTSO-E token,
a site's enrolment secret, an `obsd` webhook secret — and each is resolved
**once, at startup**, so a token cannot be re-read into a request already in
flight.

## Who is asking, and which household

Every fleet service answers questions about one household's electricity, and one
of the answers *is* the household: `histd`'s Data Act export is everything the
product generated — when the shower ran, when the car charged, which fortnight
nobody was in.

`fleetd` mints a 256-bit credential per box at enrolment. Presenting it answers
**who**; it does not answer **which site**, and a service that checks only the
first will hand box A the record of box B when box A asks for it. So authority is
two questions, and only one of them is about cryptography:

| Authority | May read | May write |
|---|---|---|
| `Site(id)` | that site | that site |
| `Operator` | any site's § 14a evidence — the Nachweis | nothing |

An operator's reach is deliberately not *everything*. A Nachweis is the record of
what the operator itself commanded and what the connection point drew, and it is
theirs to check. The Data Act export is the household's, under Article 4, and a
fleet token is not a household.

Two further properties, both of them the kind that is invisible until it is
missing:

- **Comparison is constant-time.** A token compared with `==` leaks where two
  differ, one byte of timing at a time.
- **An empty set accepts nothing.** A service configured with no tokens rejects
  everything rather than everyone: the deployment where somebody forgot the
  credentials is exactly the one nobody would notice.

## The box authenticates its own callers too

It holds the household's own electricity: `/v1/status` says what every device is
doing right now, `/v1/series` is the Data Act's local API over the measured
history, `/v1/overrides` decides what the arbiter wants, and `/s2/{asset}` lets
an external energy manager drive the house.

Every route it adds is behind one bearer token resolving to the same `Site(id)`
authority a fleet service would issue — the same model, not a second one written
for the edge. It is **one layer over the whole assembly** rather than a check in
each handler, which is the shape rather than the convenience: the alternative is
four call sites each spelling the test themselves, and one spelling it wrong.

**The box issues the token itself**, from the operating system's entropy, and
keeps it in its own store beside the EEBUS key. It is printed at start-up next to
the SKI — the two credentials a commissioning visit carries away — and
`hemsd run --check` issues both, so a dry run carries away the numbers the daemon
will serve with. Both alternatives fail in the field: a setting with a default is
a published credential, and a box that refuses to start is one an installer works
around by inventing a weak one. Secure-by-default here means *no insecure mode to
configure*, which is what the CRA asks of a product and EN 18031 of a
radio-equipped one. A deployment with its own credentials sets `[api] token`.

**An energy manager gets a credential of its own**, connected by name, listed and
withdrawn — the shape a Steuerbox already has through the SHIP trust store, and
for the same reason: a household that can see *that* something is driving its
battery but not *what* cannot revoke it. The token is shown once, the subject
carries the name so an audit line says which manager it was, and both credentials
are cleared by the factory reset.

It carries the household's capabilities **less the Data Act export**. An
aggregator drives devices; Article 4 is a right of the user, and the one-second
series says when they showered, cooked and went away. The gate enforces that,
because a capability granted and never checked is not a boundary.

`/livez`, `/readyz` and `/metrics` stay open, for the reason the next section
gives about the two open services: they carry no household data, and an
orchestrator that needed a household's credential to restart a crashed box would
be given it far too widely.

## Two services are open, and that is written down

`tariffd` serves published day-ahead auction results and `forecastd` serves
irradiance over a location. Neither carries household data, and both are
unauthenticated **on purpose**. Writing that down is what makes the difference
between the two pairs read as a decision rather than an oversight.

`fleetd`'s roster is not in that set. `/v1/fleet` says which households exist,
which build each is on and which are unreachable right now — an inventory of who
is worth attacking and which of them is unpatched — so it takes an operator's
token. The rest of `fleetd` is a box's own read against its own credential.

## Capabilities, and they narrow

A principal here carries three things: a credential that says **who**, a set of
**capabilities** that says what it may do, and a **site scope** that says which
households.

Capabilities are dotted patterns with two forms and no more — `hems.record.read`,
or the family `hems.record.*`. The grammar stops there because a delegate must
be provably no wider than its delegator, and containment has to be *decidable*:
regex and negation make it undecidable in general. It is deliberately the shape
[agentplane](https://github.com/hupe1980/agentplane) uses, because that is the
runtime the advisory agent will be built on, and its principals have to compose
with these rather than be translated into them.

| Capability | What it opens |
|---|---|
| `hems.record.read` | one household's § 14a evidence — what a Nachweis is built from |
| `hems.record.write` | writing that record. A box's own, and nothing else's |
| `hems.export.read` | the Data Act Article 4 export |
| `hems.fleet.read` | an answer about **every** household in scope — a summary, a roster |

The reason this is a set and not a role: an agent must be able to hold **less**
than whoever it acts for. An agent reading a Nachweis on an operator's behalf
should not inherit the roster, and a role delegated to an agent is still that
role. A capability set delegated to an agent is a subset, and the containment
check is what makes "no wider than its delegator" something the compiler helps
with rather than something a reviewer notices.

## The fleet is a verb, not a missing site

`hems.fleet.read` exists because the question *"may this caller read an answer
about every household"* had no name, so four call sites each decided it for
themselves — and one decided it wrong. `obsd`'s fleet summary asked
`may_read(site: None)`, and `Option::is_none_or` is `true` for `None`, so **any**
valid credential — including one household's own box token — read a summary
naming every household that failed to respect a network operator's reduction.

A box's credential does not hold that capability. An aggregate over every
household is not any one household's data, however wide that household's own
reach.

## Tenancy is a field on the credential

Every credential names the households it reaches: one site, a named tenant, or
the explicit `"*"`. `"*"` is right for a single-tenant deployment — a Stadtwerk
running hems for its own customers — and is a cross-tenant read in any other,
which is why it is written down rather than being what happens when a field is
missing. Aggregates are computed **within** the caller's scope rather than
filtered afterwards, so a count is a count of what that caller may see.

```toml
[tenants]
stadtwerke-nord = ["haus-1", "haus-2"]

[[operators]]
token  = "env:HEMS_HISTD_OPERATOR_NORD"
tenant = "stadtwerke-nord"
```

A credential naming a tenant nothing defines stops the daemon. Resolving it to
the empty set would start one that accepts the token and reads no household,
which at the other end looks like a permissions problem and is a typo.

## The agent surface authorises every call

Every fleet daemon can mount a read-only MCP surface, and each tool authorises
**the caller that reached it** against the same credentials the REST routes use
— so a token cannot reach over `/mcp` what the REST route would refuse it. See
[Agents](@/docs/agents.md).

The alternative is worth naming, because it looks reasonable and is not. A
surface given **one** authority at start-up — the daemon's own credential, say —
would answer every caller as that principal, so a deployment that configured an
operator's token would serve every household to anybody who could reach the
port. The authorisation model would be sound; the caller would simply never
reach it.

## The evidence record is what a signature protects

`obsd`'s collector holds the list of households that did *not* respect a network
operator's reduction. An unauthenticated write to it can put a compliant site on
that list or take a breach off it, so a day reaches it only as a **signed
CloudEvent** — Standard Webhooks over the message id, the timestamp and the exact
bytes, so a captured request cannot be replayed or edited.

**And each box signs with a key of its own.** A signature proves the bytes were
not edited by somebody without the key; it proves *who sent them* only if no one
else holds that key. Under one fleet-wide secret, any box could report a day
attributed to any household — so verification says *which* key signed, and a
report whose claimed site is not that key's site is refused. Two sites
configured with one key are refused at start-up, because "who signed this" would
have no answer.

TLS is the other half and it is a different guarantee: the signature says the
report is the one this box sent and has not been edited; TLS says nobody read it
on the way.

**Plain `http` only to a loopback address**, refused at start-up rather than
warned about, on every outbound endpoint a daemon is configured with — a warning
on a box nobody is watching is a warning nobody reads. It is one check in the
shell every daemon shares, not seven checks each call site has to remember.

### Where the trust anchors come from

`reqwest` on `rustls`/`aws-lc-rs`, and the anchors are configuration rather than
a default nobody looked at:

| `[service.http] tls_roots` | For | The trade |
|---|---|---|
| `platform` | a daemon calling the **open web** — `tariffd` reaching ENTSO-E, `forecastd` reaching Open-Meteo | the public root store is the question being asked, and it updates with the operating system rather than with the firmware. The image has to carry one |
| `pinned` | a box that talks only to **its own fleet** | a bundle and nothing else. "Any of a hundred and fifty public CAs may vouch for `obsd`" is weaker than anybody means by it, and it removes the packaging question a gateway with a decades-long service life would otherwise carry |

`pinned` *replaces* the platform roots rather than adding to them — the
distinction that decides whether pinning is a control or a decoration. A bundle
that is missing, or that parses to no certificate, stops the daemon: an empty
root store refuses every endpoint, and from the far end that looks exactly like
the service being down.

Because the timestamp is signed, a queued report cannot carry a signature made
when it was queued — a receiver refuses one outside five minutes, which is what
bounds a replay. So the box stores the **body** and signs at each attempt, under
an id it derives from the site and the date. That id is stable, so a corrected
day amends one report rather than adding a second, and `obsd` deduplicates on the
same string the signature covers.

## The box's own identity

SHIP authenticates a **SKI** — the hash of a public key — and nothing else. So
the box holds a private key, and where it holds it decides two things.

It is kept in the box's own database rather than regenerated, because the SKI
follows the key: it is what an installer reads off a screen and gives to the
metering point operator, and a box that made a fresh one on every boot would
have to be re-paired on every boot. The trust store is kept with it, so a power
cut does not un-approve the Steuerbox.

Both are therefore worth exactly what the disk is worth. There is no secure
element yet, and that is stated rather than glossed: anything holding that key
is this household as far as a network operator's box is concerned.

TLS 1.2 with mutual authentication is what SHIP specifies, and the crypto
provider is **named** rather than inherited — `aws-lc-rs`, the same one the fleet
client uses. `rustls` supports two and the choice is process-global, so two in
one binary panic at the first connection; `deny.toml` bans the other so a second
one is a build failure rather than something to notice in a resolution.

## The supply chain

Released builds for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
are each built natively, smoke-tested against a simulated day before they ship,
and accompanied by:

| Artefact | What it answers |
|---|---|
| CycloneDX **SBOM** | what went into this build |
| `SHA256SUMS` | is this the file that was published |
| a signed **build-provenance attestation** | which workflow, from which commit, built it |

```console
$ gh attestation verify hemsd-*.tar.gz --repo hupe1980/hems
```

The binary also carries its own dependency list (`cargo auditable`), so

```console
$ cargo audit bin hemsd
```

answers “what is in this thing” from an artefact **found in the field**, with no
build tree and no manifest to hand.

`cargo deny check` runs in CI over licences, advisories, bans and sources, and
every domain crate carries `#![forbid(unsafe_code)]` — enforced a second time by
`just purity`, which fails the build if a domain crate reaches for the clock, the
filesystem, the network or `unsafe`.

## What is deliberately not claimed

- Nothing here has been through a certification lab. The EEBUS conformance
  harness and the ElaadNL interoperability event are on the roadmap, and the
  state machine deliberately lives in the [`eebus`](https://crates.io/crates/eebus)
  crate so that there is only one implementation to certify.
- There is no secure-element or measured-boot story. The release-signing key is
  built into the binary and the box's SHIP private key sits in its database —
  both the right shape and not yet the right storage. A secure element is what
  would make the second one survive somebody taking the disk out.
- There is no pairing *flow*. A Steuerbox is trusted by putting its SKI in the
  configuration or by the box being given one to remember; a screen an installer
  can approve one on is not built.

## The fleet's database

Three daemons hold household data in PostgreSQL, and three things about that are
security decisions rather than deployment ones: the connection string is a
**reference** to a credential and never the credential, **TLS is on by default**
and turning it off is a decision the daemon logs, and an error message never
carries the URL — because a malformed connection string is exactly what gets
pasted into a ticket, and a real one has the password in it.

Encryption at rest is the deployment's, and hems does not attempt a second layer
over a managed instance's own. What it does owe is retention that actually
happens: `[A1 7.3]`'s two years in `histd` and a bounded window in `obsd`, both
as sweeps an operator can query rather than policies somebody remembers to run.

## `/metrics`

On the same footing as `/livez` and `/readyz`: an **operational** surface, not an
authenticated one, and a deployment puts it behind its own network boundary the
same way it does the probes.

What it deliberately does not carry is any household. The request label is the
matched route — `/v1/sites/{site}/export` — and never the path, because a hems
site identifier is a name like `reference-household` that no heuristic normaliser
would recognise, and a metrics endpoint is scraped, stored for months and read by
everybody. A request that matched no route is labelled `unmatched` rather than by
its URI, which is attacker-controlled. Both are held by tests.
