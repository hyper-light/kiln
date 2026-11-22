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
//! - test only: under a `tests` folder, in a `tests.rs`, or after the file's first `#[cfg(test)]`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const OUT: &str = "docs/records-inventory.md";

/// Where each table is created and dropped, and whether only tests make it.
#[derive(Default)]
struct Table {
    created: BTreeSet<String>,
    dropped: BTreeSet<String>,
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

/// Each line's text that isn't a comment, with whether it is test code.
fn code_lines(text: &str, sql: bool, test_file: bool) -> Vec<(String, bool)> {
    let mut test = test_file;
    let mut out = Vec::new();
    for line in text.lines() {
        if !sql && line.trim_start().starts_with("#[cfg(test)]") {
            test = true;
        }
        let t = line.trim_start();
        if t.starts_with("//") || (sql && t.starts_with("--")) {
            continue;
        }
        let code = if sql { t.split("--").next().unwrap_or("") } else { t };
        out.push((code.to_string(), test));
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
                tables.entry(t).or_insert_with(|| Table { test_only: true, ..Table::default() }).dropped.insert(name.clone());
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
    let mut out = String::from(
        "# Every record kiln keeps\n\n\
         CP-T1.5 to CP-T1.7 (G9 step 2): what moves to Records on Amphora. Written by\n\
         `scripts/records-inventory.sh`; `crates/kiln-seams/tests/records_inventory.rs` fails when this\n\
         file and the code differ, and says how it reads the code. A table only tests make, or one\n\
         dropped by a later migration, is listed too, and says so.\n\n## SQLite tables\n\n\
         | Table | Created in | Dropped or renamed in | Tests only |\n|---|---|---|---|\n",
    );
    for (name, t) in &tables {
        if t.created.is_empty() {
            continue;
        }
        out.push_str(&format!("| `{name}` | {} | {} | {} |\n", join(&t.created), join(&t.dropped), if t.test_only { "yes" } else { "" }));
    }
    out.push_str("\n## JSON files kiln writes\n\n| File | Written in | Tests only |\n|---|---|---|\n");
    for (name, (files, test)) in &json {
        out.push_str(&format!("| `{name}` | {} | {} |\n", join(files), if *test { "yes" } else { "" }));
    }
    Ok(out)
}

/// CP-T1.5 (G9 step 2): the inventory is the code's, table by table and record by record.
#[test]
fn cp_t1_5_every_table_and_json_record_has_its_inventory_entry() -> Result<(), String> {
    let root = repo()?;
    let want = inventory(&root)?;
    let path = root.join(OUT);
    if std::env::var_os("KILN_BLESS").is_some() {
        return std::fs::write(&path, &want).map_err(|e| format!("{}: {e}", path.display()));
    }
    let have = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e} (write it with scripts/records-inventory.sh)", path.display()))?.replace("\r\n", "\n");
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
