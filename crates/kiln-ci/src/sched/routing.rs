//! Routing (docs/ci.md W05-R19 to W05-R24): which runners may run a leg, and which one should.
//!
//! A leg's requirement is its `runs-on` labels (GitHub-exact: a runner carries every label, or is
//! named by the one label), the constraints of the routing rules that match it, the constraints
//! the workflow declares, and the capabilities it needs. Constraints are a small language over a
//! runner's facts (W13): `os == linux`, `disk.free >= 50Gi`, `cpu.flags has avx2`,
//! `binfmt contains 'linux/arm64'`, `label gpu`, combined with `&&`, `||`, `!` and parentheses.
//! Unknown facts are false, never errors. Nothing here is "smart": rules and preferences apply
//! only when someone declared them.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::expr::{self, Budget, Condition, Dialect, MapContexts, ParseOptions, Value};
use crate::facts::RunnerFacts;
use crate::trigger::pattern::Pattern;

/// A fact's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Fact {
    Str(String),
    Num(f64),
    Bool(bool),
    List(Vec<String>),
    Missing,
}

impl Fact {
    pub fn show(&self) -> String {
        match self {
            Fact::Str(s) => s.clone(),
            Fact::Num(n) => expr::js_number_to_string(*n),
            Fact::Bool(b) => b.to_string(),
            Fact::List(l) => format!("[{}]", l.join(", ")),
            Fact::Missing => "(unknown)".into(),
        }
    }
}

fn opt(s: &Option<String>) -> Fact {
    s.clone().map_or(Fact::Missing, Fact::Str)
}

fn num(n: u64) -> Fact {
    Fact::Num(n as f64)
}

/// A fact of a runner by name (W05-R22).
pub fn fact(f: &RunnerFacts, name: &str) -> Fact {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "os" => Fact::Str(f.os.clone()),
        "os.version" => opt(&f.os_version),
        "os.distro" => opt(&f.os_distro),
        "kernel" => opt(&f.kernel),
        "arch" => Fact::Str(f.arch.clone()),
        "arch_variant" | "arch.variant" => opt(&f.arch_variant),
        "cpu.cores" => Fact::Num(f64::from(f.cpu.cores)),
        "cpu.threads" => Fact::Num(f64::from(f.cpu.threads)),
        "cpu.model" => opt(&f.cpu.model),
        "cpu.flags" => Fact::List(f.cpu.flags.clone()),
        "mem.total" => num(f.mem.total),
        "mem.free" => num(f.mem.free),
        "disk.total" => num(f.disk.total),
        "disk.free" => num(f.disk.free),
        "gpu.count" => num(f.gpus.len() as u64),
        "gpu.model" => Fact::List(f.gpus.iter().map(|g| g.model.clone()).collect()),
        "gpu.vendor" => Fact::List(f.gpus.iter().map(|g| g.vendor.clone()).collect()),
        "gpu.memory" => num(f.gpus.iter().map(|g| g.memory).max().unwrap_or(0)),
        "virt.kvm" => Fact::Bool(f.virt.kvm),
        "virt.hvf" => Fact::Bool(f.virt.hvf),
        "virt.nested" => Fact::Bool(f.virt.nested),
        "executors" => Fact::List(f.executors.keys().cloned().collect()),
        "binfmt" => Fact::List(f.binfmt.clone()),
        "tools" => Fact::List(f.tools.keys().cloned().collect()),
        "net.egress" => Fact::Bool(f.net.egress),
        _ => {
            if let Some(t) = n.strip_prefix("tools.") {
                let t = t.strip_suffix(".version").unwrap_or(t);
                return f.tools.get(t).map_or(Fact::Missing, |v| Fact::Str(v.clone()));
            }
            if let Some(e) = n.strip_prefix("executors.") {
                let e = e.strip_suffix(".version").unwrap_or(e);
                return f.executors.get(e).map_or(Fact::Missing, |x| x.version.clone().map_or(Fact::Bool(true), Fact::Str));
            }
            let key = n.strip_prefix("custom.").unwrap_or(name);
            f.custom.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map_or(Fact::Missing, |(_, v)| Fact::Str(v.clone()))
        }
    }
}

/// A literal in a constraint.
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Str(String),
    Num(f64),
}

impl Lit {
    fn show(&self) -> String {
        match self {
            Lit::Str(s) => s.clone(),
            Lit::Num(n) => expr::js_number_to_string(*n),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A parsed constraint.
#[derive(Debug, Clone, PartialEq)]
pub enum Constraint {
    And(Vec<Constraint>),
    Or(Vec<Constraint>),
    Not(Box<Constraint>),
    Label(String),
    Cmp { fact: String, op: Op, value: Lit },
    In { fact: String, values: Vec<Lit> },
    Has { fact: String, item: String },
    Matches { fact: String, glob: String },
    /// A bare fact: true when it is true, a non-zero number, or a non-empty string or list.
    Truthy(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Sym(&'static str),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let two: String = chars.iter().skip(i).take(2).collect();
        if let Some(sym) = ["&&", "||", "==", "!=", "<=", ">="].iter().find(|x| **x == two) {
            out.push(Tok::Sym(sym));
            i += 2;
            continue;
        }
        if let Some(sym) = ["!", "<", ">", "(", ")", "[", "]", ","].iter().find(|x| x.starts_with(c)) {
            out.push(Tok::Sym(sym));
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            let mut j = i + 1;
            let mut v = String::new();
            loop {
                match chars.get(j) {
                    None => return Err(format!("`{s}`: a quote is not closed")),
                    Some(&d) if d == c => break,
                    Some(&d) => v.push(d),
                }
                j += 1;
            }
            out.push(Tok::Str(v));
            i = j + 1;
            continue;
        }
        let mut v = String::new();
        while let Some(&d) = chars.get(i) {
            if d.is_whitespace() || "()[],!<>=&|'\"".contains(d) {
                break;
            }
            v.push(d);
            i += 1;
        }
        // A lone `=`, `&` or `|` (`os = linux`) is no symbol and no word: without this it was an
        // empty word, again and again, never moving on.
        if v.is_empty() {
            let meant = match c {
                '=' => "==",
                '&' => "&&",
                _ => "||",
            };
            return Err(format!("`{s}`: `{c}` alone isn't an operator (`{meant}`?)"));
        }
        out.push(Tok::Word(v));
    }
    Ok(out)
}

/// A number with an optional unit: sizes (`Ki`, `Mi`, `Gi`, `Ti`; `K`, `M`, `G`, `T`, `B`) and
/// durations (`s`, `m`, `h`, `d`).
pub fn number(s: &str) -> Option<f64> {
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(s.len());
    let (n, unit) = (s.get(..split)?, s.get(split..)?);
    let n: f64 = n.parse().ok()?;
    let k: f64 = match unit {
        "" | "B" | "s" => 1.0,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "K" | "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "m" => 60.0,
        "h" => 3_600.0,
        "d" => 86_400.0,
        _ => return None,
    };
    Some(n * k)
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    src: String,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(x)) if *x == s)
    }

    fn word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn bad(&self, why: &str) -> String {
        format!("`{}`: {why}", self.src)
    }

    fn or(&mut self) -> Result<Constraint, String> {
        let mut parts = vec![self.and()?];
        while self.sym("||") {
            self.at += 1;
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap_or(Constraint::And(Vec::new())) } else { Constraint::Or(parts) })
    }

    fn and(&mut self) -> Result<Constraint, String> {
        let mut parts = vec![self.unary()?];
        while self.sym("&&") {
            self.at += 1;
            parts.push(self.unary()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap_or(Constraint::And(Vec::new())) } else { Constraint::And(parts) })
    }

    fn unary(&mut self) -> Result<Constraint, String> {
        if self.sym("!") {
            self.at += 1;
            return Ok(Constraint::Not(Box::new(self.unary()?)));
        }
        if self.sym("(") {
            self.at += 1;
            let c = self.or()?;
            if !self.sym(")") {
                return Err(self.bad("`(` is not closed"));
            }
            self.at += 1;
            return Ok(c);
        }
        self.atom()
    }

    fn value(&mut self) -> Result<Lit, String> {
        match self.next() {
            Some(Tok::Str(s)) => Ok(Lit::Str(s)),
            Some(Tok::Word(w)) => Ok(number(&w).map_or(Lit::Str(w), Lit::Num)),
            _ => Err(self.bad("a value is missing")),
        }
    }

    fn text(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Tok::Str(s) | Tok::Word(s)) => Ok(s),
            _ => Err(self.bad("a value is missing")),
        }
    }

    fn atom(&mut self) -> Result<Constraint, String> {
        if self.word("label") {
            self.at += 1;
            return Ok(Constraint::Label(self.text()?));
        }
        let fact = match self.next() {
            Some(Tok::Word(w)) => w,
            _ => return Err(self.bad("expected a fact (os, arch, disk.free, …)")),
        };
        let op = match self.peek() {
            Some(Tok::Sym("==")) => Some(Op::Eq),
            Some(Tok::Sym("!=")) => Some(Op::Ne),
            Some(Tok::Sym("<")) => Some(Op::Lt),
            Some(Tok::Sym("<=")) => Some(Op::Le),
            Some(Tok::Sym(">")) => Some(Op::Gt),
            Some(Tok::Sym(">=")) => Some(Op::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.at += 1;
            return Ok(Constraint::Cmp { fact, op, value: self.value()? });
        }
        if self.word("in") {
            self.at += 1;
            if !self.sym("[") {
                return Err(self.bad("`in` takes a list: in [a, b]"));
            }
            self.at += 1;
            let mut values = Vec::new();
            while !self.sym("]") {
                values.push(self.value()?);
                if self.sym(",") {
                    self.at += 1;
                } else if !self.sym("]") {
                    return Err(self.bad("the list is not closed with `]`"));
                }
            }
            self.at += 1;
            return Ok(Constraint::In { fact, values });
        }
        if self.word("has") || self.word("contains") {
            self.at += 1;
            return Ok(Constraint::Has { fact, item: self.text()? });
        }
        if self.word("matches") {
            self.at += 1;
            return Ok(Constraint::Matches { fact, glob: self.text()? });
        }
        Ok(Constraint::Truthy(fact))
    }
}

impl Constraint {
    pub fn parse(src: &str) -> Result<Constraint, String> {
        let toks = lex(src)?;
        if toks.is_empty() {
            return Err(format!("`{src}`: the constraint is empty"));
        }
        let mut p = Parser { toks, at: 0, src: src.to_owned() };
        let c = p.or()?;
        if p.at < p.toks.len() {
            return Err(p.bad("unexpected text after the constraint"));
        }
        Ok(c)
    }

    /// Whether a runner with these facts and labels satisfies it.
    pub fn eval(&self, f: &RunnerFacts, labels: &[String]) -> bool {
        match self {
            Constraint::And(cs) => cs.iter().all(|c| c.eval(f, labels)),
            Constraint::Or(cs) => cs.iter().any(|c| c.eval(f, labels)),
            Constraint::Not(c) => !c.eval(f, labels),
            Constraint::Label(l) => labels.iter().any(|x| x.eq_ignore_ascii_case(l)),
            Constraint::Cmp { fact: name, op, value } => {
                let v = fact(f, name);
                let Some(ord) = compare(&v, value) else { return false };
                match op {
                    Op::Eq => ord == Ordering::Equal,
                    Op::Ne => ord != Ordering::Equal,
                    Op::Lt => ord == Ordering::Less,
                    Op::Le => ord != Ordering::Greater,
                    Op::Gt => ord == Ordering::Greater,
                    Op::Ge => ord != Ordering::Less,
                }
            }
            Constraint::In { fact: name, values } => {
                let v = fact(f, name);
                values.iter().any(|x| compare(&v, x) == Some(Ordering::Equal))
            }
            Constraint::Has { fact: name, item } => match fact(f, name) {
                Fact::List(l) => l.iter().any(|x| x.eq_ignore_ascii_case(item)),
                Fact::Str(s) => s.to_ascii_lowercase().contains(&item.to_ascii_lowercase()),
                _ => false,
            },
            Constraint::Matches { fact: name, glob } => {
                let Ok(p) = Pattern::new(glob) else { return false };
                match fact(f, name) {
                    Fact::Str(s) => p.matches(&s),
                    Fact::List(l) => l.iter().any(|x| p.matches(x)),
                    _ => false,
                }
            }
            Constraint::Truthy(name) => match fact(f, name) {
                Fact::Bool(b) => b,
                Fact::Num(n) => n != 0.0,
                Fact::Str(s) => !s.is_empty(),
                Fact::List(l) => !l.is_empty(),
                Fact::Missing => false,
            },
        }
    }

    /// The facts it reads, for explanations.
    pub fn facts(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<String>) {
        match self {
            Constraint::And(cs) | Constraint::Or(cs) => cs.iter().for_each(|c| c.collect(out)),
            Constraint::Not(c) => c.collect(out),
            Constraint::Label(_) => {}
            Constraint::Cmp { fact, .. } | Constraint::In { fact, .. } | Constraint::Has { fact, .. } | Constraint::Matches { fact, .. } | Constraint::Truthy(fact) => {
                if !out.contains(fact) {
                    out.push(fact.clone());
                }
            }
        }
    }
}

/// Dotted versions compare part by part (`22.04 < 24.04`, `1.10 > 1.9`).
fn version(a: &str, b: &str) -> Option<Ordering> {
    let pa: Vec<&str> = a.split('.').collect();
    let pb: Vec<&str> = b.split('.').collect();
    if pa.len() < 2 && pb.len() < 2 {
        return None;
    }
    for i in 0..pa.len().max(pb.len()) {
        let x: u64 = pa.get(i).map_or(Some(0), |s| s.parse().ok())?;
        let y: u64 = pb.get(i).map_or(Some(0), |s| s.parse().ok())?;
        match x.cmp(&y) {
            Ordering::Equal => {}
            o => return Some(o),
        }
    }
    Some(Ordering::Equal)
}

fn compare(v: &Fact, lit: &Lit) -> Option<Ordering> {
    match (v, lit) {
        (Fact::Num(a), Lit::Num(b)) => a.partial_cmp(b),
        (Fact::Num(a), Lit::Str(s)) => number(s).and_then(|b| a.partial_cmp(&b)),
        (Fact::Str(a), Lit::Num(b)) => version(a, &lit.show()).or_else(|| a.parse::<f64>().ok().and_then(|x| x.partial_cmp(b))),
        (Fact::Str(a), Lit::Str(b)) => Some(version(a, b).unwrap_or_else(|| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()))),
        (Fact::Bool(a), Lit::Str(b)) => Some(a.to_string().cmp(&b.to_ascii_lowercase())),
        (Fact::List(l), Lit::Str(b)) => Some(if l.iter().any(|x| x.eq_ignore_ascii_case(b)) { Ordering::Equal } else { Ordering::Less }),
        _ => None,
    }
}

/// GitHub's labels for a self-hosted runner, from its facts: `self-hosted`, the OS (`Linux`,
/// `macOS`, `Windows`) and the architecture (`X64`, `ARM64`, `ARM`) (labels are
/// case-insensitive).
pub fn implicit_labels(f: &RunnerFacts) -> Vec<String> {
    let mut out = vec!["self-hosted".to_owned()];
    out.push(
        match f.os.as_str() {
            "linux" => "Linux",
            "macos" => "macOS",
            "windows" => "Windows",
            other => other,
        }
        .to_owned(),
    );
    out.push(
        match f.arch.as_str() {
            "x86_64" => "X64",
            "aarch64" => "ARM64",
            "arm" => "ARM",
            "x86" => "X86",
            other => other,
        }
        .to_owned(),
    );
    out
}

/// GitHub-exact label matching: the runner carries every label, or the one label is its name.
/// Returns the labels it lacks.
pub fn missing_labels(required: &[String], runner_labels: &[String], runner_name: &str) -> Vec<String> {
    if let [only] = required
        && only.eq_ignore_ascii_case(runner_name)
    {
        return Vec::new();
    }
    required.iter().filter(|l| !runner_labels.iter().any(|x| x.eq_ignore_ascii_case(l))).cloned().collect()
}

/// A weighted preference term (W05-R23), opt-in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preference {
    /// A numeric fact, preferred `higher` or lower.
    pub fact: Option<String>,
    pub higher: bool,
    /// Runners carrying this label.
    pub label: Option<String>,
    pub weight: Option<f64>,
}

/// What selects the legs a routing rule applies to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleMatch {
    /// The leg's `runs-on` holds all of these.
    pub labels: Vec<String>,
    /// An expression over `job` (`id`, `name`), `workflow`, `repo` and `event`.
    pub job: Option<String>,
    /// Repository globs.
    pub repos: Vec<String>,
    /// Whether the leg's steps run containers themselves (`podman`, `buildah`: [`runs_containers`]).
    pub nests_containers: Option<bool>,
}

/// A routing rule (W05-R21): declared, ordered data.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    #[serde(rename = "match")]
    pub matches: RuleMatch,
    pub require: Vec<String>,
    /// Allowed executors, in order of preference.
    pub executor: Vec<String>,
    /// The image for container and microVM executors (W13).
    pub image: Option<String>,
    pub prefer: Vec<Preference>,
    /// Docker inside the leg (W13-R04), opt-in: `socket`, the host's Docker socket in the
    /// `docker` executor's container, for trusted runs' legs only.
    pub docker: Option<String>,
}

/// The rules mapping GitHub's hosted labels to requirements (W05-R21). They ship disabled: an
/// administrator turns them on (`[ci.routing] github_labels = true`).
pub fn github_label_rules() -> Vec<Rule> {
    // Ubuntu's labels run in a fresh container of kiln's Ubuntu runner image where the runner has
    // Docker, the way GitHub gives each job a fresh machine, and on the host otherwise.
    let rule = |labels: &[&str], os: &str, arch: &str, image: &str| Rule {
        matches: RuleMatch { labels: labels.iter().map(|s| (*s).to_owned()).collect(), ..RuleMatch::default() },
        require: vec![format!("os == {os}"), format!("arch == {arch}")],
        executor: if image.is_empty() { Vec::new() } else { vec!["docker".to_owned(), "host".to_owned()] },
        image: (!image.is_empty()).then(|| image.to_owned()),
        ..Rule::default()
    };
    vec![
        rule(&["ubuntu-latest"], "linux", "x86_64", "kiln/runner-ubuntu-24.04"),
        rule(&["ubuntu-26.04"], "linux", "x86_64", "kiln/runner-ubuntu-26.04"),
        rule(&["ubuntu-24.04"], "linux", "x86_64", "kiln/runner-ubuntu-24.04"),
        rule(&["ubuntu-22.04"], "linux", "x86_64", "kiln/runner-ubuntu-22.04"),
        rule(&["ubuntu-26.04-arm"], "linux", "aarch64", "kiln/runner-ubuntu-26.04"),
        rule(&["ubuntu-24.04-arm"], "linux", "aarch64", "kiln/runner-ubuntu-24.04"),
        rule(&["ubuntu-22.04-arm"], "linux", "aarch64", "kiln/runner-ubuntu-22.04"),
        // GitHub's lightweight Ubuntu 24.04 in a container. Its smaller set of tools still has
        // Node.js and npm, the Docker CLI, gh and PowerShell, which kiln's own `-slim` (the basics
        // alone) leaves out: the full image has them.
        rule(&["ubuntu-slim"], "linux", "x86_64", "kiln/runner-ubuntu-24.04"),
        rule(&["macos-latest"], "macos", "aarch64", ""),
        rule(&["macos-26"], "macos", "aarch64", ""),
        rule(&["macos-15"], "macos", "aarch64", ""),
        rule(&["macos-14"], "macos", "aarch64", ""),
        rule(&["macos-15-intel"], "macos", "x86_64", ""),
        rule(&["macos-13"], "macos", "x86_64", ""),
        rule(&["windows-latest"], "windows", "x86_64", ""),
        rule(&["windows-2025"], "windows", "x86_64", ""),
        rule(&["windows-2022"], "windows", "x86_64", ""),
        rule(&["windows-11-arm"], "windows", "aarch64", ""),
    ]
}

/// kiln's own label rule: `native` asks for the leg's platform, which `platforms` legs already
/// require (W04-R13), so the label itself needs nothing of a runner.
pub fn native_rule() -> Rule {
    Rule { matches: RuleMatch { labels: vec!["native".into()], ..RuleMatch::default() }, ..Rule::default() }
}

/// What a leg asks of routing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegRoute {
    pub job: String,
    pub job_name: String,
    pub workflow: String,
    pub repo: String,
    pub event: String,
    pub labels: Vec<String>,
    pub group: Option<String>,
    /// From the workflow (`requires:`, `platforms`).
    pub requires: Vec<String>,
    /// Capability bits the leg needs (`CAP_CI2`, `CAP_ACTIONS`, …).
    pub caps: u64,
    /// An untrusted leg's limits (W18-R07); `None` for a trusted one.
    pub untrusted: Option<Untrusted>,
    /// Its steps run containers themselves ([`runs_containers`]).
    pub nests_containers: bool,
}

/// Where untrusted legs may run (`ci.untrusted`, W18-R07).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Untrusted {
    /// The executors, in order; `host` only when listed.
    pub executors: Vec<String>,
    /// Docker even when its engine runs as root.
    pub rootful_docker: bool,
}

/// A leg's requirement, resolved against the rules.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requirement {
    /// Labels a runner must carry (those no rule consumed).
    pub labels: Vec<String>,
    pub group: Option<String>,
    /// Constraints, with their source text.
    pub constraints: Vec<(String, Constraint)>,
    pub caps: u64,
    /// Executors allowed (empty: any).
    pub executors: Vec<String>,
    pub image: Option<String>,
    pub prefer: Vec<Preference>,
    /// Docker inside the leg, from the first rule that says (`socket`).
    pub docker: Option<String>,
    /// The rules that applied, by index, for explanations.
    pub rules: Vec<usize>,
    /// Docker only when its engine runs rootless (an untrusted leg's, W18-R07).
    pub rootless_docker: bool,
    /// The pool whose executors run it (W13-R09).
    pub pool: Option<String>,
}

impl Requirement {
    /// The signature legs with the same requirement share (W05-R04).
    pub fn signature(&self) -> String {
        let mut labels: Vec<String> = self.labels.iter().map(|l| l.to_ascii_lowercase()).collect();
        labels.sort();
        labels.dedup();
        let mut cs: Vec<&str> = self.constraints.iter().map(|(s, _)| s.as_str()).collect();
        cs.sort_unstable();
        format!(
            "labels={};group={};req={};caps={:x};exec={}{}{}",
            labels.join(","),
            self.group.as_deref().unwrap_or(""),
            cs.join(" && "),
            self.caps,
            self.executors.join(","),
            if self.rootless_docker { ";rootless" } else { "" },
            self.pool.as_deref().map(|p| format!(";pool={p}")).unwrap_or_default()
        )
    }
}

fn rule_applies(r: &Rule, leg: &LegRoute) -> bool {
    let m = &r.matches;
    if !m.labels.iter().all(|l| leg.labels.iter().any(|x| x.eq_ignore_ascii_case(l))) {
        return false;
    }
    if !m.repos.is_empty() && !m.repos.iter().any(|g| Pattern::new(g).is_ok_and(|p| p.matches(&leg.repo))) {
        return false;
    }
    if let Some(j) = &m.job
        && !job_condition(j, leg)
    {
        return false;
    }
    if m.nests_containers.is_some_and(|n| n != leg.nests_containers) {
        return false;
    }
    // A rule with nothing to match applies to every leg (a fleet-wide default).
    true
}

/// Whether a `run:` script runs containers itself: calls `podman` or `buildah` as a command (at a
/// line's start, after `;`, `&&`, `||`, `|`, `(` or a backquote, past `sudo`, `exec`, `time` and
/// `NAME=value`), not in a comment. Their containers need what a container executor's container
/// doesn't give (W13).
pub fn runs_containers(script: &str) -> bool {
    script.lines().map(str::trim_start).filter(|l| !l.starts_with('#')).any(|line| {
        line.split([';', '|', '&', '(', '`']).any(|cmd| {
            let mut words = cmd.split_whitespace().skip_while(|w| matches!(*w, "sudo" | "exec" | "time" | "-E") || (w.contains('=') && !w.starts_with('-')));
            words.next().is_some_and(|w| matches!(w.trim_start_matches('\\'), "podman" | "buildah"))
        })
    })
}

/// Whether a condition over a leg holds (a rule's `match.job`, a reservation's `for`): over `job`
/// (`id`, `name`), `workflow`, `repo` and `event`. One that doesn't parse holds for none.
pub fn job_condition(src: &str, leg: &LegRoute) -> bool {
    let Ok(c) = Condition::parse(src, &ParseOptions::permissive(Dialect::Kiln)) else { return false };
    let cx = MapContexts::default()
        .with("job", Value::object([("id".to_owned(), Value::str(&leg.job)), ("name".to_owned(), Value::str(&leg.job_name))]))
        .with("workflow", Value::str(&leg.workflow))
        .with("repo", Value::str(&leg.repo))
        .with("event", Value::str(&leg.event));
    expr::eval_condition(&c, &cx, &mut Budget::default()).unwrap_or(false)
}

/// A leg's requirement (W05-R19): rules that match it, in order, add their constraints and
/// consume the labels they matched; the workflow's own constraints follow.
pub fn resolve(leg: &LegRoute, rules: &[Rule]) -> Result<Requirement, String> {
    let mut req = Requirement { labels: leg.labels.clone(), group: leg.group.clone(), caps: leg.caps, ..Requirement::default() };
    for (i, r) in rules.iter().enumerate() {
        if !rule_applies(r, leg) {
            continue;
        }
        req.labels.retain(|l| !r.matches.labels.iter().any(|x| x.eq_ignore_ascii_case(l)));
        for c in &r.require {
            req.constraints.push((c.clone(), Constraint::parse(c)?));
        }
        if req.executors.is_empty() {
            req.executors = r.executor.clone();
        }
        if req.image.is_none() {
            req.image = r.image.clone();
        }
        if req.docker.is_none() {
            req.docker = r.docker.clone();
        }
        req.prefer.extend(r.prefer.iter().cloned());
        req.rules.push(i);
    }
    for c in &leg.requires {
        req.constraints.push((c.clone(), Constraint::parse(c)?));
    }
    // Steps that run containers themselves can't in a container executor's container (`docker`,
    // `k8s`): such a leg goes where it can, the other executors the rules allow, else a microVM.
    if leg.nests_containers && req.executors.iter().any(|e| e == "docker" || e == "k8s") {
        req.executors.retain(|e| e != "docker" && e != "k8s");
        if req.executors.is_empty() {
            req.executors = vec!["firecracker".to_owned()];
        }
    }
    // An untrusted leg: only the executors the server lets untrusted code run on, whatever the
    // rules would have chosen (W18-R07).
    if let Some(u) = &leg.untrusted {
        let chosen = std::mem::take(&mut req.executors);
        req.executors = if chosen.is_empty() { u.executors.clone() } else { chosen.iter().filter(|e| u.executors.contains(e)).cloned().collect() };
        if req.executors.is_empty() {
            return Err(format!(
                "untrusted legs run only on {} (`ci.untrusted.executors`), and the routing rules give this one {}",
                if u.executors.is_empty() { "no executor".to_owned() } else { u.executors.join(", ") },
                chosen.join(", ")
            ));
        }
        req.rootless_docker = !u.rootful_docker;
    }
    Ok(req)
}

/// A runner as routing sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunnerView {
    pub name: String,
    pub labels: Vec<String>,
    pub pool: Option<String>,
    pub facts: RunnerFacts,
    pub caps: u64,
}

impl RunnerView {
    /// Its labels and the ones GitHub gives every self-hosted runner.
    pub fn all_labels(&self) -> Vec<String> {
        let mut l = self.labels.clone();
        for x in implicit_labels(&self.facts) {
            if !l.iter().any(|y| y.eq_ignore_ascii_case(&x)) {
                l.push(x);
            }
        }
        l
    }
}

/// One requirement's check against one runner, for explanations (W05-R20).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    pub requirement: String,
    pub value: String,
    pub pass: bool,
}

/// Every requirement checked against a runner. A runner is eligible when every check passes.
pub fn checks(req: &Requirement, r: &RunnerView) -> Vec<Check> {
    let labels = r.all_labels();
    let mut out = Vec::new();
    let missing = missing_labels(&req.labels, &labels, &r.name);
    if !req.labels.is_empty() {
        out.push(Check { requirement: format!("runs-on [{}]", req.labels.join(", ")), value: format!("labels [{}]", labels.join(", ")), pass: missing.is_empty() });
    }
    if let Some(g) = &req.group {
        out.push(Check { requirement: format!("group {g}"), value: r.pool.clone().unwrap_or_else(|| "(no pool)".into()), pass: r.pool.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(g)) });
    }
    for (src, c) in &req.constraints {
        let value = c.facts().iter().map(|f| format!("{f} = {}", fact(&r.facts, f).show())).collect::<Vec<_>>().join(", ");
        out.push(Check { requirement: src.clone(), value, pass: c.eval(&r.facts, &labels) });
    }
    if req.caps != 0 {
        let lacking = req.caps & !r.caps;
        out.push(Check { requirement: format!("capabilities {:#x}", req.caps), value: format!("has {:#x}", r.caps), pass: lacking == 0 });
    }
    if !req.executors.is_empty() {
        let has: Vec<&String> = req.executors.iter().filter(|e| usable(req, &r.facts, e)).collect();
        let shown = if r.facts.executors.is_empty() {
            "host".to_owned()
        } else {
            r.facts.executors.iter().map(|(name, x)| if name == "docker" && rootless(x) { "docker (rootless)".to_owned() } else { name.clone() }).collect::<Vec<_>>().join(", ")
        };
        let wanted = req.executors.iter().map(|e| if e == "docker" && req.rootless_docker { "docker (rootless)".to_owned() } else { e.clone() }).collect::<Vec<_>>();
        out.push(Check { requirement: format!("executor in [{}]", wanted.join(", ")), value: format!("[{shown}]"), pass: !has.is_empty() });
    }
    out
}

/// Whether a runner has an executor. One that reports none runs legs on its host, as every
/// runner can.
pub fn has_executor(facts: &crate::facts::RunnerFacts, e: &str) -> bool {
    facts.executors.contains_key(e) || (e == "host" && facts.executors.is_empty())
}

fn rootless(x: &crate::facts::Executor) -> bool {
    x.details.get("rootless").is_some_and(|v| v == "true")
}

/// Whether a runner's executor `e` can take a leg of `req`: it has it, and Docker runs rootless
/// when the leg needs that (an untrusted leg's, W18-R07).
pub fn usable(req: &Requirement, facts: &crate::facts::RunnerFacts, e: &str) -> bool {
    has_executor(facts, e) && !(req.rootless_docker && e == "docker" && !facts.executors.get("docker").is_some_and(rootless))
}

pub fn eligible(req: &Requirement, r: &RunnerView) -> bool {
    checks(req, r).iter().all(|c| c.pass)
}

/// Only the capability checks fail: the runner could run the leg if its build were newer
/// (wait reason `capability`, W05-R13).
pub fn lacks_only_capability(req: &Requirement, r: &RunnerView) -> bool {
    let cs = checks(req, r);
    let caps_failed = cs.iter().any(|c| !c.pass && c.requirement.starts_with("capabilities"));
    caps_failed && cs.iter().all(|c| c.pass || c.requirement.starts_with("capabilities"))
}

/// The preference score of each candidate (W05-R23): Σ weight × term, each term normalized to
/// 0..1 across the candidates. Empty when there are no preferences.
pub fn scores(prefs: &[Preference], candidates: &[&RunnerView]) -> Vec<f64> {
    if prefs.is_empty() {
        return Vec::new();
    }
    let mut total = vec![0.0f64; candidates.len()];
    for p in prefs {
        let w = p.weight.unwrap_or(1.0);
        if let Some(name) = &p.fact {
            let vals: Vec<Option<f64>> = candidates
                .iter()
                .map(|r| match fact(&r.facts, name) {
                    Fact::Num(n) => Some(n),
                    Fact::Str(s) => number(&s),
                    Fact::Bool(b) => Some(if b { 1.0 } else { 0.0 }),
                    _ => None,
                })
                .collect();
            let known: Vec<f64> = vals.iter().flatten().copied().collect();
            let (lo, hi) = (known.iter().copied().fold(f64::MAX, f64::min), known.iter().copied().fold(f64::MIN, f64::max));
            for (t, v) in total.iter_mut().zip(&vals) {
                if let Some(v) = v {
                    let norm = if hi > lo { (v - lo) / (hi - lo) } else { 1.0 };
                    *t += w * if p.higher { norm } else { 1.0 - norm };
                }
            }
        }
        if let Some(l) = &p.label {
            for (t, r) in total.iter_mut().zip(candidates) {
                if r.all_labels().iter().any(|x| x.eq_ignore_ascii_case(l)) {
                    *t += w;
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{Cpu, Disk, Executor};

    fn linux(free: u64) -> RunnerFacts {
        RunnerFacts {
            os: "linux".into(),
            arch: "x86_64".into(),
            os_version: Some("24.04".into()),
            cpu: Cpu { cores: 16, threads: 32, flags: vec!["avx2".into(), "sse4_2".into()], model: Some("AMD EPYC 7763".into()) },
            disk: Disk { total: 1 << 40, free },
            executors: [("host".to_owned(), Executor::default()), ("docker".to_owned(), Executor::default())].into_iter().collect(),
            binfmt: vec!["linux/arm64".into()],
            ..RunnerFacts::default()
        }
    }

    fn holds(c: &str, f: &RunnerFacts, labels: &[&str]) -> bool {
        let labels: Vec<String> = labels.iter().map(|s| (*s).to_owned()).collect();
        Constraint::parse(c).is_ok_and(|c| c.eval(f, &labels))
    }

    #[test]
    fn constraints() {
        let f = linux(500 << 30);
        assert!(holds("os == linux && arch == x86_64", &f, &[]));
        assert!(holds("os == 'linux' && arch == 'x86_64'", &f, &[]));
        assert!(!holds("os == macos", &f, &[]));
        assert!(holds("disk.free >= 50Gi", &f, &[]) && !holds("disk.free >= 600Gi", &f, &[]));
        assert!(holds("cpu.cores > 8 && cpu.flags has avx2", &f, &[]));
        assert!(holds("binfmt contains 'linux/arm64'", &f, &[]));
        assert!(holds("os.version >= 22.04", &f, &[]) && !holds("os.version < 22.04", &f, &[]));
        assert!(holds("arch in [aarch64, x86_64]", &f, &[]));
        assert!(holds("cpu.model matches '*EPYC*'", &f, &[]));
        assert!(holds("label gpu || os == linux", &f, &[]) && holds("label gpu", &f, &["GPU"]) && !holds("!label gpu", &f, &["gpu"]));
        assert!(holds("!(os == windows) && executors has docker", &f, &[]));
        // Unknown facts are false, never errors.
        assert!(!holds("gpu.memory > 0", &f, &[]) && !holds("custom.rack == a1", &f, &[]) && holds("!custom.rack", &f, &[]));
        for bad in ["", "os ==", "(os == linux", "arch in x86_64", "os == linux ]", "os = linux", "label a & label b", "label a | label b"] {
            assert!(Constraint::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Constraint::parse("os = linux"), Err("`os = linux`: `=` alone isn't an operator (`==`?)".into()));
        assert_eq!(number("50Gi"), Some(50.0 * 1024.0 * 1024.0 * 1024.0));
        assert_eq!(number("8m"), Some(480.0));
    }

    #[test]
    fn labels_github_exact() {
        let r = RunnerView { name: "mac-1".into(), labels: vec!["bench".into()], facts: RunnerFacts { os: "macos".into(), arch: "aarch64".into(), ..RunnerFacts::default() }, ..RunnerView::default() };
        let l = r.all_labels();
        assert!(missing_labels(&["self-hosted".into(), "macOS".into(), "ARM64".into()], &l, &r.name).is_empty());
        assert_eq!(missing_labels(&["self-hosted".into(), "linux".into()], &l, &r.name), vec!["linux"]);
        // Named by its one label.
        assert!(missing_labels(&["mac-1".into()], &l, &r.name).is_empty());
        assert!(!missing_labels(&["mac-1".into(), "x".into()], &l, &r.name).is_empty());
    }

    /// W05-R21: labels match in any case, as GitHub's do. `[baremetal, linux]` fits a Linux runner
    /// labelled only `baremetal`, by GitHub's implicit `Linux` (production's adacachy); `[Linux]`
    /// fits a runner labelled `linux`; and `[baremetal, linux]` doesn't fit a Mac.
    #[test]
    fn w05_r21_labels_match_in_any_case() -> Result<(), String> {
        let runner = |labels: &[&str], os: &str, arch: &str| RunnerView {
            name: format!("{os}-runner"),
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            facts: RunnerFacts { os: os.into(), arch: arch.into(), ..RunnerFacts::default() },
            ..RunnerView::default()
        };
        let implicit = runner(&["baremetal"], "linux", "x86_64");
        let labelled = runner(&["linux"], "linux", "x86_64");
        let mac = runner(&["baremetal"], "macos", "aarch64");
        let asks = |labels: &[&str]| resolve(&LegRoute { labels: labels.iter().map(|l| (*l).to_owned()).collect(), ..LegRoute::default() }, &[native_rule()]);
        let (metal_linux, cap_linux) = (asks(&["baremetal", "linux"])?, asks(&["Linux"])?);
        assert!(implicit.all_labels().contains(&"Linux".to_owned()) && !implicit.labels.iter().any(|l| l == "linux"), "its only Linux label is GitHub's implicit one");
        assert!(eligible(&metal_linux, &implicit), "{:?}", checks(&metal_linux, &implicit));
        assert!(eligible(&cap_linux, &labelled), "{:?}", checks(&cap_linux, &labelled));
        assert!(eligible(&cap_linux, &implicit) && eligible(&asks(&["BAREMETAL", "LINUX"])?, &implicit));
        assert!(!eligible(&metal_linux, &mac), "a Mac isn't Linux");
        assert_eq!(missing_labels(&metal_linux.labels, &mac.all_labels(), &mac.name), vec!["linux"]);
        Ok(())
    }

    #[test]
    fn rules_resolve_and_explain() -> Result<(), String> {
        let leg = LegRoute { job: "build".into(), labels: vec!["ubuntu-latest".into()], repo: "acme/app".into(), caps: 1 << 10, ..LegRoute::default() };
        // Disabled by default: the label must be on the runner.
        let req = resolve(&leg, &[])?;
        assert_eq!(req.labels, vec!["ubuntu-latest"]);
        // Enabled: the label becomes OS and architecture.
        let req = resolve(&leg, &github_label_rules())?;
        assert!(req.labels.is_empty());
        assert_eq!(req.constraints.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), vec!["os == linux", "arch == x86_64"]);
        assert_eq!(req.image.as_deref(), Some("kiln/runner-ubuntu-24.04"));
        let good = RunnerView { name: "a".into(), facts: linux(10), caps: 1 << 10, ..RunnerView::default() };
        let old = RunnerView { caps: 0, ..good.clone() };
        let mac = RunnerView { name: "m".into(), facts: RunnerFacts { os: "macos".into(), arch: "aarch64".into(), ..RunnerFacts::default() }, caps: 1 << 10, ..RunnerView::default() };
        assert!(eligible(&req, &good) && !eligible(&req, &old) && !eligible(&req, &mac));
        assert!(lacks_only_capability(&req, &old) && !lacks_only_capability(&req, &mac));
        let why = checks(&req, &mac);
        assert!(why.iter().any(|c| c.requirement == "os == linux" && c.value == "os = macos" && !c.pass), "{why:?}");
        // Repository and job matches, and executors.
        let rule = Rule { matches: RuleMatch { job: Some("job.id == 'build'".into()), repos: vec!["acme/*".into()], ..RuleMatch::default() }, require: vec!["virt.kvm".into()], executor: vec!["firecracker".into()], ..Rule::default() };
        let req = resolve(&leg, std::slice::from_ref(&rule))?;
        assert_eq!((req.rules.clone(), req.executors.clone()), (vec![0], vec!["firecracker".to_owned()]));
        let other = LegRoute { repo: "other/x".into(), ..leg.clone() };
        assert!(resolve(&other, &[rule])?.rules.is_empty());
        // GitHub's newer labels, each its image.
        for (label, image) in [("ubuntu-26.04", "kiln/runner-ubuntu-26.04"), ("ubuntu-26.04-arm", "kiln/runner-ubuntu-26.04"), ("ubuntu-slim", "kiln/runner-ubuntu-24.04")] {
            let req = resolve(&LegRoute { labels: vec![label.into()], ..leg.clone() }, &github_label_rules())?;
            assert_eq!(req.image.as_deref(), Some(image), "{label}");
        }
        // The same requirement, the same signature.
        assert_eq!(resolve(&leg, &github_label_rules())?.signature(), resolve(&LegRoute { job: "test".into(), ..leg }, &github_label_rules())?.signature());
        Ok(())
    }

    /// Steps that run containers themselves (rails' `podman build`): such a leg leaves container
    /// executors for the others its rules allow, else a microVM, and a rule can match it to send
    /// it to one (docs/ci.md W13).
    #[test]
    fn w13_a_leg_that_runs_podman_leaves_container_executors() -> Result<(), String> {
        for script in ["podman build -t app .", "set -e\npodman run --rm app", "cd x && sudo podman ps", "FOO=1 buildah bud .", "echo $(podman --version)", "  podman \\\n  build ."] {
            assert!(runs_containers(script), "{script:?}");
        }
        for script in ["echo podman", "# podman build", "docker build .", "./podman-compose up", "apt-get install podman-docker", "grep -r podman ."] {
            assert!(!runs_containers(script), "{script:?}");
        }
        let leg = LegRoute { labels: vec!["ubuntu-latest".into()], nests_containers: true, ..LegRoute::default() };
        assert_eq!(resolve(&leg, &github_label_rules())?.executors, vec!["host".to_owned()], "not in the runner image's container");
        let docker_only = Rule { executor: vec!["docker".into()], ..Rule::default() };
        assert_eq!(resolve(&leg, std::slice::from_ref(&docker_only))?.executors, vec!["firecracker".to_owned()]);
        // The rule the corpus lab has: such legs to Firecracker microVMs, the image still the label's.
        let microvm = Rule { matches: RuleMatch { nests_containers: Some(true), ..RuleMatch::default() }, executor: vec!["firecracker".into()], ..Rule::default() };
        let rules: Vec<Rule> = std::iter::once(microvm).chain(github_label_rules()).collect();
        let req = resolve(&leg, &rules)?;
        assert_eq!((req.executors, req.image.as_deref()), (vec!["firecracker".to_owned()], Some("kiln/runner-ubuntu-24.04")));
        let plain = resolve(&LegRoute { nests_containers: false, ..leg }, &rules)?;
        assert_eq!(plain.executors, vec!["docker".to_owned(), "host".to_owned()], "others as before");
        Ok(())
    }

    /// W18-R07: an untrusted leg runs only on the executors untrusted code may use, Docker only
    /// rootless unless the server allows otherwise, and never where the rules alone send it.
    #[test]
    fn w18_r07_untrusted_executors() -> Result<(), String> {
        let untrusted = Untrusted { executors: vec!["firecracker".into(), "shards".into(), "docker".into()], rootful_docker: false };
        let leg = LegRoute { labels: vec!["linux".into()], untrusted: Some(untrusted.clone()), ..LegRoute::default() };
        let req = resolve(&leg, &[])?;
        assert_eq!((req.executors.clone(), req.rootless_docker), (untrusted.executors.clone(), true));
        let docker = |rootless: bool| RunnerView {
            name: "r".into(),
            labels: vec!["linux".into()],
            facts: RunnerFacts {
                os: "linux".into(),
                executors: [("host".to_owned(), Executor::default()), ("docker".to_owned(), Executor { details: [("rootless".to_owned(), rootless.to_string())].into(), ..Executor::default() })].into_iter().collect(),
                ..RunnerFacts::default()
            },
            ..RunnerView::default()
        };
        assert!(eligible(&req, &docker(true)) && !eligible(&req, &docker(false)), "rootful Docker isn't for untrusted code");
        assert!(checks(&req, &docker(false)).iter().any(|c| !c.pass && c.requirement.contains("docker (rootless)")));
        assert!(eligible(&resolve(&LegRoute { untrusted: Some(Untrusted { rootful_docker: true, ..untrusted.clone() }), ..leg.clone() }, &[])?, &docker(false)));
        // A rule that sends it to the host: refused, with why.
        let host = Rule { matches: RuleMatch { labels: vec!["linux".into()], ..RuleMatch::default() }, executor: vec!["host".into()], ..Rule::default() };
        let e = resolve(&leg, std::slice::from_ref(&host)).err().unwrap_or_default();
        assert!(e.contains("untrusted legs run only on firecracker, shards, docker") && e.contains("host"), "{e}");
        // A trusted leg goes where the rule says.
        assert_eq!(resolve(&LegRoute { untrusted: None, ..leg }, &[host])?.executors, vec!["host".to_owned()]);
        Ok(())
    }

    #[test]
    fn preferences_score_only_when_declared() {
        let a = RunnerView { name: "a".into(), facts: linux(100 << 30), ..RunnerView::default() };
        let b = RunnerView { name: "b".into(), facts: linux(400 << 30), labels: vec!["fast".into()], ..RunnerView::default() };
        assert!(scores(&[], &[&a, &b]).is_empty());
        let s = scores(&[Preference { fact: Some("disk.free".into()), higher: true, weight: Some(2.0), label: None }, Preference { label: Some("fast".into()), ..Preference::default() }], &[&a, &b]);
        assert_eq!(s, vec![0.0, 3.0]);
    }
}
