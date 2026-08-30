# hems-events

Every CloudEvents `type` the [hems](https://github.com/hupe1980/hems) workspace
emits, in one place.

An event type is an interface. Spelling one differently in the emitter and the
consumer produces a system that runs, logs nothing unusual, and quietly does not
work — the failure mode that costs the most to find. So the names live here as
constants and `cargo xtask check-events` fails the build if a string that looks
like an event type appears anywhere in the workspace without being in the
catalogue.

`de.hems.<aggregate>.<thing>.<past-tense-verb>` — reverse-DNS as CloudEvents 1.0
recommends, and past tense because an event is something that has already
happened.

**Nothing emits these yet.** Every crate in the workspace is sans-I/O, so there
is no bus for an event to travel on until the daemon grows one. This is the
agreed vocabulary, written down before the first emitter rather than
reverse-engineered from six of them.

## License

MIT OR Apache-2.0
