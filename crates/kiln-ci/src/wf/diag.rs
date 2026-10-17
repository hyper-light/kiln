//! Diagnostics (docs/ci.md W02-R08): a stable code, a severity, a message, the YAML path, the
//! range, and optionally a fix. Codes: `KCI1xxx` structure, `KCI2xxx` expressions, `KCI3xxx`
//! contexts, `KCI4xxx` references, `KCI5xxx` kiln-only in GHA, `KCI6xxx` lints.

use serde::{Deserialize, Serialize};

/// A position: 1-based line and column, and the byte offset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
    #[serde(skip)]
    pub offset: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Range {
    pub start: Pos,
    pub end: Pos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A replacement of a range by text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub range: Range,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<Edit>,
}

/// Serialized for the API and storage; read back as [`StoredDiag`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diag {
    pub code: &'static str,
    pub severity: Severity,
    pub message: String,
    /// The YAML path, e.g. `jobs.build.steps[2].run`.
    pub path: String,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
}

impl Diag {
    pub fn new(code: &'static str, severity: Severity, message: impl Into<String>, range: Range) -> Self {
        Self { code, severity, message: message.into(), path: String::new(), range, fix: None }
    }

    pub fn error(code: &'static str, message: impl Into<String>, range: Range) -> Self {
        Self::new(code, Severity::Error, message, range)
    }

    pub fn warning(code: &'static str, message: impl Into<String>, range: Range) -> Self {
        Self::new(code, Severity::Warning, message, range)
    }

    pub fn info(code: &'static str, message: impl Into<String>, range: Range) -> Self {
        Self::new(code, Severity::Info, message, range)
    }

    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn with_fix(mut self, title: impl Into<String>, edits: Vec<Edit>) -> Self {
        self.fix = Some(Fix { title: title.into(), edits });
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// `file:line:col: severity code: message`, as `kiln validate` prints it.
    pub fn render(&self, file: &str) -> String {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        };
        format!("{file}:{}:{}: {sev} {}: {}", self.range.start.line, self.range.start.col, self.code, self.message)
    }
}

/// A diagnostic read back from storage (its code owned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDiag {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub path: String,
    pub range: Range,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
}

/// The closest of `candidates` to `word` within edit distance 2 ("did you mean").
pub fn suggest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        let d = distance(word, c);
        if d <= 2 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(0) + usize::from(ca != cb);
            let del = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let ins = cur.get(j).copied().unwrap_or(0) + 1;
            if let Some(c) = cur.get_mut(j + 1) {
                *c = sub.min(del).min(ins);
            }
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        assert_eq!(suggest("run-on", ["runs-on", "needs", "steps"]), Some("runs-on"));
        assert_eq!(suggest("stpes", ["runs-on", "needs", "steps"]), Some("steps"));
        assert_eq!(suggest("banana", ["runs-on", "needs"]), None);
    }
}
