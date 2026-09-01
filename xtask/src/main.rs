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
        Some("check-all") => {
            check_citations(&root)?;
            check_events(&root)?;
            check_manifests(&root)?;
            check_wire(&root)
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
fn check_citations(root: &Path) -> Result<()> {
    let index = root.join("specs/README.md");
    if !index.exists() {
        println!("check-citations: specs/README.md is absent (it is gitignored); skipping");
        return Ok(());
    }
    let index = std::fs::read_to_string(&index)?;

    // Which document each citation prefix belongs to, and a string that must
    // appear in the index for that document to count as present.
    //
    // A family is added here only once the document is actually indexed. An
    // entry whose needle is broad enough to match anything is worse than no
    // entry: it reports a citation as checked when nothing checked it.
    let sources: [(&str, &str, &str); 5] = [
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
