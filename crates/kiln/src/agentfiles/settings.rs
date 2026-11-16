//! An Agentfile's harnesses and agents as settings (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.3,
//! P5.4), read from its text with Shards' own parser and written back into it, so a structured view and
//! the text are one thing: a change in either is the other's. A harness's settings are its `HARNESS`
//! line's source and the version that source names, its volumes (`VOLUME … FOR <harness>`), the hosts it
//! may reach (`NETWORK --egress=… FOR <harness>`, with the microVM's boundary and its joining that
//! network, as kiln's own Agentfiles write them), and what Shards has no directive for as kiln's labels,
//! `LABEL kiln.harness.<harness>.model` and `.sign-in` (ROOT's ruling, 2026-11-14: OCI labels, text both
//! ways). An agent's (`AGENT <name>`) are its remote MCP servers (`MCP <server> FROM <url> FOR <name>`,
//! which grant their reach too) and kiln's labels `kiln.agent.<name>.model`, `.tools` and `.skills`, the
//! settings an agent definition shares with it (`crate::agentdefs::sync`). Each is read from, and
//! written into, the stage that declares its harness or agent.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use shards_agentfile::agentfile::{self, Direction, Directive, Scope, Source, TargetKind};
use shards_agentfile::instructions::{self, Kind};
use shards_agentfile::{lex, lint, parser};

use crate::secrets::mine::{Given, Reference};

/// What kiln's labels of a harness's settings start with: `kiln.harness.<harness>.<setting>`.
pub const HARNESS_LABEL: &str = "kiln.harness.";

/// What kiln's labels of an agent's settings start with: `kiln.agent.<agent>.<setting>`.
pub const AGENT_LABEL: &str = "kiln.agent.";

/// One harness an Agentfile declares (`HARNESS <name> FROM <source>`), as its settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileHarness"))]
pub struct Harness {
    /// Its name, which kiln starts it as: one of kiln's ten (`claude`, `codex` ...).
    pub name: String,
    /// The line its `HARNESS` starts on.
    pub line: u32,
    /// `FROM`'s source as written: a path of its build context, a Git or http(s) URL, an `s3://`
    /// object, or an OCI reference.
    pub source: String,
    /// What its source names apart from where it is: an OCI reference's tag or digest, a Git URL's
    /// ref, an `s3://` object's `--version-id`. None for a path or an http(s) URL, whose files are
    /// their version.
    pub version: Option<String>,
    /// Its model (`LABEL kiln.harness.<name>.model`); none: its own default.
    pub model: Option<String>,
    /// The sign-in it starts as (`LABEL kiln.harness.<name>.sign-in`): a sign-in's name, never its
    /// value (Shards D242); none: its own, kept in its HOME.
    pub sign_in: Option<String>,
    /// Its volumes' mount points (`VOLUME … FOR <name>`).
    pub volumes: Vec<String>,
    /// The `HOST:PORT`s it may reach (`NETWORK --egress=… FOR <name>`).
    pub egress: Vec<String>,
    /// The secrets its source is fetched with, each with the person's own the text names for it (kiln plan
    /// 01a1dd2e… P4.6); none for a path or an OCI reference.
    pub credentials: Vec<Credential>,
}

/// A secret an `AGENT`'s or `HARNESS`'s source is fetched with (kiln plan 01a1dd2e… P4.6): the build secret Shards
/// reads for it, or the SSH agent, by its id ([`wanted_by`]), and the person's own secret the text names for it
/// (`LABEL kiln.secret.<id>=user:<NAME>`, `LABEL kiln.ssh.<id>=user:<NAME>`), by its name alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileCredential"))]
pub struct Credential {
    /// `GIT_AUTH_TOKEN.<host>`, `HTTP_AUTH_TOKEN_<host>`, `AWS_ACCESS_KEY_ID.<bucket>`,
    /// `AWS_SECRET_ACCESS_KEY.<bucket>`, or `default` for an SSH key.
    pub id: String,
    pub given: Given,
    /// Which of the person's own secrets it's read from (`secrets.mine.list`); none: the text names none.
    pub secret: Option<String>,
}

/// One agent an Agentfile declares (`AGENT <name> FROM <source>`), as the settings an agent definition
/// shares with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileAgent"))]
pub struct Agent {
    pub name: String,
    /// The line its `AGENT` starts on.
    pub line: u32,
    /// `FROM`'s source as written, as a harness's is ([`Harness::source`]; P4.6).
    pub source: String,
    /// What its source names apart from where it is, as a harness's does ([`Harness::version`]).
    pub version: Option<String>,
    /// Its model (`LABEL kiln.agent.<name>.model`).
    pub model: Option<String>,
    /// Its tools (`LABEL kiln.agent.<name>.tools`, a JSON array of names).
    pub tools: Vec<String>,
    /// Its skills (`LABEL kiln.agent.<name>.skills`, a JSON array of names).
    pub skills: Vec<String>,
    /// Its remote MCP servers (`MCP <server> FROM <http(s) URL> FOR <name>`).
    pub mcp: Vec<Remote>,
    /// The secrets its source is fetched with, as a harness's are ([`Harness::credentials`]).
    pub credentials: Vec<Credential>,
}

/// A remote MCP server: its name, and where it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileRemote"))]
pub struct Remote {
    pub name: String,
    pub url: String,
}

/// An Agentfile's settings: each harness's and each agent's, in the order it declares them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileSettings"))]
pub struct Settings {
    pub harnesses: Vec<Harness>,
    pub agents: Vec<Agent>,
}

/// What `agentfiles.settings` answers: the text (changed, where a change was asked), its settings, and
/// what Shards says of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentfileViewed"))]
pub struct Viewed {
    pub text: String,
    pub settings: Settings,
    pub check: super::check::Check,
}

/// A change to one harness's settings, or of them an agent's source and its credentials ([`set_agent_source`]): what's
/// given changes; `Some(None)` takes the setting out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarnessChange {
    pub source: Option<String>,
    pub version: Option<Option<String>>,
    pub model: Option<Option<String>>,
    pub sign_in: Option<Option<String>>,
    pub volumes: Option<Vec<String>>,
    pub egress: Option<Vec<String>>,
    /// Its source's credentials by their ids ([`Credential::id`]): each one of the person's own secrets by its name, or
    /// none to take it out (P4.6).
    pub credentials: BTreeMap<String, Option<String>>,
}

/// An agent's settings as an agent definition shares them with its `AGENT`: each written whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentSettings {
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub mcp: Vec<Remote>,
}

impl From<&Agent> for AgentSettings {
    fn from(a: &Agent) -> AgentSettings {
        AgentSettings {
            model: a.model.clone(),
            tools: a.tools.clone(),
            skills: a.skills.clone(),
            mcp: a.mcp.clone(),
        }
    }
}

/// Why settings can't be read from a text, or a change made to them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Shards can't read the text: its words.
    #[error("{0}")]
    Unread(String),
    #[error("it declares no {what} {name}")]
    Undeclared { what: &'static str, name: String },
    #[error("it declares {what} {name} more than once: change it in the text")]
    Twice { what: &'static str, name: String },
    /// A value a setting can't take, and why.
    #[error("{setting}: {why}")]
    Value { setting: &'static str, why: String },
    /// What the text writes of it is more than its settings say (a network or a label it shares): the
    /// text is where to change it.
    #[error("{0}")]
    InText(String),
}

type R<T> = Result<T, Refusal>;

/// One instruction as Shards read it, with what it was written as.
struct Line {
    /// Its stage, by its place in the file.
    stage: usize,
    /// The lines it spans, from 1.
    first: usize,
    last: usize,
    kind: Kind,
    /// Its name, flags and words as written.
    name: String,
    flags: Vec<String>,
    words: Vec<String>,
}

/// A text as Shards reads it: its instructions, and the escape its words take.
struct Read {
    lines: Vec<Line>,
    lex: lex::Lex,
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn read(t: &str) -> R<Read> {
    let parsed = parser::parse_as(t.as_bytes(), parser::Dialect::Agentfile)
        .map_err(|e| Refusal::Unread(text(&e.message)))?;
    let read = instructions::parse(&parsed, &lint::Linter::default())
        .map_err(|e| Refusal::Unread(text(&e.message)))?;
    let nodes: HashMap<usize, &parser::Node> = parsed
        .instructions
        .iter()
        .map(|n| (n.start_line, n))
        .collect();
    let mut lines = Vec::new();
    for (stage, s) in read.stages.into_iter().enumerate() {
        for c in s.commands {
            let Some(first) = c.location.iter().map(|r| r.0).min() else {
                continue;
            };
            let last = c
                .location
                .iter()
                .map(|r| r.1.max(r.0))
                .max()
                .unwrap_or(first);
            let Some(node) = nodes.get(&first) else {
                continue;
            };
            let words = node.next.iter().map(|n| text(&n.value)).collect();
            lines.push(Line {
                stage,
                first,
                last,
                kind: c.kind,
                name: text(&node.value),
                flags: node.flags.iter().map(|f| text(f)).collect(),
                words,
            });
        }
    }
    Ok(Read {
        lines,
        lex: lex::Lex::new(u32::from(parsed.escape)),
    })
}

fn line_no(l: usize) -> u32 {
    u32::try_from(l).unwrap_or(u32::MAX)
}

/// Whether `scope` names `name` as a member of kind `kind` (by its own kind, or as `--target-kind` says).
fn names(scope: &Scope, name: &str, kind: TargetKind) -> bool {
    scope.kind.is_none_or(|k| k == kind) && scope.names.iter().any(|n| n == name.as_bytes())
}

/// `w` as a build reads it: its quotes and escapes taken out, no variable set.
fn word(lex: &lex::Lex, w: &[u8]) -> Option<String> {
    lex.process(w, &lex::NoEnv).ok().map(|p| text(&p.word))
}

/// The settings `prefix` (`kiln.harness.claude.`) labels in `stage`, each as the build reads it, the last
/// written winning as a label's does.
fn labels(r: &Read, stage: usize, prefix: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        let Kind::Label(pairs) = &l.kind else {
            continue;
        };
        for kv in pairs {
            let Some(key) = word(&r.lex, &kv.key) else {
                continue;
            };
            if let Some(setting) = key.strip_prefix(prefix) {
                out.insert(
                    setting.to_owned(),
                    word(&r.lex, &kv.value).unwrap_or_default(),
                );
            }
        }
    }
    out
}

fn set_in(labels: &HashMap<String, String>, setting: &str) -> Option<String> {
    labels.get(setting).filter(|v| !v.is_empty()).cloned()
}

/// A list label's names (a JSON array of strings); none where it's unset.
fn listed(labels: &HashMap<String, String>, setting: &str) -> R<Vec<String>> {
    match labels.get(setting).filter(|v| !v.trim().is_empty()) {
        None => Ok(Vec::new()),
        Some(v) => serde_json::from_str(v).map_err(|e| Refusal::Value {
            setting: list_setting(setting),
            why: format!("its label is a JSON array of names (`[\"Read\", \"Bash\"]`): {e}"),
        }),
    }
}

fn list_setting(setting: &str) -> &'static str {
    if setting == "tools" {
        "tools"
    } else {
        "skills"
    }
}

/// An OCI reference's tag or digest, as written.
fn oci_version(r: &str) -> Option<String> {
    if let Some((_, digest)) = r.split_once('@') {
        return Some(digest.to_owned());
    }
    let last = r.rsplit('/').next().unwrap_or(r);
    last.rsplit_once(':').map(|(_, tag)| tag.to_owned())
}

/// An OCI reference without its tag or digest.
fn oci_name(r: &str) -> &str {
    let named = r.split('@').next().unwrap_or(r);
    let slash = named.rfind('/').map_or(0, |i| i.saturating_add(1));
    match named.get(slash..).and_then(|last| last.rfind(':')) {
        Some(colon) => named.get(..slash.saturating_add(colon)).unwrap_or(named),
        None => named,
    }
}

/// What `source` (with `fetch`) names apart from where it is ([`Harness::version`]).
fn version_of(source: &[u8], fetch: &agentfile::Fetch) -> Option<String> {
    match agentfile::source_of(source).ok()? {
        Source::Oci(r) => oci_version(&text(&r)),
        Source::Git(g) => Some(text(&g.reference)).filter(|v| !v.is_empty()),
        Source::S3 { .. } => Some(text(&fetch.version_id)).filter(|v| !v.is_empty()),
        Source::Path(_) | Source::Http(_) => None,
    }
}

/// Each path of each `VOLUME` of `stage` that names `name`, a member of kind `kind`.
fn volumes_of(r: &Read, stage: usize, name: &str, kind: TargetKind) -> Vec<String> {
    let mut out = Vec::new();
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        if let Kind::Agentfile(Directive::Volume(v)) = &l.kind
            && names(&v.scope, name, kind)
        {
            out.extend(v.paths.iter().map(|p| text(p)));
        }
    }
    out
}

/// Each `--egress` destination of each `NETWORK` of `stage` that names harness `name`.
fn egress_of(r: &Read, stage: usize, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        if let Kind::Agentfile(Directive::Network(n)) = &l.kind
            && names(&n.scope, name, TargetKind::Harness)
        {
            out.extend(
                n.ports
                    .iter()
                    .filter(|(_, d)| *d == Direction::Egress)
                    .map(|(p, _)| text(p)),
            );
        }
    }
    out
}

/// Each `MCP` of `stage` from an http(s) URL that names agent `name`.
fn remotes_of(r: &Read, stage: usize, name: &str) -> Vec<Remote> {
    let mut out = Vec::new();
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        if let Kind::Agentfile(Directive::Mcp(m)) = &l.kind
            && names(&m.scope, name, TargetKind::Agent)
            && matches!(agentfile::source_of(&m.source), Ok(Source::Http(_)))
        {
            out.push(Remote {
                name: text(&m.name),
                url: text(&m.source),
            });
        }
    }
    out
}

/// `t`'s settings: each harness's and each agent's. Refused where Shards can't read it, or a label of
/// kiln's that holds a list doesn't hold one.
pub fn of(t: &str) -> R<Settings> {
    let r = read(t)?;
    let (refs, _) = crate::secrets::mine::references(t);
    let mut harnesses = Vec::new();
    let mut agents = Vec::new();
    for l in &r.lines {
        match &l.kind {
            Kind::Agentfile(Directive::Harness(h)) => {
                let name = text(&h.name);
                let labels = labels(&r, l.stage, &format!("{HARNESS_LABEL}{name}."));
                harnesses.push(Harness {
                    line: line_no(l.first),
                    source: text(&h.source),
                    version: version_of(&h.source, &h.fetch),
                    model: set_in(&labels, "model"),
                    sign_in: set_in(&labels, "sign-in"),
                    volumes: volumes_of(&r, l.stage, &name, TargetKind::Harness),
                    egress: egress_of(&r, l.stage, &name),
                    credentials: credentials_of(&h.source, &refs),
                    name,
                });
            }
            Kind::Agentfile(Directive::Agent(a)) => {
                agents.push(agent_in(&r, l, &text(&a.name), &refs)?)
            }
            _ => {}
        }
    }
    Ok(Settings { harnesses, agents })
}

/// A change to a text's lines: `first..=last` (from 1) replaced by `with`, each a line; where `last` is
/// `first - 1`, `with` put before line `first`.
struct Edit {
    first: usize,
    last: usize,
    with: Vec<String>,
}

fn replaced(l: &Line, with: Vec<String>) -> Edit {
    Edit {
        first: l.first,
        last: l.last,
        with,
    }
}

fn after(l: &Line, with: Vec<String>) -> Edit {
    Edit {
        first: l.last.saturating_add(1),
        last: l.last,
        with,
    }
}

/// `t` with `edits` made (none may overlap), each new line ending as the text's do. No edit: `t` as it is.
fn apply(t: &str, mut edits: Vec<Edit>) -> String {
    if edits.is_empty() {
        return t.to_owned();
    }
    let ending = if t.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = t.split_inclusive('\n').map(str::to_owned).collect();
    if let Some(last) = lines.last_mut()
        && !last.ends_with('\n')
    {
        last.push_str(ending);
    }
    // From the last line up, so each edit's lines are still where Shards read them.
    edits.sort_by(|a, b| b.first.cmp(&a.first).then(b.last.cmp(&a.last)));
    for e in edits {
        let start = e.first.saturating_sub(1).min(lines.len());
        let end = e.last.min(lines.len()).max(start);
        let with: Vec<String> = e.with.into_iter().map(|w| format!("{w}{ending}")).collect();
        let _gone: Vec<String> = lines.splice(start..end, with).collect();
    }
    lines.concat()
}

/// An instruction written as one line: its name, flags and words.
fn written(name: &str, flags: &[String], words: &[String]) -> String {
    std::iter::once(name)
        .chain(flags.iter().map(String::as_str))
        .chain(words.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `v` is one word a build reads as written: nothing it would take for a quote, an escape, a
/// variable or a space.
fn plain(v: &str) -> bool {
    !v.is_empty()
        && !v
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '`' | '$'))
}

fn plain_or(setting: &'static str, v: &str) -> R<String> {
    let v = v.trim();
    if plain(v) {
        Ok(v.to_owned())
    } else {
        Err(Refusal::Value {
            setting,
            why: format!(
                "{v:?} isn't one word an Agentfile reads as written (no space, quote, backslash or `$`)"
            ),
        })
    }
}

/// A sign-in of harness `harness`, by its name: one word a build reads as written, which says its harness
/// as kiln names a sign-in, `<harness>/<name>` (`claude/ada`: `crate::secrets::signin`'s `Provider::of`),
/// never its value.
fn sign_in_name(harness: &str, v: &str) -> R<String> {
    let v = v.trim();
    match v.split_once('/') {
        Some((h, rest)) if plain(v) && h == harness && !rest.is_empty() => Ok(v.to_owned()),
        _ => Err(Refusal::Value {
            setting: "sign-in",
            why: format!(
                "{v:?} isn't a sign-in of harness {harness}: its name says its harness, `{harness}/<name>` (`{harness}/work`), and never holds its value"
            ),
        }),
    }
}

/// A list label's value: a JSON array, single-quoted so a build reads it as written (a `'` in it written
/// as JSON's escape of it).
fn list_value(names: &[String]) -> String {
    let json = serde_json::to_string(names).unwrap_or_else(|_| "[]".into());
    format!("'{}'", json.replace('\'', "\\u0027"))
}

/// Each label of `prefix` in `stage` set as `wanted` says (`None`: taken out), and those no label has yet
/// added to `new` as one `LABEL`: each `LABEL` that has one rewritten once.
fn label_edits(
    r: &Read,
    stage: usize,
    prefix: &str,
    wanted: &[(&str, Option<String>)],
    edits: &mut Vec<Edit>,
    new: &mut Vec<String>,
) {
    let mut done: Vec<&str> = Vec::new();
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        let Kind::Label(pairs) = &l.kind else {
            continue;
        };
        let setting_of =
            |k: &[u8]| word(&r.lex, k).and_then(|k| k.strip_prefix(prefix).map(str::to_owned));
        if !pairs
            .iter()
            .any(|kv| setting_of(&kv.key).is_some_and(|s| wanted.iter().any(|(w, _)| *w == s)))
        {
            continue;
        }
        let mut kept = Vec::new();
        for kv in pairs {
            let ours = setting_of(&kv.key).and_then(|s| wanted.iter().find(|(w, _)| *w == s));
            match ours {
                Some((setting, value)) => {
                    // The first is set; a second of the same key, which a build would take over it, goes.
                    if let Some(v) = value.as_ref().filter(|_| !done.contains(setting)) {
                        kept.push(format!("{prefix}{setting}={v}"));
                    }
                    done.push(*setting);
                }
                None if kv.no_delim => kept.push(format!("{} {}", text(&kv.key), text(&kv.value))),
                None => kept.push(format!("{}={}", text(&kv.key), text(&kv.value))),
            }
        }
        edits.push(replaced(
            l,
            if kept.is_empty() {
                Vec::new()
            } else {
                vec![format!("{} {}", l.name, kept.join(" "))]
            },
        ));
    }
    let fresh: Vec<String> = wanted
        .iter()
        .filter(|(s, _)| !done.contains(s))
        .filter_map(|(s, v)| v.as_ref().map(|v| format!("{prefix}{s}={v}")))
        .collect();
    if !fresh.is_empty() {
        new.push(format!("LABEL {}", fresh.join(" ")));
    }
}

/// Each label of `stage` keyed as `wanted` says (whole keys), set or taken out ([`label_edits`]), where it asks any.
fn labels_set(
    r: &Read,
    stage: usize,
    wanted: &[(String, Option<String>)],
    edits: &mut Vec<Edit>,
    new: &mut Vec<String>,
) {
    if wanted.is_empty() {
        return;
    }
    let keyed: Vec<(&str, Option<String>)> = wanted
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    label_edits(r, stage, "", &keyed, edits, new);
}

/// The secrets Shards' build fetches `source` with (P4.6), by their ids: a Git source's token for its host over
/// HTTP(S) (`GIT_AUTH_TOKEN.<host>`; git's own `git://` takes none) or its SSH agent over SSH (`default`); an http(s)
/// URL's Bearer token for its host (`HTTP_AUTH_TOKEN_<host>`); an `s3://` object's key pair for its bucket
/// (`AWS_ACCESS_KEY_ID.<bucket>`, `AWS_SECRET_ACCESS_KEY.<bucket>`). A path or an OCI reference, none. Each id is the
/// first Shards' fetch looks for (its build's `git`, `http` and `s3` sources): the host's or bucket's own.
fn wanted_by(source: &[u8]) -> Vec<(String, Given)> {
    match agentfile::source_of(source) {
        Ok(Source::Git(g)) => match shards_agentfile::git::parse_url(&g.remote) {
            Ok(u) if u.scheme == b"ssh" => vec![("default".to_owned(), Given::Ssh)],
            Ok(u) if matches!(u.scheme.as_slice(), b"http" | b"https") => {
                git_host(&text(&u.remote))
                    .map(|h| vec![(format!("GIT_AUTH_TOKEN.{h}"), Given::Secret)])
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        },
        Ok(Source::Http(url)) => http_host(&text(&url))
            .map(|h| vec![(format!("HTTP_AUTH_TOKEN_{h}"), Given::Secret)])
            .unwrap_or_default(),
        Ok(Source::S3 { bucket, .. }) => {
            let b = text(&bucket);
            vec![
                (format!("AWS_ACCESS_KEY_ID.{b}"), Given::Secret),
                (format!("AWS_SECRET_ACCESS_KEY.{b}"), Given::Secret),
            ]
        }
        Ok(Source::Path(_) | Source::Oci(_)) | Err(_) => Vec::new(),
    }
}

/// A Git URL's host as Shards' git source names its token by it: its authority past any user, its port kept.
fn git_host(url: &str) -> Option<String> {
    let authority = url.split_once("://")?.1.split(['/', '?', '#']).next()?;
    authority
        .rsplit('@')
        .next()
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
}

/// An http(s) URL's host as Shards' http source names its secrets by it: past any user, without its port, an IPv6
/// address without its brackets.
fn http_host(url: &str) -> Option<String> {
    let authority = url.split_once("://")?.1.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next(),
        None => host.split(':').next(),
    };
    name.filter(|h| !h.is_empty()).map(str::to_owned)
}

/// The secrets `source` is fetched with ([`wanted_by`]), each with the person's own `refs` names for it.
fn credentials_of(source: &[u8], refs: &[Reference]) -> Vec<Credential> {
    wanted_by(source)
        .into_iter()
        .map(|(id, given)| {
            let secret = refs
                .iter()
                .find(|r| r.id == id && r.given == given)
                .map(|r| r.name.clone());
            Credential { id, given, secret }
        })
        .collect()
}

/// The labels `asked` sets for a directive in `stage` whose source is `source` (P4.6): each credential's reference by
/// its whole key (`kiln.secret.<id>`, `kiln.ssh.<id>`), `user:<NAME>`, or none to take it out. An id its source isn't
/// fetched with, a name no secret could have, and a reference the text keeps in another stage (a second would name it
/// twice) are refused, never repeating what was given.
fn credential_labels(
    r: &Read,
    stage: usize,
    source: &str,
    asked: &BTreeMap<String, Option<String>>,
) -> R<Vec<(String, Option<String>)>> {
    let wanted = wanted_by(source.as_bytes());
    let mut out = Vec::new();
    for (id, name) in asked {
        let Some(given) = wanted.iter().find(|(w, _)| w == id).map(|(_, g)| *g) else {
            let ids: Vec<&str> = wanted.iter().map(|(w, _)| w.as_str()).collect();
            return Err(Refusal::Value {
                setting: "credentials",
                why: format!(
                    "its source isn't fetched with {id}: it's fetched with {}",
                    if ids.is_empty() {
                        "none".to_owned()
                    } else {
                        ids.join(", ")
                    }
                ),
            });
        };
        let key = format!("{}{id}", given.label());
        let elsewhere = r.lines.iter().filter(|l| l.stage != stage).any(|l| {
            matches!(&l.kind, Kind::Label(pairs) if pairs.iter().any(|kv| word(&r.lex, &kv.key).as_deref() == Some(key.as_str())))
        });
        if elsewhere {
            return Err(Refusal::InText(format!(
                "{key} is named in another stage: change it in the text"
            )));
        }
        let value = match name.as_deref() {
            None => None,
            Some(n) => {
                let n = kiln_ci::secrets::check_name(n).map_err(|_| Refusal::Value {
                    setting: "credentials",
                    why: format!("{id}: that's no secret's name (letters, digits and `_`, as `secrets.mine.list` names them)"),
                })?;
                Some(format!("{}{n}", crate::secrets::mine::USER))
            }
        };
        out.push((key, value));
    }
    Ok(out)
}

/// `t` with agent `name`'s source changed as `change` says (kiln plan 01a1dd2e… P4.6): its `AGENT` line's source and
/// the version it names, as a harness's are ([`set_harness`]), and its source's credentials. An agent's model, tools
/// and skills are its definition's ([`set_agent`]) and the rest a harness's: a change of them here is refused.
pub fn set_agent_source(t: &str, name: &str, change: &HarnessChange) -> R<String> {
    if change.model.is_some()
        || change.sign_in.is_some()
        || change.volumes.is_some()
        || change.egress.is_some()
    {
        return Err(Refusal::Value {
            setting: "agent",
            why: "an agent's source, its version and its credentials are set here; its model, tools and skills are its agent definition's, and a sign-in, volumes and egress a harness's".into(),
        });
    }
    let r = read(t)?;
    let a_line = declaring(&r, "agent", name)?;
    let Kind::Agentfile(Directive::Agent(a)) = &a_line.kind else {
        return Err(Refusal::Undeclared {
            what: "agent",
            name: name.to_owned(),
        });
    };
    let (mut edits, mut new) = (Vec::new(), Vec::new());
    if change.source.is_some() || change.version.is_some() {
        edits.push(replaced(a_line, vec![sourced(a_line, a, change)?]));
    }
    let source = change
        .source
        .as_deref()
        .map_or_else(|| text(&a.source), |s| s.trim().to_owned());
    let wanted = credential_labels(&r, a_line.stage, &source, &change.credentials)?;
    labels_set(&r, a_line.stage, &wanted, &mut edits, &mut new);
    if !new.is_empty() {
        edits.push(after(a_line, new));
    }
    Ok(apply(t, edits))
}

/// The one line of `r` that declares `what` `name` (`HARNESS` or `AGENT`).
fn declaring<'a>(r: &'a Read, what: &'static str, name: &str) -> R<&'a Line> {
    let found: Vec<&Line> = r
        .lines
        .iter()
        .filter(|l| match &l.kind {
            Kind::Agentfile(Directive::Harness(h)) => {
                what == "harness" && h.name == name.as_bytes()
            }
            Kind::Agentfile(Directive::Agent(a)) => what == "agent" && a.name == name.as_bytes(),
            _ => false,
        })
        .collect();
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err(Refusal::Undeclared {
            what,
            name: name.to_owned(),
        }),
        _ => Err(Refusal::Twice {
            what,
            name: name.to_owned(),
        }),
    }
}

/// `t` with harness `name`'s settings changed as `change` says: its `HARNESS` line, its labels, volumes
/// and egress rewritten where they are, and what it had none of yet written after its `HARNESS`.
pub fn set_harness(t: &str, name: &str, change: &HarnessChange) -> R<String> {
    let r = read(t)?;
    let h_line = declaring(&r, "harness", name)?;
    let Kind::Agentfile(Directive::Harness(h)) = &h_line.kind else {
        return Err(Refusal::Undeclared {
            what: "harness",
            name: name.to_owned(),
        });
    };
    let (mut edits, mut new) = (Vec::new(), Vec::new());
    if change.source.is_some() || change.version.is_some() {
        edits.push(replaced(h_line, vec![sourced(h_line, h, change)?]));
    }
    // kiln's labels and its credentials' references, rewritten at once: one `LABEL` may hold both.
    let mut wanted: Vec<(String, Option<String>)> = Vec::new();
    if let Some(m) = &change.model {
        wanted.push((
            format!("{HARNESS_LABEL}{name}.model"),
            m.as_deref().map(|m| plain_or("model", m)).transpose()?,
        ));
    }
    if let Some(s) = &change.sign_in {
        wanted.push((
            format!("{HARNESS_LABEL}{name}.sign-in"),
            s.as_deref().map(|s| sign_in_name(name, s)).transpose()?,
        ));
    }
    let source = change
        .source
        .as_deref()
        .map_or_else(|| text(&h.source), |s| s.trim().to_owned());
    wanted.extend(credential_labels(
        &r,
        h_line.stage,
        &source,
        &change.credentials,
    )?);
    labels_set(&r, h_line.stage, &wanted, &mut edits, &mut new);
    if let Some(v) = &change.volumes {
        volume_edits(&r, h_line.stage, name, v, &mut edits, &mut new)?;
    }
    if let Some(e) = &change.egress {
        egress_edits(&r, h_line.stage, name, e, &mut edits, &mut new)?;
    }
    if !new.is_empty() {
        edits.push(after(h_line, new));
    }
    Ok(apply(t, edits))
}

/// Harness line `l` (`h`) written again with its source, or the version its source names, changed.
fn sourced(l: &Line, h: &agentfile::Domain, change: &HarnessChange) -> R<String> {
    let mut source = match &change.source {
        Some(s) => plain_or("source", s)?,
        None => text(&h.source),
    };
    let mut flags = l.flags.clone();
    if let Some(v) = &change.version {
        let v = v.as_deref().map(|v| plain_or("version", v)).transpose()?;
        source = versioned(&source, &mut flags, v.as_deref())?;
    }
    agentfile::source_of(source.as_bytes()).map_err(|e| Refusal::Value {
        setting: "source",
        why: text(&e),
    })?;
    let mut words = l.words.clone();
    let at = words.get_mut(2).ok_or_else(|| {
        Refusal::Unread(format!(
            "line {}: {} names no source",
            l.first,
            l.name.to_ascii_uppercase()
        ))
    })?;
    *at = source;
    Ok(written(&l.name, &flags, &words))
}

/// `source` naming version `v` (none: the version it names taken out), where its kind names one apart.
fn versioned(source: &str, flags: &mut Vec<String>, v: Option<&str>) -> R<String> {
    let kind = agentfile::source_of(source.as_bytes()).map_err(|e| Refusal::Value {
        setting: "source",
        why: text(&e),
    })?;
    match kind {
        Source::Oci(_) => {
            let name = oci_name(source);
            Ok(match v {
                None => name.to_owned(),
                Some(d) if d.contains(':') => format!("{name}@{d}"),
                Some(tag) => format!("{name}:{tag}"),
            })
        }
        Source::Git(_) if source.contains('?') => Err(Refusal::InText(
            "its Git source names its ref in a query (`?ref=`): change its version in the text"
                .into(),
        )),
        Source::Git(_) => {
            if v.is_some_and(|v| v.contains([':', '#'])) {
                return Err(Refusal::Value {
                    setting: "version",
                    why:
                        "a Git ref has no `:` or `#`, which its source's fragment keeps for itself"
                            .into(),
                });
            }
            let (url, fragment) = source.split_once('#').unwrap_or((source, ""));
            let subdir = fragment.split_once(':').map(|(_, s)| s);
            Ok(match (v, subdir) {
                (Some(v), Some(s)) => format!("{url}#{v}:{s}"),
                (Some(v), None) => format!("{url}#{v}"),
                (None, Some(s)) => format!("{url}#:{s}"),
                (None, None) => url.to_owned(),
            })
        }
        Source::S3 { .. } => {
            flags.retain(|f| !f.starts_with("--version-id"));
            if let Some(v) = v {
                flags.push(format!("--version-id={v}"));
            }
            Ok(source.to_owned())
        }
        Source::Path(_) => Err(Refusal::Value {
            setting: "version",
            why: "a path's version is the files at it: change its source".into(),
        }),
        Source::Http(_) => Err(Refusal::Value {
            setting: "version",
            why: "an http(s) source's version is its URL (with its --checksum): change its source"
                .into(),
        }),
    }
}

/// Harness `name`'s volumes made `paths`: each `VOLUME` of `stage` naming it rewritten without it (taken
/// out where it named it alone), and one `VOLUME --target-kind=harness <path> FOR <name>` each added.
fn volume_edits(
    r: &Read,
    stage: usize,
    name: &str,
    paths: &[String],
    edits: &mut Vec<Edit>,
    new: &mut Vec<String>,
) -> R<()> {
    let paths: Vec<String> = paths
        .iter()
        .map(|p| plain_or("volumes", p))
        .collect::<R<_>>()?;
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        let Kind::Agentfile(Directive::Volume(v)) = &l.kind else {
            continue;
        };
        if !names(&v.scope, name, TargetKind::Harness) {
            continue;
        }
        let others: Vec<String> = v
            .scope
            .names
            .iter()
            .filter(|n| *n != name.as_bytes())
            .map(|n| text(n))
            .collect();
        if others.is_empty() {
            edits.push(replaced(l, Vec::new()));
        } else {
            let (before, _) = split_for(&l.words);
            let words: Vec<String> = before
                .iter()
                .cloned()
                .chain(std::iter::once("FOR".to_owned()))
                .chain(others)
                .collect();
            edits.push(replaced(l, vec![written(&l.name, &l.flags, &words)]));
        }
    }
    new.extend(
        paths
            .iter()
            .map(|p| format!("VOLUME --target-kind=harness {p} FOR {name}")),
    );
    Ok(())
}

/// `words` split at their `FOR`: before it, and after it.
fn split_for(words: &[String]) -> (&[String], &[String]) {
    match words.iter().position(|w| w.eq_ignore_ascii_case("for")) {
        Some(at) => (
            words.get(..at).unwrap_or_default(),
            words.get(at.saturating_add(1)..).unwrap_or_default(),
        ),
        None => (words, &[]),
    }
}

/// The ports the microVM's boundary opens for destinations `dests`, each once, in their order: TCP's,
/// which a destination is (Shards D236).
fn ports_of(dests: &[(String, u16)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, port) in dests {
        let p = format!("{port}/tcp");
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Harness `name`'s egress made `dests`, on the network of `stage` that is its alone (`NETWORK … FOR
/// <name>`): its `--egress` flags, its microVM boundary's ports (`EXPOSE … AS egress FOR <network>`) and
/// its joining it (`CONNECT <name> WITH <name> ON <network>`), as kiln's own Agentfiles write them; that
/// network and those lines made where it has none, and taken out where it reaches nothing. A harness
/// whose egress is on a network it shares, or on more than one, is changed in the text.
fn egress_edits(
    r: &Read,
    stage: usize,
    name: &str,
    dests: &[String],
    edits: &mut Vec<Edit>,
    new: &mut Vec<String>,
) -> R<()> {
    let mut checked = Vec::new();
    for d in dests {
        let d = d.trim();
        match agentfile::destination_of(d.as_bytes()) {
            Some(Ok((host, port))) => checked.push((d.to_owned(), (text(&host), port))),
            Some(Err(e)) => {
                return Err(Refusal::Value {
                    setting: "egress",
                    why: text(&e),
                });
            }
            None => {
                return Err(Refusal::Value {
                    setting: "egress",
                    why: format!("{d:?} isn't a destination, HOST:PORT"),
                });
            }
        }
    }
    let networks: Vec<(&Line, &agentfile::Network)> = r
        .lines
        .iter()
        .filter(|l| l.stage == stage)
        .filter_map(|l| match &l.kind {
            Kind::Agentfile(Directive::Network(n))
                if names(&n.scope, name, TargetKind::Harness)
                    && n.ports.iter().any(|(_, d)| *d == Direction::Egress) =>
            {
                Some((l, n))
            }
            _ => None,
        })
        .collect();
    let ports = ports_of(&checked.iter().map(|(_, d)| d.clone()).collect::<Vec<_>>());
    let egress: Vec<String> = checked
        .iter()
        .map(|(d, _)| format!("--egress={d}"))
        .collect();
    match networks.as_slice() {
        [] if checked.is_empty() => Ok(()),
        [] => {
            let net = format!("{name}-egress");
            if r.lines.iter().any(|l| matches!(&l.kind, Kind::Agentfile(Directive::Network(n)) if n.name == net.as_bytes())) {
                return Err(Refusal::InText(format!("it has a network named {net} already: give harness {name} its egress in the text")));
            }
            new.push(format!("NETWORK {} {net} FOR {name}", egress.join(" ")));
            new.push(format!(
                "EXPOSE --harnesses={name} {} AS egress FOR {net}",
                ports.join(" ")
            ));
            new.push(format!("CONNECT {name} WITH {name} ON {net}"));
            Ok(())
        }
        [(l, n)] if n.scope.names.len() == 1 => {
            let net = text(&n.name);
            let rest: Vec<&Line> = r
                .lines
                .iter()
                .filter(|o| o.stage == stage && serves(o, &net, name))
                .collect();
            let others = n.ports.iter().any(|(_, d)| *d != Direction::Egress);
            if checked.is_empty() && !others {
                edits.push(replaced(l, Vec::new()));
                for o in rest {
                    edits.push(replaced(o, Vec::new()));
                }
                return Ok(());
            }
            let mut flags: Vec<String> = l
                .flags
                .iter()
                .filter(|f| !f.starts_with("--egress"))
                .cloned()
                .collect();
            flags.extend(egress);
            edits.push(replaced(l, vec![written(&l.name, &flags, &l.words)]));
            let exposes: Vec<&&Line> = rest
                .iter()
                .filter(|o| matches!(&o.kind, Kind::Agentfile(Directive::Expose(_))))
                .collect();
            match exposes.as_slice() {
                [e] if checked.is_empty() => edits.push(replaced(e, Vec::new())),
                [e] => {
                    let (_, after_for) = split_ports(&e.words);
                    let words: Vec<String> = ports
                        .iter()
                        .cloned()
                        .chain(after_for.iter().cloned())
                        .collect();
                    edits.push(replaced(e, vec![written(&e.name, &e.flags, &words)]));
                }
                [] if !checked.is_empty() => new.push(format!(
                    "EXPOSE --harnesses={name} {} AS egress FOR {net}",
                    ports.join(" ")
                )),
                [] => {}
                _ => {
                    return Err(Refusal::InText(format!(
                        "network {net} has its egress opened at the microVM's boundary more than once: change it in the text"
                    )));
                }
            }
            if !checked.is_empty()
                && !rest
                    .iter()
                    .any(|o| matches!(&o.kind, Kind::Agentfile(Directive::Connect(_))))
            {
                new.push(format!("CONNECT {name} WITH {name} ON {net}"));
            }
            Ok(())
        }
        [(_, n)] => Err(Refusal::InText(format!(
            "harness {name}'s egress is on network {}, which it shares: change it in the text",
            text(&n.name)
        ))),
        _ => Err(Refusal::InText(format!(
            "harness {name}'s egress is on more than one network: change it in the text"
        ))),
    }
}

/// Whether `l` is the microVM boundary's egress for network `net` alone, or harness `name`'s `CONNECT`
/// with itself on it alone: the lines a harness's own network has as kiln's Agentfiles write it.
fn serves(l: &Line, net: &str, name: &str) -> bool {
    let on = |nets: &[Vec<u8>]| nets.len() == 1 && nets.iter().all(|n| n == net.as_bytes());
    match &l.kind {
        Kind::Agentfile(Directive::Expose(e)) => {
            e.direction == Direction::Egress && on(&e.networks)
        }
        Kind::Agentfile(Directive::Connect(c)) => {
            c.both_ways
                && on(&c.on)
                && c.ports.is_empty()
                && [&c.from, &c.to]
                    .iter()
                    .all(|m| m.len() == 1 && m.iter().all(|n| n == name.as_bytes()))
        }
        _ => false,
    }
}

/// An `EXPOSE … AS …`'s words split where its ports end: its ports, and the rest.
fn split_ports(words: &[String]) -> (&[String], &[String]) {
    let at = words
        .iter()
        .position(|w| w.eq_ignore_ascii_case("as") || w.eq_ignore_ascii_case("for"))
        .unwrap_or(words.len());
    (
        words.get(..at).unwrap_or_default(),
        words.get(at..).unwrap_or_default(),
    )
}

/// `t` with agent `name`'s shared settings made `s`: its labels and remote MCP servers rewritten where
/// they are, and what it had none of yet written after its `AGENT`. Its MCP servers from other sources
/// are its image's own, and stay.
pub fn set_agent(t: &str, name: &str, s: &AgentSettings) -> R<String> {
    let r = read(t)?;
    let a_line = declaring(&r, "agent", name)?;
    let (mut edits, mut new) = (Vec::new(), Vec::new());
    let model = s
        .model
        .as_deref()
        .map(|m| plain_or("model", m))
        .transpose()?;
    for (setting, names) in [("tools", &s.tools), ("skills", &s.skills)] {
        if names.iter().any(|n| n.trim().is_empty()) {
            return Err(Refusal::Value {
                setting: list_setting(setting),
                why: "each has its name".into(),
            });
        }
    }
    let lists = |names: &[String]| (!names.is_empty()).then(|| list_value(names));
    let wanted = [
        ("model", model),
        ("tools", lists(s.tools.as_slice())),
        ("skills", lists(s.skills.as_slice())),
    ];
    let had = AgentSettings::from(&agent_in(&r, a_line, name, &[])?);
    let changed: Vec<(&str, Option<String>)> = wanted
        .into_iter()
        .filter(|(setting, _)| match *setting {
            "model" => had.model != s.model,
            "tools" => had.tools != s.tools,
            _ => had.skills != s.skills,
        })
        .collect();
    if !changed.is_empty() {
        label_edits(
            &r,
            a_line.stage,
            &format!("{AGENT_LABEL}{name}."),
            &changed,
            &mut edits,
            &mut new,
        );
    }
    if had.mcp != s.mcp {
        mcp_edits(&r, a_line.stage, name, &s.mcp, &mut edits, &mut new)?;
    }
    if !new.is_empty() {
        edits.push(after(a_line, new));
    }
    Ok(apply(t, edits))
}

/// Agent `name`, declared at `l`, as its settings; its credentials named by `refs` (the text's references).
fn agent_in(r: &Read, l: &Line, name: &str, refs: &[Reference]) -> R<Agent> {
    let labels = labels(r, l.stage, &format!("{AGENT_LABEL}{name}."));
    let (source, version, credentials) = match &l.kind {
        Kind::Agentfile(Directive::Agent(a)) => (
            text(&a.source),
            version_of(&a.source, &a.fetch),
            credentials_of(&a.source, refs),
        ),
        _ => (String::new(), None, Vec::new()),
    };
    Ok(Agent {
        name: name.to_owned(),
        line: line_no(l.first),
        source,
        version,
        model: set_in(&labels, "model"),
        tools: listed(&labels, "tools")?,
        skills: listed(&labels, "skills")?,
        mcp: remotes_of(r, l.stage, name),
        credentials,
    })
}

/// Agent `name`'s remote MCP servers made `servers`: each `MCP` of `stage` from a URL naming it taken
/// out (rewritten without it where it names others too), and one `MCP <server> FROM <url> FOR <name>`
/// each added.
fn mcp_edits(
    r: &Read,
    stage: usize,
    name: &str,
    servers: &[Remote],
    edits: &mut Vec<Edit>,
    new: &mut Vec<String>,
) -> R<()> {
    for m in servers {
        let n = m.name.trim();
        if n.is_empty()
            || !n.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
            })
            || !n.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        {
            return Err(Refusal::Value {
                setting: "mcp",
                why: format!(
                    "{n:?} isn't an MCP server's name in an Agentfile: a lowercase letter, then lowercase letters, digits, `-`, `_` or `.`"
                ),
            });
        }
        if !matches!(
            agentfile::source_of(m.url.trim().as_bytes()),
            Ok(Source::Http(_))
        ) || !plain(m.url.trim())
        {
            return Err(Refusal::Value {
                setting: "mcp",
                why: format!("{n}'s {:?} isn't an http(s) URL", m.url),
            });
        }
    }
    for l in r.lines.iter().filter(|l| l.stage == stage) {
        let Kind::Agentfile(Directive::Mcp(m)) = &l.kind else {
            continue;
        };
        if !names(&m.scope, name, TargetKind::Agent)
            || !matches!(agentfile::source_of(&m.source), Ok(Source::Http(_)))
        {
            continue;
        }
        let others: Vec<String> = m
            .scope
            .names
            .iter()
            .filter(|n| *n != name.as_bytes())
            .map(|n| text(n))
            .collect();
        if others.is_empty() {
            edits.push(replaced(l, Vec::new()));
        } else {
            let (before, _) = split_for(&l.words);
            let words: Vec<String> = before
                .iter()
                .cloned()
                .chain(std::iter::once("FOR".to_owned()))
                .chain(others)
                .collect();
            edits.push(replaced(l, vec![written(&l.name, &l.flags, &words)]));
        }
    }
    new.extend(
        servers
            .iter()
            .map(|m| format!("MCP {} FROM {} FOR {name}", m.name.trim(), m.url.trim())),
    );
    Ok(())
}

#[cfg(test)]
mod tests;
