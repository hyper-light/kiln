//! CP-T1.5 to CP-T1.7 (G9 step 2): every record kiln keeps, before it moves to Records on Amphora.
//! docs/records-inventory.md lists every SQLite table kiln's code creates (and the migration or
//! file that drops or renames it, if one does) and every JSON file kiln writes; this test reads the
//! code the same way each time and fails when the file and the code differ, so a new table or JSON
//! record can't appear without its entry. `scripts/records-inventory.sh` writes the file again
//! (this test with `KILN_BLESS=1`).
//!
//! What counts, read from source text, not from a running kiln:
//! - a table: `CREATE TABLE`, `CREATE TEMP TABLE`, `CREATE VIRTUAL TABLE` (with or without
//!   `IF NOT EXISTS`) in a `.rs` file under a crate's `src`, or in any case in a migration's `.sql`,
//!   outside comments; `ALTER TABLE a RENAME TO b` creates `b` and drops `a`; `DROP TABLE` drops.
//!   A name built at run time (`{name}`) is listed as the code writes it; one the reader can't
//!   find on the keyword's line is listed as `?`, to be read by a person.
//! - a JSON record: a `.json` file name in a source file that both encodes JSON (`serde_json::to_`)
//!   and writes a file (`fs::write`, `File::create`, `OpenOptions`, `write_atomic`).
//! - test only: under a `tests` folder, in a `tests.rs`, or in an item under a `#[cfg(..)]` that
//!   holds only in tests (`test`, `all(test, ..)`, an `any` of those), from the attribute to the
//!   item's closing brace, or its `;`; braces in strings, characters and comments don't count. A
//!   drop made only in tests says so.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const OUT: &str = "docs/records-inventory.md";
/// Where the file's part written by hand starts (the moves' designs): kept as it is when the file is
/// written again, and not read against the code.
const BY_HAND: &str = "\n## Designed by hand\n";

/// How each JSON file kiln writes (not tests only) is kept once kiln's records are on Amphora: kiln's own state, which
/// kiln writes and reads back, as Records (`crates/kiln/src/records/files.rs` carries each, and its test holds this to
/// it); a format another program reads, lab output, scratch for one leg or push, or a cache, as a file. A new file
/// fails [`cp_t1_5_every_table_and_json_record_has_its_inventory_entry`] until it has its line here.
const KEPT: &[(&str, &str)] = &[
    ("../../ui/tests/bp-measure.json", "lab output: a file"),
    (".gemini/antigravity-cli/settings.json", "a file: Antigravity's own settings, in its home"),
    (".gemini/config/hooks.json", "a file: Antigravity's hooks, in its home"),
    (".gemini/config/mcp_config.json", "a file: Antigravity's MCP configuration, in its home"),
    (".keep.json", "Records `kiln.files`"),
    (".sigstore.json", "a file: a Sigstore bundle, for verifiers"),
    ("/vm.json", "a file: Firecracker's VM configuration"),
    ("_kiln_build_{idx}.json", "scratch: BuildKit's metadata of one leg's build, gone with it"),
    ("agent-handover.json", "Records `kiln.files`"),
    ("agent-limit.json", "Records `kiln.files`"),
    ("attachments/uploads/{id}.json", "not a file: an upload's metadata, a key in kiln's blob store (kiln-seams' Blobs), which moves with its blobs"),
    ("bar.json", "lab output: a file"),
    ("binaries/kiln-{target}.json", "not a file: a release binary's metadata, a key in kiln's blob store (kiln-seams' Blobs), which moves with its blobs"),
    ("builds.json", "Records `kiln.files`"),
    ("clouds.json", "Records `kiln.files`"),
    ("config.json", "a file: Docker's credentials configuration"),
    ("config.v2.json", "a file: Docker's container configuration, only read"),
    ("context.json", "scratch: one push's signature check, gone with it"),
    ("copies.json", "Records `kiln.files`"),
    ("epochs.json", "Records `kiln.files`"),
    ("estimates.json", "a file: criterion's output, only read"),
    ("event.json", "a file: GitHub's event payload, for a leg's actions"),
    ("eventhubs-{name}.json", "Records `kiln.files`"),
    ("extension/manifest.json", "a file: a browser extension's manifest"),
    ("handovers-decided.json", "Records `kiln.files`"),
    ("handovers-taken.json", "Records `kiln.files`"),
    ("harness-models.json", "Records `kiln.files`"),
    ("harness.json", "Records `kiln.files`"),
    ("hold.json", "lab output: a file"),
    ("holders.json", "Records `kiln.files`"),
    ("identity.json", "a file: this device's sign-in and its key, a secret kept 0600 on its machine (`kiln login`)"),
    ("indexes.json", "Records `kiln.files`"),
    ("instance.json", "Records `kiln.files`"),
    ("integrations.json", "scratch: one leg's integrations, for `kiln auth` in it, gone with it"),
    ("jail.json", "a file: the jailer's configuration of a VM"),
    ("job.json", "lab output: a file"),
    ("keep-off.json", "Records `kiln.files`"),
    ("kiln-mcp.json", "a file: the agents' MCP configuration"),
    ("kiln-{t}.json", "a file: a binary kept the old way (`<data>/binaries`), taken into kiln's blob store as the server starts"),
    ("lane.json", "Records `kiln.files`"),
    ("local.json", "Records `kiln.files`"),
    ("manifest.json", "a file: a browser extension's manifest, or an image's (docker save), only read"),
    ("member.json", "Records `kiln.files`"),
    ("members.json", "Records `kiln.files`"),
    ("mirrors.json", "Records `kiln.files`"),
    ("moved-git.json", "Records `kiln.files`"),
    ("names.json", "Records `kiln.files`"),
    ("next.json", "Records `kiln.files`"),
    ("openapi.json", "not a file: the API's answer to GET /api/openapi.json"),
    ("pace.json", "Records `kiln.files`"),
    ("package.json", "scratch: a session's package, taken by Host::adopt_package, gone with it"),
    ("pins.json", "Records `kiln.files`"),
    ("plugin.json", "a file: Claude Code's plugin manifest"),
    ("profile-checks.json", "lab output: a file"),
    ("provenance.sigstore.json", "a file: a Sigstore bundle, for verifiers"),
    ("reads.json", "a cache: rustc's reads, under ~/.cache/kiln/rustc, made again when gone"),
    ("record.json", "lab output: a file"),
    ("rehydrations.json", "Records `kiln.files`"),
    ("replaces.json", "Records `kiln.files`"),
    ("result.json", "lab output: a file"),
    ("run.json", "lab output: a file"),
    ("runner-engine.json", "Records `kiln.files`"),
    ("runs.json", "Records `kiln.files`"),
    ("sample.json", "a file: criterion's output, only read"),
    ("sbom.cdx.json", "a file: a CycloneDX SBOM, for other tools"),
    ("sbom.sigstore.json", "a file: a Sigstore bundle, for verifiers"),
    ("sbom.spdx.json", "a file: an SPDX SBOM, for other tools"),
    ("scrapes.json", "Records `kiln.files`"),
    ("search-meta.json", "Records `kiln.files`"),
    ("shaped-{}.json", "lab output: a file"),
    ("supervise-{}.json", "Records `kiln.files`"),
    ("target/ci-bench.json", "lab output: a file"),
    ("tasks.json", "Records `kiln.files`"),
    ("terminal-handover.json", "Records `kiln.files`"),
    ("times_ms.json", "lab output: a file"),
    ("told.json", "Records `kiln.files`"),
    ("ui/package.json", "not written: a check that a folder is kiln's checkout (updates.rs)"),
    ("update-available.json", "a file: a note that an update is ready, for the person; kiln doesn't read it back"),
    ("vm.json", "a file: Firecracker's VM configuration"),
    ("{base}/index.json", "not a file: an image index fetched over HTTP"),
    ("{dir}.d/osi.json", "a file: a Shards image's configuration, only read"),
    ("{hex}.config.json", "a cache: an image's configuration beside its root file system, made again when gone"),
    ("{hex}.json", "a cache: an image's manifest beside its root file system, made again when gone"),
    ("{id}.json", "Records `kiln.files` (host.rs: archived sessions; visuals.rs); scratch (runtime.rs: a microVM's call)"),
    ("{key}.json", "Records `kiln.files`"),
    ("{name}.json", "Records `kiln.files`"),
    ("{n}.json", "Records `kiln.files`"),
    ("{owner}.json", "Records `kiln.files`"),
    ("{project}.json", "Records `kiln.files`"),
    ("{public_id}.json", "Records `kiln.files`"),
    ("{safe}-{}.json", "Records `kiln.files`"),
    ("{safe}.json", "a file: a session's MCP configuration, for its agent"),
    ("{}-gcp.json", "a file: Google Cloud's credentials, for a leg's tools"),
    ("{}-{}.json", "Records `kiln.files`"),
    ("{}.json", "Records `kiln.files` (kept.rs, firecracker.rs, place.rs); scratch (pushsig.rs: a push's verdicts)"),
    ("{}/.gemini/config/mcp_config.json", "a file: Antigravity's MCP configuration, in its guest's home"),
];

/// Where each table is created and dropped (each file with whether only its tests drop it), and
/// whether only tests make it.
#[derive(Default)]
struct Table {
    created: BTreeSet<String>,
    dropped: BTreeMap<String, bool>,
    test_only: bool,
}

fn repo() -> Result<PathBuf, String> {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.parent().and_then(Path::parent).map(Path::to_path_buf).ok_or_else(|| format!("no repository above {}", here.display()))
}

/// Every `.rs` under each crate's `src`, and every `.sql` under kiln's migrations, sorted.
fn sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let crates = root.join("crates");
    let mut stack = Vec::new();
    for entry in std::fs::read_dir(&crates).map_err(|e| format!("{}: {e}", crates.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        stack.push(entry.path().join("src"));
    }
    let mut out = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                stack.push(path);
            } else if matches!(path.extension().and_then(|e| e.to_str()), Some("rs" | "sql")) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The path as the inventory names it: from the repository's root, with forward slashes.
fn rel(root: &Path, path: &Path) -> String {
    let p = path.strip_prefix(root).unwrap_or(path);
    p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
}

/// The identifier at the start of `s`: quotes dropped, a schema's `main.` or `temp.` kept off.
fn ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let name: String = s.chars().skip_while(|c| matches!(c, '"' | '`' | '[')).take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '{' | '}')).collect();
    let name = name.strip_prefix("main.").or_else(|| name.strip_prefix("temp.")).unwrap_or(&name).to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// The text after `if not exists` / `if exists`, when it starts with it.
fn after_if(s: &str) -> &str {
    let t = s.trim_start();
    let low = t.to_ascii_lowercase();
    for skip in ["if not exists", "if exists"] {
        if low.starts_with(skip) {
            return t.get(skip.len()..).unwrap_or("");
        }
    }
    t
}

/// The top-level arguments of a `cfg` predicate's `all(..)` or `any(..)`.
fn cfg_args(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start) = (Vec::new(), 0usize, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(s.get(start..i).unwrap_or(""));
                start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    out.push(s.get(start..).unwrap_or(""));
    out.into_iter().filter(|a| !a.trim().is_empty()).collect()
}

/// Whether a `cfg` predicate holds only in tests: `test`, an `all(..)` with one such in it, or an
/// `any(..)` of nothing else.
fn only_in_tests(p: &str) -> bool {
    let p = p.trim();
    let args = |f: &str| p.strip_prefix(f).and_then(|r| r.strip_suffix(')')).map(cfg_args);
    if p == "test" {
        true
    } else if let Some(a) = args("all(") {
        a.into_iter().any(only_in_tests)
    } else if let Some(a) = args("any(") {
        !a.is_empty() && a.into_iter().all(only_in_tests)
    } else {
        false
    }
}

/// Whether a line starts with a `#[cfg(..)]` that holds only in tests (`#[cfg(all(test, unix))]`).
fn cfg_test(t: &str) -> bool {
    let Some(rest) = t.strip_prefix("#[cfg(") else { return false };
    let mut depth = 1usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth = depth.saturating_add(1),
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return rest.get(..i).is_some_and(only_in_tests);
                }
            }
            _ => {}
        }
    }
    false
}

/// Where a line of Rust leaves off, for the next: in code, a string, a raw string (with its `#`s)
/// or block comments (nested so deep).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Lex {
    #[default]
    Code,
    Str,
    Raw(usize),
    Block(usize),
}

/// The `{` and `}` a line of Rust opens and closes as code: none in a string, a character or a
/// comment. `lex` is where the line before left off, and becomes where this one does.
fn braces(line: &str, lex: &mut Lex) -> (i64, i64) {
    let b: Vec<char> = line.chars().collect();
    let at = |i: usize| b.get(i).copied();
    let (mut opens, mut closes, mut i) = (0i64, 0i64, 0usize);
    while let Some(c) = at(i) {
        let next = at(i.saturating_add(1));
        match *lex {
            Lex::Str if c == '\\' => i = i.saturating_add(1),
            Lex::Str if c == '"' => *lex = Lex::Code,
            Lex::Str => {}
            Lex::Raw(n) => {
                if c == '"' && (1..=n).all(|k| at(i.saturating_add(k)) == Some('#')) {
                    *lex = Lex::Code;
                    i = i.saturating_add(n);
                }
            }
            Lex::Block(d) => {
                if c == '*' && next == Some('/') {
                    *lex = if d <= 1 { Lex::Code } else { Lex::Block(d.saturating_sub(1)) };
                    i = i.saturating_add(1);
                } else if c == '/' && next == Some('*') {
                    *lex = Lex::Block(d.saturating_add(1));
                    i = i.saturating_add(1);
                }
            }
            Lex::Code => match c {
                '/' if next == Some('/') => break,
                '/' if next == Some('*') => {
                    *lex = Lex::Block(1);
                    i = i.saturating_add(1);
                }
                '"' => *lex = Lex::Str,
                // A raw string's `r` (or `br`, `cr`), not the end of a longer name.
                'r' if matches!(next, Some('"' | '#')) => {
                    let word = b.get(..=i).unwrap_or(&[]).iter().rev().take_while(|c| c.is_alphanumeric() || **c == '_').count();
                    let hashes = b.get(i.saturating_add(1)..).unwrap_or(&[]).iter().take_while(|c| **c == '#').count();
                    let quote = i.saturating_add(1).saturating_add(hashes);
                    let prefix: String = b.get(i.saturating_add(1).saturating_sub(word)..=i).unwrap_or(&[]).iter().collect();
                    if matches!(prefix.as_str(), "r" | "br" | "cr") && at(quote) == Some('"') {
                        *lex = Lex::Raw(hashes);
                        i = quote;
                    }
                }
                // A character (`'{'`, `'\''`, `'\u{7b}'`); a lifetime (`'a`) is left as it is.
                '\'' => {
                    if next == Some('\\') {
                        i = (i.saturating_add(3)..b.len()).find(|&j| at(j) == Some('\'')).unwrap_or(b.len());
                    } else if at(i.saturating_add(2)) == Some('\'') {
                        i = i.saturating_add(2);
                    }
                }
                '{' => opens = opens.saturating_add(1),
                '}' => closes = closes.saturating_add(1),
                _ => {}
            },
        }
        i = i.saturating_add(1);
    }
    (opens, closes)
}

/// Each line's text that isn't a comment, with whether it is test code: in a test file, or in the
/// item a `#[cfg(..)]` that holds only in tests is on, from the attribute to the item's end (its braces closed again, its
/// `;` when it has none, or the braces around it closed). Code after that item is kiln's again: an attribute on one helper near
/// a file's top doesn't make the tables the file goes on to create (kiln's store's `SCHEMA`) a
/// test's.
fn code_lines(text: &str, sql: bool, test_file: bool) -> Vec<(String, bool)> {
    // The brace depth at the `#[cfg(test)]` whose item the reader is in, and whether that item's
    // braces have opened yet.
    let mut item: Option<(i64, bool)> = None;
    let mut depth: i64 = 0;
    let mut lex = Lex::Code;
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if !sql && item.is_none() && lex == Lex::Code && cfg_test(t) {
            item = Some((depth, false));
        }
        let comment = (lex == Lex::Code && t.starts_with("//")) || (sql && t.starts_with("--"));
        if !comment {
            let code = if sql { t.split("--").next().unwrap_or("") } else { t };
            out.push((code.to_string(), test_file || item.is_some()));
        }
        if sql || comment {
            continue;
        }
        let (opens, closes) = braces(t, &mut lex);
        depth = depth.saturating_add(opens).saturating_sub(closes);
        if let Some((at, opened)) = item.as_mut() {
            *opened |= opens > 0;
            // Its braces closed again; its `;` before any brace; or, for an attribute on a field or
            // an arm, the braces around it closed.
            if (*opened && depth <= *at) || (!*opened && t.trim_end().ends_with(';')) || depth < *at {
                item = None;
            }
        }
    }
    out
}

/// Tables a line creates, renames and drops: (created, dropped).
fn tables_in(line: &str, sql: bool) -> (Vec<String>, Vec<String>) {
    let hay = if sql { line.to_ascii_uppercase() } else { line.to_string() };
    let (mut made, mut gone) = (Vec::new(), Vec::new());
    let mut at = 0;
    while let Some(rest) = hay.get(at..) {
        let found = ["CREATE TABLE", "CREATE TEMP TABLE", "CREATE TEMPORARY TABLE", "CREATE VIRTUAL TABLE", "DROP TABLE", "ALTER TABLE"].iter().filter_map(|k| rest.find(k).map(|i| (i, *k))).min();
        let Some((i, kw)) = found else { break };
        let start = at + i + kw.len();
        let tail = line.get(start..).unwrap_or("");
        at = start;
        // The keyword ends a string literal in Rust (`sql.starts_with("CREATE VIRTUAL TABLE")`):
        // text kiln compares, not SQL it runs.
        if !sql && tail.starts_with('"') {
            continue;
        }
        if kw == "DROP TABLE" {
            gone.push(ident(after_if(tail)).unwrap_or_else(|| "?".into()));
        } else if kw == "ALTER TABLE" {
            let Some(from) = ident(tail) else { continue };
            let up = tail.to_ascii_uppercase();
            if let Some(j) = up.find("RENAME TO")
                && up.get(..j).is_some_and(|before| !before.contains("COLUMN") && before.split_whitespace().count() == 1)
                && let Some(to) = ident(tail.get(j + "RENAME TO".len()..).unwrap_or(""))
            {
                gone.push(from);
                made.push(to);
            }
        } else {
            made.push(ident(after_if(tail)).unwrap_or_else(|| "?".into()));
        }
    }
    (made, gone)
}

/// The file's string constants (`const MARK: &str = "cluster_mark";`), by lowercased name, so a
/// table named at run time from one (`{MARK}`) is listed by its value.
fn str_consts(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim_start().trim_start_matches("pub ").trim_start_matches("pub(crate) ");
        let Some(rest) = t.strip_prefix("const ") else { continue };
        let Some((name, value)) = rest.split_once(": &str = \"") else { continue };
        let Some(value) = value.strip_suffix("\";") else { continue };
        out.insert(name.trim().to_ascii_lowercase(), value.to_ascii_lowercase());
    }
    out
}

/// `.json` file names written in a file that encodes JSON and writes files.
fn json_names(lines: &[(String, bool)]) -> Vec<(String, bool)> {
    let all: String = lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
    let writes = ["fs::write", "File::create", "OpenOptions", "write_atomic"].iter().any(|w| all.contains(w));
    if !all.contains("serde_json::to_") || !writes {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (line, test) in lines {
        let mut rest = line.as_str();
        while let Some(end) = rest.find(".json\"") {
            let head = rest.get(..end).unwrap_or("");
            if let Some(q) = head.rfind('"') {
                let name = format!("{}.json", head.get(q + 1..).unwrap_or(""));
                if name.len() > ".json".len() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '{' | '}')) {
                    out.push((name, *test));
                }
            }
            rest = rest.get(end + ".json\"".len()..).unwrap_or("");
        }
    }
    out
}

fn inventory(root: &Path) -> Result<String, String> {
    let mut tables: BTreeMap<String, Table> = BTreeMap::new();
    let mut json: BTreeMap<String, (BTreeSet<String>, bool)> = BTreeMap::new();
    for path in sources(root)? {
        let name = rel(root, &path);
        if name.starts_with("crates/kiln-seams/tests/") {
            continue;
        }
        let sql = name.ends_with(".sql");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{name}: {e}"))?;
        let test_file = name.contains("/tests/") || name.ends_with("/tests.rs") || name.ends_with("_tests.rs");
        let lines = code_lines(&text, sql, test_file);
        let consts = str_consts(&text);
        let named = |t: String| t.strip_prefix('{').and_then(|n| n.strip_suffix('}')).and_then(|n| consts.get(n)).cloned().unwrap_or(t);
        for (line, test) in &lines {
            let (made, gone) = tables_in(line, sql);
            let (made, gone): (Vec<String>, Vec<String>) = (made.into_iter().map(named).collect(), gone.into_iter().map(named).collect());
            for t in made {
                let e = tables.entry(t).or_insert_with(|| Table { test_only: true, ..Table::default() });
                e.created.insert(name.clone());
                e.test_only &= *test;
            }
            for t in gone {
                let by_tests = tables.entry(t).or_insert_with(|| Table { test_only: true, ..Table::default() }).dropped.entry(name.clone()).or_insert(true);
                *by_tests &= *test;
            }
        }
        if !sql {
            for (j, test) in json_names(&lines) {
                let e = json.entry(j).or_insert_with(|| (BTreeSet::new(), true));
                e.0.insert(name.clone());
                e.1 &= test;
            }
        }
    }
    let join = |s: &BTreeSet<String>| s.iter().map(|x| format!("`{x}`")).collect::<Vec<_>>().join(", ");
    let drops = |d: &BTreeMap<String, bool>| d.iter().map(|(x, tests)| format!("`{x}`{}", if *tests { " (tests only)" } else { "" })).collect::<Vec<_>>().join(", ");
    let mut out = String::from(
        "# Every record kiln keeps\n\n\
         CP-T1.5 to CP-T1.7 (G9 step 2): what moves to Records on Amphora. Written by\n\
         `scripts/records-inventory.sh`; `crates/kiln-seams/tests/records_inventory.rs` fails when this\n\
         file and the code differ, and says how it reads the code. A table only tests make, or one\n\
         dropped by a later migration, is listed too, and says so; so does a drop only tests make. Each\n\
         JSON file says how it's kept on Amphora: as Records (`crates/kiln/src/records/files.rs`), or as\n\
         a file, and why. The part after \"Designed by hand\" is written by hand.\n\n## SQLite tables\n\n\
         | Table | Created in | Dropped or renamed in | Tests only |\n|---|---|---|---|\n",
    );
    for (name, t) in &tables {
        if t.created.is_empty() {
            continue;
        }
        out.push_str(&format!("| `{name}` | {} | {} | {} |\n", join(&t.created), drops(&t.dropped), if t.test_only { "yes" } else { "" }));
    }
    out.push_str("\n## JSON files kiln writes\n\n| File | Written in | Tests only | Kept as |\n|---|---|---|---|\n");
    let kept: BTreeMap<&str, &str> = KEPT.iter().copied().collect();
    let mut unsaid = Vec::new();
    for (name, (files, test)) in &json {
        let how = if *test { "" } else { kept.get(name.as_str()).copied().unwrap_or_else(|| { unsaid.push(name.clone()); "?" }) };
        out.push_str(&format!("| `{name}` | {} | {} | {how} |\n", join(files), if *test { "yes" } else { "" }));
    }
    if !unsaid.is_empty() {
        return Err(format!("JSON files kiln writes with no line in KEPT (say how each is kept): {unsaid:?}"));
    }
    let stale: Vec<&&str> = kept.keys().filter(|k| !json.get(**k).is_some_and(|(_, test)| !test)).collect();
    if !stale.is_empty() {
        return Err(format!("KEPT names files kiln no longer writes (or only its tests do): {stale:?}"));
    }
    Ok(out)
}

/// CP-T1.5 (G9 step 2): the inventory is the code's, table by table and record by record.
#[test]
fn cp_t1_5_every_table_and_json_record_has_its_inventory_entry() -> Result<(), String> {
    let root = repo()?;
    let want = inventory(&root)?;
    let path = root.join(OUT);
    let read = std::fs::read_to_string(&path).map(|t| t.replace("\r\n", "\n"));
    // The part written by hand, from its heading on: kept, and not read against the code.
    let (have, by_hand) = match &read {
        Ok(t) => match t.find(BY_HAND) {
            Some(at) => (t.get(..at).unwrap_or("").to_owned(), t.get(at..).unwrap_or("").to_owned()),
            None => (t.clone(), String::new()),
        },
        Err(_) => (String::new(), String::new()),
    };
    if std::env::var_os("KILN_BLESS").is_some() {
        return std::fs::write(&path, format!("{want}{by_hand}")).map_err(|e| format!("{}: {e}", path.display()));
    }
    if let Err(e) = &read {
        return Err(format!("{}: {e} (write it with scripts/records-inventory.sh)", path.display()));
    }
    if have == want {
        return Ok(());
    }
    let (h, w): (BTreeSet<&str>, BTreeSet<&str>) = (have.lines().collect(), want.lines().collect());
    let missing: Vec<&&str> = w.difference(&h).collect();
    let extra: Vec<&&str> = h.difference(&w).collect();
    Err(format!("{OUT} differs from the code: run scripts/records-inventory.sh.\nmissing: {missing:#?}\nno longer in the code: {extra:#?}"))
}

/// The reader finds what it should and nothing in comments.
#[test]
fn cp_t1_5_the_reader_finds_creates_renames_and_drops() {
    assert_eq!(tables_in("CREATE TABLE IF NOT EXISTS \"runs\" (id)", false), (vec!["runs".to_string()], vec![]));
    assert_eq!(tables_in("create table t(x); drop table if exists u;", true), (vec!["t".to_string()], vec!["u".to_string()]));
    assert_eq!(tables_in("ALTER TABLE a RENAME TO b", true), (vec!["b".to_string()], vec!["a".to_string()]));
    assert_eq!(tables_in("ALTER TABLE a RENAME COLUMN x TO y", true), (vec![], vec![]));
    assert_eq!(tables_in("ALTER TABLE a ADD COLUMN y", true), (vec![], vec![]));
    assert_eq!(tables_in("sql.to_ascii_uppercase().starts_with(\"CREATE VIRTUAL TABLE\")", false), (vec![], vec![]));
    assert_eq!(str_consts("pub const MARK: &str = \"cluster_mark\";").get("mark").map(String::as_str), Some("cluster_mark"));
    assert_eq!(code_lines("// CREATE TABLE prose\nx", false, false).len(), 1);
    let lines = code_lines("serde_json::to_vec(&x); fs::write(dir.join(\"state.json\"), b);\n#[cfg(test)]\nlet p = \"t.json\";", false, false);
    assert_eq!(json_names(&lines), vec![("state.json".to_string(), false), ("t.json".to_string(), true)]);
}

/// A `#[cfg(test)]` makes test code of its item alone: a helper's attribute near a file's top
/// leaves the tables the file creates after it kiln's, and a test module's tables stay tests'.
#[test]
fn cp_t1_5_a_cfg_test_covers_its_item_alone() {
    let text = "struct S {\n    #[cfg(test)]\n    inline: u8,\n}\n#[cfg(test)]\nuse std::fmt;\n#[cfg(test)]\nfn helper() {\n    let t = \"CREATE TABLE a (x)\";\n}\nconst SCHEMA: &str = \"CREATE TABLE repos (id)\";\n#[cfg(test)]\nmod tests {\n    fn t() {\n        let f = format!(\"{n}\");\n    }\n    const B: &str = \"CREATE TABLE b (x)\";\n}\nconst C: &str = \"CREATE TABLE c (x)\";";
    let tests: Vec<(&str, bool)> = code_lines(text, false, false).iter().filter_map(|(l, test)| ["inline", "use std", "CREATE TABLE a", "repos", "CREATE TABLE b", "CREATE TABLE c"].into_iter().find(|w| l.contains(w)).map(|w| (w, *test))).collect();
    assert_eq!(tests, vec![("inline", true), ("use std", true), ("CREATE TABLE a", true), ("repos", false), ("CREATE TABLE b", true), ("CREATE TABLE c", false)]);
}

/// CP-T1.5: a `#[cfg(..)]` that holds only in tests makes test code of its item, whatever else it
/// asks (`all(test, target_os = "linux")`, as headroom/pace.rs's Linux tests have it); one that
/// also holds outside tests doesn't.
#[test]
fn cp_t1_5_a_cfg_that_holds_only_in_tests_makes_test_code() {
    for (attr, tests) in [
        ("#[cfg(test)]", true),
        ("#[cfg(test)]mod tests{", true),
        ("#[cfg(all(test, target_os = \"linux\"))]", true),
        ("#[cfg(all(test, any(target_os = \"linux\", target_os = \"macos\")))]", true),
        ("#[cfg(any(test, all(test, windows)))]", true),
        ("#[cfg(any(unix, test))]", false),
        ("#[cfg(all(unix, any(test, feature = \"chaos\")))]", false),
        ("#[cfg(not(test))]", false),
        ("#[cfg_attr(test, derive(Debug))]", false),
    ] {
        assert_eq!(cfg_test(attr), tests, "{attr}");
    }
    let text = "#[cfg(all(test, target_os = \"linux\"))]\nmod tests {\n    const B: &str = \"CREATE TABLE b (x)\";\n}\nconst C: &str = \"CREATE TABLE c (x)\";";
    let tests: Vec<(&str, bool)> = code_lines(text, false, false).iter().filter_map(|(l, test)| ["CREATE TABLE b", "CREATE TABLE c"].into_iter().find(|w| l.contains(w)).map(|w| (w, *test))).collect();
    assert_eq!(tests, vec![("CREATE TABLE b", true), ("CREATE TABLE c", false)]);
}

/// CP-T1.5: braces in strings, raw strings, characters and comments aren't the code's, so a test
/// module with a `"}"` in it is test code to its end.
#[test]
fn cp_t1_5_braces_in_strings_characters_and_comments_dont_count() {
    let mut lex = Lex::Code;
    assert_eq!(braces(r#"let s = "}{}\"}"; let c = '}'; let e = '\''; let u = '\u{7d}'; f::<'a>() { // }"#, &mut lex), (1, 0));
    assert_eq!(lex, Lex::Code);
    assert_eq!(braces("let s = r#\"a \"} {", &mut lex), (0, 0));
    assert_eq!(lex, Lex::Raw(1));
    assert_eq!(braces("}\"#; x {", &mut lex), (1, 0));
    assert_eq!(braces("/* { /* } */ } */ }", &mut lex), (0, 1));
    assert_eq!(braces("let s = \"a {", &mut lex), (0, 0));
    assert_eq!(braces("} b\"; }", &mut lex), (0, 1));
    let text = "#[cfg(test)]\nmod tests {\n    fn t() {\n        let close = \"}\";\n    }\n    const B: &str = \"CREATE TABLE b (x)\";\n}\nconst C: &str = \"CREATE TABLE c (x)\";";
    let tests: Vec<(&str, bool)> = code_lines(text, false, false).iter().filter_map(|(l, test)| ["CREATE TABLE b", "CREATE TABLE c"].into_iter().find(|w| l.contains(w)).map(|w| (w, *test))).collect();
    assert_eq!(tests, vec![("CREATE TABLE b", true), ("CREATE TABLE c", false)]);
}
