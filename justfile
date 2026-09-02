# hems — task runner (https://just.systems)
#
# `just` on its own lists every recipe.

set shell := ["bash", "-uc"]

# Keep in sync with rust-toolchain.toml and `rust-version` in Cargo.toml.
msrv := "1.94"

# The one version every publishable crate carries, from `[workspace.package]`.
version := `sed -n '/^\[workspace\.package\]/,/^\[/p' Cargo.toml | sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1`

# 📋 List all recipes
default:
    @just --list

# ✅ Everything CI runs, in CI order
ci: fmt-check lint purity test guards deny doc
    @echo "✅ all checks passed"

# 🎨 Format the workspace
fmt:
    cargo fmt --all

# 🎨 Fail if anything is unformatted
fmt-check:
    cargo fmt --all --check

# 🔍 Clippy, warnings as errors
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# The control planes take time as a parameter. The moment one of them reads a
# clock, a whole winter day stops being a unit test — which is the property the
# guard and the arbiter are built around.
#
# `hems-drv` is on the list for the same reason and with more force: a driver is
# the crate most likely to reach for a socket, because a socket is what it is
# *about*. Keeping it sans-I/O is what makes a § 14a failsafe — a sixty-second
# heartbeat and a two-hour minimum — an assertion rather than two hours of
# waiting.
#
# 🧊 Enforce the "no I/O, no clock" promise of the domain crates
purity:
    #!/usr/bin/env bash
    set -uo pipefail
    pure="hems-core hems-device hems-drv hems-flex hems-grid hems-tariff hems-forecast hems-optimizer hems-realtime hems-events"
    fail=0
    for crate in $pure; do
        hits="$(grep -rn --include='*.rs' -E \
            'SystemTime::now|Instant::now|OffsetDateTime::now|std::(fs|env|net|process)|\bunsafe\b' \
            "crates/$crate/src" 2>/dev/null | grep -vE ':[[:space:]]*(///|//!|//)' || true)"
        if [ -n "$hits" ]; then
            echo "❌ $crate reached for ambient state:" >&2
            echo "$hits" >&2
            fail=1
        fi
    done
    [ "$fail" -eq 0 ] && echo "🧊 pure: no clock, no I/O, no unsafe in the domain crates"
    exit "$fail"

# 🧪 Every test
test:
    cargo test --workspace --all-features

# 🧪 One crate's tests
test-crate crate:
    cargo test -p {{ crate }} --all-features

# 🛡️ Workspace guards: citations, the event catalogue, publishable manifests
guards:
    cargo xtask check-all

# 📜 Licences and advisories
deny:
    cargo deny check

# 📚 Documentation, warnings as errors
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Everything a box refuses to start with, without opening a socket: a driver for
# an asset the site does not have, two drivers for one asset, a controllable
# device whose driver cannot command it, and a § 14a household with nothing that
# could hear a reduction. What an installer runs before leaving the cellar.
#
# 🔌 Check a real household's configuration
check-box config="services/hemsd/hemsd.example.toml":
    cargo run -p hemsd -- run --check --config {{ config }}

# The box on the wall rather than a simulated day: a site and a driver set from
# TOML, one task per driver holding its own socket, and the guard and the arbiter
# deciding against real measurements. It will sit there reconnecting to devices
# that are not on this machine, which is the correct behaviour and is why this is
# a convenience rather than a demonstration.
#
# 🔌 Manage a real household
box config="services/hemsd/hemsd.example.toml":
    cargo run -p hemsd -- run --config {{ config }}

# 🏠 One simulated day through the whole control stack
demo day="winter":
    cargo run -p hemsd -- simulate --day {{ day }}

# 🏠 Every day, and the six comparisons worth seeing
demo-all:
    @just demo winter
    @just demo summer
    @just demo deadline
    @just demo shared
    @just demo offline
    @just demo autumn
    @just demo capped
    @echo "  ── the same winter day with the future known in advance ──"
    @echo "  ── (what a saving figure quoted without a forecast measures) ──"
    cargo run -q -p hemsd -- simulate --day winter --perfect-foresight
    @echo "  ── the same winter day with battery wear priced at zero ──"
    cargo run -q -p hemsd -- simulate --day winter --wear-eur-per-kwh 0
    @echo "  ── the same September day with a fixed three-phase wallbox ──"
    cargo run -q -p hemsd -- simulate --day autumn --no-phase-switching
    @echo "  ── the same capped day with an intelligent meter, which lifts § 9 EEG ──"
    cargo run -q -p hemsd -- simulate --day capped --imsys
    @echo "  ── the shared reduction with every asset weighted the same ──"
    cargo run -q -p hemsd -- simulate --day shared --uniform-weights
    @echo "  ── the same winter day inside a § 42c sharing community ──"
    cargo run -q -p hemsd -- simulate --day winter --sharing

# A modulating heat pump is a linear program; a single-speed one is a binary per
# slot in every one of ninety-six re-plans. Committing the tail per clock hour
# rather than per quarter hour took the day from 13:19 to 2:12 for a plan
# that costs the same to the cent — but two minutes against nine seconds is
# still why it is not in `demo-all` and not in CI.
#
# It is also the only configuration in which a minimum runtime constrains
# anything, so it is the only one that can show the constraint being obeyed.
#
# ❄️ The same winter day on a compressor that has to cycle
demo-on-off day="winter":
    cargo run --release -p hemsd -- simulate --day {{ day }} --heat-pump-on-off

# A single realisation pays a hedge's premium on every day and makes its claim on
# none, so measured once, insurance is always a pure loss. This runs the day under
# several weathers under each risk policy — minutes rather than seconds, because
# three futures cost seven times the solve, which is why it is not in CI.
#
# 🎲 What planning against three futures costs, and what it buys
risk day="deadline" days="4":
    cargo run --release -p hemsd -- risk --day {{ day }} --days {{ days }}

# Forecast error is correlated across a day, so ninety-six quarter hours of one
# Tuesday are close to one draw: a day's coverage figure is a coin toss reported
# to three significant figures. This runs twenty of them and merges the scores,
# which is the only thing in the workspace that can say whether the band the
# planner hedges against is the width it claims to be. Minutes, not seconds.
#
# 📏 Is the forecast band the width it says it is?
backtest day="summer" days="20":
    cargo run --release -p hemsd -- backtest --day {{ day }} --days {{ days }}

# `cargo publish` cannot be undone, so the dry run is the cheap half of the
# decision: it packages the eleven publishable crates in dependency order, verifies
# each builds from its own tarball, and skips the two that are `publish = false`.
#
# 🚢 Everything the release workflow checks, before the tag exists
release-check:
    cargo publish --workspace --locked --dry-run
    cargo build --locked --release -p hemsd
    ./target/release/hemsd simulate --day winter
    @echo "🚢 verified — tag it with: git tag v{{ version }} && git push origin v{{ version }}"

# 🔒 Minimum supported Rust version
msrv:
    cargo +{{ msrv }} check --workspace --all-features

# The fleet, on loopback, so a `readyz` and a `/v1/fleet` are one command away.
# Each daemon is independent — none of them needs the others to start — so this
# is a convenience rather than a topology.
#
# The secret is on both sides because the report is a **signed** CloudEvent:
# `obsd` refuses an unsigned day, since the thing being written is the
# list of households that did not respect a network operator's reduction. A
# demonstration secret in a justfile is a demonstration secret; a real one comes
# from the enrolment.
#
# 🤖 What the advisory plane says about a fleet, and the replay that proves it
agent-demo:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo test -q -p agentd 2>&1 | tail -3
    @echo
    @echo "  The specialists are pure functions; the journal is why they run on a"
    @echo "  runtime. A replay re-executes the logic and reads every effect back,"
    @echo "  so \"why did the queue say that in March\" is a replay rather than an"
    @echo "  argument — and nothing an agent says moves a watt."

# 🛰️ One box reporting a day into the fleet view
fleet-demo day="winter":
    #!/usr/bin/env bash
    set -uo pipefail
    cargo build -q -p obsd -p hemsd
    conf="$(mktemp -t obsd-demo-XXXXXX.toml)"
    # One signing key **per household**, because a signature over a shared secret
    # says the bytes were not edited and nothing about who sent them (D114). The
    # secret is a *reference*: `obsd` reads it from the environment, so the
    # credential is not in the file even in a demonstration.
    cat > "$conf" <<'TOML'
    [webhook_secrets]
    reference-household = ["env:HEMS_OBSD_SECRET_REFERENCE_HOUSEHOLD"]

    [[operators]]
    token  = "env:HEMS_OBSD_OPERATOR_TOKEN"
    tenant = "*"
    TOML
    sed -i'' -e 's/^    //' "$conf"
    HEMS_OBSD_SECRET_REFERENCE_HOUSEHOLD=whsec_fleet-demo HEMS_OBSD_OPERATOR_TOKEN=tok-demo \
        ./target/debug/obsd --config "$conf" &
    obsd=$!
    trap 'kill $obsd 2>/dev/null; rm -f "$conf"' EXIT
    sleep 2
    HEMS_OBSD_SECRET=whsec_fleet-demo \
        ./target/debug/hemsd simulate --day {{ day }} --report-to http://127.0.0.1:8080
    echo
    echo "  ── the fleet view ──"
    view="$(curl -s -H "Authorization: Bearer tok-demo" localhost:8080/v1/fleet)"
    echo "$view"
    # Checked, not merely printed. This demonstration ran for months with a
    # panic in it — `reqwest::blocking` dropped inside `#[tokio::main]` (D115) —
    # and nothing noticed, because printing a summary with no sites in it looks
    # like output. A demonstration that asserts nothing is a test that cannot
    # fail, which is the failure this workspace keeps finding in itself.
    case "$view" in
        *'"sites":1'*) ;;
        *) echo "  ✗ the day did not reach the fleet view"; exit 1 ;;
    esac
    echo "  ── readiness, which names every dependency and when it was last good ──"
    curl -s localhost:8080/readyz
    echo

# 🌐 Serve the documentation site locally
site:
    cd site && zola serve

# 🌐 Build the documentation site
site-build:
    cd site && zola build
