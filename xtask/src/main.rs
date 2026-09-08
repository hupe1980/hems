//! Workspace guards.
//!
//! Checks that are cheap to run and expensive to skip. Each one exists because
//! the failure it catches is silent: nothing crashes, nothing logs, the system
//! is simply wrong in a way that surfaces months later.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let root = workspace_root()?;
    match std::env::args().nth(1).as_deref() {
        Some("check-citations") => check_citations(&root),
        Some("check-events") => check_events(&root),
        Some("check-manifests") => check_manifests(&root),
        Some("check-wire") => check_wire(&root),
        Some("check-vital") => check_vital(&root),
        Some("check-examples") => check_examples(&root),
        Some("check-stats") => check_stats(&root),
        Some("check-deps") => check_deps(&root),
        Some("check-all") => {
            check_citations(&root)?;
            check_events(&root)?;
            check_manifests(&root)?;
            check_wire(&root)?;
            check_vital(&root)?;
            check_examples(&root)?;
            check_deps(&root)?;
            check_stats(&root)
        }
        Some("help" | "--help" | "-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => {
            print_help();
            bail!("unknown task: {other}")
        }
    }
}

fn print_help() {
    println!(
        "\
cargo xtask <task>

  check-citations   every regulatory citation in the code names a document that
                    specs/README.md actually indexes
  check-events      every CloudEvents type used in the workspace is in the
                    hems-events catalogue
  check-manifests   every publishable crate can actually be packaged: the files
                    its manifest promises exist
  check-wire        every serialisable quantity, instant and date names how it
                    travels, so a value that becomes money or a Nachweis cannot
                    go through an f64 or come back as a tuple
  check-vital       a daemon's background loops are spawned through
                    Health::vital, so /livez can actually fail
  check-examples    every daemon ships an annotated example configuration, a
                    test parses it so it cannot drift from the struct, and every
                    endpoint it sets is one the daemon will accept
  check-stats       the landing page's citation and crate counts are the ones
                    the build actually produces
  check-deps        the sibling-crate versions the architecture notes state are
                    the ones the workspace manifest resolves
  check-all         all of the above
"
    );
}

fn workspace_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("Cargo.toml").exists() && dir.join("crates").exists() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("no workspace root above the current directory");
        }
    }
}

/// Collect every `.rs` file under `crates/` and `services/`.
fn rust_sources(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for top in ["crates", "services", "xtask"] {
        collect(&root.join(top), &mut files)?;
    }
    files.sort();
    Ok(files)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

// ── check-citations ─────────────────────────────────────────────────────────

/// Every regulatory claim in hems cites its source in the form `[A1 4.5.2]` or
/// `[LPC-031]`. This checks that the documents those refer to are indexed in
/// `specs/README.md`, so a citation can always be followed to a file and a
/// retrieval URL.
///
/// The failure it prevents: a rule that cites a Festlegung nobody can produce,
/// which is indistinguishable from a rule somebody invented.
/// Which document each citation prefix belongs to, and a string that must
/// appear in `specs/README.md` for that document to count as present.
///
/// A family is added here only once the document is actually indexed. An entry
/// whose needle is broad enough to match anything is worse than no entry: it
/// reports a citation as checked when nothing checked it.
///
/// Module-level so `check-citations` and `check-stats` count the same thing —
/// two lists would be two definitions of what a citation is, and the number on
/// the landing page would be checked against the wrong one.
const SOURCES: [(&str, &str, &str); 5] = [
    ("[A1 ", "BK6-22-300 Anlage 1", "bk6-22-300-anlage1"),
    (
        "[MiSpeL A1 ",
        "MiSpeL Anlage 1 (Abgrenzungsoption)",
        "mispel-anlage1-abgrenzungsoption",
    ),
    (
        "[MiSpeL A2 ",
        "MiSpeL Anlage 2 (Pauschaloption)",
        "mispel-anlage2-pauschaloption",
    ),
    (
        "[LPC-",
        "EEBUS Limitation of Power Consumption",
        "LimitationOfPowerConsumption",
    ),
    (
        "[MGCP-",
        "EEBUS Monitoring of Grid Connection Point",
        "MonitoringOfGridConnectionPoint",
    ),
];

/// How many regulatory citations the workspace carries.
fn count_citations(root: &Path) -> Result<usize> {
    let mut citations = 0usize;
    for file in rust_sources(root)? {
        let text = std::fs::read_to_string(&file)?;
        for (prefix, _, _) in &SOURCES {
            citations += text.matches(prefix).count();
        }
    }
    Ok(citations)
}

fn check_citations(root: &Path) -> Result<()> {
    let index = root.join("specs/README.md");
    if !index.exists() {
        println!("check-citations: specs/README.md is absent (it is gitignored); skipping");
        return Ok(());
    }
    let index = std::fs::read_to_string(&index)?;

    let sources = SOURCES;

    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut citations = 0usize;
    for file in rust_sources(root)? {
        let text = std::fs::read_to_string(&file)?;
        for (prefix, _, _) in &sources {
            let count = text.matches(prefix).count();
            if count > 0 {
                used.insert(prefix);
                citations += count;
            }
        }
    }

    let mut missing = Vec::new();
    for (prefix, document, needle) in &sources {
        if used.contains(prefix) && !index.contains(needle) {
            missing.push(format!(
                "  {prefix}…]  →  {document}  (not indexed in specs/README.md)"
            ));
        }
    }

    if missing.is_empty() {
        println!(
            "check-citations: {citations} citations across {} document families, all indexed",
            used.len()
        );
        Ok(())
    } else {
        eprintln!("check-citations: citations to documents the index does not carry:");
        for m in &missing {
            eprintln!("{m}");
        }
        bail!("{} uncited document(s)", missing.len())
    }
}

// ── check-manifests ─────────────────────────────────────────────────────────

/// A manifest that names a file must name one that exists.
///
/// The failure it prevents: `readme = "README.md"` with no such file. Nothing
/// notices — `cargo build`, `cargo test` and `cargo clippy` are all perfectly
/// happy — until the day somebody runs `cargo publish` and finds that six of the
/// crates cannot be packaged. Which is exactly what an audit of this workspace
/// found.
/// Every long-running task a daemon spawns from `main` goes through
/// `hems_service::Health::vital`, so `/livez` can fail.
///
/// # Why this is a guard rather than a review note
///
/// `Health::set_live` had no caller for the life of the project, so every
/// daemon's `/livez` returned 200 as long as its HTTP server was answering —
/// including one whose control loop had panicked half an hour before. That is a
/// liveness probe that is worse than none: the orchestrator told to restart on it
/// never restarts anything, and the fault is invisible *because* the process
/// looks healthy. D132 named it and `Health::vital` fixed it — in `hemsd`.
///
/// Three fleet daemons kept the defect for months afterwards: `tariffd`'s and
/// `forecastd`'s fetch loops and `histd`'s retention sweep were plain
/// `tokio::spawn`s, so a tariffd whose poller had died went on serving
/// yesterday's prices to every box in the fleet with a green liveness probe. The
/// primitive was right and nobody called it, which is exactly the shape D132 is
/// about — so remembering is not the mechanism. This is.
///
/// The one exception is the signal handler: `shutdown::on_signal` is *supposed*
/// to return, and failing liveness when it does would fail every clean shutdown.
/// Every daemon ships an example configuration, and something parses it.
///
/// A daemon's configuration is documented in two places: the doc comments on its
/// `Settings`, which a developer reads, and the annotated example, which is what
/// whoever deploys it copies. The second is the one that goes stale — nothing
/// compiles it, and a commented file that has drifted from the struct is worse
/// than none, because every line of it looks authoritative to the person
/// standing in front of a broken deployment.
///
/// So two things are checked: that the file exists, and that some test
/// `include_str!`s it. The second is the half that matters — a file nothing
/// parses is a file that has already drifted and nobody has noticed.
/// The landing page's numbers are the ones the build produces.
///
/// A figure on a landing page is the most-read number in a project and the
/// least-checked: nothing compiles it, so it is wrong within a fortnight of the
/// thing it describes moving. Two of the five were — 459 citations against 469,
/// and 1019 tests against 1050.
///
/// Only the ones with a mechanical source are checked. The **citation** count
/// and the **pure crate** count are computed here; the test count is not, because
/// producing it means running the suite, and a guard that had to do that would
/// be the slowest thing in `just ci` for a number on a web page. It is checked
/// by hand against `cargo test`, and `just ci` runs both.
fn check_stats(root: &Path) -> Result<()> {
    let config = root.join("site/config.toml");
    if !config.exists() {
        println!("check-stats: no site to check");
        return Ok(());
    }
    let config = std::fs::read_to_string(&config)?;
    let stated = |key: &str| -> Option<usize> {
        config
            .lines()
            .find(|l| l.trim_start().starts_with(key))
            .and_then(|l| l.split('"').nth(1))
            .and_then(|v| v.parse().ok())
    };

    let mut wrong = Vec::new();
    let mut check = |key: &str, actual: usize| match stated(key) {
        Some(claimed) if claimed == actual => {}
        Some(claimed) => wrong.push(format!(
            "  {key}: the site says {claimed}, the build counts {actual}"
        )),
        None => wrong.push(format!("  {key}: the site does not state it")),
    };

    check("stat_rules", count_citations(root)?);
    check("stat_tests", count_tests(root)?);
    check("stat_crates", pure_crates(root)?);

    if wrong.is_empty() {
        println!("check-stats: the landing page's counted figures match the build");
        return Ok(());
    }
    eprintln!("check-stats: site/config.toml states a figure the build does not produce:");
    for line in &wrong {
        eprintln!("{line}");
    }
    bail!("the landing page's numbers have drifted")
}

/// The crates `just purity` holds to "no clock, no socket".
/// Every `#[test]` and `#[tokio::test]` in the workspace.
///
/// The attributes rather than a `cargo test` summary, so the guard costs a file
/// walk instead of a full run — and it is the same number a reader would get by
/// counting. Doctests are not included and are a handful; the figure on the
/// landing page is "tests", which these are.
fn count_tests(root: &Path) -> Result<usize> {
    let mut tests = 0usize;
    for file in rust_sources(root)? {
        for line in std::fs::read_to_string(&file)?.lines() {
            let line = line.trim();
            if line == "#[test]" || line == "#[tokio::test]" {
                tests += 1;
            }
        }
    }
    Ok(tests)
}

fn pure_crates(root: &Path) -> Result<usize> {
    let justfile = std::fs::read_to_string(root.join("justfile"))?;
    Ok(justfile
        .lines()
        .find(|l| l.trim_start().starts_with("pure="))
        .map_or(0, |l| l.matches("hems-").count()))
}

fn check_examples(root: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut unparsed = Vec::new();
    let mut plaintext = Vec::new();
    let mut checked = 0usize;
    let services = root.join("services");
    if !services.exists() {
        println!("check-examples: no services to check");
        return Ok(());
    }
    let mut daemons: Vec<PathBuf> = std::fs::read_dir(&services)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("src/main.rs").exists())
        .collect();
    daemons.sort();

    for daemon in daemons {
        let name = daemon
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        checked += 1;
        let example = daemon.join(format!("{name}.example.toml"));
        if !example.exists() {
            missing.push(name.clone());
            continue;
        }
        // Anywhere in the crate: `config.rs` for most, `lib.rs` for `agentd`.
        let needle = format!("{name}.example.toml");
        let mut sources = Vec::new();
        collect(&daemon.join("src"), &mut sources)?;
        let parsed = sources.into_iter().any(|p| {
            std::fs::read_to_string(&p)
                .map(|text| text.contains("include_str!") && text.contains(&needle))
                .unwrap_or(false)
        });
        if !parsed {
            unparsed.push(name.clone());
        }

        // Every URL the example actually sets — commented lines are suggestions
        // a reader has to uncomment, and this guard is about what a copied file
        // does. A `key = "http://host"` that is not loopback is a deployment the
        // daemon refuses at start-up, so shipping one is shipping a file that
        // looks authoritative and does not work.
        for (n, line) in std::fs::read_to_string(&example)?.lines().enumerate() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some(url) = line.split('"').nth(1) else {
                continue;
            };
            if (url.starts_with("http://") || url.starts_with("https://"))
                && !is_confidential_url(url)
            {
                plaintext.push(format!("  {name}.example.toml:{}: {url}", n + 1));
            }
        }
    }

    if missing.is_empty() && unparsed.is_empty() && plaintext.is_empty() {
        println!("check-examples: {checked} daemons, each with an example a test parses");
        return Ok(());
    }
    if !missing.is_empty() {
        eprintln!("check-examples: a daemon ships no annotated example, or nothing parses it:");
        for line in &missing {
            eprintln!("{line}");
        }
    }
    if !unparsed.is_empty() {
        eprintln!("check-examples: an example exists and no test reads it:");
        for line in &unparsed {
            eprintln!("{line}");
        }
    }
    if !plaintext.is_empty() {
        eprintln!(
            "check-examples: an example configures an endpoint the daemon will refuse at \
             start-up — `https` anywhere, plain `http` only to a loopback address (D85):"
        );
        for line in &plaintext {
            eprintln!("{line}");
        }
    }
    bail!("a shipped example is wrong")
}

/// Whether a URL in a shipped example is one the daemon will actually accept.
///
/// The parse check says an example matches its struct; this says it describes a
/// deployment that can start. An example recommending `http://histd.internal`
/// recommends that a household's § 14a evidence and the token that writes it
/// cross a network in the clear, and it looks as authoritative as the line above
/// it (D85).
///
/// The same rule as `hems_service::http::confidential`, spelled out rather than
/// called: `xtask` guards the workspace and so may not depend on it.
fn is_confidential_url(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest.is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        // Not a URL at all — a path, a bare host, an `env:` reference. Not this
        // guard's question.
        return true;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit_once(':')
        .map_or(rest, |(host, _)| host);
    let host = host.trim_matches(['[', ']']);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn check_vital(root: &Path) -> Result<()> {
    let mut bare = Vec::new();
    let mut checked = 0usize;
    let services = root.join("services");
    if !services.exists() {
        println!("check-vital: no services to check");
        return Ok(());
    }
    for entry in std::fs::read_dir(&services)? {
        let main = entry?.path().join("src/main.rs");
        if !main.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&main)?;
        for (n, line) in text.lines().enumerate() {
            let Some(rest) = line.split_once("tokio::spawn(").map(|(_, r)| r) else {
                continue;
            };
            checked += 1;
            // The signal handler is the one task that ends on purpose.
            if rest.contains("on_signal") {
                continue;
            }
            bare.push(format!(
                "  {}:{}: {}",
                main.strip_prefix(root).unwrap_or(&main).display(),
                n + 1,
                line.trim()
            ));
        }
    }

    if bare.is_empty() {
        println!("check-vital: {checked} spawns in daemon mains, every long-running one vital");
        Ok(())
    } else {
        eprintln!(
            "check-vital: a daemon spawns a task outside `Health::vital`, so `/livez` cannot \
             fail when it dies (D132):"
        );
        for b in &bare {
            eprintln!("{b}");
        }
        eprintln!(
            "  use `health.vital(name, shutdown, task)`, or `shutdown::on_signal` if the task \
             is meant to return."
        );
        bail!("{} background task(s) outside Health::vital", bare.len())
    }
}

fn check_manifests(root: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut checked = 0usize;

    for dir in ["crates", "services"] {
        let base = root.join(dir);
        if !base.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&base)? {
            let crate_dir = entry?.path();
            let manifest = crate_dir.join("Cargo.toml");
            if !manifest.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest)?;
            for (key, default) in [("readme", "README.md"), ("license-file", "")] {
                let Some(named) = manifest_file(&text, key, default) else {
                    continue;
                };
                checked += 1;
                if !crate_dir.join(&named).exists() {
                    missing.push(format!(
                        "  {}: {key} = {named:?}, which is not there",
                        manifest.strip_prefix(root).unwrap_or(&manifest).display()
                    ));
                }
            }
        }
    }

    if missing.is_empty() {
        println!("check-manifests: {checked} manifest file references, all present");
        Ok(())
    } else {
        eprintln!("check-manifests: manifests promising files that do not exist:");
        for m in &missing {
            eprintln!("{m}");
        }
        bail!("{} broken manifest reference(s)", missing.len())
    }
}

/// The file a manifest key names, if it names one.
///
/// `key = "path"` gives the path; a bare `key = true` (which Cargo reads as the
/// conventional filename) gives `default`, when there is one.
fn manifest_file(manifest: &str, key: &str, default: &str) -> Option<String> {
    let line = manifest
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(key) && l[key.len()..].trim_start().starts_with('='))?;
    let value = line.split_once('=')?.1.trim();
    if value == "true" {
        return (!default.is_empty()).then(|| default.to_string());
    }
    if value == "false" {
        return None;
    }
    Some(value.trim_matches('"').to_string())
}

// ── check-events ────────────────────────────────────────────────────────────

/// Every string that looks like a hems CloudEvents type has to be in the
/// catalogue.
///
/// The failure it prevents: an emitter and a consumer that spell the same event
/// differently. Nothing breaks, nothing logs, and the feature simply never
/// happens.
fn check_events(root: &Path) -> Result<()> {
    let prefix = hems_events::PREFIX;
    let mut unknown: Vec<(PathBuf, String)> = Vec::new();
    let mut found = 0usize;

    for file in rust_sources(root)? {
        // The catalogue itself is where the names are declared.
        if file.ends_with("hems-events/src/lib.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file)?;
        for literal in string_literals(&text) {
            if !literal.starts_with(prefix) {
                continue;
            }
            found += 1;
            if !hems_events::is_known(&literal) {
                unknown.push((file.clone(), literal));
            }
        }
    }

    if unknown.is_empty() {
        println!("check-events: {found} event references, all in the catalogue");
        Ok(())
    } else {
        eprintln!("check-events: event types that are not in hems-events:");
        for (file, literal) in &unknown {
            eprintln!("  {}: {literal:?}", file.display());
        }
        bail!("{} uncatalogued event type(s)", unknown.len())
    }
}

/// Every double-quoted string literal in `text`, escapes handled naïvely.
///
/// Good enough for a guard: a false positive is a build failure with an
/// explanatory message, which is a cheap way to be wrong.
fn string_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut literal = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => break,
                other => literal.push(other),
            }
        }
        out.push(literal);
    }
    out
}

// ── check-wire ──────────────────────────────────────────────────────────────

/// Every quantity, instant and date that can be serialised says how it travels.
///
/// A quantity that becomes money or a Nachweis is `rust_decimal::Decimal`, and
/// the impl it inherits is not good enough for one: it reads
/// with `deserialize_any`, which accepts a JSON *number* — a value that has
/// already lost digits to an `f64` before it arrives — and which a format with
/// no self-describing wire cannot answer at all. `postcard` and `bincode` are
/// exactly what an embedded store speaks.
///
/// `serde(with = "rust_decimal::serde::str")` fixes it per field. The
/// alternative is `rust_decimal`'s `serde-str` feature, and a *library* may not
/// reach for it: Cargo features are global to a build graph, so it would change
/// how every `Decimal` deserialises in a crate that never named hems — and a
/// feature any other crate sets would decide how hems's own quantities travel.
///
/// The same argument applies to a `time::Date`: its inherited impl writes the
/// compact `(year, ordinal)` tuple unless `serde-human-readable` is on, so a
/// commissioning date lands in a configuration file as `[2024, 1]`.
///
/// Which is why this is a guard rather than a convention. One forgotten
/// attribute is silent.
fn check_wire(root: &Path) -> Result<()> {
    let mut bare = Vec::new();
    let mut checked = 0usize;

    for path in rust_sources(root)? {
        let text = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = text.lines().collect();
        // One frame per open brace, saying whether its body is the body of a
        // type that derives `Serialize`. A `fn` parameter and a `let` binding
        // are not fields, and a private helper struct nothing serialises is not
        // one either.
        let mut stack: Vec<bool> = Vec::new();
        let mut derives_serialize = false;

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("#[") {
                if trimmed.contains("Serialize") {
                    derives_serialize = true;
                }
                continue;
            }

            if stack.last().copied().unwrap_or(false)
                && let Some(name) = decimal_field(trimmed)
                // A field that never travels has no wire form to state, and
                // demanding one would push a caller towards `serde(with)` on a
                // shape that cannot be written at all — a `BTreeMap` keyed by a
                // struct, say, whose JSON keys would have to be strings.
                && !is_skipped(&lines[..i])
            {
                checked += 1;
                if !states_its_form(&lines[..i]) {
                    bare.push(format!(
                        "  {}:{}: {name} does not say how it travels",
                        path.strip_prefix(root).unwrap_or(&path).display(),
                        i + 1,
                    ));
                }
            }

            let opens = trimmed.ends_with('{');
            if opens {
                let inherited = stack.last().copied().unwrap_or(false);
                let is_type = trimmed.contains("struct ") || trimmed.contains("enum ");
                // An enum variant carrying named fields inherits its enum's
                // derive; anything else that opens a block inside a type body
                // (an `impl`, a `fn`) does not.
                let body_of_a_type = if is_type {
                    derives_serialize
                } else if trimmed.contains("fn ") || trimmed.starts_with("impl") {
                    false
                } else {
                    inherited
                };
                stack.push(body_of_a_type);
            }
            if trimmed.starts_with('}') && !opens {
                stack.pop();
            }
            if !trimmed.is_empty() {
                derives_serialize = false;
            }
        }
    }

    if bare.is_empty() {
        println!("check-wire: {checked} quantities and instants, all naming their wire form");
        Ok(())
    } else {
        eprintln!(
            "check-wire: a quantity or an instant must say how it travels — \
             `rust_decimal::serde::str` for a Decimal, `time::serde::rfc3339` for \
             an instant, `hems_core::wire::iso_date` for a date:"
        );
        for b in &bare {
            eprintln!("{b}");
        }
        bail!("{} field(s) with no stated wire representation", bare.len())
    }
}

/// Whether the attribute block immediately above a field says how it travels.
///
/// A `cfg_attr` that carries a `default` as well as a `with` wraps onto three
/// lines, so this walks back over the whole block rather than looking at one
/// line — which is the difference between a guard and a guard that has to be
/// switched off.
/// Whether the field on this line is `serde(skip)`ped, and therefore never
/// travels at all.
fn is_skipped(before: &[&str]) -> bool {
    for line in before.iter().rev() {
        let trimmed = line.trim();
        if trimmed.contains("serde(skip)") || trimmed.contains("serde(skip_serializing)") {
            return true;
        }
        if !(trimmed.starts_with("//")
            || trimmed.starts_with("#[")
            || trimmed.starts_with(')')
            || trimmed.starts_with("feature = ")
            || trimmed.starts_with("serde(")
            || trimmed.is_empty())
        {
            return false;
        }
    }
    false
}

fn states_its_form(before: &[&str]) -> bool {
    for line in before.iter().rev() {
        let trimmed = line.trim();
        if trimmed.contains("with = \"") {
            return true;
        }
        // Doc comments and the rest of an attribute block are still "above the
        // field"; anything else ends the search.
        if !(trimmed.starts_with("//")
            || trimmed.starts_with("#[")
            || trimmed.starts_with(')')
            || trimmed.starts_with("feature = ")
            || trimmed.starts_with("serde(")
            || trimmed.is_empty())
        {
            return false;
        }
    }
    false
}

/// The name of the field on this line, if it declares one whose wire form has
/// to be stated: a quantity or an instant.
fn decimal_field(trimmed: &str) -> Option<&str> {
    let (name, rest) = trimmed.split_once(": ")?;
    let carries = ["Decimal", "OffsetDateTime", "Date"]
        .iter()
        .any(|t| rest.contains(t));
    if !rest.ends_with(',') || !carries {
        return None;
    }
    let name = name.strip_prefix("pub ").unwrap_or(name).trim();
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()))
    .then_some(name)
}

/// The versions the architecture notes state are the ones the manifest requires.
///
/// A dependency table in prose is the most quoted fact about a workspace and the
/// least checked one: nothing compiles it, so it drifts within a release of
/// whatever it describes — and a design argument resting on "what version X
/// does" then rests on a version nobody builds against.
///
/// Only the version is checked. What follows it in that cell is the reasoning,
/// which cannot be mechanised; the number can.
///
/// `concepts/` is internal and absent from a clone, so a missing file is not a
/// failure — the same rule [`check_stats`] follows for a missing site.
fn check_deps(root: &Path) -> Result<()> {
    let notes = root.join("concepts/ARCHITECTURE.md");
    if !notes.exists() {
        println!("check-deps: no architecture notes to check");
        return Ok(());
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let required = workspace_requirements(&manifest);
    let notes = std::fs::read_to_string(&notes)?;

    let mut checked = 0usize;
    let mut wrong = Vec::new();
    for line in notes.lines() {
        // `| `crate` | role | 0.9, and then some prose |`
        let mut cells = line.split('|').map(str::trim);
        if cells.next().is_some_and(|before| !before.is_empty()) {
            continue;
        }
        let (Some(name), Some(_role), Some(state)) = (cells.next(), cells.next(), cells.next())
        else {
            continue;
        };
        let Some(name) = name.strip_prefix('`').and_then(|n| n.strip_suffix('`')) else {
            continue;
        };
        let Some(want) = required.get(name) else {
            continue;
        };
        let stated = state
            .split(|c: char| !c.is_ascii_digit() && c != '.')
            .find(|token| token.contains('.') && token.starts_with(|c: char| c.is_ascii_digit()));
        checked += 1;
        match stated {
            Some(stated) if stated == want => {}
            Some(stated) => wrong.push(format!(
                "  {name}: the notes say {stated}, the manifest requires {want}"
            )),
            None => wrong.push(format!(
                "  {name}: the notes state no version, the manifest requires {want}"
            )),
        }
    }

    if wrong.is_empty() {
        println!("check-deps: {checked} stated sibling-crate versions, all matching the manifest");
        return Ok(());
    }
    eprintln!("check-deps: concepts/ARCHITECTURE.md states a version the manifest does not:");
    for line in &wrong {
        eprintln!("{line}");
    }
    bail!("the architecture notes' dependency table has drifted")
}

/// Every `[workspace.dependencies]` entry that names a version, by crate name.
///
/// A hand-rolled scan rather than a TOML parser: `xtask` is a guard that has to
/// build before anything else does, the section is a flat list of one-line
/// entries, and the alternative is a dependency for twenty lines of `split`.
fn workspace_requirements(manifest: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == "[workspace.dependencies]";
            continue;
        }
        if !inside || trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        // `name = "0.9"` or `name = { version = "0.9", … }`.
        let after = rest.trim();
        // A `path` entry is one of this workspace's own crates. Its version is
        // the workspace's own and moves with every release of it, so a table
        // stating one would be a table restating `[workspace.package]` — and the
        // crate table this scan also walks lists dependencies in that column
        // rather than a version.
        if after.contains("path =") {
            continue;
        }
        let version = if let Some(quoted) = after.strip_prefix('"') {
            quoted.split('"').next().map(str::to_owned)
        } else {
            after
                .split_once("version")
                .and_then(|(_, v)| v.split('"').nth(1))
                .map(str::to_owned)
        };
        if let Some(version) = version {
            out.insert(name.to_owned(), version);
        }
    }
    out
}
