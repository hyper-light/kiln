//! docs/ci.md W02-AC03 (the corpus conforms to the exported schemas), W02-AC06 (mutated files
//! never panic, and every problem is a diagnostic inside the file), W02-AC07 (a 2,000-line
//! workflow in under 20 ms).

use std::path::{Path, PathBuf};
use std::time::Instant;

use kiln_ci::expr::Dialect;
use kiln_ci::wf::{SchemaKind, parse_workflow, schema, yaml};

fn corpus() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "yml" || x == "yaml") {
                out.push(p);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gha");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .filter_map(|f| {
            let mut src = std::fs::read_to_string(&f).ok()?;
            for (p, v) in [("$cron-daily", "0 0 * * *"), ("$cron-weekly", "0 0 * * 0"), ("$codeql-languages-matrix", "language: [ 'actions' ]")] {
                src = src.replace(p, v);
            }
            Some((f.strip_prefix(&root).unwrap_or(&f).display().to_string(), src))
        })
        .collect()
}

#[test]
fn w02_ac03_the_corpus_conforms_to_the_gha_schema() -> Result<(), String> {
    let s = schema(SchemaKind::Gha);
    let mut failures = Vec::new();
    for (name, src) in corpus() {
        let mut d = Vec::new();
        let Some(node) = yaml::parse(&src, &mut d) else { continue };
        for p in kiln_ci::wf::schema::check(&s, &node.to_json()) {
            failures.push(format!("{name}: {p}"));
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.into_iter().take(60).collect::<Vec<_>>().join("\n")) }
}

#[test]
fn w02_ac03_structural_errors_fail_the_schema_too() {
    let s = schema(SchemaKind::Gha);
    for src in ["on: push\njobs:\n  a:\n    run-on: x\n", "on: push\njobs:\n  a:\n    runs-on: x\n    steps:\n      - nope: 1\n", "on: pushy\njobs: {}\n"] {
        let mut d = Vec::new();
        let node = yaml::parse(src, &mut d);
        assert!(node.is_some_and(|n| !kiln_ci::wf::schema::check(&s, &n.to_json()).is_empty()), "{src}");
    }
}

/// splitmix64, seeded, so failures reproduce.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }
}

/// Mutated corpus files never panic, never hang (each case finishes within 30 s, or the test fails
/// with the input), and every problem is a diagnostic inside the file.
#[test]
fn w02_ac06_mutated_workflows_never_panic() -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel::<(usize, String)>();
    let worker = std::thread::spawn(move || -> Result<(), String> {
        let files = corpus();
        let mut rng = Rng(42);
        let tokens = ["${{", "}}", ":", "-", "  ", "\n", "[", "]", "{", "}", "'", "\"", "&a", "*a", "if: ", "uses: ", "run: ", "needs: [x]", "matrix:", "on:", "#", "|", ">", "\t"];
        let files: Vec<(String, String)> = files.into_iter().filter(|(_, s)| s.len() < 20_000).collect();
        // W02-AC06 asks for 10⁵ cases: `KILN_CI_MUTATIONS=100000 cargo test --release` (nightly).
        let n = std::env::var("KILN_CI_MUTATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(if cfg!(debug_assertions) { 1_500 } else { 20_000 });
        for i in 0..n {
            let Some((_, src)) = files.get(rng.below(files.len())) else { return Ok(()) };
            let mut chars: Vec<char> = src.chars().collect();
            for _ in 0..(1 + rng.below(4)) {
                let at = rng.below(chars.len().max(1));
                match rng.below(3) {
                    0 => {
                        let end = (at + 1 + rng.below(20)).min(chars.len());
                        chars.drain(at.min(end)..end);
                    }
                    1 => {
                        let t = tokens.get(rng.below(tokens.len())).copied().unwrap_or("");
                        for (k, c) in t.chars().enumerate() {
                            chars.insert((at + k).min(chars.len()), c);
                        }
                    }
                    _ => {
                        if let Some(c) = chars.get_mut(at) {
                            *c = if *c == ' ' { 'x' } else { ' ' };
                        }
                    }
                }
            }
            let mutated: String = chars.into_iter().collect();
            let _ = tx.send((i, mutated.clone()));
            let lines = u32::try_from(mutated.lines().count() + 2).unwrap_or(u32::MAX);
            for dialect in [Dialect::Gha, Dialect::Kiln] {
                let (_, diags) = parse_workflow(".github/workflows/m.yml", &mutated, dialect);
                if let Some(d) = diags.iter().find(|d| d.range.start.line > lines) {
                    return Err(format!("case {i}: a diagnostic outside the file: {d:?}\n{mutated}"));
                }
            }
        }
        Ok(())
    });
    let mut last = (0, String::new());
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(c) => last = c,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Err(format!("case {} has not finished in 30 s:\n{}", last.0, last.1)),
        }
    }
    worker.join().map_err(|_| format!("case {} panicked:\n{}", last.0, last.1))?
}

#[test]
fn w02_ac07_a_large_workflow_reads_quickly() {
    let mut src = String::from("on: [push, pull_request]\njobs:\n");
    while src.lines().count() < 2_000 {
        let n = src.lines().count();
        src.push_str(&format!("  job{n}:\n    runs-on: ubuntu-latest\n    needs: []\n    if: github.event_name == 'push' && contains(github.ref, 'main')\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo ${{{{ github.sha }}}} ${{{{ matrix.os }}}}\n        env: {{ A: '${{{{ github.ref }}}}' }}\n"));
    }
    let start = Instant::now();
    let (wf, diags) = parse_workflow(".github/workflows/big.yml", &src, Dialect::Gha);
    let took = start.elapsed();
    assert!(wf.is_some() && diags.iter().all(|d| !d.is_error()), "{:?}", diags.iter().find(|d| d.is_error()));
    let limit = if cfg!(debug_assertions) { 400 } else { 20 };
    assert!(took.as_millis() < limit, "{} lines in {took:?}", src.lines().count());
}
