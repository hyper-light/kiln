//! kiln's rewrite rules for its units (docs/ci.md W24-R30): equivalences, each proven by a
//! differential test that runs both definitions of each of its cases and compares what they
//! produced ([`witness`]). `kiln_ci::work::eclass::Rules::register` refuses a rule without such a
//! witness, or one whose runs differ. A step's operations are its script's lines.

use std::collections::BTreeSet;

use kiln_ci::work::eclass::{Case, Pattern, Rewrite, Rule, Witness};
use kiln_ci::work::key::Kind;
#[cfg(unix)]
use sha2::{Digest as _, Sha256};
#[cfg(unix)]
use std::path::Path;

/// A rule, and the definitions its differential test runs both ways.
#[derive(Debug, Clone)]
pub struct Proven {
    pub rule: Rule,
    pub cases: Vec<(String, String)>,
}

fn step(name: &str, rewrite: Rewrite, cases: &[(&str, &str)]) -> Proven {
    let cases = cases.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
    Proven { rule: Rule { name: name.to_owned(), kinds: BTreeSet::from([Kind::Step]), rewrite }, cases }
}

/// kiln's rules for steps.
pub fn kiln() -> Vec<Proven> {
    let flags = |f: &[&str]| f.iter().map(|x| (*x).to_owned()).collect::<BTreeSet<String>>();
    vec![
        // Environment order: variables exported in either order are one (one set twice isn't).
        step("env-order", Rewrite::Assign { prefix: "export ".into() }, &[("export B=2\nexport A=1\necho \"$A$B\" > out", "export A=1\nexport B=2\necho \"$A$B\" > out")]),
        // Flag order: `sort`'s flags that take no value, in any order.
        step(
            "sort-flags",
            Rewrite::Flags { op: Pattern::Starts("sort ".into()), flags: flags(&["-n", "-r", "-u"]) },
            &[("printf '3\\n1\\n3\\n2\\n' > in\nsort -r -u -n in > out", "printf '3\\n1\\n3\\n2\\n' > in\nsort -n -u -r in > out")],
        ),
        // A definition re-serialized without change in meaning: blank lines, or spaces alone.
        step("blank-lines", Rewrite::Drop { op: Pattern::Is(String::new()) }, &[("\necho a > out\n\n   \n", "echo a > out")]),
        // A no-op step: `true` does nothing, wherever it is.
        step("no-op", Rewrite::Drop { op: Pattern::Is("true".into()) }, &[("echo a > out\ntrue", "echo a > out"), ("true\necho a > out\n  true", "echo a > out")]),
        // File operations that commute: `mkdir -p` makes the same folders in any order.
        step(
            "mkdir-commutes",
            Rewrite::Commute { a: Pattern::Starts("mkdir -p ".into()), b: Pattern::Starts("mkdir -p ".into()) },
            &[("mkdir -p a/b\nmkdir -p c", "mkdir -p c\nmkdir -p a/b"), ("mkdir -p a\nmkdir -p a/b", "mkdir -p a/b\nmkdir -p a")],
        ),
    ]
}

/// The witness of `p`'s differential test: each case's two definitions run by `run`, which gives
/// the digest of what a run produced.
pub fn witness(p: &Proven, run: &dyn Fn(&str) -> anyhow::Result<String>) -> anyhow::Result<Witness> {
    let cases = p.cases.iter().map(|(before, after)| Ok(Case { before: before.clone(), after: after.clone(), before_output: run(before)?, after_output: run(after)? })).collect::<anyhow::Result<Vec<Case>>>()?;
    Ok(Witness { test: format!("kiln::ci::rules::{}", p.rule.name), cases })
}

/// Every file and folder under `dir`, by path from it, with a file's bytes.
#[cfg(unix)]
fn tree(dir: &Path, at: &Path, out: &mut Vec<(String, Vec<u8>)>) -> anyhow::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let rel = p.strip_prefix(at)?.to_string_lossy().into_owned();
        if e.file_type()?.is_dir() {
            out.push((format!("{rel}/"), Vec::new()));
            tree(&p, at, out)?;
        } else {
            out.push((rel, std::fs::read(&p)?));
        }
    }
    Ok(())
}

#[cfg(unix)]
/// The differential tests' runner: runs `def` with `sh` in a folder of its own: the digest of what it printed, how it exited
/// and every file and folder it left.
fn run_in(d: &Path, def: &str) -> anyhow::Result<String> {
    let out = std::process::Command::new("sh").arg("-c").arg(def).current_dir(d).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).output()?;
    let mut files = Vec::new();
    tree(d, d, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    h.update(&out.stdout);
    h.update(out.status.code().unwrap_or(-1).to_le_bytes());
    for (path, bytes) in files {
        h.update(path.as_bytes());
        h.update([0]);
        h.update(&bytes);
        h.update([0]);
    }
    Ok(format!("sha256:{}", h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// [`run_in`] a folder made for it, and gone after.
#[cfg(unix)]
pub fn run(def: &str) -> anyhow::Result<String> {
    let d = std::env::temp_dir().join(format!("kiln-rule-{}", crate::util::new_id()));
    std::fs::create_dir_all(&d)?;
    let out = run_in(&d, def);
    let _ = std::fs::remove_dir_all(&d);
    out
}

#[cfg(all(test, unix))]
mod tests {
    use kiln_ci::work::eclass::Rules;

    use super::*;

    /// W24-R30: each of kiln's rules, its witness run for real: both definitions of every case
    /// leave identical outputs, the rule registers on that witness, and it makes them one.
    #[test]
    fn w24_r30_each_rule_has_a_witness_with_identical_outputs() -> anyhow::Result<()> {
        let mut rules = Rules::default();
        for p in kiln() {
            let w = witness(&p, &run)?;
            for c in &w.cases {
                anyhow::ensure!(c.before_output == c.after_output, "{}: {:?} and {:?} differ", p.rule.name, c.before, c.after);
            }
            rules.register(p.rule.clone(), w).map_err(|e| anyhow::anyhow!("{}: {e:?}", p.rule.name))?;
        }
        for p in kiln() {
            for (before, after) in &p.cases {
                anyhow::ensure!(rules.normalize(Kind::Step, before).0 == rules.normalize(Kind::Step, after).0, "{}: {before:?} and {after:?} aren't one", p.rule.name);
            }
        }
        Ok(())
    }

    /// W24-R30: a rule whose differential test sees different outputs is refused, and so is one
    /// without a witness.
    #[test]
    fn w24_r30_a_rule_without_a_witness_is_refused() -> anyhow::Result<()> {
        let mut rules = Rules::default();
        let unsound = step("writes-commute", Rewrite::Commute { a: Pattern::Starts("echo a".into()), b: Pattern::Starts("echo b".into()) }, &[("echo a > f\necho b > f", "echo b > f\necho a > f")]);
        let w = witness(&unsound, &run)?;
        anyhow::ensure!(w.cases.iter().all(|c| c.before_output != c.after_output), "the last write wins: the runs differ");
        anyhow::ensure!(rules.register(unsound.rule.clone(), w).is_err(), "refused on its own witness");
        anyhow::ensure!(rules.register(unsound.rule, Witness { test: "kiln::ci::rules::none".into(), cases: Vec::new() }).is_err(), "refused without one");
        anyhow::ensure!(rules.rules().count() == 0);
        Ok(())
    }
}
