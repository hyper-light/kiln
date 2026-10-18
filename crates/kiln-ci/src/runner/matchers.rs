//! Problem matchers (docs/ci.md W06-R06), GitHub-exact.
//!
//! `::add-matcher::<file>` registers the file's matchers for the rest of the job: ahead of the
//! ones before, replacing any with the same owner (owners compare without case).
//! `::remove-matcher owner=<o>::` (or `::remove-matcher::<file>`) drops them. Each line of output
//! that isn't a command is tried against the step's matchers in order, colour codes stripped. The
//! first match resets the others. If it makes an issue, the issue is logged and annotated in
//! place of the line.
//!
//! A matcher of several patterns matches over consecutive lines. The last pattern may `loop`: it
//! then matches again on the lines that follow, each time with what the earlier patterns caught
//! (eslint's file line, then one line per problem).

use std::sync::Arc;

use serde_json::Value;

/// The properties a pattern may capture, in GitHub's order and with its names.
const PROPS: [&str; 9] = ["file", "line", "column", "severity", "code", "message", "fromPath", "endLine", "endColumn"];
const MESSAGE: usize = 5;

/// A pattern's regular expression. GitHub's are .NET's: lookaround and backreferences included
/// (setup-dotnet's `(?<!MSB)`), so a backtracking engine where they appear.
#[derive(Debug)]
struct Re(fancy_regex::Regex);

impl Re {
    fn new(p: &str) -> Result<Re, String> {
        fancy_regex::RegexBuilder::new(p).backtrack_limit(100_000).build().map(Re).map_err(|e| e.to_string())
    }

    /// The groups' count, group 0 included.
    fn groups(&self) -> usize {
        self.0.captures_len()
    }

    /// Each group's text when `line` matches: `""` for a group that took no part, as .NET has
    /// it. A line that runs past the backtracking limit doesn't match (GitHub times such a line
    /// out).
    fn captures(&self, line: &str) -> Option<Vec<String>> {
        let c = self.0.captures(line).ok()??;
        Some((0..c.len()).map(|i| c.get(i).map_or(String::new(), |m| m.as_str().to_owned())).collect())
    }
}

#[derive(Debug)]
struct Pattern {
    re: Re,
    /// Group numbers for [`PROPS`].
    props: [Option<usize>; 9],
    looped: bool,
}

/// One matcher of a file, checked.
#[derive(Debug)]
pub struct Config {
    pub owner: String,
    /// The default severity: `error`, `warning` or `notice` (any case), or none.
    severity: Option<String>,
    patterns: Vec<Pattern>,
}

/// Looks a key up the way the runner's JSON reader does: exact, else without case.
fn key<'a>(o: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    o.get(name).or_else(|| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
}

fn int(v: Option<&Value>, name: &str) -> Result<Option<usize>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().and_then(|n| usize::try_from(n).ok()).map(Some).ok_or_else(|| format!("The property '{name}' is set to {n} which is out of range")),
        Some(Value::String(s)) => s.trim().parse::<usize>().map(Some).map_err(|_| format!("The property '{name}' is set to {s} which is out of range")),
        Some(other) => Err(format!("The property '{name}' is set to {other} which is out of range")),
    }
}

/// A matcher file's matchers, checked as the runner checks them.
pub fn parse(json: &str) -> Result<Vec<Config>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("The matcher file is not valid JSON: {e}"))?;
    let list = v.as_object().and_then(|o| key(o, "problemMatcher")).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out: Vec<Config> = Vec::new();
    for m in &list {
        let o = m.as_object().cloned().unwrap_or_default();
        let owner = key(&o, "owner").and_then(Value::as_str).unwrap_or_default().to_owned();
        if owner.is_empty() {
            return Err("Owner must not be empty".into());
        }
        if out.iter().any(|c| c.owner.eq_ignore_ascii_case(&owner)) {
            return Err(format!("Duplicate owner name '{owner}'"));
        }
        let severity = key(&o, "severity").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        if let Some(s) = &severity
            && !["error", "warning", "notice"].iter().any(|x| s.eq_ignore_ascii_case(x))
        {
            return Err(format!("Matcher '{owner}' contains unexpected default severity '{s}'"));
        }
        let raw = key(&o, "pattern").and_then(Value::as_array).cloned().unwrap_or_default();
        if raw.is_empty() {
            return Err(format!("Matcher '{owner}' does not contain any patterns"));
        }
        let mut seen: [bool; 9] = [false; 9];
        let mut patterns = Vec::new();
        for (i, p) in raw.iter().enumerate() {
            let p = p.as_object().cloned().unwrap_or_default();
            let (first, last) = (i == 0, i + 1 == raw.len());
            let looped = key(&p, "loop").and_then(Value::as_bool).unwrap_or(false);
            if looped && (first || !last) {
                return Err("Only the last pattern in a multiline matcher may set 'loop'".into());
            }
            let re = Re::new(key(&p, "regexp").and_then(Value::as_str).unwrap_or_default()).map_err(|e| format!("Matcher '{owner}': {e}"))?;
            let mut props = [None; 9];
            for (n, name) in PROPS.iter().enumerate() {
                let Some(g) = int(key(&p, name), name)? else { continue };
                let slot_seen = seen.get_mut(n).ok_or("unreachable")?;
                if *slot_seen {
                    return Err(format!("The property '{name}' is set twice"));
                }
                if g >= re.groups() {
                    return Err(format!("The property '{name}' is set to {g} which is out of range"));
                }
                *slot_seen = true;
                if let Some(slot) = props.get_mut(n) {
                    *slot = Some(g);
                }
            }
            if looped && props.get(MESSAGE).copied().flatten().is_none() {
                return Err("The loop pattern must set 'message'".into());
            }
            patterns.push(Pattern { re, props, looped });
        }
        if !seen.get(MESSAGE).copied().unwrap_or(false) {
            return Err("At least one pattern must set 'message'".into());
        }
        out.push(Config { owner, severity, patterns });
    }
    Ok(out)
}

/// What a match caught, by [`PROPS`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found([Option<String>; 9]);

impl Found {
    fn new(running: Option<&Found>, p: &Pattern, groups: &[String], default_severity: Option<&str>) -> Found {
        let mut f = Found::default();
        for (n, slot) in f.0.iter_mut().enumerate() {
            let earlier = running.and_then(|r| r.0.get(n).cloned().flatten());
            *slot = earlier.or_else(|| p.props.get(n).copied().flatten().and_then(|g| groups.get(g).cloned()));
        }
        if let (Some(d), Some(slot)) = (default_severity, f.0.get_mut(3))
            && slot.as_deref().is_none_or(str::is_empty)
        {
            *slot = Some(d.to_owned());
        }
        f
    }

    fn get(&self, n: usize) -> Option<&str> {
        self.0.get(n).and_then(|v| v.as_deref())
    }
}

/// A matcher with its place in a multi-line match.
#[derive(Debug)]
struct Matcher {
    config: Arc<Config>,
    /// What patterns `0..n-1` caught so far.
    state: Vec<Option<Found>>,
}

impl Matcher {
    fn new(config: Arc<Config>) -> Matcher {
        let n = config.patterns.len().saturating_sub(1);
        Matcher { config, state: vec![None; n] }
    }

    fn reset(&mut self) {
        self.state.iter_mut().for_each(|s| *s = None);
    }

    fn matches(&mut self, line: &str) -> Option<Found> {
        let config = Arc::clone(&self.config);
        let ps = &config.patterns;
        let default = config.severity.as_deref();
        if let [p] = ps.as_slice() {
            return p.re.captures(line).map(|g| Found::new(None, p, &g, default));
        }
        for i in (0..ps.len()).rev() {
            let running = if i > 0 { self.state.get(i - 1).cloned().flatten() } else { None };
            if i > 0 && running.is_none() {
                continue;
            }
            let p = ps.get(i)?;
            let last = i + 1 == ps.len();
            match p.re.captures(line) {
                Some(g) if last => {
                    self.reset();
                    if p.looped
                        && let Some(slot) = self.state.get_mut(i - 1)
                    {
                        *slot = running.clone();
                    }
                    return Some(Found::new(running.as_ref(), p, &g, default));
                }
                Some(g) => {
                    if let Some(slot) = self.state.get_mut(i) {
                        *slot = Some(Found::new(running.as_ref(), p, &g, None));
                    }
                }
                None => {
                    if i > 0
                        && let Some(slot) = self.state.get_mut(i - 1)
                    {
                        *slot = None;
                    }
                }
            }
        }
        None
    }
}

/// The job's matchers, in order: what each step starts with.
#[derive(Debug, Default, Clone)]
pub struct Job(Vec<Arc<Config>>);

impl Job {
    /// Adds a file's matchers ahead of the others, replacing those of the same owners. The line
    /// the runner logs, and the matchers to hand to the running step.
    pub fn add(&mut self, new: Vec<Config>) -> (String, Vec<Arc<Config>>) {
        let new: Vec<Arc<Config>> = new.into_iter().map(Arc::new).collect();
        let rest: Vec<Arc<Config>> = self.0.drain(..).filter(|c| !new.iter().any(|n| n.owner.eq_ignore_ascii_case(&c.owner))).collect();
        self.0 = new.iter().cloned().chain(rest).collect();
        let owners: Vec<String> = new.iter().map(|c| format!("'{}'", c.owner)).collect();
        (format!("Added matchers: {}. Problem matchers scan action output for known warning or error strings and report these inline.", owners.join(", ")), new)
    }

    /// Removes the owners' matchers: the line the runner logs, and the owners removed.
    pub fn remove(&mut self, owners: &[String]) -> (String, Vec<String>) {
        let mut gone = Vec::new();
        self.0.retain(|c| {
            let keep = !owners.iter().any(|o| o.eq_ignore_ascii_case(&c.owner));
            if !keep {
                gone.push(c.owner.clone());
            }
            keep
        });
        let shown: Vec<String> = gone.iter().map(|o| format!("'{o}'")).collect();
        (format!("Removed matchers: {}", shown.join(", ")), gone)
    }

    /// A step's matchers, fresh.
    pub fn step(&self) -> Step {
        Step(self.0.iter().cloned().map(Matcher::new).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A running step's matchers.
#[derive(Debug, Default)]
pub struct Step(Vec<Matcher>);

/// An issue from a matched line. `file` is as the tool printed it: the runner roots it (in
/// `from_path`'s directory, else the workspace), and keeps it only if it is a file of the
/// repository.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issue {
    /// `error`, `warning` or `notice`.
    pub level: &'static str,
    pub message: String,
    pub file: Option<String>,
    pub from_path: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub end_line: Option<u32>,
    pub end_col: Option<u32>,
    pub code: Option<String>,
}

/// .NET's `int.TryParse` with `NumberStyles.None`: digits only.
fn number(s: Option<&str>) -> Option<u32> {
    let s = s.filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))?;
    s.parse().ok()
}

fn issue(f: &Found) -> Option<Issue> {
    let message = f.get(MESSAGE).filter(|m| !m.trim().is_empty())?.to_owned();
    let level = match f.get(3).unwrap_or_default() {
        s if s.is_empty() || s.eq_ignore_ascii_case("error") => "error",
        s if s.eq_ignore_ascii_case("warning") => "warning",
        s if s.eq_ignore_ascii_case("notice") => "notice",
        _ => return None,
    };
    Some(Issue {
        level,
        message,
        file: f.get(0).filter(|s| !s.trim().is_empty()).map(str::to_owned),
        from_path: f.get(6).filter(|s| !s.trim().is_empty()).map(str::to_owned),
        line: number(f.get(1)),
        col: number(f.get(2)),
        end_line: number(f.get(7)),
        end_col: number(f.get(8)),
        code: f.get(4).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned),
    })
}

/// `line` without its colour codes (`ESC [ … m`).
fn strip_colors(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("\x1b[") {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("\x1b[") {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let body = tail.get(2..).unwrap_or_default();
        let digits = body.bytes().take_while(|b| b.is_ascii_digit() || *b == b';').count();
        match body.get(digits..digits + 1) {
            Some("m") => rest = body.get(digits + 1..).unwrap_or_default(),
            _ => {
                out.push_str("\x1b[");
                rest = body;
            }
        }
    }
    out.push_str(rest);
    std::borrow::Cow::Owned(out)
}

impl Step {
    /// A step's own additions take the front one at a time, as the runner's events do: of a file
    /// with several matchers, the last comes first in the step that added them.
    pub fn added(&mut self, configs: &[Arc<Config>]) {
        for c in configs {
            self.0.retain(|m| !m.config.owner.eq_ignore_ascii_case(&c.owner));
            self.0.insert(0, Matcher::new(Arc::clone(c)));
        }
    }

    pub fn removed(&mut self, owners: &[String]) {
        self.0.retain(|m| !owners.iter().any(|o| o.eq_ignore_ascii_case(&m.config.owner)));
    }

    /// The issue a line of output makes, if any.
    pub fn line(&mut self, line: &str) -> Option<Issue> {
        if self.0.is_empty() {
            return None;
        }
        let stripped = strip_colors(line.trim_end_matches(['\r', '\n']));
        for i in 0..self.0.len() {
            let found = self.0.get_mut(i).and_then(|m| m.matches(&stripped));
            let Some(found) = found else { continue };
            for (j, other) in self.0.iter_mut().enumerate() {
                if j != i {
                    other.reset();
                }
            }
            if let Some(issue) = issue(&found) {
                return Some(issue);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// setup-node's `.github/tsc.json`.
    const TSC: &str = r#"{ "problemMatcher": [ { "owner": "tsc", "pattern": [ { "regexp": "^([^\\s].*)[\\(:](\\d+)[,:](\\d+)(?:\\):\\s+|\\s+-\\s+)(error|warning|info)\\s+TS(\\d+)\\s*:\\s*(.*)$", "file": 1, "line": 2, "column": 3, "severity": 4, "code": 5, "message": 6 } ] } ] }"#;

    /// setup-node's `.github/eslint-stylish.json`: a file line, then a line per problem.
    const STYLISH: &str = r#"{ "problemMatcher": [ { "owner": "eslint-stylish", "pattern": [ { "regexp": "^([^\\s].*)$", "file": 1 }, { "regexp": "^\\s+(\\d+):(\\d+)\\s+(error|warning|info)\\s+(.*)\\s\\s+(.*)$", "line": 1, "column": 2, "severity": 3, "message": 4, "code": 5, "loop": true } ] } ] }"#;

    fn job(files: &[&str]) -> Result<Job, String> {
        let mut j = Job::default();
        for f in files {
            j.add(parse(f)?);
        }
        Ok(j)
    }

    #[test]
    fn w06_ac02_tsc_lines_become_issues() -> Result<(), String> {
        let mut s = job(&[TSC])?.step();
        let i = s.line("src/index.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.").ok_or("no issue")?;
        assert_eq!(
            i,
            Issue { level: "error", message: "Type 'string' is not assignable to type 'number'.".into(), file: Some("src/index.ts".into()), line: Some(3), col: Some(7), code: Some("2322".into()), ..Issue::default() }
        );
        // Colour codes are stripped first; other lines are text.
        assert_eq!(s.line("\x1b[96msrc/a.ts\x1b[0m:\x1b[93m1\x1b[0m:\x1b[93m2\x1b[0m - \x1b[91mwarning\x1b[0m TS1: careful").map(|i| (i.level, i.file)), Some(("warning", Some("src/a.ts".into()))));
        assert_eq!(s.line("Found 1 error."), None);
        Ok(())
    }

    #[test]
    fn w06_ac02_a_looping_pattern_reports_each_problem_under_its_file() -> Result<(), String> {
        let mut s = job(&[STYLISH])?.step();
        let lines = ["/w/src/a.js", "  1:10  error  'x' is defined but never used  no-unused-vars", "  2:1   warning  Unexpected console statement  no-console", "", "/w/src/b.js", "  9:3  error  Missing semicolon  semi"];
        let got: Vec<(Option<String>, Option<u32>, &str, String, Option<String>)> = lines.iter().filter_map(|l| s.line(l)).map(|i| (i.file, i.line, i.level, i.message, i.code)).collect();
        assert_eq!(
            got,
            vec![
                (Some("/w/src/a.js".into()), Some(1), "error", "'x' is defined but never used".into(), Some("no-unused-vars".into())),
                (Some("/w/src/a.js".into()), Some(2), "warning", "Unexpected console statement".into(), Some("no-console".into())),
                (Some("/w/src/b.js".into()), Some(9), "error", "Missing semicolon".into(), Some("semi".into())),
            ]
        );
        Ok(())
    }

    #[test]
    fn w06_ac02_matchers_are_ordered_replaced_and_removed_by_owner() -> Result<(), String> {
        let warn = r#"{"problemMatcher":[{"owner":"Gen","severity":"warning","pattern":[{"regexp":"^W: (.*)$","message":1}]}]}"#;
        let any = r#"{"problemMatcher":[{"owner":"any","pattern":[{"regexp":"^(.*): (.*)$","file":1,"message":2}]}]}"#;
        let mut j = job(&[any, warn])?;
        // The later file comes first: `W: x` is gen's (a warning by default), not any's.
        assert_eq!(j.step().line("W: x").map(|i| (i.level, i.file)), Some(("warning", None)));
        let (said, _) = j.add(parse(&warn.replace("warning", "notice"))?);
        assert!(said.starts_with("Added matchers: 'Gen'. Problem matchers scan"), "{said}");
        assert_eq!(j.step().line("W: x").map(|i| i.level), Some("notice"));
        let (said, gone) = j.remove(&["gen".into()]);
        assert_eq!((said.as_str(), gone), ("Removed matchers: 'Gen'", vec!["Gen".to_owned()]));
        assert_eq!(j.step().line("W: x").map(|i| (i.level, i.file)), Some(("error", Some("W".into()))));
        Ok(())
    }

    #[test]
    fn w06_ac02_issues_need_a_message_and_a_known_severity() -> Result<(), String> {
        let m = r#"{"problemMatcher":[{"owner":"m","pattern":[{"regexp":"^(\\w+)\\|([^|]*)\\|(.*)$","severity":1,"line":2,"message":3}]}]}"#;
        let mut s = job(&[m])?.step();
        assert_eq!(s.line("fatal|1|boom"), None, "an unknown severity makes no issue");
        assert_eq!(s.line("error|1|   "), None, "nor a blank message");
        assert_eq!(s.line("ERROR|+1|x").map(|i| (i.level, i.line)), Some(("error", None)), "lines are digits only");
        assert_eq!(s.line("Notice|007|x").map(|i| (i.level, i.line)), Some(("notice", Some(7))));
        Ok(())
    }

    #[test]
    fn w06_ac02_dotnet_s_matcher_and_its_lookbehind() -> Result<(), String> {
        // setup-dotnet's `.github/csc.json`.
        let csc = r#"{ "problemMatcher": [ { "owner": "csc", "pattern": [ { "regexp": "^([^\\s].*)\\((\\d+)(?:,\\d+|,\\d+,\\d+)?\\):\\s+(error|warning)\\s+([a-zA-Z]+(?<!MSB)\\d+):\\s*(.*?)\\s+\\[(.*?)\\]$", "file": 1, "line": 2, "severity": 3, "code": 4, "message": 5, "fromPath": 6 } ] } ] }"#;
        let mut s = job(&[csc])?.step();
        let i = s.line("Program.cs(5,13): error CS0103: The name 'x' does not exist in the current context [/w/app/app.csproj]").ok_or("no issue")?;
        assert_eq!((i.file.as_deref(), i.line, i.code.as_deref(), i.from_path.as_deref()), (Some("Program.cs"), Some(5), Some("CS0103"), Some("/w/app/app.csproj")));
        assert_eq!(s.line("Build.proj(3,5): error MSB4019: The imported project was not found. [/w/app/app.csproj]"), None, "MSB codes are MSBuild's, not the compiler's");
        Ok(())
    }

    #[test]
    fn w06_ac02_bad_matcher_files_are_refused_as_github_refuses_them() {
        let bad = |json: &str| parse(json).err().unwrap_or_default();
        assert_eq!(bad(r#"{"problemMatcher":[{"pattern":[{"regexp":"x","message":0}]}]}"#), "Owner must not be empty");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","severity":"fatal","pattern":[{"regexp":"x","message":0}]}]}"#), "Matcher 'a' contains unexpected default severity 'fatal'");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","pattern":[]}]}"#), "Matcher 'a' does not contain any patterns");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","pattern":[{"regexp":"(x)","message":1,"loop":true}]}]}"#), "Only the last pattern in a multiline matcher may set 'loop'");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","pattern":[{"regexp":"(x)","file":1},{"regexp":"(y)","file":1,"message":1}]}]}"#), "The property 'file' is set twice");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","pattern":[{"regexp":"(x)","message":2}]}]}"#), "The property 'message' is set to 2 which is out of range");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","pattern":[{"regexp":"(x)","file":1}]}]}"#), "At least one pattern must set 'message'");
        assert_eq!(bad(r#"{"problemMatcher":[{"owner":"a","pattern":[{"regexp":"(x)","message":1}]},{"owner":"A","pattern":[{"regexp":"(x)","message":1}]}]}"#), "Duplicate owner name 'A'");
        // The runner's JSON reader takes keys in any case.
        assert!(parse(r#"{"ProblemMatcher":[{"Owner":"a","Pattern":[{"Regexp":"(x)","Message":"1"}]}]}"#).is_ok());
    }
}
