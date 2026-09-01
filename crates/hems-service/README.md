# hems-service

The shell every [hems](https://github.com/hupe1980/hems) daemon shares:
configuration from a file and the environment, structured logging, a health and
readiness surface, and a shutdown that finishes what it started.

Six daemons is six copies of the same forty lines — and written six times, those
forty lines diverge in the direction that costs most, because the one that is
wrong is the one whose readiness probe lies.

## What it is not

It is **not** `mako-service`. Extracting that was considered and rejected: its
OIDC layer carries a `mako_roles` claim and a `Sparte` grant, and its Cedar
schema is built on market roles a household energy manager does not have. What
was left after removing them was five domain-free modules, and copying five
domain-free modules is cheaper than maintaining a diff guard against a fork that
is *supposed* to diverge.

So this is small on purpose. It owns configuration, logging, the health surface
and the shutdown, and it owns nothing about energy.

## Live and ready are different questions

An orchestrator asks both and does opposite things with the answers. **Live**
means "this process is not wedged" and a `false` gets it killed. **Ready** means
"this process can serve traffic" and a `false` takes it out of rotation and
leaves it alone.

Answering the second with the first is the mistake that makes a fleet oscillate:
a daemon whose upstream price source is down is not *broken*, and restarting it
does not bring the price source back. `/readyz` therefore answers with the whole
picture — every dependency, whether it is passing, and **when it was last
good** — so the first click in an incident is also the last.

## Sans-I/O ends here

Every domain crate in the workspace takes time as a parameter and opens no
socket. This crate is where that stops being true, and it is the only shared
place it does: `hems-core`, `hems-grid` and `hems-optimizer` stay testable in a
millisecond because the clock and the socket live here instead.

Even the configuration overlay is a pure function of a lookup — `load_from`
takes the environment as a closure — so a test never has to mutate the process
environment to check that `HEMS_TARIFFD_LISTEN` beats the file.

## License

MIT OR Apache-2.0
