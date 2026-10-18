//! The JSON documents: Mocha's `json` reporter and CTRF.

use serde_json::Value;

use super::{Case, Outcome, trace};

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned).filter(|s| !s.is_empty())
}

fn millis(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0).map(|x| x.round() as u64)
}

/// Mocha's `--reporter json`: `tests` (every test), `pending` and `failures`, each with its
/// `title`, `fullTitle` (its describe blocks, then the title), `file`, `duration` and `err`.
pub fn mocha(text_: &str) -> Result<Vec<Case>, String> {
    let v: Value = serde_json::from_str(text_).map_err(|e| format!("not JSON: {e}"))?;
    let tests = v.get("tests").and_then(Value::as_array).ok_or("Mocha's JSON has `tests`")?;
    let pending: Vec<String> = v.get("pending").and_then(Value::as_array).into_iter().flatten().filter_map(|t| text(t, "fullTitle")).collect();
    Ok(tests
        .iter()
        .map(|t| {
            let title = text(t, "title").unwrap_or_default();
            let full = text(t, "fullTitle").unwrap_or_else(|| title.clone());
            let suite = full.strip_suffix(&title).map(str::trim).unwrap_or_default().to_owned();
            let err = t.get("err").filter(|e| e.as_object().is_some_and(|o| !o.is_empty()));
            let outcome = if err.is_some() {
                Outcome::Failed
            } else if t.get("pending").and_then(Value::as_bool) == Some(true) || pending.contains(&full) {
                Outcome::Skipped
            } else {
                Outcome::Passed
            };
            Case {
                suite,
                name: title,
                file: text(t, "file"),
                line: None,
                outcome,
                duration_ms: millis(t, "duration"),
                message: err.and_then(|e| text(e, "message")),
                trace: err.and_then(|e| text(e, "stack")).and_then(|s| trace(&s)),
                retried: 0,
            }
        })
        .collect())
}

/// CTRF (`{"results": {"tests": [...]}}`): each test's `name`, `status`, `duration` (ms),
/// `message`, `trace`, `filePath`, `line`, `suite` and, for a flaky one, `retries`.
pub fn ctrf(text_: &str) -> Result<Vec<Case>, String> {
    let v: Value = serde_json::from_str(text_).map_err(|e| format!("not JSON: {e}"))?;
    let tests = v.pointer("/results/tests").and_then(Value::as_array).ok_or("CTRF has `results.tests`")?;
    Ok(tests
        .iter()
        .map(|t| {
            let outcome = match t.get("status").and_then(Value::as_str).unwrap_or_default() {
                "passed" => Outcome::Passed,
                "failed" => Outcome::Failed,
                "skipped" | "pending" | "other" => Outcome::Skipped,
                _ => Outcome::Error,
            };
            let suite = match t.get("suite") {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" > "),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let flaky = t.get("flaky").and_then(Value::as_bool) == Some(true);
            let retries = t.get("retries").and_then(Value::as_u64).and_then(|r| u32::try_from(r).ok()).unwrap_or(0);
            Case {
                suite,
                name: text(t, "name").unwrap_or_default(),
                file: text(t, "filePath"),
                line: t.get("line").and_then(Value::as_u64).and_then(|l| u32::try_from(l).ok()),
                outcome,
                duration_ms: millis(t, "duration"),
                message: text(t, "message"),
                trace: text(t, "trace").and_then(|s| trace(&s)),
                retried: if flaky && outcome == Outcome::Passed { retries.max(1) } else { 0 },
            }
        })
        .collect())
}
