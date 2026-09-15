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
        Some("check-wire") => check_wire(&root).map(drop),
        Some("check-vital") => check_vital(&root),
        Some("check-examples") => check_examples(&root),
        Some("check-stats") => check_stats(&root),
        Some("check-notes") => check_notes(&root),
        Some("check-deps") => check_deps(&root),
        Some("check-deps-used") => check_deps_used(&root).map(drop),
        Some("check-all") => {
            check_citations(&root)?;
            check_events(&root)?;
            check_manifests(&root)?;
            check_wire(&root)?;
            check_vital(&root)?;
            check_examples(&root)?;
            check_deps(&root)?;
            check_deps_used(&root)?;
            check_notes(&root)?;
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
  check-stats       the landing page's and README's citation, test and crate
                    counts are the ones the build actually produces
  check-notes       every decision, risk and milestone label the code cites
                    resolves to an entry in the architecture notes, and no label
                    is defined twice
  check-deps        the sibling-crate versions the architecture notes state are
                    the ones the workspace manifest resolves
  check-deps-used   every dependency a crate declares is one its source reaches,
                    so a published manifest does not promise what it never uses
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
/// Every `D107`, `R32` and `M5c` the source cites resolves to an entry, and no
/// label is defined twice.
///
/// The workspace's doc comments cite decisions, risks and milestones by label —
/// three hundred places — and the registers they resolve to are **internal
/// notes** that are not published with the crates. A label is therefore the only
/// thing a reader outside this repository gets, which makes a dangling one worse
/// than an ordinary broken link: there is nothing else to go on. The registers
/// promise stable labels, and a promise nothing checks is a convention.
///
/// A withdrawn decision stays in the log **as withdrawn** rather than being
/// deleted, which is what this enforces from the other end: the code that cites
/// one is usually the code that replaced it, and that is the citation most worth
/// resolving. D107 was deleted and three tests went on pointing at it.
///
/// Absent notes are not a failure. `concepts/` is internal, so a clone without
/// it must still pass CI — the guard says it found nothing to resolve.
fn check_notes(root: &Path) -> Result<()> {
    let notes = root.join("concepts");
    let mut known: BTreeSet<String> = BTreeSet::new();
    let mut wrong: Vec<String> = Vec::new();
    let mut registers: Vec<(char, &str)> = Vec::new();

    for (letter, file) in [
        ('D', "DECISIONS.md"),
        ('R', "RISKS.md"),
        ('M', "ROADMAP.md"),
    ] {
        let path = notes.join(file);
        if !path.exists() {
            continue;
        }
        registers.push((letter, file));
        for line in std::fs::read_to_string(&path)?.lines() {
            // The register's own row shape: `| D42 | …`. A label inside a row's
            // prose is a cross-reference, not a definition.
            let Some(rest) = line.strip_prefix("| ") else {
                continue;
            };
            let Some(label) = label_at(rest, letter) else {
                continue;
            };
            if !known.insert(label.clone()) {
                wrong.push(format!("  {file}: {label} is defined twice"));
            }
        }
    }
    if registers.is_empty() {
        println!("check-notes: no architecture notes in this checkout, nothing to resolve");
        return Ok(());
    }

    let mut cited = 0usize;
    for file in rust_sources(root)? {
        let text = std::fs::read_to_string(&file)?;
        let relative = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string();
        for (line_no, line) in text.lines().enumerate() {
            for (letter, register) in &registers {
                for label in cites(line, *letter) {
                    cited += 1;
                    if !known.contains(&label) {
                        wrong.push(format!(
                            "  {relative}:{}: {label} resolves to nothing in {register}",
                            line_no + 1
                        ));
                    }
                }
            }
        }
    }

    let named = stale_names(root, &mut wrong)?;
    let linked = internal_links(root, &mut wrong)?;
    let filed = feedback_files(root, &mut wrong)?;

    if wrong.is_empty() {
        println!(
            "check-notes: {cited} citations of {} decisions, risks and milestones, {named} names \
             a current-state note gives, {linked} links between notes and {filed} sibling-crate \
             feedback files, all resolving",
            known.len()
        );
        return Ok(());
    }
    eprintln!("check-notes: the notes and the code disagree:");
    for line in &wrong {
        eprintln!("{line}");
    }
    bail!("a label or a name in the architecture notes resolves to nothing")
}

/// Identifiers the **current-state** notes name that the workspace does not
/// define.
///
/// A note that describes what exists is a map, and a map naming a road that was
/// renamed is worse than one that leaves it out. Three of these were found by
/// hand in one pass: an `ARCHITECTURE.md` row marked ✅ that named a
/// `TariffId` catalogue which does not exist and is still an open question in
/// `ROADMAP.md`; `GRID_RULES.md` describing the § 14a evidence flag as a variant
/// when it is a predicate; and `HEMSD.md` carrying an `obsd` summary field under
/// a name the decision log had already recorded as renamed.
///
/// The corpus is `crates/` and `services/` — **not** `xtask/`. Including this
/// file made the guard blind to any defect its own doc comment described: the
/// first version quoted the renamed field by name, so the corpus contained it
/// and the reintroduced defect passed.
///
/// **Snake-case only**, and that is the whole of the precision. A field or a
/// function is almost always this workspace's own and is exactly what gets
/// renamed; a `CamelCase` name in these notes is as often a type in an upstream
/// crate or one that is designed and not written. The registers that legitimately
/// name what does *not* exist — the decision log's rejected alternatives, the
/// risks, the roadmap, the market comparison — are skipped wholesale rather than
/// annotated, because naming the unbuilt is their job.
fn stale_names(root: &Path, wrong: &mut Vec<String>) -> Result<usize> {
    /// Named in a current-state note and deliberately not ours.
    const FOREIGN: [(&str, &str); 3] = [
        (
            "dlms_cosem",
            "the DLMS/COSEM protocol family, not a crate here",
        ),
        (
            "max_power_kw",
            "a field of the § 41e dispatch event `flexd` will emit",
        ),
        (
            "g_shared",
            "the shared-limit term in the planner's own notation",
        ),
    ];
    // These four are *about* what does not exist: rejected alternatives, open
    // risks, unbuilt work, other people's products.
    const REGISTERS: [&str; 4] = [
        "DECISIONS.md",
        "RISKS.md",
        "ROADMAP.md",
        "MARKET_LANDSCAPE.md",
    ];

    let notes = root.join("concepts");
    if !notes.exists() {
        return Ok(0);
    }
    let mut corpus = String::new();
    for file in rust_sources(root)? {
        if file.starts_with(root.join("xtask")) {
            continue;
        }
        corpus.push_str(&std::fs::read_to_string(&file)?);
    }
    for manifest in ["Cargo.toml", "justfile", "deny.toml"] {
        if let Ok(text) = std::fs::read_to_string(root.join(manifest)) {
            corpus.push_str(&text);
        }
    }

    let mut checked = 0usize;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&notes)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();
    for path in files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        if REGISTERS.contains(&name.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        for identifier in backticked_snake_case(&text) {
            if FOREIGN.iter().any(|(allowed, _)| *allowed == identifier) {
                continue;
            }
            checked += 1;
            if !corpus.contains(&identifier) {
                wrong.push(format!(
                    "  concepts/{name}: `{identifier}` is named as if it exists and the \
                     workspace does not define it"
                ));
            }
        }
    }
    Ok(checked)
}

/// Every `` `snake_case_name` `` in a document: backticked, lower-case, and
/// carrying at least one underscore, with nothing path-like or file-like in it.
///
/// Fenced blocks are removed first. Splitting the whole document on the backtick
/// and taking alternate spans is the obvious reading and it is wrong: a ``` fence
/// is three of them, so one code block inverts the parity and every inline span
/// after it is read as prose. The first draft checked whatever happened to fall
/// on the right side of the first fence — it passed on a document where the
/// defect this guard exists for had been put back by hand.
fn backticked_snake_case(text: &str) -> Vec<String> {
    let mut prose = String::with_capacity(text.len());
    let mut fenced = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            prose.push_str(line);
            prose.push('\n');
        }
    }

    let mut found = Vec::new();
    for span in prose.split('`').skip(1).step_by(2) {
        if span.len() < 3 || span.len() > 60 {
            continue;
        }
        if !span.contains('_') {
            continue;
        }
        if !span
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        if span.starts_with('_') || span.ends_with('_') {
            continue;
        }
        found.push(span.to_owned());
    }
    found.sort();
    found.dedup();
    found
}

/// Every relative link from one note to another resolves to a file that is there.
///
/// Cheap, and the moment it earns its keep is a **split**: a note that grows to
/// cover four topics gets divided, and every inbound link that pointed at the
/// half that moved is now wrong. There is no build step over these documents and
/// nothing else would say so.
fn internal_links(root: &Path, wrong: &mut Vec<String>) -> Result<usize> {
    let notes = root.join("concepts");
    if !notes.exists() {
        return Ok(0);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&notes)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    files.sort();

    let mut checked = 0usize;
    for path in &files {
        let from = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        for target in markdown_links(&std::fs::read_to_string(path)?) {
            checked += 1;
            // The anchor is not checked: a heading is prose and renaming one is
            // not the defect this exists for.
            let file = target.split('#').next().unwrap_or_default();
            if file.is_empty() {
                continue;
            }
            if !notes.join(file).exists() {
                wrong.push(format!(
                    "  concepts/{from}: links to {file}, which is not there"
                ));
            }
        }
    }
    Ok(checked)
}

/// Sibling-crate feedback files the notes name but the repository does not have.
///
/// `<CRATE>_FEEDBACK.md` in the root is where a bug or a feature request for a
/// sibling crate is written down, and the notes cite them by name. A citation
/// that resolves to nothing is worse here than an ordinary broken link, because
/// what it claims is that a request has been *filed* with another team: the
/// roadmap said "the one request of `METERING_FEEDBACK.md` that 0.23 has not
/// answered" while no such file existed, so the item read as waiting on somebody
/// who had never been asked.
///
/// **Only the notes that describe current state and open work.** The decision
/// log legitimately names files that are gone — a feedback file is deleted when
/// its last item is resolved upstream, and the decision recording that closure
/// is exactly where the name survives.
fn feedback_files(root: &Path, wrong: &mut Vec<String>) -> Result<usize> {
    const CURRENT: [&str; 2] = ["ROADMAP.md", "RISKS.md"];
    let notes = root.join("concepts");
    let mut checked = 0usize;
    for name in CURRENT {
        let path = notes.join(name);
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        for (i, _) in text.match_indices("_FEEDBACK.md") {
            let start = text[..i]
                .rfind(|c: char| !c.is_ascii_uppercase() && c != '_')
                .map_or(0, |b| b + 1);
            let file = &text[start..i + "_FEEDBACK.md".len()];
            checked += 1;
            if !root.join(file).exists() {
                wrong.push(format!(
                    "  concepts/{name}: names {file}, which is not in the repository —                      a request nobody has filed"
                ));
            }
        }
    }
    Ok(checked)
}

/// The targets of every `[text](target.md)` in a document.
fn markdown_links(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    for (i, _) in text.match_indices("](") {
        let start = i + 2;
        let Some(end) = text[start..].find(')').map(|n| start + n) else {
            continue;
        };
        let target = &text[start..end];
        // Only sibling notes: an absolute URL, a path with a directory in it and
        // an image are all somebody else's to resolve.
        if target.contains("://") || target.contains('/') || !target.contains(".md") {
            continue;
        }
        if i > 0 && bytes[i - 1] == b'!' {
            continue;
        }
        found.push(target.to_owned());
    }
    found
}

/// A label at the very start of `text`: the letter, at least one digit, and any
/// lower-case suffix (`M5c`).
fn label_at(text: &str, letter: char) -> Option<String> {
    let rest = text.strip_prefix(letter)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let suffix: String = rest[digits.len()..]
        .chars()
        .take_while(char::is_ascii_lowercase)
        .collect();
    Some(format!("{letter}{digits}{suffix}"))
}

/// Every citation of one register's letter on one line.
///
/// Bounded by a non-identifier character on both sides, so a hexadecimal
/// literal, an identifier like `R2D2` and — the one that actually fired —
/// `SOLAR_CONSTANT_W_PER_M2` are not read as labels. The underscore counts as
/// part of the identifier; leaving it out found two milestones inside a constant
/// naming a unit.
fn cites(line: &str, letter: char) -> Vec<String> {
    let identifier = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    for (i, c) in line.char_indices() {
        if c != letter || (i > 0 && identifier(bytes[i - 1])) {
            continue;
        }
        let Some(label) = label_at(&line[i..], letter) else {
            continue;
        };
        // Three digits is the widest register; more is a version or a part
        // number that happens to start with the same letter.
        if label.len() > 5 {
            continue;
        }
        if line[i + label.len()..]
            .bytes()
            .next()
            .is_some_and(identifier)
        {
            continue;
        }
        found.push(label);
    }
    found
}

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

    let citations = count_citations(root)?;
    let tests = count_tests(root)?;
    check("stat_rules", citations);
    check("stat_tests", tests);
    check("stat_crates", pure_crates(root)?);
    wrong.extend(readme_figures(
        root,
        citations,
        tests,
        wire_fields(root)?,
        declared_dependencies(root)?,
    )?);

    if wrong.is_empty() {
        println!(
            "check-stats: every counted figure the site, the README and the notes state matches the build"
        );
        return Ok(());
    }
    eprintln!("check-stats: a stated figure is not one the build produces:");
    for line in &wrong {
        eprintln!("{line}");
    }
    bail!("the stated numbers have drifted")
}

/// Every count `README.md` states in prose.
///
/// The README argues that a figure nothing compares is wrong within a fortnight,
/// and states five of its own. It also stated the citation count **twice**, and
/// the two copies had drifted apart from each other as well as from the build —
/// which is why this checks every occurrence of an anchor rather than the first:
/// two statements of one number are two chances to be wrong.
///
/// A missing pattern is a failure rather than a skip. Otherwise rewording the
/// sentence silently removes the check, which is how a guard stops guarding
/// without anybody deciding to.
fn readme_figures(
    root: &Path,
    citations: usize,
    tests: usize,
    wire: usize,
    dependencies: usize,
) -> Result<Vec<String>> {
    // Written with a space between the thousands, as everything in German
    // convention here is, so the comparison is on the digits.
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    // The number immediately before `marker`: the trailing run of digits and the
    // spaces between them, scanned backwards and put back the right way round.
    let before = |text: &str, marker: &str| -> String {
        let head = text.split_once(marker).map(|(head, _)| head).unwrap_or("");
        head.chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || *c == ' ')
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    };

    let mut wrong = Vec::new();
    for (document, what, marker, actual) in [
        ("README.md", "tests", " tests. `just ci` runs", tests),
        ("README.md", "citations", " citations across", citations),
        (
            "README.md",
            "citations",
            " of them against an index of primary sources",
            citations,
        ),
        (
            "README.md",
            "quantities",
            " quantities and instants, each of which",
            wire,
        ),
        (
            "README.md",
            "dependencies",
            " declared dependencies,",
            dependencies,
        ),
        // The architecture notes state the test count too. They are internal, so
        // a checkout without them is not a failure — but a checkout *with* them
        // must not carry a figure the build contradicts.
        ("concepts/OVERVIEW.md", "tests", " tests, `just ci`", tests),
    ] {
        let path = root.join(document);
        let internal = document.starts_with("concepts/");
        if internal && !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;

        // Every occurrence, not the first: the same figure is stated more than
        // once and the copies have drifted apart from each other before.
        let mut rest = text.as_str();
        let mut found = 0usize;
        while let Some(at) = rest.find(marker) {
            found += 1;
            match digits(&before(rest, marker)).parse::<usize>() {
                Ok(claimed) if claimed == actual => {}
                Ok(claimed) => wrong.push(format!(
                    "  {document} {what}: it says {claimed}, the build counts {actual}"
                )),
                Err(_) => wrong.push(format!(
                    "  {document} {what}: no number before \"{}\"",
                    marker.trim()
                )),
            }
            rest = &rest[at + marker.len()..];
        }
        if found == 0 {
            wrong.push(format!(
                "  {document} {what}: the sentence this guard reads is gone, \
                 so the figure is unchecked"
            ));
        }
    }
    Ok(wrong)
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
fn check_wire(root: &Path) -> Result<usize> {
    let checked = wire_fields(root)?;
    println!("check-wire: {checked} quantities and instants, all naming their wire form");
    Ok(checked)
}

/// The count, without the line saying so — `check-stats` holds the README to this
/// number and must not reprint a guard that has already run in `check-all`.
fn wire_fields(root: &Path) -> Result<usize> {
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
        Ok(checked)
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
/// Every dependency a crate declares is one its source actually reaches.
///
/// # Why a guard rather than a tidy-up
///
/// A declared dependency nobody uses is not free and it is not visible. It costs
/// a **downstream** consumer a resolution and a compile — every crate here is
/// published — and it costs this workspace nothing measurable, which is exactly
/// why eight of them accumulated without anybody noticing. No test can fail for
/// one, `cargo build` is silent, and `cargo tree` only answers if asked.
///
/// One of the eight was a **layering** defect rather than weight: `hems-sim`
/// declared `hems-forecast`. Nothing used it, but the edge said the simulator —
/// "the day that happens" — may read the forecaster — "the day that was
/// expected" — and keeping those apart is the whole of D35. An unused edge is
/// how a used one arrives.
///
/// # How it decides
///
/// Textually, and deliberately so: it is a build-graph question answered from
/// the source, needing no nightly toolchain and no network. A dependency counts
/// as reached when its identifier appears as a path root (`foo::`), in a `use`,
/// in an attribute, or as an `extern crate` — anywhere in the crate's `src` and
/// `tests`. That over-accepts (a mention inside a string literal counts) and
/// never under-accepts, which is the right direction for a guard whose failure
/// mode would otherwise be a false alarm on somebody's Friday afternoon.
///
/// A dependency that is genuinely needed without being named — a feature-only
/// edge, a linked system library — says so with `# xtask: unreferenced` on its
/// own line above the entry, and the reason belongs beside it.
fn check_deps_used(root: &Path) -> Result<usize> {
    let checked = declared_dependencies(root)?;
    println!("check-deps-used: {checked} declared dependencies, every one reached");
    Ok(checked)
}

/// The count, without the line saying so. See [`wire_fields`].
fn declared_dependencies(root: &Path) -> Result<usize> {
    let mut checked = 0usize;
    let mut unused = Vec::new();

    for member in workspace_members(root)? {
        let dir = root.join(&member);
        let manifest_path = dir.join("Cargo.toml");
        let Ok(manifest) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let mut source = String::new();
        for sub in ["src", "tests", "benches", "examples"] {
            collect_rust(&dir.join(sub), &mut source);
        }

        let mut section = "";
        let mut exempt_next = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                section = trimmed;
                continue;
            }
            // The section first. An exemption comment is about the entry below
            // it, so one written anywhere else — in `[package]`, in a profile —
            // must not survive to exempt the first dependency that follows it,
            // which is a guard quietly not guarding.
            if !matches!(
                section,
                "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
            ) {
                exempt_next = false;
                continue;
            }
            if trimmed.contains("# xtask: unreferenced") {
                exempt_next = true;
                continue;
            }
            let Some(name) = trimmed.split('=').next().map(str::trim) else {
                continue;
            };
            if name.is_empty() || trimmed.starts_with('#') || !trimmed.contains('=') {
                continue;
            }
            if std::mem::take(&mut exempt_next) {
                continue;
            }
            checked += 1;
            let ident = name.replace('-', "_");
            if !reaches(&source, &ident) {
                unused.push(format!(
                    "  {member}: `{name}` is declared and never reached"
                ));
            }
        }
    }

    if unused.is_empty() {
        return Ok(checked);
    }
    eprintln!("check-deps-used: a manifest promises what its source does not use:");
    for line in &unused {
        eprintln!("{line}");
    }
    bail!("remove it, or mark it `# xtask: unreferenced` with the reason")
}

/// Whether `source` reaches the crate named by `ident`.
///
/// Four shapes, and the last two are what make an attribute-only dependency —
/// `#[derive(Foo)]`, `#[serde(…)]` — count as reached.
fn reaches(source: &str, ident: &str) -> bool {
    let hit = |needle: String| source.contains(&needle);
    hit(format!("{ident}::"))
        || hit(format!("use {ident}"))
        || hit(format!("extern crate {ident}"))
        || hit(format!("[{ident}"))
        || hit(format!("({ident}"))
}

/// Concatenate every `.rs` file under `dir` into `out`.
fn collect_rust(dir: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            out.push_str(&text);
            out.push('\n');
        }
    }
}

/// The workspace's member directories, in the order the root manifest lists them.
fn workspace_members(root: &Path) -> Result<Vec<String>> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let Some(start) = manifest.find("members = [") else {
        bail!("the workspace manifest has no member list")
    };
    let rest = &manifest[start..];
    let Some(end) = rest.find(']') else {
        bail!("the workspace manifest's member list is unterminated")
    };
    Ok(rest[..end]
        .split(',')
        .filter_map(|token| {
            let token = token.trim();
            token
                .strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .map(str::to_owned)
        })
        .collect())
}

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
