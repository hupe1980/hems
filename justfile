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
# 🧊 Enforce the "no I/O, no clock" promise of the domain crates
purity:
    #!/usr/bin/env bash
    set -uo pipefail
    pure="hems-core hems-device hems-flex hems-grid hems-tariff hems-forecast hems-optimizer hems-realtime hems-events"
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

# 🏠 One simulated day through the whole control stack
demo day="winter":
    cargo run -p hemsd -- simulate --day {{ day }}

# 🏠 Every day, and the five comparisons worth seeing
demo-all:
    @just demo winter
    @just demo summer
    @just demo deadline
    @just demo shared
    @just demo offline
    @just demo autumn
    @just demo capped
    @echo "  ── the same winter day with the future known in advance ──"
    @echo "  ── (what every saving figure in this project was, before v1.2) ──"
    cargo run -q -p hemsd -- simulate --day winter --perfect-foresight
    @echo "  ── the same winter day with battery wear priced at zero ──"
    cargo run -q -p hemsd -- simulate --day winter --wear-eur-per-kwh 0
    @echo "  ── the same September day with a fixed three-phase wallbox ──"
    cargo run -q -p hemsd -- simulate --day autumn --no-phase-switching
    @echo "  ── the same capped day with an intelligent meter, which lifts § 9 EEG ──"
    cargo run -q -p hemsd -- simulate --day capped --imsys
    @echo "  ── the shared reduction with every asset weighted the same ──"
    cargo run -q -p hemsd -- simulate --day shared --uniform-weights

# `cargo publish` cannot be undone, so the dry run is the cheap half of the
# decision: it packages the ten publishable crates in dependency order, verifies
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

# 🌐 Serve the documentation site locally
site:
    cd site && zola serve

# 🌐 Build the documentation site
site-build:
    cd site && zola build
