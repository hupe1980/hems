# hems-events

Every CloudEvents `type` the [hems](https://github.com/hupe1980/hems) workspace
emits, the envelope one travels in, and the signature that says which box sent
it.

An event type is an interface. Spelling one differently in the emitter and the
consumer produces a system that runs, logs nothing unusual, and quietly does not
work — the failure mode that costs the most to find. So the names live here as
constants and `cargo xtask check-events` fails the build if a string that looks
like an event type appears anywhere in the workspace without being in the
catalogue.

`de.hems.<aggregate>.<thing>.<past-tense-verb>` — reverse-DNS as CloudEvents 1.0
recommends, and past tense because an event is something that has already
happened: `de.hems.grid.lpc.limit.received`, not `de.hems.grid.set_limit`.

## One of them is on a wire, and the rest are not yet

`SITE_DAY_REPORTED` is what a box sends `obsd` at the end of a day — the only
link between the edge and the fleet that exists today. It travels as a
CloudEvent in `envelope` and is **signed** with `webhook` (Standard Webhooks over
the message id, the timestamp and the exact bytes), because the thing being
written is the list of households that did not respect a network operator's
reduction. A fleet view that accepts an unsigned day is one anybody who can reach
it may write to, and a captured request must not be replayable, re-attributable
or editable.

The rest of the catalogue is the agreed vocabulary, written down before its first
emitter rather than reverse-engineered from six of them. It arrives with the
driver loop and the local bus `hemsd` still has to grow.

- 🧊 **No I/O, no clock.** `sign` and `verify` are pure functions of bytes, a
  secret and an instant, so a replayed or tampered event is a unit test.
- 🔐 **Several secrets at once**, so a rotation is a deployment rather than an
  outage: verification tries each configured secret.
- ⏱️ **A tolerance on the timestamp**, because a signature that never expires is
  a capture that never expires.

## License

MIT OR Apache-2.0
