//! A rate target's file: how to start one server, seed it, and the reads it's timed on (docs/runtime-lab.md,
//! "Fixed rates"). Targets differ only in this file, so kiln and another product are measured by the same
//! generator. Strings in it are templates: `${name}` is a value kiln-lab knows (`port`, `dir`, `token`, a
//! request's `id`) or one the seed printed; `$$` is a `$`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use super::wire::{self, Found};

// ------------------------------------------------------------------------------------- templates

#[derive(Clone, Debug, PartialEq, Eq)]
enum Part {
    Lit(String),
    Var(String),
}

/// A string with `${name}`s in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

/// How a list's value goes into a template: its first element, or each in turn, request by request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pick {
    #[default]
    First,
    Cycle,
}

/// A value a template may name: one, or a list (a seed's sessions, largest first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    One(String),
    List(Vec<String>),
}

/// The values a target's templates name. The token's is never shown (its `Debug` leaves it out).
#[derive(Clone, Default)]
pub struct Vars {
    values: BTreeMap<String, Value>,
}

impl std::fmt::Debug for Vars {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut m = f.debug_map();
        for (k, v) in &self.values {
            match (k.as_str(), v) {
                ("token", _) => m.entry(k, &"(not shown)"),
                (_, Value::One(s)) => m.entry(k, s),
                (_, Value::List(l)) => m.entry(k, l),
            };
        }
        m.finish()
    }
}

/// What every target's templates may name, beside its seed's values: `id` only in a workload's.
pub const BUILT_IN: [&str; 3] = ["port", "dir", "token"];

impl Vars {
    pub fn set(&mut self, name: &str, v: Value) {
        self.values.insert(name.to_owned(), v);
    }

    pub fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    /// `name`'s value, a list's first.
    pub fn one(&self, name: &str) -> Option<&str> {
        match self.values.get(name)? {
            Value::One(s) => Some(s),
            Value::List(l) => l.first().map(String::as_str),
        }
    }

    /// Whether `name` is a list taken in turn (so a template naming it differs request by request).
    fn cycles(&self, name: &str, pick: Pick) -> bool {
        pick == Pick::Cycle && matches!(self.values.get(name), Some(Value::List(l)) if l.len() > 1)
    }

    /// A seed's values: what it printed, a JSON object, or else the last line of it that isn't empty, whose values
    /// are strings, numbers, booleans or lists of strings and numbers.
    pub fn of_seed(stdout: &str) -> Result<Vec<(String, Value)>, String> {
        let whole: Option<serde_json::Value> = serde_json::from_str(stdout.trim()).ok().filter(serde_json::Value::is_object);
        let v = match whole {
            Some(v) => v,
            None => {
                let line = stdout.lines().rev().map(str::trim).find(|l| !l.is_empty()).ok_or("the seed printed nothing")?;
                serde_json::from_str(line).map_err(|e| format!("the seed's last line isn't JSON: {e}"))?
            }
        };
        let obj = v.as_object().ok_or("the seed's last line isn't a JSON object")?;
        let mut keys: Vec<(&String, &serde_json::Value)> = obj.iter().collect();
        keys.sort_by(|a, b| a.0.cmp(b.0));
        let scalar = |v: &serde_json::Value| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        };
        keys.into_iter()
            .map(|(k, v)| {
                let value = match v {
                    serde_json::Value::Array(a) => Value::List(a.iter().map(|x| scalar(x).ok_or_else(|| format!("the seed's {k} holds a {x}, not a string or number"))).collect::<Result<_, _>>()?),
                    v => Value::One(scalar(v).ok_or_else(|| format!("the seed's {k} is {v}, not a string, number or list"))?),
                };
                if matches!(&value, Value::List(l) if l.is_empty()) {
                    return Err(format!("the seed's {k} is an empty list"));
                }
                Ok((k.clone(), value))
            })
            .collect()
    }
}

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
}

impl Template {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            lit.push_str(rest.get(..i).unwrap_or(""));
            let after = rest.get(i + 1..).unwrap_or("");
            if let Some(more) = after.strip_prefix('$') {
                lit.push('$');
                rest = more;
            } else if let Some(open) = after.strip_prefix('{') {
                let end = open.find('}').ok_or_else(|| format!("an unclosed ${{ in {s:?}"))?;
                let name = open.get(..end).unwrap_or("");
                if name.is_empty() || !name.chars().all(is_name) {
                    return Err(format!("${{{name}}} in {s:?}: a name is letters, digits, _, - and ."));
                }
                if !lit.is_empty() {
                    parts.push(Part::Lit(std::mem::take(&mut lit)));
                }
                parts.push(Part::Var(name.to_owned()));
                rest = open.get(end + 1..).unwrap_or("");
            } else {
                lit.push('$');
                rest = after;
            }
        }
        lit.push_str(rest);
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        Ok(Self { parts })
    }

    /// The names it holds.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|p| match p {
            Part::Var(n) => Some(n.as_str()),
            Part::Lit(_) => None,
        })
    }

    /// Whether it renders the same for every request (no `${id}`, no list taken in turn).
    pub fn fixed(&self, vars: &Vars, pick: Pick) -> bool {
        self.names().all(|n| n != "id" && !vars.cycles(n, pick))
    }

    /// It, with `${id}` as `id` and each list's value the first, or the `n`th in turn.
    pub fn render(&self, vars: &Vars, pick: Pick, id: u64, n: u64) -> Result<String, String> {
        let mut out = String::new();
        self.render_into(vars, pick, id, n, &mut out)?;
        Ok(out)
    }

    pub fn render_into(&self, vars: &Vars, pick: Pick, id: u64, n: u64, out: &mut String) -> Result<(), String> {
        for p in &self.parts {
            match p {
                Part::Lit(s) => out.push_str(s),
                Part::Var(name) if name == "id" => out.push_str(&id.to_string()),
                Part::Var(name) => match vars.values.get(name) {
                    Some(Value::One(s)) => out.push_str(s),
                    Some(Value::List(l)) => {
                        let at = match pick {
                            Pick::First => 0,
                            Pick::Cycle => usize::try_from(n % (l.len().max(1) as u64)).unwrap_or(0),
                        };
                        out.push_str(l.get(at).map_or("", String::as_str));
                    }
                    None => return Err(format!("nothing is named ${{{name}}}")),
                },
            }
        }
        Ok(())
    }

    /// The literal text, for a template with no names (a `ready` path).
    #[cfg(test)]
    fn literal(&self) -> String {
        self.parts.iter().map(|p| if let Part::Lit(s) = p { s.as_str() } else { "" }).collect()
    }
}

// ------------------------------------------------------------------------------------- the file

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    server: ServerFile,
    #[serde(default)]
    workload: Vec<WorkloadFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    kind: String,
    #[serde(default)]
    command: Vec<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    token: Option<String>,
    ready: Option<String>,
    ready_within_s: Option<u64>,
    seed: Option<Vec<String>>,
    seed_within_s: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkloadFile {
    name: String,
    kind: String,
    method: Option<String>,
    path: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body: Option<String>,
    ok_status: Option<Vec<u16>>,
    pipeline: Option<u32>,
    request: Option<String>,
    id: Option<String>,
    #[serde(rename = "final")]
    last: Option<TestFile>,
    ok: Option<TestFile>,
    after: Option<String>,
    in_flight: Option<u32>,
    #[serde(default)]
    hello: Vec<HelloFile>,
    pick: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestFile {
    at: Option<String>,
    is: Option<Scalar>,
    absent: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Text(String),
    Int(i64),
    Bool(bool),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloFile {
    send: String,
    until: Option<String>,
}

/// A target, read and checked.
#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub server: Server,
    pub workloads: Vec<Workload>,
}

#[derive(Clone, Debug)]
pub enum Server {
    /// kiln itself: the `--kiln` binaries, started and filled as `kiln-lab run` does.
    Kiln,
    Command(Command),
}

#[derive(Clone, Debug)]
pub struct Command {
    pub argv: Vec<Template>,
    pub cwd: Option<Template>,
    pub env: Vec<(String, Template)>,
    pub token: Token,
    pub ready: Ready,
    pub ready_within: Duration,
    pub seed: Option<Vec<Template>>,
    pub seed_within: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// kiln-lab makes a fresh one.
    Generate,
    /// Read from this file once the server is ready.
    File(Template),
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ready {
    /// The port takes a connection.
    Tcp,
    /// `GET` of this path answers under 500.
    Get(String),
}

/// How long a server has to answer: as long as `kiln-lab run` gives kiln's (`crate::fleet`, 120 s).
const READY_WITHIN: Duration = Duration::from_secs(120);
/// How long a seed has: as long as `kiln-lab run`'s fill (`crate::lab::round`, 1,800 s).
const SEED_WITHIN: Duration = Duration::from_secs(1800);

#[derive(Clone, Debug)]
pub struct Workload {
    pub name: String,
    pub pick: Pick,
    pub kind: Kind,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Http(Http),
    WsRpc(WsRpc),
}

#[derive(Clone, Debug)]
pub struct Http {
    pub method: String,
    pub path: Template,
    pub headers: Vec<(String, Template)>,
    pub body: Option<Template>,
    pub ok_status: Option<Vec<u16>>,
    pub pipeline: u32,
}

#[derive(Clone, Debug)]
pub struct WsRpc {
    pub path: Template,
    pub headers: Vec<(String, Template)>,
    pub request: Template,
    pub id: Vec<String>,
    pub last: Option<Test>,
    pub ok: Option<Test>,
    pub after: Option<Template>,
    pub in_flight: u32,
    pub hello: Vec<Hello>,
}

/// A test of an answer's message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Test {
    /// The scalar at the pointer is this text.
    Is(Vec<String>, String),
    /// The pointer reaches nothing (or `null`).
    Absent(Vec<String>),
}

impl Test {
    pub fn at(&self) -> &[String] {
        match self {
            Test::Is(p, _) | Test::Absent(p) => p,
        }
    }

    /// Whether what its pointer reached passes.
    pub fn holds(&self, found: Option<&Found>) -> bool {
        match (self, found) {
            (Test::Is(_, want), Some(Found::Scalar(s))) => s == want,
            (Test::Is(..), _) => false,
            (Test::Absent(_), None) => true,
            (Test::Absent(_), Some(Found::Scalar(s))) => s == "null",
            (Test::Absent(_), Some(Found::Container)) => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hello {
    pub send: Template,
    pub until: Option<Template>,
}

/// The most a connection may have in flight at once: kiln's HTTP/2 streams a connection (docs/http.md, 256).
const MOST_IN_FLIGHT: u32 = 256;

fn headers(h: &BTreeMap<String, String>) -> Result<Vec<(String, Template)>, String> {
    h.iter()
        .map(|(k, v)| {
            if k.is_empty() || !k.chars().all(|c| c.is_ascii_graphic() && c != ':') {
                return Err(format!("a header named {k:?}"));
            }
            Ok((k.clone(), Template::parse(v)?))
        })
        .collect()
}

fn test_of(t: TestFile, what: &str) -> Result<Test, String> {
    match t {
        TestFile { at: Some(at), is: Some(is), absent: None } => {
            let is = match is {
                Scalar::Text(s) => s,
                Scalar::Int(i) => i.to_string(),
                Scalar::Bool(b) => b.to_string(),
            };
            Ok(Test::Is(wire::pointer(&at)?, is))
        }
        TestFile { at: None, is: None, absent: Some(p) } => Ok(Test::Absent(wire::pointer(&p)?)),
        _ => Err(format!("{what} is {{ at = \"/pointer\", is = \"value\" }} or {{ absent = \"/pointer\" }}")),
    }
}

fn depth(n: Option<u32>, what: &str) -> Result<u32, String> {
    match n.unwrap_or(1) {
        0 => Err(format!("{what} is at least 1")),
        n if n > MOST_IN_FLIGHT => Err(format!("{what} is at most {MOST_IN_FLIGHT}")),
        n => Ok(n),
    }
}

fn workload(w: WorkloadFile) -> Result<Workload, String> {
    let name = w.name.clone();
    let err = |e: String| format!("workload {name}: {e}");
    let pick = match w.pick.as_deref() {
        None | Some("first") => Pick::First,
        Some("cycle") => Pick::Cycle,
        Some(p) => return Err(err(format!("pick is first or cycle, not {p}"))),
    };
    if !w.path.starts_with('/') {
        return Err(err(format!("its path starts with /: {:?}", w.path)));
    }
    let path = Template::parse(&w.path).map_err(err)?;
    let hdrs = headers(&w.headers).map_err(err)?;
    let kind = match w.kind.as_str() {
        "http" => {
            if w.request.is_some() || w.id.is_some() || w.last.is_some() || w.ok.is_some() || w.after.is_some() || w.in_flight.is_some() || !w.hello.is_empty() {
                return Err(err("an http workload takes method, path, headers, body, ok_status, pipeline and pick".into()));
            }
            let method = w.method.unwrap_or_else(|| "GET".into());
            if method.is_empty() || !method.chars().all(|c| c.is_ascii_uppercase()) {
                return Err(err(format!("a method of {method:?}")));
            }
            let body = w.body.as_deref().map(Template::parse).transpose().map_err(err)?;
            Kind::Http(Http { method, path, headers: hdrs, body, ok_status: w.ok_status, pipeline: depth(w.pipeline, "pipeline").map_err(err)? })
        }
        "ws-rpc" => {
            if w.method.is_some() || w.body.is_some() || w.ok_status.is_some() || w.pipeline.is_some() {
                return Err(err("a ws-rpc workload takes path, headers, request, id, final, ok, after, in_flight, hello and pick".into()));
            }
            let request = Template::parse(w.request.as_deref().ok_or_else(|| err("a ws-rpc workload needs its request".into()))?).map_err(err)?;
            let id = wire::pointer(w.id.as_deref().ok_or_else(|| err("a ws-rpc workload needs id, where an answer carries its request's".into()))?).map_err(err)?;
            let last = w.last.map(|t| test_of(t, "final")).transpose().map_err(err)?;
            let ok = w.ok.map(|t| test_of(t, "ok")).transpose().map_err(err)?;
            let after = w.after.as_deref().map(Template::parse).transpose().map_err(err)?;
            let hello = w
                .hello
                .into_iter()
                .map(|h| Ok(Hello { send: Template::parse(&h.send)?, until: h.until.as_deref().map(Template::parse).transpose()? }))
                .collect::<Result<Vec<_>, String>>()
                .map_err(err)?;
            Kind::WsRpc(WsRpc { path, headers: hdrs, request, id, last, ok, after, in_flight: depth(w.in_flight, "in_flight").map_err(err)?, hello })
        }
        k => return Err(err(format!("its kind is http or ws-rpc, not {k}"))),
    };
    Ok(Workload { name: w.name, pick, kind })
}

fn server(s: ServerFile) -> Result<Server, String> {
    match s.kind.as_str() {
        "kiln" => {
            if !s.command.is_empty() || s.cwd.is_some() || !s.env.is_empty() || s.token.is_some() || s.ready.is_some() || s.ready_within_s.is_some() || s.seed.is_some() || s.seed_within_s.is_some() {
                return Err("a kiln server takes nothing else: kiln-lab starts each --kiln as `kiln-lab run` does (its environment from --env)".into());
            }
            Ok(Server::Kiln)
        }
        "command" => {
            if s.command.is_empty() {
                return Err("a command server needs its command".into());
            }
            let token = match s.token.as_deref() {
                None | Some("generate") => Token::Generate,
                Some("none") => Token::None,
                Some(t) => match t.strip_prefix("file:") {
                    Some(f) if !f.is_empty() => Token::File(Template::parse(f)?),
                    _ => return Err(format!("token is generate, none or file:<path>, not {t:?}")),
                },
            };
            let ready = match s.ready.as_deref() {
                None | Some("tcp") => Ready::Tcp,
                Some(p) if p.starts_with('/') => Ready::Get(p.to_owned()),
                Some(r) => return Err(format!("ready is tcp or a path (/health), not {r:?}")),
            };
            Ok(Server::Command(Command {
                argv: s.command.iter().map(|a| Template::parse(a)).collect::<Result<_, _>>()?,
                cwd: s.cwd.as_deref().map(Template::parse).transpose()?,
                env: s.env.iter().map(|(k, v)| Ok((k.clone(), Template::parse(v)?))).collect::<Result<_, String>>()?,
                token,
                ready,
                ready_within: s.ready_within_s.map_or(READY_WITHIN, Duration::from_secs),
                seed: s.seed.map(|argv| argv.iter().map(|a| Template::parse(a)).collect::<Result<_, _>>()).transpose()?,
                seed_within: s.seed_within_s.map_or(SEED_WITHIN, Duration::from_secs),
            }))
        }
        k => Err(format!("a server's kind is kiln or command, not {k}")),
    }
}

impl Target {
    pub fn parse(text: &str) -> Result<Target, String> {
        let f: File = toml::from_str(text).map_err(|e| e.to_string())?;
        if f.name.trim().is_empty() {
            return Err("a target needs a name".into());
        }
        let server = server(f.server)?;
        if f.workload.is_empty() {
            return Err("a target needs a [[workload]]".into());
        }
        let workloads = f.workload.into_iter().map(workload).collect::<Result<Vec<_>, _>>()?;
        let mut names: Vec<&str> = workloads.iter().map(|w| w.name.as_str()).collect();
        names.sort_unstable();
        if let Some(w) = names.windows(2).find(|w| w.first() == w.get(1)) {
            return Err(format!("two workloads named {}", w.first().copied().unwrap_or("")));
        }
        let t = Target { name: f.name, server, workloads };
        // What can be known before the server is up: a name no one gives is a mistake now, not an hour in.
        let seeded = matches!(&t.server, Server::Command(c) if c.seed.is_some());
        if !seeded {
            let mut known: Vec<&str> = BUILT_IN.to_vec();
            if matches!(t.server, Server::Kiln) {
                known.push("session");
            }
            t.check(&|n| known.contains(&n))?;
        }
        Ok(t)
    }

    pub fn load(path: &Path) -> anyhow::Result<Target> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Target::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    /// Every name its templates hold is `known` (`id` in workloads' too): the first that isn't, said.
    pub fn check(&self, known: &dyn Fn(&str) -> bool) -> Result<(), String> {
        let server: Vec<&Template> = match &self.server {
            Server::Kiln => Vec::new(),
            Server::Command(c) => {
                let mut v: Vec<&Template> = c.argv.iter().chain(c.cwd.iter()).chain(c.env.iter().map(|(_, t)| t)).collect();
                if let Token::File(f) = &c.token {
                    v.push(f);
                }
                v
            }
        };
        // The server's own templates are rendered before its seed: only what's built in.
        for t in &server {
            if let Some(n) = t.names().find(|n| !BUILT_IN.contains(n)) {
                return Err(format!("the server's ${{{n}}}: its command, cwd, env and token file name only {}", BUILT_IN.join(", ")));
            }
        }
        if let Server::Command(Command { seed: Some(argv), .. }) = &self.server
            && let Some(n) = argv.iter().flat_map(Template::names).find(|n| !BUILT_IN.contains(n))
        {
            return Err(format!("the seed's ${{{n}}}: its command names only {}", BUILT_IN.join(", ")));
        }
        for w in &self.workloads {
            let ts: Vec<&Template> = match &w.kind {
                Kind::Http(h) => std::iter::once(&h.path).chain(h.headers.iter().map(|(_, t)| t)).chain(h.body.iter()).collect(),
                Kind::WsRpc(r) => std::iter::once(&r.path)
                    .chain(r.headers.iter().map(|(_, t)| t))
                    .chain(std::iter::once(&r.request))
                    .chain(r.after.iter())
                    .chain(r.hello.iter().flat_map(|h| std::iter::once(&h.send).chain(h.until.iter())))
                    .collect(),
            };
            if let Some(n) = ts.iter().flat_map(|t| t.names()).find(|n| *n != "id" && !known(n)) {
                return Err(format!("workload {}: nothing is named ${{{n}}}", w.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KILN: &str = include_str!("../../targets/kiln.toml");

    /// Goal 10 (Ada's fixed-rate benchmark): templates name values, `$$` is a dollar, a list is its first or each in
    /// turn, `${id}` the request's; a name no one gives is said.
    #[test]
    fn rate_templates_render_their_names() {
        let mut v = Vars::default();
        v.set("port", Value::One("4100".into()));
        v.set("thread", Value::List(vec!["a".into(), "b".into(), "c".into()]));
        let t = Template::parse(r#"{"id":"${id}","t":"${thread}","p":${port},"cost":"$$5 $x"}"#).unwrap_or(Template { parts: Vec::new() });
        assert_eq!(t.render(&v, Pick::First, 7, 4), Ok(r#"{"id":"7","t":"a","p":4100,"cost":"$5 $x"}"#.to_owned()));
        assert_eq!(t.render(&v, Pick::Cycle, 8, 4), Ok(r#"{"id":"8","t":"b","p":4100,"cost":"$5 $x"}"#.to_owned()));
        assert!(!t.fixed(&v, Pick::First), "${{id}} differs request by request");
        let path = Template::parse("/x/${thread}").unwrap_or(Template { parts: Vec::new() });
        assert!(path.fixed(&v, Pick::First) && !path.fixed(&v, Pick::Cycle));
        assert!(Template::parse("${unclosed").is_err());
        assert!(Template::parse("${a b}").is_err());
        assert!(Template::parse("/x/${nobody}").is_ok_and(|t| t.render(&v, Pick::First, 1, 1).is_err()));
        assert_eq!(Template::parse("/plain").map(|t| t.literal()), Ok("/plain".to_owned()));
        // The token is never shown.
        v.set("token", Value::One("s3cret".into()));
        assert!(!format!("{v:?}").contains("s3cret"));
    }

    /// Goal 10 (Ada's fixed-rate benchmark): kiln's own target file reads, its two reads the list and a session's
    /// newest page, each naming only what a kiln target has.
    #[test]
    fn rate_kilns_target_file_reads() {
        let t = Target::parse(KILN);
        let t = t.as_ref().map_err(String::as_str);
        assert!(t.is_ok(), "{t:?}");
        let Ok(t) = t else { return };
        assert!(matches!(t.server, Server::Kiln));
        let reads: Vec<(&str, String)> = t
            .workloads
            .iter()
            .filter_map(|w| match &w.kind {
                Kind::Http(h) => Some((w.name.as_str(), format!("{} {}", h.method, h.path.render(&kiln_vars(), w.pick, 1, 0).unwrap_or_default()))),
                Kind::WsRpc(_) => None,
            })
            .collect();
        assert_eq!(reads, vec![("sessions", "GET /api/harness/sessions".to_owned()), ("newest", "GET /api/harness/sessions/s-20000/events".to_owned())]);
    }

    fn kiln_vars() -> Vars {
        let mut v = Vars::default();
        v.set("session", Value::List(vec!["s-20000".into(), "s-400".into()]));
        v
    }

    /// Goal 10 (Ada's fixed-rate benchmark): a command target with a seed and a ws-rpc read, as T3 Code's is
    /// written, reads whole; its seed's last line gives its values.
    #[test]
    fn rate_a_command_target_with_a_seed_and_ws_rpc_reads() {
        let text = r#"
name = "t3code"
[server]
kind = "command"
command = ["node", "dist/bin.mjs", "--port", "${port}"]
cwd = "~/Projects/t3code"
env = { T3_HOME = "${dir}/state" }
token = "file:${dir}/token"
ready = "/health"
ready_within_s = 3600
seed = ["cat", "${dir}/seed.json"]

[[workload]]
name = "threads"
kind = "http"
path = "/api/threads?x=1"
headers = { Authorization = "Bearer ${token}", x-t3-protocol = "2" }
ok_status = [200]

[[workload]]
name = "thread"
kind = "ws-rpc"
path = "/ws?token=${token}"
request = '''{"_tag":"Request","id":${id},"tag":"thread.get","payload":{"threadId":"${thread}"},"headers":[]}'''
id = "/requestId"
final = { at = "/_tag", is = "Exit" }
ok = { at = "/exit/_tag", is = "Success" }
after = '''{"_tag":"Interrupt","requestId":"${id}","interruptors":[]}'''
in_flight = 2
pick = "cycle"

[[workload.hello]]
send = '''{"_tag":"Request","id":"${id}","tag":"auth","payload":{}}'''
until = '"requestId":"${id}"'
"#;
        let t = Target::parse(text);
        assert!(t.is_ok(), "{:?}", t.as_ref().err());
        let Ok(t) = t else { return };
        assert!(matches!(t.server, Server::Command(_)), "a command server");
        let Server::Command(c) = &t.server else { return };
        let token_file = Template::parse("${dir}/token").unwrap_or(Template { parts: Vec::new() });
        assert_eq!((c.ready.clone(), c.token.clone(), c.ready_within, c.seed_within), (Ready::Get("/health".into()), Token::File(token_file), Duration::from_secs(3600), SEED_WITHIN));
        assert!(matches!(t.workloads.first(), Some(Workload { kind: Kind::Http(h), .. }) if h.headers.iter().any(|(k, _)| k == "x-t3-protocol") && h.ok_status == Some(vec![200])));
        assert!(matches!(t.workloads.get(1), Some(Workload { kind: Kind::WsRpc(_), .. })), "a ws-rpc workload");
        let Some(Workload { kind: Kind::WsRpc(r), pick, .. }) = t.workloads.get(1) else { return };
        assert_eq!(r.request.render(&Vars::default(), Pick::First, 41, 0).err(), Some("nothing is named ${thread}".to_owned()));
        assert_eq!((*pick, r.in_flight, r.id.clone(), r.hello.len()), (Pick::Cycle, 2, vec!["requestId".to_owned()], 1));
        assert_eq!(r.last, Some(Test::Is(vec!["_tag".into()], "Exit".into())));
        let seeded = Vars::of_seed("progress...\n{\"thread\": [\"t1\", \"t2\"], \"n\": 32}\n\n");
        assert_eq!(seeded, Ok(vec![("n".into(), Value::One("32".into())), ("thread".into(), Value::List(vec!["t1".into(), "t2".into()]))]));
        let mut v = Vars::default();
        for (k, x) in seeded.unwrap_or_default() {
            v.set(&k, x);
        }
        for b in BUILT_IN {
            v.set(b, Value::One(String::new()));
        }
        assert_eq!(t.check(&|n| v.has(n)), Ok(()));
        assert!(t.check(&|n| BUILT_IN.contains(&n)).is_err_and(|e| e.contains("${thread}")));
        assert!(Vars::of_seed("[1]").is_err() && Vars::of_seed("").is_err() && Vars::of_seed("{\"x\": []}").is_err());
        // Printed whole over several lines, too.
        assert_eq!(Vars::of_seed("{\n  \"thread\": [\n    \"t1\"\n  ]\n}\n"), Ok(vec![("thread".into(), Value::List(vec!["t1".into()]))]));
        // An answer passes its tests.
        assert!(Test::Absent(vec!["error".into()]).holds(None) && Test::Absent(vec!["error".into()]).holds(Some(&Found::Scalar("null".into()))));
        assert!(!Test::Absent(vec!["error".into()]).holds(Some(&Found::Container)));
    }

    /// Goal 10 (Ada's fixed-rate benchmark): a file that's wrong is refused before anything starts, saying why.
    #[test]
    fn rate_a_wrong_target_file_is_refused_saying_why() {
        let base = "name = \"x\"\n[server]\nkind = \"kiln\"\n";
        let http = |rest: &str| format!("{base}[[workload]]\nname = \"a\"\nkind = \"http\"\n{rest}");
        let wrong: Vec<(String, &str)> = vec![
            (base.to_owned(), "[[workload]]"),
            (http("path = \"/a\"\nmethd = \"GET\"\n"), "methd"),
            (http("path = \"a\"\n"), "starts with /"),
            (http("path = \"/${thread}\"\n"), "${thread}"),
            (format!("{base}[[workload]]\nname = \"a\"\nkind = \"ws-rpc\"\npath = \"/ws\"\nrequest = \"{{}}\"\n"), "needs id"),
            (http("path = \"/a\"\npipeline = 0\n"), "at least 1"),
            (http("path = \"/a\"\n[[workload]]\nname = \"a\"\nkind = \"http\"\npath = \"/b\"\n"), "two workloads"),
            ("name = \"x\"\n[server]\nkind = \"kiln\"\ncommand = [\"x\"]\n[[workload]]\nname = \"a\"\nkind = \"http\"\npath = \"/a\"\n".to_owned(), "takes nothing else"),
            ("name = \"x\"\n[server]\nkind = \"command\"\ncommand = [\"x\", \"${session}\"]\n[[workload]]\nname = \"a\"\nkind = \"http\"\npath = \"/a\"\n".to_owned(), "${session}"),
            ("name = \"x\"\n[server]\nkind = \"command\"\ncommand = [\"x\"]\ntoken = \"file:\"\n[[workload]]\nname = \"a\"\nkind = \"http\"\npath = \"/a\"\n".to_owned(), "token is"),
        ];
        for (text, why) in &wrong {
            let got = Target::parse(text).map(|t| t.name);
            assert!(got.as_ref().is_err_and(|e| e.contains(why)), "{text:?}: {got:?} (wanted an error saying {why:?})");
        }
    }
}
