//! The line formats: TAP, `go test -json` and libtest's JSON.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{Case, Outcome, trace};

/// TAP 12, 13 and 14: `ok`/`not ok` lines with `# SKIP` and `# TODO`, YAML diagnostics, and
/// subtests (indented, before the line that sums them up, as node:test and tap print them),
/// whose leaves are the cases, under their parents' names.
pub fn tap(text: &str) -> Result<Vec<Case>, String> {
    struct Line {
        indent: usize,
        case: Case,
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut yaml: Option<(usize, Vec<String>)> = None;
    let mut seen_any = false;
    for raw in text.lines() {
        let indent = raw.len() - raw.trim_start().len();
        let l = raw.trim();
        if let Some((at, body)) = &mut yaml {
            if l == "..." {
                let block = std::mem::take(body);
                let at = *at;
                yaml = None;
                if let Some(last) = lines.get_mut(at) {
                    diagnostics(&mut last.case, &block);
                }
            } else {
                body.push(raw.to_owned());
            }
            continue;
        }
        if l == "---" {
            if !lines.is_empty() {
                yaml = Some((lines.len() - 1, Vec::new()));
            }
            continue;
        }
        if let Some(why) = l.strip_prefix("Bail out!") {
            lines.push(Line { indent, case: Case { name: "Bail out!".into(), outcome: Outcome::Error, message: Some(why.trim().to_owned()).filter(|m| !m.is_empty()), ..Case::default() } });
            break;
        }
        let (ok, rest) = if let Some(r) = l.strip_prefix("not ok") {
            (false, r)
        } else if let Some(r) = l.strip_prefix("ok") {
            (true, r)
        } else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with(' ') {
            continue; // `okay`, not a result
        }
        seen_any = true;
        let rest = rest.trim_start();
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start();
        let rest = rest.strip_prefix('-').unwrap_or(rest).trim_start();
        let (desc, directive) = match rest.split_once(" # ").or_else(|| rest.strip_prefix("# ").map(|d| ("", d))) {
            Some((d, x)) => (d.trim(), Some(x.trim())),
            None => (rest.trim(), None),
        };
        let lower = directive.map(str::to_ascii_lowercase);
        let skip = lower.as_deref().is_some_and(|d| d.starts_with("skip"));
        let todo = lower.as_deref().is_some_and(|d| d.starts_with("todo"));
        let outcome = if skip || (todo && !ok) {
            Outcome::Skipped
        } else if ok {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        let message = directive.filter(|_| skip || todo).map(str::to_owned);
        lines.push(Line { indent, case: Case { name: desc.to_owned(), outcome, message, ..Case::default() } });
    }
    if !seen_any && lines.is_empty() {
        return Err("no TAP results (`ok` or `not ok` lines)".into());
    }
    // A line is a parent when deeper lines came right before it: they're its subtests.
    let mut out: Vec<(usize, Case)> = Vec::new();
    for Line { indent, case } in lines {
        let children: Vec<usize> = out.iter().enumerate().rev().take_while(|(_, (d, _))| *d > indent).map(|(i, _)| i).collect();
        if children.is_empty() {
            out.push((indent, case));
            continue;
        }
        for i in children {
            if let Some((d, c)) = out.get_mut(i) {
                c.suite = if c.suite.is_empty() { case.name.clone() } else { format!("{} > {}", case.name, c.suite) };
                // Its children are no longer deeper than what's left to come at this depth.
                *d = indent;
            }
        }
    }
    Ok(out.into_iter().map(|(_, c)| c).collect())
}

/// A TAP 13 YAML block's `message`, `at`/`file`/`line`, `duration_ms` and the rest as the trace.
fn diagnostics(c: &mut Case, block: &[String]) {
    let text = block.join("\n");
    let v: Value = serde_yaml::from_str::<serde_json::Value>(&dedent(&text)).unwrap_or(Value::Null);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str().map(str::to_owned).or_else(|| (!x.is_null()).then(|| x.to_string())));
    if let Some(m) = s("message") {
        c.message = Some(m.trim().to_owned());
    }
    if let Some(ms) = v.get("duration_ms").and_then(Value::as_f64) {
        c.duration_ms = Some(ms.max(0.0).round() as u64);
    }
    if let Some(at) = s("at").or_else(|| s("location")) {
        // `file.js:12:5`, or `name (file.js:12:5)`.
        let inner = at.rsplit_once('(').map_or(at.as_str(), |(_, x)| x.trim_end_matches(')'));
        let mut parts = inner.rsplitn(3, ':');
        let (a, b) = (parts.next(), parts.next());
        let (file, line) = match (a.and_then(|x| x.parse::<u32>().ok()), b.and_then(|x| x.parse::<u32>().ok()), parts.next()) {
            (Some(_col), Some(line), Some(file)) => (Some(file.to_owned()), Some(line)),
            (Some(line), _, _) => (b.map(str::to_owned), Some(line)),
            _ => (None, None),
        };
        c.file = c.file.take().or(file);
        c.line = c.line.or(line);
    }
    if let (Some(f), None) = (s("file"), &c.file) {
        c.file = Some(f);
    }
    if c.line.is_none() {
        c.line = v.get("line").and_then(Value::as_u64).and_then(|l| u32::try_from(l).ok());
    }
    if c.outcome.is_failure() {
        c.trace = s("stack").and_then(|st| trace(&st)).or_else(|| trace(&text));
    }
}

fn dedent(s: &str) -> String {
    let n = s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    s.lines().map(|l| l.get(n.min(l.len())..).unwrap_or_default()).collect::<Vec<_>>().join("\n")
}

/// `go test -json`: each test's last `pass`, `fail` or `skip`, its output as the trace, and the
/// `file.go:12: message` line of a failure as its place and message. A package that fails with
/// no test failing (it didn't build) is an error.
pub fn go_test_json(text: &str) -> Result<Vec<Case>, String> {
    struct T {
        outcome: Option<Outcome>,
        elapsed: Option<f64>,
        output: String,
    }
    let mut tests: BTreeMap<(String, String), T> = BTreeMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    let mut parsed = 0;
    for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            // `go test -json` passes the build's own output through as plain lines.
            if parsed == 0 && n > 20 {
                return Err("not go test -json output: its lines aren't JSON events".into());
            }
            continue;
        };
        parsed += 1;
        let get = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let key = (get("Package"), get("Test"));
        let t = tests.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            T { outcome: None, elapsed: None, output: String::new() }
        });
        match get("Action").as_str() {
            "pass" => t.outcome = Some(Outcome::Passed),
            "fail" => t.outcome = Some(Outcome::Failed),
            "skip" => t.outcome = Some(Outcome::Skipped),
            "output" => t.output.push_str(&get("Output")),
            _ => {}
        }
        if let Some(e) = v.get("Elapsed").and_then(Value::as_f64) {
            t.elapsed = Some(e);
        }
    }
    if parsed == 0 {
        return Err("no go test -json events".into());
    }
    let failing_pkgs: std::collections::BTreeSet<&str> = order.iter().filter(|(_, t)| !t.is_empty()).filter(|k| tests.get(*k).is_some_and(|t| t.outcome == Some(Outcome::Failed))).map(|(p, _)| p.as_str()).collect();
    let mut out = Vec::new();
    for key in &order {
        let Some(t) = tests.get(key) else { continue };
        let (pkg, test) = key;
        let Some(outcome) = t.outcome else { continue };
        if test.is_empty() {
            // The package's own result: a failure with no failing test is a build or setup error.
            if outcome == Outcome::Failed && !failing_pkgs.contains(pkg.as_str()) {
                out.push(Case { suite: pkg.clone(), name: "(package)".into(), outcome: Outcome::Error, message: t.output.lines().find(|l| !l.trim().is_empty()).map(|l| l.trim().to_owned()), trace: trace(&t.output), ..Case::default() });
            }
            continue;
        }
        let place = t.output.lines().find_map(|l| {
            let l = l.trim();
            let (file, rest) = l.split_once(".go:")?;
            let (line, msg) = rest.split_once(": ")?;
            Some((format!("{}.go", file.rsplit(' ').next().unwrap_or(file)), line.parse::<u32>().ok()?, msg.trim().to_owned()))
        });
        let failed = outcome.is_failure();
        out.push(Case {
            suite: pkg.clone(),
            name: test.clone(),
            file: place.as_ref().filter(|_| failed).map(|p| p.0.clone()),
            line: place.as_ref().filter(|_| failed).map(|p| p.1),
            outcome,
            duration_ms: t.elapsed.map(|e| (e * 1000.0).round() as u64),
            message: place.filter(|_| failed).map(|p| p.2),
            trace: if failed || outcome == Outcome::Skipped { trace(&t.output) } else { None },
            retried: 0,
        });
    }
    Ok(out)
}

/// libtest's JSON (`cargo test -- -Z unstable-options --format json`, nextest's
/// `libtest-json`): `{"type":"test","event":"ok"|"failed"|"ignored"|"timeout",…}` lines. A nextest
/// name `binary$path` keeps the binary as the suite.
pub fn libtest_json(text: &str) -> Result<Vec<Case>, String> {
    let mut out = Vec::new();
    let mut parsed = 0;
    for line in text.lines().filter(|l| l.trim_start().starts_with('{')) {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        parsed += 1;
        if v.get("type").and_then(Value::as_str) != Some("test") {
            continue;
        }
        let outcome = match v.get("event").and_then(Value::as_str).unwrap_or_default() {
            "ok" => Outcome::Passed,
            "failed" => Outcome::Failed,
            "ignored" => Outcome::Skipped,
            "timeout" => Outcome::Error,
            _ => continue,
        };
        let full = v.get("name").and_then(Value::as_str).unwrap_or_default();
        let (suite, name) = full.split_once('$').map_or(("", full), |(s, n)| (s, n));
        let stdout = v.get("stdout").and_then(Value::as_str).unwrap_or_default();
        let message = v.get("message").and_then(Value::as_str).map(str::to_owned).or_else(|| stdout.lines().find(|l| l.contains("panicked at")).map(|l| l.trim().to_owned()));
        // `panicked at src/lib.rs:12:5:` names the place.
        let place = message.as_deref().and_then(|m| m.split("panicked at ").nth(1)).and_then(|p| {
            let p = p.trim_end_matches(':');
            let mut it = p.rsplitn(3, ':');
            let (_col, line, file) = (it.next()?, it.next()?.parse::<u32>().ok()?, it.next()?);
            // Older Rust: `panicked at 'message', src/lib.rs:12:5`.
            Some((file.rsplit(", ").next().unwrap_or(file).to_owned(), line))
        });
        out.push(Case {
            suite: suite.to_owned(),
            name: name.to_owned(),
            file: place.as_ref().map(|p| p.0.clone()),
            line: place.map(|p| p.1),
            outcome,
            duration_ms: v.get("exec_time").and_then(Value::as_f64).map(|s| (s * 1000.0).round() as u64),
            message,
            trace: if outcome.is_failure() { trace(stdout) } else { None },
            retried: 0,
        });
    }
    if parsed == 0 {
        return Err("no libtest JSON events".into());
    }
    Ok(out)
}
