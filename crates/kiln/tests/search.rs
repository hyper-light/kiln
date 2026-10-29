//! The search across a change (docs/ci.md W26-R07), against a server of its own with real git: a
//! native repository of 1,000 files (Rust, TypeScript, Python), a diff changing two of them, and
//! the diff searched by text and by code: each hit with its file, line, the definition it sits in
//! and its text, the changed lines first; a query nothing answers; a version whose index isn't built
//! yet, built by its first search; 50 searches of a warm index within 250 ms at p95; and the
//! indexes dropped when the diff is abandoned.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {}: {out}", args.join(" "));
    Ok(out)
}

/// A user with `grants`, and a personal access token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// `s` as a query string's value.
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The lines of `src/expand.rs` as the diff has it: line 22, in `fn expand`, is the diff's.
const EXPAND: &[&str] = &[
    "//! Expanding a matrix into its values (the search fixture of docs/ci.md W26-R07).",
    "",
    "use crate::matrix::Matrix;",
    "",
    "/// How many values a row may hold.",
    "pub const ROW_MAX: usize = 4096;",
    "",
    "/// What expanding left out.",
    "pub struct Report {",
    "    pub skipped: usize,",
    "}",
    "",
    "/// The values of `m`, row by row: each row copied once, the matrix left as it was.",
    "pub fn expand(m: &Matrix) -> Vec<f64> {",
    "    let mut out = Vec::new();",
    "    for row in m.rows() {",
    "        if row.len() > ROW_MAX {",
    "            continue;",
    "        }",
    "        out.extend_from_slice(row);",
    "    }",
    "    // an empty matrix expands to nothing: out stays empty",
    "    out",
    "}",
];

/// The lines of `src/matrix.rs` on trunk; the diff adds `is_empty` after them. Line 40, in
/// `fn matrix_of`, isn't the diff's.
const MATRIX: &[&str] = &[
    "//! A matrix of rows (the search fixture of docs/ci.md W26-R07).",
    "",
    "/// Rows of values, each as long as the first.",
    "#[derive(Debug, Clone, Default, PartialEq)]",
    "pub struct Matrix {",
    "    rows: Vec<Vec<f64>>,",
    "}",
    "",
    "/// Why a matrix couldn't be made.",
    "#[derive(Debug, Clone, PartialEq)]",
    "pub enum Ragged {",
    "    /// Row `row` has `len` values, not `want`.",
    "    Row { row: usize, len: usize, want: usize },",
    "}",
    "",
    "impl Matrix {",
    "    /// Its rows.",
    "    pub fn rows(&self) -> impl Iterator<Item = &[f64]> {",
    "        self.rows.iter().map(Vec::as_slice)",
    "    }",
    "",
    "    /// How many rows it has.",
    "    pub fn height(&self) -> usize {",
    "        self.rows.len()",
    "    }",
    "",
    "    /// How many values a row has.",
    "    pub fn width(&self) -> usize {",
    "        self.rows.first().map_or(0, Vec::len)",
    "    }",
    "}",
    "",
    "/// A matrix of `rows`, each as long as the first.",
    "pub fn matrix_of(rows: Vec<Vec<f64>>) -> Result<Matrix, Ragged> {",
    "    let want = rows.first().map_or(0, Vec::len);",
    "    for (row, r) in rows.iter().enumerate() {",
    "        if r.len() != want {",
    "            return Err(Ragged::Row { row, len: r.len(), want });",
    "        }",
    "        // a row of none is an empty matrix's, and fits any width",
    "    }",
    "    Ok(Matrix { rows })",
    "}",
];

const MAIN: &str = "mod expand;\nmod matrix;\n\nuse crate::expand::expand;\n\nfn main() {\n    let m = matrix::matrix_of(vec![vec![1.0, 2.0], vec![3.0, 4.0]]).unwrap_or_default();\n    let all = expand(&m);\n    let none = expand(&matrix::Matrix::default()).len();\n    println!(\"{} {none}\", all.len());\n}\n";

fn rust_part(i: usize) -> String {
    let mut s = format!("//! Part {i} of the generated tree (the search fixture of docs/ci.md W26-R07).\n\nuse std::collections::BTreeMap;\n\n");
    for f in 0..6 {
        s.push_str(&format!("/// Folds row set {i}.{f} into a total.\npub fn fold_{i}_{f}(values: &[f64]) -> f64 {{\n    let mut total = 0.0;\n    for v in values {{\n        total += v * {f}.5;\n    }}\n    total\n}}\n\n"));
    }
    s.push_str(&format!("pub struct Part{i} {{\n    pub names: BTreeMap<String, usize>,\n}}\n\nimpl Part{i} {{\n    pub fn name_of(&self, key: &str) -> Option<usize> {{\n        self.names.get(key).copied()\n    }}\n}}\n"));
    if i == 7 {
        s.push_str("\npub fn total_of_none() -> f64 {\n    // an empty matrix folds to zero\n    0.0\n}\n");
    }
    if i.is_multiple_of(50) {
        s.push_str(&format!("\npub fn spread_{i}(grid: &crate::matrix::Matrix) -> usize {{\n    crate::expand::expand(grid).len() + expand(grid).len()\n}}\n"));
    }
    s
}

fn ts_view(i: usize) -> String {
    let mut s = format!("// View {i} of the generated tree (the search fixture of docs/ci.md W26-R07).\nimport {{ useState }} from \"react\";\n\n");
    for f in 0..4 {
        s.push_str(&format!("export function Row{i}x{f}(props: {{ label: string; count: number }}) {{\n  const [open, setOpen] = useState(false);\n  return <li onClick={{() => setOpen(!open)}}>{{props.label}}: {{props.count * {f}}}</li>;\n}}\n\n"));
    }
    if i.is_multiple_of(60) {
        s.push_str(&format!("export function Cells{i}(cells: number[][]) {{\n  return expand(cells).length;\n}}\n"));
    }
    s
}

fn py_task(i: usize) -> String {
    let mut s = format!("\"\"\"Task {i} of the generated tree (the search fixture of docs/ci.md W26-R07).\"\"\"\n\n\n");
    for f in 0..4 {
        s.push_str(&format!("def run_{i}_{f}(values):\n    total = 0\n    for v in values:\n        total += v * {f}\n    return total\n\n\n"));
    }
    s.push_str(&format!("class Task{i}:\n    def __init__(self, name):\n        self.name = name\n\n    def describe(self):\n        return f\"task {i}: {{self.name}}\"\n"));
    s
}

/// The trunk's 1,000 files.
fn trunk() -> Vec<(String, String)> {
    let mut files = vec![
        ("src/main.rs".to_owned(), MAIN.to_owned()),
        ("src/expand.rs".to_owned(), EXPAND.iter().enumerate().filter(|(i, _)| *i != 21).map(|(_, l)| format!("{l}\n")).collect()),
        ("src/matrix.rs".to_owned(), MATRIX.iter().map(|l| format!("{l}\n")).collect()),
        ("README.md".to_owned(), "# matrix\n\nExpands matrices (the search fixture of docs/ci.md W26-R07).\n".to_owned()),
        ("Cargo.toml".to_owned(), "[package]\nname = \"matrix\"\nversion = \"0.1.0\"\nedition = \"2024\"\n".to_owned()),
    ];
    files.extend((0..640).map(|i| (format!("src/gen/part_{i:04}.rs"), rust_part(i))));
    files.extend((0..240).map(|i| (format!("ui/src/gen/view_{i:03}.tsx"), ts_view(i))));
    files.extend((0..115).map(|i| (format!("tools/gen/task_{i:03}.py"), py_task(i))));
    files
}

fn write(work: &Path, files: &[(String, String)]) -> anyhow::Result<()> {
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    Ok(())
}

fn search(s: &Server, token: &str, path: &str) -> anyhow::Result<(u16, Value)> {
    let (st, body) = http(s.port, "GET", path, token, None)?;
    Ok((st, serde_json::from_str(&body).unwrap_or_else(|_| json!({ "body": body }))))
}

fn hits(v: &Value) -> Vec<&Value> {
    v.get("hits").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
}

/// Each hit as `path:line (kind symbol)`.
fn said(v: &Value) -> Vec<String> {
    hits(v)
        .iter()
        .map(|h| {
            let s = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or("-").to_owned();
            format!("{}:{} ({} {})", s("path"), h.get("line").and_then(Value::as_u64).unwrap_or(0), s("kind"), s("symbol"))
        })
        .collect()
}

/// W26-R07: a diff's files at a version, searched by text and by code, each hit with its
/// definition and the changed lines first; within 250 ms at p95 on a warm index of 1,000 files.
#[test]
fn w26_r07_a_search_across_the_change() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &common::Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/matrix", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/matrix"])?;
    let url = format!("http://x:{dev}@127.0.0.1:{}/git/acme/matrix.git", s.port);

    // Trunk: 1,000 files.
    git(&home, dir.path(), &["clone", "-q", &url, "work"])?;
    let work = dir.path().join("work");
    let files = trunk();
    assert_eq!(files.len(), 1000);
    write(&work, &files)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the matrix, in 1,000 files"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;

    // D1: `expand` says what an empty matrix gives (line 22), and a matrix says it's empty.
    let change = "Change-Id: I4141414141414141414141414141414141414141";
    git(&home, &work, &["checkout", "-qb", "topic"])?;
    let expand: String = EXPAND.iter().map(|l| format!("{l}\n")).collect();
    let mut matrix: String = MATRIX.iter().map(|l| format!("{l}\n")).collect();
    matrix.push_str("\nimpl Matrix {\n    /// Whether it has no rows.\n    pub fn is_empty(&self) -> bool {\n        self.rows.is_empty()\n    }\n}\n");
    write(&work, &[("src/expand.rs".into(), expand), ("src/matrix.rs".into(), matrix.clone())])?;
    git(&home, &work, &["commit", "-qam", &format!("expand: an empty matrix\n\n{change}")])?;
    let out = git(&home, &work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(out.contains("D1 v1"), "{out}");

    // The first search of v1 builds its index, and lists the changed line first.
    let at = |n: u32, q: &str, more: &str| format!("/api/diffs/D1/versions/{n}/search?q={}{more}", enc(q));
    let started = Instant::now();
    let (st, v) = search(&s, &dev, &at(1, "empty matrix", ""))?;
    let first = started.elapsed();
    anyhow::ensure!(st == 200, "search: {st} {v}");
    assert_eq!(v.get("index").and_then(Value::as_str), Some("built"), "{v}");
    assert_eq!((v.get("files").and_then(Value::as_u64), v.get("skipped").and_then(Value::as_u64)), (Some(1000), Some(0)), "{v}");
    assert_eq!(said(&v), ["src/expand.rs:22 (function expand)", "src/matrix.rs:40 (function matrix_of)", "src/gen/part_0007.rs:70 (function total_of_none)"], "{v}");
    let changed: Vec<(bool, bool)> = hits(&v).iter().map(|h| (h.get("changed") == Some(&json!(true)), h.get("in_change") == Some(&json!(true)))).collect();
    assert_eq!(changed, [(true, true), (false, true), (false, false)], "the changed line, then the change's other file, then the tree: {v}");
    let h = hits(&v).first().copied().cloned().unwrap_or_default();
    assert_eq!((h.get("column"), h.get("text").and_then(Value::as_str)), (Some(&json!(11)), Some("    // an empty matrix expands to nothing: out stays empty")), "{h}");
    let tree1 = v.get("tree").and_then(Value::as_str).unwrap_or_default().to_owned();
    let index1 = dir.path().join("data/cache/search").join(&tree1);
    assert!(index1.join("table.bin").is_file(), "the index is under the server's cache: {}", index1.display());

    // The same search again reads the index built.
    let (_, again) = search(&s, &dev, &at(1, "empty matrix", ""))?;
    assert_eq!((again.get("index").and_then(Value::as_str), said(&again)), (Some("ready"), said(&v)));

    // Code: each call of `expand`, in Rust and TSX, never its definition.
    let (st, calls) = search(&s, &dev, &at(1, "expand($A)", "&mode=code"))?;
    anyhow::ensure!(st == 200, "code search: {st} {calls}");
    let places: Vec<String> = hits(&calls).iter().map(|h| format!("{}:{}", h.get("path").and_then(Value::as_str).unwrap_or_default(), h.get("line").and_then(Value::as_u64).unwrap_or(0))).collect();
    assert!(places.contains(&"src/main.rs:8".to_owned()) && places.contains(&"src/main.rs:9".to_owned()), "{places:?}");
    assert!(places.contains(&"ui/src/gen/view_060.tsx:25".to_owned()), "a TSX call: {places:?}");
    assert!(!places.iter().any(|p| p.starts_with("src/expand.rs")), "the definition isn't a call: {places:?}");
    // 13 Rust parts call it bare (their scoped calls are another shape), 4 views, and main twice.
    assert_eq!(calls.get("total").and_then(Value::as_u64), Some(19), "{places:?}");
    let in_main = hits(&calls).iter().find(|h| h.get("path") == Some(&json!("src/main.rs"))).and_then(|h| h.get("symbol").and_then(Value::as_str)).map(str::to_owned);
    assert_eq!(in_main.as_deref(), Some("main"));
    let (_, py) = search(&s, &dev, &at(1, "self.name = $N", "&mode=code&lang=python"))?;
    assert_eq!(py.get("total").and_then(Value::as_u64), Some(115), "{py}");
    assert!(hits(&py).iter().all(|h| h.get("symbol") == Some(&json!("__init__")) && h.get("kind") == Some(&json!("method"))), "{py}");

    // A query nothing answers finds nothing, and isn't refused.
    let (st, none) = search(&s, &dev, &at(1, "no line holds these words", ""))?;
    assert_eq!((st, none.get("total").and_then(Value::as_u64), hits(&none).len()), (200, Some(0), 0), "{none}");
    let (st, none) = search(&s, &dev, &at(1, "never_called($A)", "&mode=code"))?;
    assert_eq!((st, none.get("total").and_then(Value::as_u64)), (200, Some(0)), "{none}");
    // What can't be read is refused, saying why.
    for (q, more) in [("expand(($A)", "&mode=code"), ("x", "&mode=fuzzy"), ("/(/", ""), ("x", "&limit=0")] {
        let (st, why) = search(&s, &dev, &at(1, q, more))?;
        assert_eq!(st, 400, "{q} {more}: {why}");
    }
    let (st, _) = search(&s, &dev, "/api/diffs/D1/versions/9/search?q=x")?;
    assert_eq!(st, 404, "a version that isn't");

    // 50 searches of the warm index: p95 within 250 ms (W26-R07).
    let queries: Vec<(&str, &str)> = vec![
        ("empty matrix", ""),
        ("fold_3_2", ""),
        ("BTreeMap", ""),
        ("total +=", ""),
        ("name_of", ""),
        ("Part31", ""),
        ("useState", ""),
        ("def run_1_", ""),
        ("/fold_\\d+_3/", ""),
        ("expand", ""),
        ("matrix", ""),
        ("values", ""),
        ("no such words", ""),
        ("setOpen(!open)", ""),
        ("self.names.get", ""),
        ("Row 12", ""),
        ("task 99", ""),
        ("/fn spread_\\d+/", ""),
        ("Option<usize>", ""),
        ("props.count", ""),
        ("ROW_MAX", ""),
        ("describe", ""),
        ("generated tree", ""),
        ("Ragged::Row", ""),
        ("v * 3", ""),
        ("expand($A)", "&mode=code"),
        ("fold_1_2($V)", "&mode=code"),
        ("$M.copied()", "&mode=code"),
        ("useState($A)", "&mode=code"),
        ("self.names.get($K)", "&mode=code"),
        ("matrix_of($R)", "&mode=code"),
        ("Matrix { rows }", "&mode=code"),
        ("total += $X", "&mode=code"),
        ("return total", "&mode=code&lang=python"),
        ("expand($A).len()", "&mode=code&lang=rust"),
        ("setOpen($A)", "&mode=code"),
        ("$A.rows.is_empty()", "&mode=code"),
        ("Vec::new()", "&mode=code"),
        ("Err($E)", "&mode=code"),
        ("self.name = $N", "&mode=code"),
        ("empty matrix", "&limit=1"),
        ("fold_9", "&limit=10"),
        ("Task1", ""),
        ("/total \\+= v \\* [0-9]/", ""),
        ("label: string", ""),
        ("crate::expand::expand", ""),
        ("unwrap_or_default", ""),
        ("is_empty", ""),
        ("x", ""),
        ("Cells120", ""),
    ];
    assert_eq!(queries.len(), 50);
    let mut took = Vec::new();
    for (q, more) in &queries {
        let started = Instant::now();
        let (st, v) = search(&s, &dev, &at(1, q, more))?;
        took.push(started.elapsed());
        anyhow::ensure!(st == 200, "{q} {more}: {st} {v}");
    }
    let mut sorted = took.clone();
    sorted.sort();
    let p95 = sorted.get(47).copied().unwrap_or(Duration::MAX);
    let slowest: Vec<String> = {
        let mut by: Vec<(Duration, &str)> = took.iter().zip(&queries).map(|(d, (q, _))| (*d, *q)).collect();
        by.sort();
        by.iter().rev().take(5).map(|(d, q)| format!("{q} {} ms", d.as_millis())).collect()
    };
    eprintln!("W26-R07: first search (the index built) {} ms; warm: median {} ms, p95 {} ms; slowest: {slowest:?}", first.as_millis(), sorted.get(24).map_or(0, Duration::as_millis), p95.as_millis());
    assert!(p95 <= Duration::from_millis(250), "p95 {} ms over 50 searches of a warm index; slowest: {slowest:?}", p95.as_millis());

    // v2, whose index isn't built: its first search builds it, and reads v2's files.
    write(&work, &[("src/expand.rs".into(), EXPAND.iter().map(|l| format!("{l}\n")).collect::<String>().replace("an empty matrix expands to nothing", "a matrix with no rows expands to nothing"))])?;
    git(&home, &work, &["commit", "-q", "--amend", "-am", &format!("expand: a matrix with no rows\n\n{change}")])?;
    let out = git(&home, &work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(out.contains("D1 v2"), "{out}");
    let (st, v2) = search(&s, &dev, &at(2, "empty matrix", ""))?;
    anyhow::ensure!(st == 200, "v2: {st} {v2}");
    assert_eq!(v2.get("index").and_then(Value::as_str), Some("built"), "{v2}");
    assert_eq!(said(&v2), ["src/matrix.rs:40 (function matrix_of)", "src/gen/part_0007.rs:70 (function total_of_none)"], "{v2}");
    let (_, rows) = search(&s, &dev, &at(2, "no rows", ""))?;
    assert_eq!(said(&rows).first().map(String::as_str), Some("src/expand.rs:22 (function expand)"), "{rows}");
    let tree2 = v2.get("tree").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert_ne!(tree1, tree2);

    // Abandoned, the diff's indexes go with it.
    let (st, body) = http(s.port, "POST", "/api/diffs/D1/abandon", &dev, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "abandon: {st} {body}");
    let root = dir.path().join("data/cache/search");
    assert!(!root.join(&tree1).exists() && !root.join(&tree2).exists(), "the abandoned diff's indexes are dropped");
    Ok(())
}
