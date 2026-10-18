//! Quarantine (docs/ci.md W16-R05): trunk's `.kiln/quarantine.yaml` lists tests whose failures
//! a job with `quarantine: enforce` lets pass, with a warning. They still run and are still
//! reported. An entry past its `until` stops applying and is flagged.
//!
//! ```yaml
//! - test: "com.acme.AppTest.wobbles"      # suite.name, suite::name, the name, or the id
//!   reason: races the clock
//!   issue: https://example.com/issues/12
//!   until: 2026-12-31
//! - test: "tests::net::*"                  # `*` matches any run of characters
//!   reason: the sandbox has no network
//! ```

use serde::{Deserialize, Serialize};

use super::Case;

/// One entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub test: String,
    pub reason: String,
    #[serde(default)]
    pub issue: Option<String>,
    /// The last day it applies (`YYYY-MM-DD`, UTC).
    #[serde(default)]
    pub until: Option<String>,
}

/// The file's entries; an entry without `test` or `reason`, or with a bad `until`, is an error.
pub fn parse(yaml: &str) -> Result<Vec<Entry>, String> {
    if yaml.trim().is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<Entry> = serde_yaml::from_str(yaml).map_err(|e| format!(".kiln/quarantine.yaml: {e}"))?;
    for e in &list {
        if e.test.trim().is_empty() || e.reason.trim().is_empty() {
            return Err(format!(".kiln/quarantine.yaml: `{}` needs a test and a reason", e.test));
        }
        if let Some(u) = &e.until
            && day_end(u).is_none()
        {
            return Err(format!(".kiln/quarantine.yaml: `{}`'s until `{u}` isn't a date (YYYY-MM-DD)", e.test));
        }
    }
    Ok(list)
}

/// The end of `YYYY-MM-DD` (UTC), in Unix seconds.
fn day_end(d: &str) -> Option<i64> {
    let mut p = d.trim().splitn(3, '-');
    let (y, m, day): (i64, i64, i64) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days + 1) * 86_400 - 1)
}

impl Entry {
    /// Whether it still applies at `now`.
    pub fn active(&self, now: i64) -> bool {
        self.until.as_deref().and_then(day_end).is_none_or(|end| now <= end)
    }

    /// Whether it names `c`: its id, its name, or its suite and name joined by `.`, `::`, ` > ` or
    /// `/`, with `*` matching any run of characters.
    pub fn matches(&self, c: &Case) -> bool {
        let pat = self.test.trim();
        let mut names = vec![c.id(), c.name.clone()];
        if !c.suite.is_empty() {
            for sep in [".", "::", " > ", "/", " "] {
                names.push(format!("{}{sep}{}", c.suite, c.name));
            }
        }
        names.iter().any(|n| glob(pat, n))
    }
}

/// `*` as any run of characters; everything else literal.
fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts.first().copied().unwrap_or_default(), parts.last().copied().unwrap_or_default());
    if !s.starts_with(first) || !s.ends_with(last) || s.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = s.get(first.len()..s.len() - last.len()).unwrap_or_default();
    for p in parts.get(1..parts.len() - 1).unwrap_or_default() {
        match rest.find(p) {
            Some(at) => rest = rest.get(at + p.len()..).unwrap_or_default(),
            None => return false,
        }
    }
    true
}

/// What quarantine says about a step's failures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Failed tests an active entry covers, each with its entry's reason.
    pub quarantined: Vec<(String, String)>,
    /// Failed tests nothing covers.
    pub failing: Vec<String>,
    /// Entries past their `until`.
    pub expired: Vec<String>,
}

impl Verdict {
    /// Every failure is quarantined (and there was one): with `enforce`, the step passes.
    pub fn only_quarantined(&self) -> bool {
        !self.quarantined.is_empty() && self.failing.is_empty()
    }
}

/// Judges `cases`' failures against `entries` at `now`.
pub fn judge(entries: &[Entry], cases: &[Case], now: i64) -> Verdict {
    let mut v = Verdict { expired: entries.iter().filter(|e| !e.active(now)).map(|e| e.test.clone()).collect(), ..Verdict::default() };
    for c in cases.iter().filter(|c| c.outcome.is_failure()) {
        let label = if c.suite.is_empty() { c.name.clone() } else { format!("{} {}", c.suite, c.name) };
        match entries.iter().find(|e| e.active(now) && e.matches(c)) {
            Some(e) => v.quarantined.push((label, e.reason.clone())),
            None => v.failing.push(label),
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::super::Outcome;
    use super::*;

    fn case(suite: &str, name: &str, o: Outcome) -> Case {
        Case { suite: suite.into(), name: name.into(), outcome: o, ..Case::default() }
    }

    /// W16-AC03's rules: quarantined-only failures pass, one more failure doesn't, an expired
    /// entry doesn't apply and is flagged.
    #[test]
    fn w16_ac03_quarantine() {
        let entries = parse("- test: com.acme.AppTest.wobbles\n  reason: races the clock\n- test: 'tests::net::*'\n  reason: no network\n- test: old\n  reason: fixed long ago\n  until: 2020-01-31\n").unwrap_or_default();
        assert_eq!(entries.len(), 3);
        let now = 1_792_324_800; // 2026-10-18
        let only = [case("com.acme.AppTest", "wobbles", Outcome::Failed), case("", "tests::net::dial", Outcome::Failed), case("x", "fine", Outcome::Passed)];
        let v = judge(&entries, &only, now);
        assert!(v.only_quarantined(), "{v:?}");
        assert_eq!(v.expired, vec!["old".to_owned()]);
        let more = [case("com.acme.AppTest", "wobbles", Outcome::Failed), case("com.acme.AppTest", "breaks", Outcome::Failed)];
        assert!(!judge(&entries, &more, now).only_quarantined(), "an unquarantined failure fails");
        let expired = [case("", "old", Outcome::Failed)];
        let v = judge(&entries, &expired, now);
        assert!(!v.only_quarantined() && v.failing == vec!["old".to_owned()], "an expired entry no longer applies");
        assert!(!judge(&entries, &[case("x", "fine", Outcome::Passed)], now).only_quarantined(), "nothing failed: nothing to excuse");
        assert!(day_end("2020-01-31").is_some_and(|t| t == 1_580_515_199), "the end of the day, UTC");
        assert!(parse("- test: x\n").is_err(), "a reason is required");
        assert!(parse("- test: x\n  reason: y\n  until: soon\n").is_err());
    }
}
