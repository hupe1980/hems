+++
title = "Security and supply chain"
description = "The box trusts a key it was built with, not a server. Secrets are references, authorisation is per site, and every release ships with an SBOM and a provenance attestation."
weight = 12
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

## A credential in a configuration file is a credential in a repository

Configuration is read from a file and then the environment, which is right for a
poll interval and wrong for a credential: one in a configuration file is one in
an image, in a backup, and eventually in a repository — and no orchestrator
injects secrets that way.

So the **reference** is configured and the value is not:

```toml
webhook_secrets = ["env:HEMS_OBSD_WEBHOOK_SECRET"]   # from the environment
webhook_secrets = ["file:/run/secrets/webhook"]      # from a mounted file
webhook_secrets = ["whsec_literal"]                  # the secret itself
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

## Two services are open, and that is written down

`tariffd` serves published day-ahead auction results and `forecastd` serves
irradiance over a location. Neither carries household data, and both are
unauthenticated **on purpose**. Writing that down is what makes the difference
between the two pairs read as a decision rather than an oversight.

## The evidence record is what a signature protects

`obsd`'s collector holds the list of households that did *not* respect a network
operator's reduction. An unauthenticated write to it can put a compliant site on
that list or take a breach off it, so a day reaches it only as a **signed
CloudEvent** — Standard Webhooks over the message id, the timestamp and the exact
bytes, so a captured request cannot be replayed, re-attributed or edited.

TLS is the other half and it is a different guarantee: the signature says the
report is the one this box sent and has not been edited; TLS says nobody read it
on the way. Plain `http` is allowed only to a loopback address.

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

- The SHIP/SPINE transport under EEBUS — TLS, SKI pairing, a trust store — is
  **not written yet**. Until it is, a § 14a limit can only arrive from the
  simulator or from the sans-I/O driver being handed bytes.
- Nothing here has been through a certification lab. The EEBUS conformance
  harness and the ElaadNL interoperability event are on the roadmap, and the
  state machine deliberately lives in the [`eebus`](https://crates.io/crates/eebus)
  crate so that there is only one implementation to certify.
- There is no secure-element or measured-boot story. The built-in key is a key in
  a binary, which is the right shape and not yet the right storage.
