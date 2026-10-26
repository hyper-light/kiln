//! The Dockerfile frontend (docs/ci.md W24-R29): a Dockerfile as kiln reads it
//! ([`crate::dockerfile`]) lowered to container operations ([`super`]), the way BuildKit's own
//! frontend (dockerfile2llb) lowers it to LLB: its stages, its build arguments and the expansion of
//! variables where Dockerfiles define it, each instruction's operations, and the image the target
//! stage exports. What kiln can't build natively yet comes back by name ([`NotNative`]), with its
//! instruction and line, never dropped.
//!
//! It's pure. What only the network knows, a base image's config (its variables, working folder,
//! user, shell and digest), the caller resolves and passes in ([`Options::images`]). Until then
//! [`Build::unresolved`] lists those images and they read as images with an empty config: lower a
//! build once to learn what to resolve, then again with it to run it.
//!
//! Where it differs from BuildKit on purpose: a stage the target doesn't need is lowered too, so
//! every instruction is accounted for, and if it fails that's noted on it ([`Stage::error`])
//! rather than failing the build, as BuildKit never looks at it. Its instructions come from kiln's
//! reader, which joins continued lines with a space where BuildKit joins them as they are.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::shell::Lex;
use super::{Command, ConfigEdit, CopyFiles, CopySource, Exec, Export, Git, Graph, Healthcheck, Http, Image, Input, Local, Mkdir, Mkfile, Mount, MountKind, Network, NodeId, Op, Platform, Security, Sharing};
use crate::dockerfile::{self as reader, Instruction};
use crate::wf::diag::Severity;

/// `PATH` where a base image sets none, as BuildKit sets it (except on Windows).
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Build arguments that are BuildKit's own switches, never variables.
const NON_ENV_ARGS: &[&str] = &["BUILDKIT_SBOM_SCAN_CONTEXT", "BUILDKIT_SBOM_SCAN_STAGE"];

/// What a lowering is given besides the Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The platform it builds for (`TARGETPLATFORM`).
    pub platform: Platform,
    /// The platform it builds on (`BUILDPLATFORM`).
    pub build_platform: Platform,
    /// `--build-arg`s, which override `ARG` defaults.
    pub build_args: BTreeMap<String, String>,
    /// `--target`: the stage to build; the last one when it's `None`.
    pub target: Option<String>,
    /// Images' configs as the caller resolved them ([`Build::unresolved`] says which it needs).
    pub images: BTreeMap<ImageRef, ImageConfig>,
    /// The main build context's name.
    pub context: String,
    /// The main context's `.dockerignore` patterns.
    pub ignore: Vec<String>,
    /// The escape character (`# escape=`, which [`lower_source`] reads).
    pub escape: char,
}

impl Options {
    /// Building for `platform` on it, with no build arguments, for the last stage, with nothing
    /// resolved, from a main context called `context`.
    pub fn new(platform: Platform) -> Self {
        Options { build_platform: platform.clone(), platform, build_args: BTreeMap::new(), target: None, images: BTreeMap::new(), context: "context".into(), ignore: Vec::new(), escape: '\\' }
    }
}

/// An image a build reads, as it names it: normalized (`docker.io/library/alpine:3.20`, with its
/// digest when it has one) and for one platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ImageRef {
    pub reference: String,
    pub platform: Platform,
}

/// What the frontend reads of an image's config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    /// The manifest's digest, which pins the image.
    pub digest: Option<String>,
    /// `NAME=value`, as OCI configs list them.
    pub env: Vec<String>,
    pub working_dir: String,
    pub user: String,
    pub shell: Vec<String>,
    pub on_build: Vec<String>,
    pub os: String,
}

/// Why a Dockerfile can't be lowered, and where; line 0 when no line is to blame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct Error {
    pub line: u32,
    pub message: String,
}

/// A Dockerfile lowered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub graph: Graph,
    pub stages: Vec<Stage>,
    /// The stage built.
    pub target: usize,
    /// Its image: an [`Export`] node.
    pub export: NodeId,
    /// Every instruction, in order, with what it became.
    pub instructions: Vec<Lowered>,
    /// What isn't native yet, in order.
    pub not_native: Vec<NotNative>,
    /// Images whose configs weren't given (Options::images): their digests aren't pinned, and a
    /// base image's config was read as empty.
    pub unresolved: BTreeSet<ImageRef>,
}

/// A stage, lowered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// Lower-cased; empty when it has none.
    pub name: String,
    /// Its `FROM`'s.
    pub line: u32,
    pub platform: Platform,
    /// Its filesystem: `None` is an empty one.
    pub rootfs: Option<NodeId>,
    /// Its config: `None` is BuildKit's empty config.
    pub config: Option<NodeId>,
    /// Its layers, in order.
    pub layers: Vec<NodeId>,
    /// Whether the target needs it.
    pub reachable: bool,
    /// Why it couldn't be lowered, when the target doesn't need it. What its instructions before
    /// the failure made stays in the graph, read by nothing.
    pub error: Option<String>,
}

/// An instruction and what it became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lowered {
    pub line: u32,
    pub keyword: String,
    /// Its stage; `None` for an `ARG` before the first `FROM`.
    pub stage: Option<usize>,
    pub became: Became,
}

/// What an instruction became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Became {
    /// These operations: those it added, or found already there (the same operation over the same
    /// inputs is one node), its main one last.
    Ops(Vec<NodeId>),
    /// Nothing of its own, only what what follows sees: an `ARG`, a `FROM` of an earlier stage or
    /// of `scratch`.
    Scope,
    /// Nothing: kiln can't build it natively yet ([`Build::not_native`] says why).
    NotNative(String),
    /// Nothing: its stage, which the target doesn't need, failed to lower (why).
    Failed(String),
}

/// Something kiln can't build natively yet: the build would go to BuildKit as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotNative {
    pub line: u32,
    /// Its instruction's keyword (`RUN`, `ONBUILD`), or `syntax` for a parser directive.
    pub instruction: String,
    pub what: String,
}

/// Reads `src` with kiln's reader and lowers it ([`lower`]). Its parser directives count too: the
/// escape character, and a `syntax` that names another frontend, which isn't native (unless
/// `BUILDKIT_SYNTAX` overrides it, which [`lower`] judges).
pub fn lower_source(src: &str, opts: &Options) -> Result<Build, Error> {
    let parsed = reader::parse(src);
    if let Some(d) = parsed.diagnostics.iter().find(|d| d.severity == Severity::Error) {
        return Err(Error { line: d.range.start.line, message: format!("{}: {}", d.code, d.message) });
    }
    let directives = directives(src);
    let mut opts = opts.clone();
    if directives.get("escape").is_some_and(|(v, _)| v.as_str() == "`") {
        opts.escape = '`';
    }
    let mut build = lower(&parsed.instructions, &opts)?;
    if let Some((frontend, line)) = directives.get("syntax")
        && !opts.build_args.contains_key("BUILDKIT_SYNTAX")
        && !standard_frontend(frontend)
    {
        build.not_native.insert(0, NotNative { line: *line, instruction: "syntax".into(), what: format!("`# syntax={frontend}` names another frontend; kiln's is Dockerfile's own") });
    }
    Ok(build)
}

/// BuildKit's own switches among the build's arguments, which change what it does and which kiln
/// doesn't model: each one isn't native (`BUILDKIT_SYNTAX` only when it names another frontend;
/// `BUILDKIT_DOCKERFILE_CHECK` only configures lints).
fn switches(opts: &Options) -> Vec<NotNative> {
    let mut out = Vec::new();
    for (k, v) in &opts.build_args {
        let what = match k.as_str() {
            "BUILDKIT_SYNTAX" if !standard_frontend(v) => format!("BUILDKIT_SYNTAX={v} names another frontend; kiln's is Dockerfile's own"),
            "BUILDKIT_SYNTAX" | "BUILDKIT_DOCKERFILE_CHECK" => continue,
            _ if k.starts_with("BUILDKIT_") => format!("{k}={v}: a BuildKit switch kiln doesn't model yet"),
            _ => continue,
        };
        out.push(NotNative { line: 0, instruction: "build-arg".into(), what });
    }
    out
}

/// The parser directives at the top of `src` (`# name=value`), by lower-cased name, with their lines.
fn directives(src: &str) -> BTreeMap<String, (String, u32)> {
    let mut out = BTreeMap::new();
    for (i, l) in src.lines().enumerate() {
        let Some(body) = l.trim_start().strip_prefix('#') else { break };
        let Some((k, v)) = body.split_once('=') else { break };
        let k = k.trim().to_ascii_lowercase();
        if k.is_empty() || k.contains(char::is_whitespace) {
            break;
        }
        out.entry(k).or_insert((v.trim().to_owned(), u32::try_from(i + 1).unwrap_or(u32::MAX)));
    }
    out
}

/// Whether `syntax` is Docker's own Dockerfile frontend, which kiln's implements.
fn standard_frontend(syntax: &str) -> bool {
    let s = syntax.trim();
    let s = s.strip_prefix("docker.io/").unwrap_or(s);
    let name = s.split(['@', ':']).next().unwrap_or_default();
    matches!(name, "docker/dockerfile" | "docker/dockerfile-upstream")
}

/// Lowers a Dockerfile's instructions (kiln's reader's: [`crate::dockerfile::parse`]).
pub fn lower(instructions: &[Instruction], opts: &Options) -> Result<Build, Error> {
    let read = read_all(instructions, opts.escape)?;
    let Some(last) = read.stages.len().checked_sub(1) else {
        return Err(Error { line: 0, message: "the Dockerfile has no stage to build: it needs a FROM".into() });
    };
    let names: Vec<String> = read.stages.iter().map(|s| s.name.clone()).collect();
    let target = match &opts.target {
        Some(t) => find_stage(&names, t).ok_or_else(|| Error { line: 0, message: format!("target stage {t:?} could not be found") })?,
        None => last,
    };
    let target_name = names.get(target).cloned().unwrap_or_default();
    let lex = Lex::new(opts.escape);
    let mut meta = default_args(opts, if target_name.is_empty() { "default" } else { &target_name });
    let mut instructions_out = Vec::new();
    for (ins, pairs) in &read.globals {
        for (name, default) in pairs {
            let value = match (opts.build_args.get(name), default) {
                (Some(v), _) => Some(v.clone()),
                (None, Some(d)) => Some(lex.word(d, &|k| meta.get(k).cloned()).map_err(|message| Error { line: ins.line, message })?),
                (None, None) => None,
            };
            if let Some(v) = value {
                meta.insert(name.clone(), v);
            }
        }
        instructions_out.push(Lowered { line: ins.line, keyword: ins.keyword.clone(), stage: None, became: Became::Scope });
    }
    let epoch = epoch(opts, &meta)?;
    let reach = reachable(&read, &meta, target, &lex);
    let mut l = Lowerer { opts, lex, graph: Graph::new(), meta, epoch, stages: Vec::new(), names, instructions: instructions_out, not_native: switches(opts), unresolved: BTreeSet::new() };
    for (i, s) in read.stages.iter().enumerate() {
        match l.stage(i, s) {
            Ok(st) => l.stages.push(st),
            Err((at, e)) => {
                if reach.get(i).copied().unwrap_or(true) {
                    return Err(e);
                }
                let why = e.message.clone();
                if at.is_none() {
                    l.instructions.push(Lowered { line: s.from.line, keyword: s.from.keyword.clone(), stage: Some(i), became: Became::Failed(why.clone()) });
                }
                for (ins, _) in s.cmds.iter().skip(at.unwrap_or(0)) {
                    l.instructions.push(Lowered { line: ins.line, keyword: ins.keyword.clone(), stage: Some(i), became: Became::Failed(why.clone()) });
                }
                l.stages.push(St::failed(opts.platform.clone(), e.to_string()));
            }
        }
    }
    let t = l.stages.get(target).cloned().ok_or_else(|| Error { line: 0, message: "the target stage wasn't lowered".into() })?;
    let mut inputs: Vec<Input> = t.config.map(|c| Input::new("config", c)).into_iter().collect();
    inputs.extend(t.layers.iter().enumerate().map(|(k, layer)| Input::new(format!("layer.{k}"), *layer)));
    let export = l.add(Op::Export(Export { platform: opts.platform.clone(), epoch }), inputs).map_err(|message| Error { line: 0, message })?;
    let stages = read
        .stages
        .iter()
        .zip(&l.stages)
        .enumerate()
        .map(|(i, (s, st))| Stage { name: s.name.clone(), line: s.from.line, platform: st.platform.clone(), rootfs: st.fs, config: st.config, layers: st.layers.clone(), reachable: reach.get(i).copied().unwrap_or(false), error: st.failed.clone() })
        .collect();
    Ok(Build { graph: l.graph, stages, target, export, instructions: l.instructions, not_native: l.not_native, unresolved: l.unresolved })
}

/// The stage `name` names (case aside): the last one so named, as BuildKit finds it.
fn find_stage(names: &[String], name: &str) -> Option<usize> {
    let name = name.to_ascii_lowercase();
    names.iter().rposition(|n| !n.is_empty() && *n == name)
}

/// The build arguments every Dockerfile has: the platforms', and `TARGETSTAGE`.
fn default_args(opts: &Options, target: &str) -> BTreeMap<String, String> {
    let (b, t) = (&opts.build_platform, &opts.platform);
    let defaults = [
        ("BUILDPLATFORM", b.to_string()),
        ("BUILDOS", b.os.clone()),
        ("BUILDOSVERSION", String::new()),
        ("BUILDARCH", b.arch.clone()),
        ("BUILDVARIANT", b.variant.clone()),
        ("TARGETPLATFORM", t.to_string()),
        ("TARGETOS", t.os.clone()),
        ("TARGETOSVERSION", String::new()),
        ("TARGETARCH", t.arch.clone()),
        ("TARGETVARIANT", t.variant.clone()),
        ("TARGETSTAGE", target.to_owned()),
    ];
    defaults.into_iter().map(|(k, v)| (k.to_owned(), opts.build_args.get(k).cloned().unwrap_or(v))).collect()
}

/// `SOURCE_DATE_EPOCH`, from the build's arguments or a global `ARG`, in seconds.
fn epoch(opts: &Options, meta: &BTreeMap<String, String>) -> Result<Option<i64>, Error> {
    match opts.build_args.get("SOURCE_DATE_EPOCH").or_else(|| meta.get("SOURCE_DATE_EPOCH")) {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) => v.parse::<i64>().map(Some).map_err(|_| Error { line: 0, message: format!("SOURCE_DATE_EPOCH {v:?} isn't a number of seconds") }),
    }
}

/// The proxy settings among the build's arguments; of two spellings of one, the lower-cased
/// wins, as it does in BuildKit.
fn proxy(opts: &Options) -> BTreeMap<String, String> {
    let mut p = BTreeMap::new();
    for (k, v) in &opts.build_args {
        let up = k.to_ascii_uppercase();
        if matches!(up.as_str(), "HTTP_PROXY" | "HTTPS_PROXY" | "FTP_PROXY" | "NO_PROXY" | "ALL_PROXY") {
            p.insert(up, v.clone());
        }
    }
    p
}

/// Which stages the target needs: itself, its base stages, and the stages it copies or mounts
/// from, all the way down.
fn reachable(read: &Read<'_>, meta: &BTreeMap<String, String>, target: usize, lex: &Lex) -> Vec<bool> {
    let names: Vec<String> = read.stages.iter().map(|s| s.name.clone()).collect();
    let deps: Vec<Vec<usize>> = read
        .stages
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut d = Vec::new();
            if let Ok(base) = lex.word(&s.base, &|k| meta.get(k).cloned())
                && let Some(j) = names.iter().take(i).rposition(|n| !n.is_empty() && *n == base)
            {
                d.push(j);
            }
            for (_, cmd) in &s.cmds {
                match cmd {
                    Instr::Copy(c) => {
                        if let Some(f) = &c.from {
                            match f.parse::<i64>() {
                                Ok(n) => d.extend(usize::try_from(n).ok()),
                                Err(_) => d.extend(find_stage(&names, f)),
                            }
                        }
                    }
                    Instr::Run(r) => {
                        for m in &r.mounts {
                            if let Some(f) = mount_from(m) {
                                d.extend(find_stage(&names, &f));
                            }
                        }
                    }
                    _ => {}
                }
            }
            d
        })
        .collect();
    let mut seen = vec![false; read.stages.len()];
    let mut todo = vec![target];
    while let Some(i) = todo.pop() {
        if let Some(s) = seen.get_mut(i)
            && !*s
        {
            *s = true;
            todo.extend(deps.get(i).into_iter().flatten().copied());
        }
    }
    seen
}

// ---------------------------------------------------------------------------------- reading

/// An instruction read: its flags and arguments, before anything is expanded.
enum Instr {
    Arg(Vec<(String, Option<String>)>),
    Env(Vec<(String, String)>),
    Label(Vec<(String, String)>),
    Run(RunCmd),
    Copy(CopyCmd),
    Workdir(String),
    User(String),
    Expose(Vec<String>),
    Volume(Vec<String>),
    StopSignal(String),
    Cmd(Line),
    Entrypoint(Line),
    Shell(Vec<String>),
    Healthcheck(Healthcheck),
    Maintainer(String),
    Onbuild(String),
}

/// A command line: exec form (a JSON list) or shell form.
enum Line {
    Json(Vec<String>),
    Shell(String),
}

/// A heredoc: its word, its body (its tabs stripped already for `<<-`), and whether its body is
/// expanded (its word wasn't quoted).
struct Heredoc {
    name: String,
    data: String,
    expand: bool,
}

struct RunCmd {
    mounts: Vec<String>,
    network: Network,
    security: Security,
    devices: Vec<String>,
    line: Line,
    heredocs: Vec<Heredoc>,
}

struct CopyCmd {
    add: bool,
    from: Option<String>,
    chown: String,
    chmod: String,
    link: bool,
    parents: bool,
    exclude: Vec<String>,
    keep_git_dir: Option<bool>,
    checksum: String,
    unpack: Option<bool>,
    paths: Vec<String>,
    docs: Vec<Heredoc>,
    dest: String,
}

struct StageSrc<'a> {
    from: &'a Instruction,
    base: String,
    name: String,
    platform: Option<String>,
    cmds: Vec<(&'a Instruction, Instr)>,
}

struct Read<'a> {
    globals: Vec<(&'a Instruction, Vec<(String, Option<String>)>)>,
    stages: Vec<StageSrc<'a>>,
}

fn read_all(instructions: &[Instruction], escape: char) -> Result<Read<'_>, Error> {
    let mut r = Read { globals: Vec::new(), stages: Vec::new() };
    for ins in instructions {
        let at = |message: String| Error { line: ins.line, message };
        if ins.keyword == "FROM" {
            let (base, name, platform) = read_from(ins, escape).map_err(at)?;
            r.stages.push(StageSrc { from: ins, base, name, platform, cmds: Vec::new() });
            continue;
        }
        let cmd = read(ins, escape).map_err(at)?;
        match (r.stages.last_mut(), cmd) {
            (Some(s), cmd) => s.cmds.push((ins, cmd)),
            (None, Instr::Arg(pairs)) => r.globals.push((ins, pairs)),
            (None, _) => return Err(at(format!("{} comes before any FROM: only ARG may", ins.keyword))),
        }
    }
    Ok(r)
}

fn read_from(ins: &Instruction, escape: char) -> Result<(String, String, Option<String>), String> {
    let (flags, rest) = split_flags(&ins.args, escape);
    let f = parse_flags(&flags, &[("platform", FlagKind::Str)])?;
    let (base, name) = match whitespace(&rest).as_slice() {
        [base] => (base.clone(), String::new()),
        [base, as_, name] if as_.eq_ignore_ascii_case("as") => {
            let lower = name.to_ascii_lowercase();
            let mut chars = lower.chars();
            let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
            if !valid {
                return Err(format!("invalid name for build stage: {name:?}, name can't start with a number or contain symbols"));
            }
            (base.clone(), lower)
        }
        _ => return Err("FROM requires either one or three arguments".into()),
    };
    Ok((base, name, f.str("platform").filter(|p| !p.is_empty()).map(str::to_owned)))
}

fn read(ins: &Instruction, escape: char) -> Result<Instr, String> {
    let (flags, rest) = split_flags(&ins.args, escape);
    let rest = rest.trim().to_owned();
    let kw = ins.keyword.as_str();
    let no_flags = |flags: &[String]| parse_flags(flags, &[]).map(|_| ());
    let one = |rest: String| if rest.is_empty() { Err(format!("{kw} requires exactly one argument")) } else { Ok(rest) };
    Ok(match kw {
        "ARG" => {
            no_flags(&flags)?;
            let words = parse_words(&rest, escape);
            if words.is_empty() {
                return Err("ARG requires at least one argument".into());
            }
            let mut pairs = Vec::new();
            for w in words {
                let pair = match w.split_once('=') {
                    Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
                    None => (w, None),
                };
                if pair.0.is_empty() {
                    return Err("ARG names can not be blank".into());
                }
                pairs.push(pair);
            }
            Instr::Arg(pairs)
        }
        "ENV" => {
            no_flags(&flags)?;
            Instr::Env(name_values(&rest, "ENV", escape)?)
        }
        "LABEL" => {
            no_flags(&flags)?;
            Instr::Label(name_values(&rest, "LABEL", escape)?)
        }
        "RUN" => {
            let f = parse_flags(&flags, &[("mount", FlagKind::Strs), ("network", FlagKind::Str), ("security", FlagKind::Str), ("device", FlagKind::Strs)])?;
            let mounts = f.strs("mount");
            for m in &mounts {
                if let Some(from) = mount_from(m)
                    && from.find('$').is_some_and(|i| i + 1 != from.len())
                {
                    return Err("'from' doesn't support variable expansion, define alias stage instead".into());
                }
            }
            let network = match f.str("network").unwrap_or("default") {
                "default" => Network::Default,
                "none" => Network::None,
                "host" => Network::Host,
                other => return Err(format!("invalid network mode {other:?}")),
            };
            let security = match f.str("security").unwrap_or("sandbox") {
                "sandbox" => Security::Sandbox,
                "insecure" => Security::Insecure,
                other => return Err(format!("security {other:?} is not valid")),
            };
            if rest.is_empty() {
                return Err("RUN requires at least one argument".into());
            }
            Instr::Run(RunCmd { mounts, network, security, devices: f.strs("device"), line: maybe_json(&rest)?, heredocs: heredocs_of(ins)? })
        }
        "COPY" | "ADD" => read_copy(ins, kw == "ADD", &flags, &rest)?,
        "WORKDIR" => {
            no_flags(&flags)?;
            Instr::Workdir(one(rest)?)
        }
        "USER" => {
            no_flags(&flags)?;
            Instr::User(one(rest)?)
        }
        "STOPSIGNAL" => {
            no_flags(&flags)?;
            Instr::StopSignal(one(rest)?)
        }
        "MAINTAINER" => {
            no_flags(&flags)?;
            Instr::Maintainer(one(rest)?)
        }
        "EXPOSE" => {
            no_flags(&flags)?;
            let mut ports = whitespace(&rest);
            if ports.is_empty() {
                return Err("EXPOSE requires at least one argument".into());
            }
            ports.sort();
            Instr::Expose(ports)
        }
        "VOLUME" => {
            no_flags(&flags)?;
            let volumes: Vec<String> = maybe_json_list(&rest)?.into_iter().map(|v| v.trim().to_owned()).collect();
            if volumes.is_empty() {
                return Err("VOLUME requires at least one argument".into());
            }
            if volumes.iter().any(String::is_empty) {
                return Err("VOLUME specified can not be an empty string".into());
            }
            Instr::Volume(volumes)
        }
        "CMD" | "ENTRYPOINT" => {
            no_flags(&flags)?;
            if rest.is_empty() {
                return Err(format!("{kw} requires at least one argument"));
            }
            let line = maybe_json(&rest)?;
            if kw == "CMD" { Instr::Cmd(line) } else { Instr::Entrypoint(line) }
        }
        "SHELL" => {
            no_flags(&flags)?;
            match maybe_json(&rest)? {
                Line::Json(shell) if !shell.is_empty() => Instr::Shell(shell),
                Line::Json(_) => return Err("SHELL requires at least one argument".into()),
                Line::Shell(_) => return Err("SHELL requires the arguments to be in JSON form".into()),
            }
        }
        "HEALTHCHECK" => Instr::Healthcheck(read_health(&flags, &rest)?),
        "ONBUILD" => {
            no_flags(&flags)?;
            let trigger = rest.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
            match trigger.as_str() {
                "" => return Err("ONBUILD requires at least one argument".into()),
                "ONBUILD" => return Err("Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed".into()),
                "MAINTAINER" | "FROM" => return Err(format!("{trigger} isn't allowed as an ONBUILD trigger")),
                _ => {}
            }
            Instr::Onbuild(rest)
        }
        other => return Err(format!("unknown instruction: {other}")),
    })
}

fn read_copy(ins: &Instruction, add: bool, flags: &[String], rest: &str) -> Result<Instr, String> {
    let spec: &[(&'static str, FlagKind)] = if add {
        &[("chown", FlagKind::Str), ("chmod", FlagKind::Str), ("link", FlagKind::Bool), ("keep-git-dir", FlagKind::Bool), ("checksum", FlagKind::Str), ("unpack", FlagKind::Bool), ("exclude", FlagKind::Strs)]
    } else {
        &[("chown", FlagKind::Str), ("from", FlagKind::Str), ("chmod", FlagKind::Str), ("link", FlagKind::Bool), ("exclude", FlagKind::Strs), ("parents", FlagKind::Bool)]
    };
    let f = parse_flags(flags, spec)?;
    let kw = if add { "ADD" } else { "COPY" };
    let words = maybe_json_list(rest)?;
    let Some((dest, srcs)) = words.split_last().filter(|(_, s)| !s.is_empty()) else {
        return Err(format!("{kw} requires at least two arguments, but only one was provided. Destination could not be determined"));
    };
    if lone_heredoc(dest).is_some() {
        return Err(format!("{kw} cannot accept a heredoc as a destination"));
    }
    let bodies: BTreeMap<String, String> = heredocs_of(ins)?.into_iter().map(|h| (h.name, h.data)).collect();
    let mut paths = Vec::new();
    let mut docs = Vec::new();
    for s in srcs {
        match lone_heredoc(s) {
            Some((name, expand)) => docs.push(Heredoc { data: bodies.get(&name).cloned().unwrap_or_default(), name, expand }),
            None => paths.push(s.clone()),
        }
    }
    Ok(Instr::Copy(CopyCmd {
        add,
        from: f.str("from").filter(|v| !v.is_empty()).map(str::to_owned),
        chown: f.str("chown").unwrap_or_default().to_owned(),
        chmod: f.str("chmod").unwrap_or_default().to_owned(),
        link: f.bool("link").unwrap_or(false),
        parents: f.bool("parents").unwrap_or(false),
        exclude: f.strs("exclude"),
        keep_git_dir: f.bool("keep-git-dir"),
        checksum: f.str("checksum").unwrap_or_default().to_owned(),
        unpack: f.bool("unpack"),
        paths,
        docs,
        dest: dest.clone(),
    }))
}

fn read_health(flags: &[String], rest: &str) -> Result<Healthcheck, String> {
    let (typ, after) = match rest.split_once(char::is_whitespace) {
        Some((t, a)) => (t, a.trim_start()),
        None => (rest, ""),
    };
    if typ.is_empty() {
        return Err("HEALTHCHECK requires at least one argument".into());
    }
    let none = Healthcheck { test: vec!["NONE".into()], interval: 0, timeout: 0, start_period: 0, start_interval: 0, retries: 0 };
    if typ.eq_ignore_ascii_case("NONE") {
        if !after.is_empty() {
            return Err("HEALTHCHECK NONE takes no arguments".into());
        }
        return Ok(none);
    }
    let f = parse_flags(flags, &[("interval", FlagKind::Str), ("timeout", FlagKind::Str), ("start-period", FlagKind::Str), ("start-interval", FlagKind::Str), ("retries", FlagKind::Str)])?;
    if !typ.eq_ignore_ascii_case("CMD") {
        return Err(format!("Unknown type {typ:?} in HEALTHCHECK (try CMD)"));
    }
    let test = match maybe_json(after)? {
        Line::Json(v) if !v.is_empty() => std::iter::once("CMD".to_owned()).chain(v).collect(),
        Line::Shell(s) if !s.is_empty() => vec!["CMD-SHELL".to_owned(), s],
        _ => return Err("Missing command after HEALTHCHECK CMD".into()),
    };
    let dur = |name: &str| interval(name, f.str(name).unwrap_or_default());
    let retries = match f.str("retries").unwrap_or_default() {
        "" => 0,
        r => {
            let n: i32 = r.parse().map_err(|_| format!("strconv.ParseInt: parsing {r:?}: invalid syntax"))?;
            u32::try_from(n).map_err(|_| format!("--retries cannot be negative ({n})"))?
        }
    };
    Ok(Healthcheck { test, interval: dur("interval")?, timeout: dur("timeout")?, start_period: dur("start-period")?, start_interval: dur("start-interval")?, retries })
}

/// A health check's duration flag, in nanoseconds: 0 when it's absent or zero, else at least 1ms.
fn interval(name: &str, value: &str) -> Result<u64, String> {
    if value.is_empty() {
        return Ok(0);
    }
    match go_duration(value)? {
        0 => Ok(0),
        d if d < 1_000_000 => Err(format!("Interval {name:?} cannot be less than 1ms")),
        d => u64::try_from(d).map_err(|_| format!("Interval {name:?} is too long")),
    }
}

/// A Go duration (`1m30s`, `1.5h`, `250ms`) in nanoseconds.
fn go_duration(s: &str) -> Result<i128, String> {
    let bad = || format!("time: invalid duration {s:?}");
    let (neg, mut rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if rest == "0" {
        return Ok(0);
    }
    if rest.is_empty() {
        return Err(bad());
    }
    let mut total: i128 = 0;
    while !rest.is_empty() {
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let int = rest.get(..int_len).unwrap_or_default();
        rest = rest.get(int_len..).unwrap_or_default();
        let mut frac = "";
        if let Some(r) = rest.strip_prefix('.') {
            let n = r.bytes().take_while(u8::is_ascii_digit).count();
            frac = r.get(..n).unwrap_or_default();
            rest = r.get(n..).unwrap_or_default();
        }
        if int.is_empty() && frac.is_empty() {
            return Err(bad());
        }
        let unit_len = rest.char_indices().find(|(_, c)| c.is_ascii_digit() || *c == '.').map_or(rest.len(), |(i, _)| i);
        let unit = rest.get(..unit_len).unwrap_or_default();
        rest = rest.get(unit_len..).unwrap_or_default();
        let scale: i128 = match unit {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            "" => return Err(format!("time: missing unit in duration {s:?}")),
            u => return Err(format!("time: unknown unit {u:?} in duration {s:?}")),
        };
        if int.len() > 30 {
            return Err(bad());
        }
        let whole: i128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| bad())? };
        let mut v = whole.checked_mul(scale).ok_or_else(bad)?;
        let mut step = scale;
        for d in frac.bytes() {
            step /= 10;
            v += i128::from(d - b'0') * step;
        }
        total = total.checked_add(v).ok_or_else(bad)?;
    }
    if total > i128::from(i64::MAX) {
        return Err(bad());
    }
    Ok(if neg { -total } else { total })
}

/// BuildKit's flag words: those that start with `--` before an instruction's arguments, each with
/// its quotes and escapes taken out; a bare `--` ends them. The rest is the arguments.
fn split_flags(line: &str, escape: char) -> (Vec<String>, String) {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let rest_at = |k: usize| chars.get(k).map_or(String::new(), |(pos, _)| line.get(*pos..).unwrap_or_default().to_owned());
    let mut words = Vec::new();
    let mut word = String::new();
    // 0: between words; 1: in a word; 2: in quotes.
    let mut phase = 0;
    let mut quote = '\0';
    let mut blank_ok = false;
    let mut k = 0;
    loop {
        let Some(&(_, ch)) = chars.get(k) else {
            if phase != 0 && word != "--" && (blank_ok || !word.is_empty()) {
                words.push(word);
            }
            return (words, String::new());
        };
        if phase == 0 {
            if ch.is_whitespace() {
                k += 1;
                continue;
            }
            if ch != '-' || chars.get(k + 1).is_none_or(|(_, c)| *c != '-') {
                return (words, rest_at(k));
            }
            phase = 1;
        }
        if phase == 1 {
            if ch.is_whitespace() {
                if word == "--" {
                    return (words, rest_at(k));
                }
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                blank_ok = false;
                phase = 0;
                k += 1;
                continue;
            }
            if ch == '\'' || ch == '"' {
                quote = ch;
                blank_ok = true;
                phase = 2;
                k += 1;
                continue;
            }
        } else if ch == quote {
            phase = 1;
            k += 1;
            continue;
        }
        let mut c = ch;
        if ch == escape {
            match chars.get(k + 1) {
                None => {
                    if phase == 2 {
                        phase = 1;
                    }
                    k += 1;
                    continue;
                }
                Some(&(_, n)) => {
                    c = n;
                    k += 1;
                }
            }
        }
        word.push(c);
        k += 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlagKind {
    Bool,
    Str,
    Strs,
}

/// Flags as BuildKit takes them: each known one once (those that repeat aside), with a value
/// unless it's a switch.
#[derive(Default)]
struct Flags(BTreeMap<&'static str, Vec<String>>);

impl Flags {
    fn str(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(|v| v.last()).map(String::as_str)
    }

    fn strs(&self, k: &str) -> Vec<String> {
        self.0.get(k).cloned().unwrap_or_default()
    }

    fn bool(&self, k: &str) -> Option<bool> {
        self.str(k).map(|v| v == "true")
    }
}

fn parse_flags(words: &[String], known: &[(&'static str, FlagKind)]) -> Result<Flags, String> {
    let mut f = Flags::default();
    for w in words {
        let Some(body) = w.strip_prefix("--") else { return Err(format!("arg should start with -- : {w}")) };
        if body.is_empty() {
            break;
        }
        let (name, value) = match body.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (body, None),
        };
        let Some(&(key, kind)) = known.iter().find(|(k, _)| *k == name) else { return Err(format!("unknown flag: {name}")) };
        if f.0.contains_key(key) && kind != FlagKind::Strs {
            return Err(format!("duplicate flag specified: {name}"));
        }
        let v = match (kind, value) {
            (FlagKind::Bool, Some("")) => return Err(format!("missing a value on flag: {name}")),
            (FlagKind::Bool, v) => match v.unwrap_or_default().to_ascii_lowercase().as_str() {
                "true" | "" => "true".to_owned(),
                "false" => "false".to_owned(),
                other => return Err(format!("expecting boolean value for flag {name}, not: {other}")),
            },
            (_, None) => return Err(format!("missing a value on flag: {name}")),
            (_, Some(v)) => v.to_owned(),
        };
        f.0.entry(key).or_default().push(v);
    }
    Ok(f)
}

/// Splits at runs of spaces and tabs, as BuildKit's reader does for whitespace-separated
/// arguments.
fn whitespace(s: &str) -> Vec<String> {
    s.split(['\t', '\x0b', '\x0c', '\r', ' ']).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

/// Words that may be quoted, their quotes kept for expansion to take out (BuildKit's
/// `parseWords`), for `ARG`, `ENV` and `LABEL`.
fn parse_words(rest: &str, escape: char) -> Vec<String> {
    let chars: Vec<char> = rest.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    // 0: between words; 1: in a word; 2: in quotes.
    let mut phase = 0;
    let mut quote = '\0';
    let mut blank_ok = false;
    let mut k = 0;
    loop {
        let Some(&ch) = chars.get(k) else {
            if phase != 0 && (blank_ok || !word.is_empty()) {
                words.push(word);
            }
            return words;
        };
        if phase == 0 {
            if ch.is_whitespace() {
                k += 1;
                continue;
            }
            phase = 1;
        }
        if phase == 1 {
            if ch.is_whitespace() {
                phase = 0;
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                blank_ok = false;
                k += 1;
                continue;
            }
            if ch == '\'' || ch == '"' {
                quote = ch;
                blank_ok = true;
                phase = 2;
            }
            if ch == escape {
                if k + 1 == chars.len() {
                    k += 1;
                    continue;
                }
                word.push(ch);
                k += 1;
            }
        } else {
            if ch == quote {
                phase = 1;
            }
            if ch == escape && quote != '\'' {
                if k + 1 == chars.len() {
                    phase = 1;
                    k += 1;
                    continue;
                }
                word.push(ch);
                k += 1;
            }
        }
        if let Some(&c) = chars.get(k) {
            word.push(c);
        }
        k += 1;
    }
}

/// `ENV` and `LABEL` arguments: `name=value …`, or the old `name value` with the rest of the line
/// as the value.
fn name_values(rest: &str, kw: &str, escape: char) -> Result<Vec<(String, String)>, String> {
    let words = parse_words(rest, escape);
    let Some(first) = words.first() else { return Err(format!("{kw} requires at least one argument")) };
    if !first.contains('=') {
        let Some(i) = rest.find(['\t', '\x0b', '\x0c', '\r', ' ']) else { return Err(format!("{kw} must have two arguments")) };
        let key = rest.get(..i).unwrap_or_default();
        let value = rest.get(i..).unwrap_or_default().trim_start_matches(['\t', '\x0b', '\x0c', '\r', ' ']);
        return Ok(vec![(key.to_owned(), value.to_owned())]);
    }
    let mut out = Vec::new();
    for w in &words {
        let Some((k, v)) = w.split_once('=') else { return Err(format!("Syntax error - can't find = in {w:?}. Must be of the form: name=value")) };
        if k.is_empty() {
            return Err(format!("{kw} names can not be blank"));
        }
        out.push((k.to_owned(), v.to_owned()));
    }
    Ok(out)
}

/// A JSON list of strings (exec form), or else the line as it is (shell form).
fn maybe_json(rest: &str) -> Result<Line, String> {
    let t = rest.trim_start();
    if t.starts_with('[')
        && let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(t)
    {
        let mut out = Vec::new();
        for v in values {
            match v {
                serde_json::Value::String(s) => out.push(s),
                _ => return Err("when using JSON array syntax, arrays must be comprised of strings only".into()),
            }
        }
        return Ok(Line::Json(out));
    }
    Ok(Line::Shell(rest.to_owned()))
}

/// A JSON list of strings, or else the line's whitespace-separated words.
fn maybe_json_list(rest: &str) -> Result<Vec<String>, String> {
    match maybe_json(rest)? {
        Line::Json(v) => Ok(v),
        Line::Shell(s) => Ok(whitespace(&s)),
    }
}

/// The heredocs kiln's reader found in `text`, in its order and by its rules
/// (`crate::dockerfile`'s `heredocs_in`): each one's word, and whether the word was quoted.
fn heredoc_words(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<<") {
        let after = rest.get(i + 2..).unwrap_or_default();
        let after = after.strip_prefix('-').unwrap_or(after);
        let quoted = after.starts_with(['"', '\'']);
        let word: String = after.trim_start_matches(['"', '\'']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !word.is_empty() && (quoted || word.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')) {
            out.push((word.clone(), quoted));
        }
        rest = after.get(word.len()..).unwrap_or_default();
    }
    out
}

/// An instruction's heredocs: kiln's reader's bodies, with their words.
fn heredocs_of(ins: &Instruction) -> Result<Vec<Heredoc>, String> {
    let words = heredoc_words(&ins.args);
    if words.len() != ins.heredocs.len() {
        return Err(format!("{} heredocs were read for {} openers", ins.heredocs.len(), words.len()));
    }
    Ok(words.into_iter().zip(&ins.heredocs).map(|((name, quoted), data)| Heredoc { name, data: data.clone(), expand: !quoted }).collect())
}

/// BuildKit's `MustParseHeredoc`: when `s` is one heredoc opener and nothing else (`<<EOF`,
/// `<<-"EOF"`, `3<<EOF`), its word, and whether its body is expanded (the word wasn't quoted).
fn lone_heredoc(s: &str) -> Option<(String, bool)> {
    let rest = s.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix("<<")?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let rest = rest.trim_start_matches([' ', '\t', '\n', '\x0c', '\r']);
    if rest.is_empty() || rest.contains('<') {
        return None;
    }
    let lex = Lex { skip_unset: true, ..Lex::new('\\') };
    let plain = lex.words(rest, &|_| None).ok()?;
    let [word] = plain.as_slice() else { return None };
    let raw = Lex { raw_quotes: true, ..lex }.words(rest, &|_| None).ok()?;
    let quotes = |w: &str| w.chars().filter(|c| matches!(c, '\'' | '"')).count();
    let expand = raw.first().is_some_and(|r| quotes(r) == quotes(word));
    Some((word.clone(), expand))
}

/// A `--mount`'s fields, CSV as BuildKit reads them: commas split them, double quotes keep one
/// together (`""` is a quote inside them).
fn csv_fields(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        let mut field = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err(format!("failed to parse csv mounts: unterminated quote in {s:?}")),
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        field.push('"');
                    }
                    Some('"') => break,
                    Some(c) => field.push(c),
                }
            }
            match chars.next() {
                None => {
                    out.push(field);
                    return Ok(out);
                }
                Some(',') => out.push(field),
                Some(c) => return Err(format!("failed to parse csv mounts: {c:?} after a quoted field in {s:?}")),
            }
            continue;
        }
        loop {
            match chars.next() {
                None => {
                    out.push(field);
                    return Ok(out);
                }
                Some(',') => break,
                Some(c) => field.push(c),
            }
        }
        out.push(field);
    }
}

/// A `--mount`'s `from`, as written.
fn mount_from(spec: &str) -> Option<String> {
    csv_fields(spec).ok()?.into_iter().find_map(|f| f.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case("from")).map(|(_, v)| v.to_owned()))
}

// --------------------------------------------------------------------------------- lowering

/// A stage as it's lowered.
#[derive(Debug, Clone)]
struct St {
    platform: Platform,
    /// Its image's OS, which picks the default shell.
    os: String,
    fs: Option<NodeId>,
    config: Option<NodeId>,
    layers: Vec<NodeId>,
    /// Its variables: its image's, `ENV`'s and the `ARG`s in scope.
    env: BTreeMap<String, String>,
    workdir: String,
    user: String,
    shell: Vec<String>,
    /// Whether a `CMD` came in this stage.
    cmd_set: bool,
    /// Its `ONBUILD` triggers, which a stage built on it would run.
    onbuild: Vec<String>,
    failed: Option<String>,
}

impl St {
    fn failed(platform: Platform, why: String) -> Self {
        St { os: platform.os.clone(), platform, fs: None, config: None, layers: Vec::new(), env: BTreeMap::new(), workdir: "/".into(), user: String::new(), shell: Vec::new(), cmd_set: false, onbuild: Vec::new(), failed: Some(why) }
    }

    fn shell(&self) -> Vec<String> {
        if !self.shell.is_empty() {
            return self.shell.clone();
        }
        if self.os == "windows" { vec!["cmd".into(), "/S".into(), "/C".into()] } else { vec!["/bin/sh".into(), "-c".into()] }
    }
}

struct Lowerer<'o> {
    opts: &'o Options,
    lex: Lex,
    graph: Graph,
    /// The global build arguments.
    meta: BTreeMap<String, String>,
    epoch: Option<i64>,
    stages: Vec<St>,
    names: Vec<String>,
    instructions: Vec<Lowered>,
    not_native: Vec<NotNative>,
    unresolved: BTreeSet<ImageRef>,
}

impl Lowerer<'_> {
    fn add(&mut self, op: Op, inputs: Vec<Input>) -> Result<NodeId, String> {
        self.graph.add(op, inputs).map_err(|e| e.to_string())
    }

    fn expand(&self, word: &str, env: &BTreeMap<String, String>) -> Result<String, String> {
        self.lex.word(word, &|k| env.get(k).cloned())
    }

    fn not_native(&mut self, ins: &Instruction, what: String) {
        self.not_native.push(NotNative { line: ins.line, instruction: ins.keyword.clone(), what });
    }

    fn record(&mut self, ins: &Instruction, stage: usize, became: Became) {
        if let Became::Ops(ids) = &became {
            for id in ids {
                self.graph.note(id, ins.line);
            }
        }
        self.instructions.push(Lowered { line: ins.line, keyword: ins.keyword.clone(), stage: Some(stage), became });
    }

    fn label(&self, i: usize) -> String {
        match self.names.get(i) {
            Some(n) if !n.is_empty() => n.clone(),
            _ => format!("stage-{i}"),
        }
    }

    /// Stage `i`; on failure, where (`None` for its `FROM`, else its instruction's place) and why.
    fn stage(&mut self, i: usize, s: &StageSrc<'_>) -> Result<St, (Option<usize>, Error)> {
        let (mut st, became) = self.begin(i, s).map_err(|message| (None, Error { line: s.from.line, message }))?;
        self.record(s.from, i, became);
        for (k, (ins, cmd)) in s.cmds.iter().enumerate() {
            let became = self.dispatch(i, &mut st, ins, cmd).map_err(|message| (Some(k), Error { line: ins.line, message }))?;
            self.record(ins, i, became);
        }
        Ok(st)
    }

    /// A stage's `FROM`: an earlier stage, `scratch`, or an image.
    fn begin(&mut self, i: usize, s: &StageSrc<'_>) -> Result<(St, Became), String> {
        let base = self.expand(&s.base, &self.meta)?;
        if base.is_empty() {
            return Err(format!("base name ({}) should not be blank", s.base));
        }
        let flag_platform = match &s.platform {
            Some(p) => {
                let v = self.expand(p, &self.meta).map_err(|e| format!("failed to process arguments for platform {p}: {e}"))?;
                if v.is_empty() {
                    return Err(format!("empty platform value from expression {p}"));
                }
                Some(Platform::parse(&v).ok_or_else(|| format!("failed to parse platform {v}: give it as os/arch[/variant]"))?)
            }
            None => None,
        };
        // An earlier stage, by its exact name.
        if let Some(j) = self.names.iter().take(i).rposition(|n| !n.is_empty() && *n == base) {
            let b = self.stages.get(j).cloned().ok_or_else(|| format!("stage {base:?} isn't lowered"))?;
            if let Some(why) = &b.failed {
                return Err(format!("stage {base:?} failed: {why}"));
            }
            for t in &b.onbuild {
                self.not_native(s.from, format!("ONBUILD {t} (from stage {base}): kiln doesn't run ONBUILD triggers yet"));
            }
            self.windows(s.from, &b.platform);
            return Ok((St { cmd_set: false, onbuild: Vec::new(), ..b }, Became::Scope));
        }
        // `scratch` is always for the target platform, whatever `--platform` says, as in BuildKit.
        let platform = if base == "scratch" { self.opts.platform.clone() } else { flag_platform.unwrap_or_else(|| self.opts.platform.clone()) };
        self.windows(s.from, &platform);
        if base == "scratch" {
            let mut env = BTreeMap::new();
            if platform.os != "windows" {
                env.insert("PATH".to_owned(), DEFAULT_PATH.to_owned());
            }
            let st = St { os: platform.os.clone(), platform, fs: None, config: None, layers: Vec::new(), env, workdir: "/".into(), user: String::new(), shell: Vec::new(), cmd_set: false, onbuild: Vec::new(), failed: None };
            return Ok((st, Became::Scope));
        }
        let name = ImageName::parse(&base).map_err(|e| format!("failed to parse stage name {base:?}: {e}"))?;
        let r = ImageRef { reference: name.full(), platform: platform.clone() };
        let config = match self.opts.images.get(&r) {
            Some(c) => c.clone(),
            None => {
                self.unresolved.insert(r);
                ImageConfig::default()
            }
        };
        let image = self.add(Op::Image(Image { reference: name.reference(), digest: name.digest.clone().or_else(|| config.digest.clone()), platform: platform.clone() }), Vec::new())?;
        let os = if config.os.is_empty() { platform.os.clone() } else { config.os.clone() };
        let mut env: BTreeMap<String, String> = config.env.iter().map(|e| e.split_once('=').map_or((e.clone(), String::new()), |(k, v)| (k.to_owned(), v.to_owned()))).collect();
        if !env.contains_key("PATH") && os != "windows" {
            env.insert("PATH".to_owned(), DEFAULT_PATH.to_owned());
        }
        for t in &config.on_build {
            self.not_native(s.from, format!("ONBUILD {t} (from image {base}): kiln doesn't run ONBUILD triggers yet"));
        }
        let workdir = if config.working_dir.is_empty() { "/".to_owned() } else { normalize("", &config.working_dir, false) };
        let st = St { os, platform, fs: Some(image), config: Some(image), layers: vec![image], env, workdir, user: config.user, shell: config.shell, cmd_set: false, onbuild: Vec::new(), failed: None };
        Ok((st, Became::Ops(vec![image])))
    }

    fn windows(&mut self, from: &Instruction, platform: &Platform) {
        if platform.os == "windows" {
            self.not_native(from, format!("a {platform} stage: kiln builds Linux images natively, not Windows ones"));
        }
    }

    fn dispatch(&mut self, i: usize, st: &mut St, ins: &Instruction, cmd: &Instr) -> Result<Became, String> {
        let one = |id: NodeId| Became::Ops(vec![id]);
        Ok(match cmd {
            Instr::Arg(pairs) => {
                self.arg(st, pairs)?;
                Became::Scope
            }
            Instr::Env(pairs) => {
                let set = self.pairs(st, pairs, "ENV")?;
                st.env.extend(set.clone());
                one(self.edit(st, ConfigEdit::Env(set))?)
            }
            Instr::Label(pairs) => {
                let set = self.pairs(st, pairs, "LABEL")?;
                one(self.edit(st, ConfigEdit::Label(set))?)
            }
            Instr::Run(r) => Became::Ops(self.run(i, st, ins, r)?),
            Instr::Copy(c) => Became::Ops(self.copy(i, st, ins, c)?),
            Instr::Workdir(p) => Became::Ops(self.workdir(st, p)?),
            Instr::User(u) => {
                let u = self.expand(u, &st.env)?;
                st.user = u.clone();
                one(self.edit(st, ConfigEdit::User(u))?)
            }
            Instr::Expose(raw) => {
                let mut ports = BTreeSet::new();
                for p in raw {
                    for w in self.lex.words(p, &|k| st.env.get(k).cloned())? {
                        ports.extend(parse_port(&w)?);
                    }
                }
                one(self.edit(st, ConfigEdit::Expose(ports))?)
            }
            Instr::Volume(raw) => {
                let mut volumes = BTreeSet::new();
                for v in raw {
                    let v = self.expand(v, &st.env)?;
                    if v.is_empty() {
                        return Err("VOLUME specified can not be an empty string".into());
                    }
                    volumes.insert(v);
                }
                one(self.edit(st, ConfigEdit::Volume(volumes))?)
            }
            Instr::StopSignal(s) => {
                let s = self.expand(s, &st.env)?;
                check_signal(&s)?;
                one(self.edit(st, ConfigEdit::StopSignal(s))?)
            }
            Instr::Cmd(line) => {
                let args = command_args(st, line);
                st.cmd_set = true;
                one(self.edit(st, ConfigEdit::Cmd(args))?)
            }
            Instr::Entrypoint(line) => {
                let args = command_args(st, line);
                let keep_cmd = st.cmd_set;
                one(self.edit(st, ConfigEdit::Entrypoint { args, keep_cmd })?)
            }
            Instr::Shell(shell) => {
                st.shell = shell.clone();
                one(self.edit(st, ConfigEdit::Shell(shell.clone()))?)
            }
            Instr::Healthcheck(h) => one(self.edit(st, ConfigEdit::Healthcheck(h.clone()))?),
            Instr::Maintainer(m) => one(self.edit(st, ConfigEdit::Maintainer(m.clone()))?),
            Instr::Onbuild(t) => {
                st.onbuild.push(t.clone());
                let what = format!("ONBUILD {t}: kiln doesn't run ONBUILD triggers yet");
                self.not_native(ins, what.clone());
                Became::NotNative(what)
            }
        })
    }

    /// An edit of the stage's config, over its config so far.
    fn edit(&mut self, st: &mut St, edit: ConfigEdit) -> Result<NodeId, String> {
        let inputs = st.config.map(|c| vec![Input::new("base", c)]).unwrap_or_default();
        let id = self.add(Op::Config(edit), inputs)?;
        st.config = Some(id);
        Ok(id)
    }

    /// A stage's `ARG`s, as BuildKit takes them: a build argument given wins, else the default
    /// (expanded), else a global `ARG`'s value; with a value, it's a variable from here on.
    fn arg(&self, st: &mut St, pairs: &[(String, Option<String>)]) -> Result<(), String> {
        for (name, default) in pairs {
            let value = match (self.opts.build_args.get(name), default) {
                (Some(v), _) => Some(v.clone()),
                (None, Some(d)) => Some(self.expand(d, &st.env)?),
                (None, None) => self.meta.get(name).cloned(),
            };
            if let Some(v) = value
                && !NON_ENV_ARGS.contains(&name.as_str())
            {
                st.env.insert(name.clone(), v);
            }
        }
        Ok(())
    }

    /// `ENV` or `LABEL` pairs, expanded, every one with the variables as they were before the
    /// instruction.
    fn pairs(&self, st: &St, pairs: &[(String, String)], kw: &str) -> Result<BTreeMap<String, String>, String> {
        let mut set = BTreeMap::new();
        for (k, v) in pairs {
            let k = self.expand(k, &st.env)?;
            if k.is_empty() {
                return Err(format!("{kw} names can not be blank"));
            }
            set.insert(k, self.expand(v, &st.env)?);
        }
        Ok(set)
    }

    /// The main build context.
    fn context(&mut self) -> Result<NodeId, String> {
        self.add(Op::Local(Local { name: self.opts.context.clone(), include: Vec::new(), exclude: self.opts.ignore.clone() }), Vec::new())
    }

    /// An image named by `COPY --from` or a mount's `from`: for the target platform, as BuildKit
    /// resolves it.
    fn implicit_image(&mut self, reference: &str) -> Result<NodeId, String> {
        let name = ImageName::parse(reference).map_err(|e| format!("failed to parse stage name {reference:?}: {e}"))?;
        let platform = self.opts.platform.clone();
        let r = ImageRef { reference: name.full(), platform: platform.clone() };
        let digest = name.digest.clone().or_else(|| self.opts.images.get(&r).and_then(|c| c.digest.clone()));
        if !self.opts.images.contains_key(&r) {
            self.unresolved.insert(r);
        }
        self.add(Op::Image(Image { reference: name.reference(), digest, platform }), Vec::new())
    }

    /// An earlier stage's filesystem (`None`: empty).
    fn stage_tree(&self, j: usize, named: &str) -> Result<Option<NodeId>, String> {
        let s = self.stages.get(j).ok_or_else(|| format!("stage {named:?} isn't lowered"))?;
        match &s.failed {
            Some(why) => Err(format!("stage {named:?} failed: {why}")),
            None => Ok(s.fs),
        }
    }

    fn run(&mut self, i: usize, st: &mut St, ins: &Instruction, r: &RunCmd) -> Result<Vec<NodeId>, String> {
        if !r.devices.is_empty() {
            self.not_native(ins, format!("RUN --device={}: kiln doesn't give builds devices yet", r.devices.join(",")));
        }
        let mut made = Vec::new();
        let mut mounts = Vec::new();
        let mut inputs: Vec<Input> = st.fs.map(|fs| Input::new("rootfs", fs)).into_iter().collect();
        let command = match (&r.line, r.heredocs.as_slice()) {
            (Line::Json(args), []) => Command::Argv(args.clone()),
            (Line::Json(_), _) => return Err("parsing produced an invalid run command: a heredoc with the exec form".into()),
            (Line::Shell(cmd), []) => Command::Shell { shell: st.shell(), script: cmd.clone() },
            (Line::Shell(cmd), docs) => match (lone_heredoc(cmd), docs.first()) {
                (Some(_), Some(doc)) if st.os != "windows" && doc.data.starts_with("#!") => {
                    // A script with its own interpreter: written to a file and run.
                    let path = format!("/{}", doc.name);
                    let file = self.add(Op::Mkfile(Mkfile { path, mode: 0o755, data: doc.data.clone(), owner: None }), Vec::new())?;
                    made.push(file);
                    inputs.push(Input::new(format!("mount.{}", mounts.len()), file).reading(["/".to_owned()]));
                    mounts.push(Mount { target: "/dev/pipes/".into(), kind: MountKind::Bind { source: "/".into(), readonly: true } });
                    Command::Shell { shell: st.shell(), script: format!("/dev/pipes/{}", doc.name) }
                }
                (Some(_), Some(doc)) => Command::Shell { shell: st.shell(), script: doc.data.clone() },
                _ => {
                    // The heredocs put back after the line, for the shell to read.
                    let mut full = cmd.clone();
                    for d in docs {
                        full.push('\n');
                        full.push_str(&d.data);
                        full.push_str(&d.name);
                    }
                    Command::Shell { shell: st.shell(), script: full }
                }
            },
        };
        for spec in &r.mounts {
            let env = st.env.clone();
            let m = parse_mount(spec, &|w| self.expand(w, &env))?;
            let role = format!("mount.{}", mounts.len());
            let kind = match m.kind {
                MountType::Secret => {
                    let mut id = if m.source.is_empty() { m.id.clone() } else { m.source.clone() };
                    if id.is_empty() {
                        if m.target.is_empty() {
                            return Err("one of source, target required".into());
                        }
                        id = base(&m.target);
                    }
                    let target = match (&m.env, m.target.is_empty()) {
                        (None, true) => format!("/run/secrets/{}", base(&id)),
                        _ => m.target.clone(),
                    };
                    mounts.push(Mount { target, kind: MountKind::Secret { id, env: m.env.clone(), required: m.required, mode: m.mode, uid: m.uid, gid: m.gid } });
                    continue;
                }
                MountType::Ssh => {
                    if !m.source.is_empty() {
                        return Err("ssh does not support source".into());
                    }
                    let id = if m.id.is_empty() { "default".to_owned() } else { m.id.clone() };
                    mounts.push(Mount { target: m.target.clone(), kind: MountKind::Ssh { id, required: m.required, mode: m.mode, uid: m.uid, gid: m.gid } });
                    continue;
                }
                MountType::Tmpfs => MountKind::Tmpfs { size: m.size },
                MountType::Bind => MountKind::Bind { source: join(&["/", &m.source]), readonly: m.readonly },
                MountType::Cache => MountKind::Cache {
                    id: if m.id.is_empty() { clean(&m.target) } else { m.id.clone() },
                    sharing: m.sharing.unwrap_or(Sharing::Shared),
                    source: join(&["/", &m.source]),
                    readonly: m.readonly,
                    mode: m.mode,
                    uid: m.uid,
                    gid: m.gid,
                },
            };
            // Where its tree comes from: the context for a bind mount without `from`, nothing
            // for a cache without one, else the stage or image `from` names.
            let tree = match (&m.kind, m.from.as_str()) {
                (MountType::Tmpfs, _) | (MountType::Cache, "") | (_, "scratch") => None,
                (_, "") => {
                    let c = self.context()?;
                    made.push(c);
                    Some(c)
                }
                (_, from) => match find_stage(&self.names, from) {
                    Some(j) if j >= i => return Err(format!("cannot mount from stage {from:?} to {:?}, stage needs to be defined before current command", m.target)),
                    Some(j) => self.stage_tree(j, from)?,
                    None => {
                        let img = self.implicit_image(from)?;
                        made.push(img);
                        Some(img)
                    }
                },
            };
            let target = if clean(&m.target).starts_with('/') { m.target.clone() } else { join(&["/", &st.workdir, &m.target]) };
            if target == "/" {
                return Err(format!("invalid mount target {target:?}"));
            }
            if let Some(t) = tree {
                let source = join(&["/", &m.source]);
                inputs.push(Input::new(role, t).reading([source]));
            }
            mounts.push(Mount { target, kind });
        }
        let exec = Exec { command, env: st.env.clone(), cwd: st.workdir.clone(), user: st.user.clone(), mounts, network: r.network, security: r.security, platform: st.platform.clone(), proxy: proxy(self.opts) };
        let id = self.add(Op::Exec(exec), inputs)?;
        st.fs = Some(id);
        st.layers.push(id);
        made.push(id);
        Ok(dedup(made))
    }

    fn copy(&mut self, i: usize, st: &mut St, ins: &Instruction, c: &CopyCmd) -> Result<Vec<NodeId>, String> {
        let kw = if c.add { "ADD" } else { "COPY" };
        let mut made = Vec::new();
        // Where its local sources come from: `--from` isn't expanded, as BuildKit refuses it.
        enum Origin {
            Context,
            Tree(Option<NodeId>),
        }
        let from = match &c.from {
            None => Origin::Context,
            Some(v) => {
                if self.lex.word(v, &|_| None)? != *v {
                    return Err("variable expansion is not supported for --from, define a new stage with FROM using ARG from global scope as a workaround".into());
                }
                let here = self.label(i);
                let stage = match v.parse::<i64>() {
                    Ok(n) => match usize::try_from(n).ok().filter(|n| *n < self.names.len()) {
                        Some(j) => Some(j),
                        None => return Err(format!("invalid stage index {n}")),
                    },
                    Err(_) => find_stage(&self.names, v),
                };
                match stage {
                    Some(j) if j >= i => return Err(format!("cannot copy from stage {v:?}, it needs to be defined before current stage {here:?}")),
                    Some(j) => Origin::Tree(self.stage_tree(j, v)?),
                    None => {
                        let img = self.implicit_image(v)?;
                        made.push(img);
                        Origin::Tree(Some(img))
                    }
                }
            }
        };
        let env = st.env.clone();
        let chown = self.expand(&c.chown, &env)?;
        let chmod = self.expand(&c.chmod, &env)?;
        let checksum = if c.add { self.expand(&c.checksum, &env)? } else { c.checksum.clone() };
        let paths = c.paths.iter().map(|p| self.expand(p, &env)).collect::<Result<Vec<_>, _>>()?;
        let dest = relative_to(&st.workdir, &self.expand(&c.dest, &env)?);
        if !chown.is_empty() && c.link && chown_by_name(&chown) {
            return Err(format!("--chown={chown}: user and group names can't be resolved when used with --link, use numeric uid[:gid] instead"));
        }
        check_chmod(&chmod)?;
        if !checksum.is_empty() {
            if !c.add {
                return Err("checksum can't be specified for COPY".into());
            }
            if paths.len() != 1 {
                return Err("checksum can't be specified for multiple sources".into());
            }
            if !paths.iter().all(|p| is_http(p) || matches!(git_ref(p), Ok(Some(_)))) {
                return Err("checksum requires HTTP(S) or Git sources".into());
            }
        }
        let mut sources = Vec::new();
        let mut inputs = Vec::new();
        let source = |path: String| CopySource { path, follow_symlinks: false, contents_only: false, wildcard: false, unpack: false, include: Vec::new(), required: Vec::new() };
        for src in &paths {
            let role = format!("src.{}", sources.len());
            if let Some(g) = git_ref(src)? {
                if !c.add {
                    return Err("source can't be a git ref for COPY".into());
                }
                if let (Some(flag), Some(url)) = (c.keep_git_dir, g.keep_git_dir)
                    && flag != url
                {
                    return Err("inconsistent keep-git-dir configuration".into());
                }
                let checksum = match (checksum.as_str(), &g.checksum) {
                    ("", sum) => sum.clone(),
                    (flag, Some(url)) if flag != url => return Err(format!("checksum mismatch {flag:?} != {url:?}")),
                    (flag, _) => Some(flag.to_owned()),
                };
                for q in &g.unsupported {
                    self.not_native(ins, format!("{kw} {src}: the git query `{q}` isn't native yet"));
                }
                let git = self.add(Op::Git(Git { url: g.remote.clone(), reference: g.reference.clone(), subdir: g.subdir.clone(), checksum, keep_git_dir: g.keep_git_dir.or(c.keep_git_dir).unwrap_or(false), submodules: g.submodules.unwrap_or(true) }), Vec::new())?;
                made.push(git);
                inputs.push(Input::new(role, git));
                sources.push(source("/".into()));
            } else if is_http(src) {
                if !c.add {
                    return Err("source can't be a URL for COPY".into());
                }
                let checksum = if checksum.is_empty() {
                    None
                } else {
                    check_digest(&checksum)?;
                    Some(checksum.clone())
                };
                let filename = url_filename(src);
                let http = self.add(Op::Http(Http { url: src.clone(), checksum, filename: filename.clone() }), Vec::new())?;
                made.push(http);
                inputs.push(Input::new(role, http).reading([format!("/{filename}")]));
                sources.push(CopySource { unpack: c.unpack.unwrap_or(false), ..source(filename) });
            } else {
                let (mut path, mut include, mut required) = (src.clone(), Vec::new(), Vec::new());
                if c.parents {
                    let (parent, pattern) = match src.split_once("/./") {
                        Some((p, pat)) => (p.to_owned(), pat.to_owned()),
                        None => ("/".to_owned(), src.clone()),
                    };
                    let pattern = normalize("/", &pattern, false);
                    if !wildcards(&parent) && !wildcards(&pattern) {
                        required.push(normalize("/", &join(&[&parent, &pattern]), false));
                    }
                    include.push(pattern.strip_prefix('/').unwrap_or(&pattern).to_owned());
                    path = parent;
                }
                let path = normalize("/", &path, false);
                let tree = match from {
                    Origin::Context => {
                        let ctx = self.context()?;
                        made.push(ctx);
                        Some(ctx)
                    }
                    Origin::Tree(t) => t,
                };
                if let Some(t) = tree {
                    inputs.push(Input::new(role, t).reading([path.clone()]));
                }
                sources.push(CopySource { path, follow_symlinks: true, contents_only: true, wildcard: true, unpack: c.unpack.unwrap_or(c.add), include, required });
            }
        }
        for doc in &c.docs {
            let data = if doc.expand { Lex { skip_quotes: true, ..Lex::new('\\') }.word(&doc.data, &|k| env.get(k).cloned())? } else { doc.data.clone() };
            let path = format!("/{}", doc.name);
            let file = self.add(Op::Mkfile(Mkfile { path: path.clone(), mode: 0o644, data, owner: None }), Vec::new())?;
            made.push(file);
            inputs.push(Input::new(format!("src.{}", sources.len()), file));
            sources.push(source(path));
        }
        let op = Op::Copy(CopyFiles { dest, sources, chown: Some(chown).filter(|s| !s.is_empty()), chmod: Some(chmod.clone()).filter(|s| !s.is_empty()), exclude: c.exclude.clone() });
        // `--link` (without `--chmod`, as BuildKit has it): copied onto nothing, then laid over the
        // stage, so its layer doesn't depend on what's under it.
        if c.link && chmod.is_empty() {
            let copied = self.add(op, inputs)?;
            made.push(copied);
            st.layers.push(copied);
            let head = match st.fs {
                Some(fs) => {
                    let merged = self.add(Op::Merge, vec![Input::new("input.0", fs), Input::new("input.1", copied)])?;
                    made.push(merged);
                    merged
                }
                None => copied,
            };
            st.fs = Some(head);
        } else {
            let mut all: Vec<Input> = st.fs.map(|fs| Input::new("dest", fs)).into_iter().collect();
            all.extend(inputs);
            let copied = self.add(op, all)?;
            made.push(copied);
            st.layers.push(copied);
            st.fs = Some(copied);
        }
        Ok(dedup(made))
    }

    fn workdir(&mut self, st: &mut St, path: &str) -> Result<Vec<NodeId>, String> {
        let p = self.expand(path, &st.env)?;
        let wd = normalize(&st.workdir, &p, false);
        st.workdir = wd.clone();
        let mut made = Vec::new();
        if wd != "/" {
            let owner = Some(st.user.clone()).filter(|u| !u.is_empty());
            let inputs = st.fs.map(|fs| vec![Input::new("dest", fs)]).unwrap_or_default();
            let mk = self.add(Op::Mkdir(Mkdir { path: wd.clone(), mode: 0o755, parents: true, owner, created: self.epoch }), inputs)?;
            st.fs = Some(mk);
            st.layers.push(mk);
            made.push(mk);
        }
        made.push(self.edit(st, ConfigEdit::Workdir(wd))?);
        Ok(made)
    }
}

/// `ids` in order, each once.
fn dedup(ids: Vec<NodeId>) -> Vec<NodeId> {
    let mut seen = BTreeSet::new();
    ids.into_iter().filter(|id| seen.insert(*id)).collect()
}

/// A `CMD` or `ENTRYPOINT`'s arguments: a shell form's run by the stage's shell.
fn command_args(st: &St, line: &Line) -> Vec<String> {
    match line {
        Line::Json(v) => v.clone(),
        Line::Shell(s) => st.shell().into_iter().chain([s.clone()]).collect(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountType {
    Bind,
    Cache,
    Tmpfs,
    Secret,
    Ssh,
}

impl MountType {
    fn name(self) -> &'static str {
        match self {
            MountType::Bind => "bind",
            MountType::Cache => "cache",
            MountType::Tmpfs => "tmpfs",
            MountType::Secret => "secret",
            MountType::Ssh => "ssh",
        }
    }
}

/// A `--mount`, read as BuildKit reads it (in its fields' order: a field that depends on the type
/// must come after `type`).
struct MountSpec {
    kind: MountType,
    from: String,
    source: String,
    target: String,
    readonly: bool,
    size: Option<u64>,
    id: String,
    sharing: Option<Sharing>,
    required: bool,
    env: Option<String>,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

fn parse_mount(spec: &str, expand: &dyn Fn(&str) -> Result<String, String>) -> Result<MountSpec, String> {
    let mut m = MountSpec { kind: MountType::Bind, from: String::new(), source: String::new(), target: String::new(), readonly: false, size: None, id: String::new(), sharing: None, required: false, env: None, mode: None, uid: None, gid: None };
    let mut ro_auto = true;
    for field in csv_fields(spec)? {
        let Some((key, value)) = field.split_once('=') else {
            match field.to_ascii_lowercase().as_str() {
                "readonly" | "ro" => {
                    m.readonly = true;
                    ro_auto = false;
                }
                "readwrite" | "rw" => {
                    m.readonly = false;
                    ro_auto = false;
                }
                "required" if matches!(m.kind, MountType::Secret | MountType::Ssh) => m.required = true,
                "required" => return Err(format!("unexpected key 'required' for mount type '{}'", m.kind.name())),
                _ => return Err(format!("invalid field '{field}' must be a key=value pair")),
            }
            continue;
        };
        let key = key.to_ascii_lowercase();
        let value = expand(value)?;
        let bad = || format!("invalid value for {key}: {value}");
        match key.as_str() {
            "type" => {
                m.kind = match value.to_ascii_lowercase().as_str() {
                    "bind" => MountType::Bind,
                    "cache" => MountType::Cache,
                    "tmpfs" => MountType::Tmpfs,
                    "secret" => MountType::Secret,
                    "ssh" => MountType::Ssh,
                    _ => return Err(format!("unsupported mount type {value:?}")),
                }
            }
            "from" => m.from = value,
            "source" | "src" => m.source = value,
            "target" | "dst" | "destination" => m.target = value,
            "readonly" | "ro" => {
                m.readonly = parse_bool(&value).ok_or_else(bad)?;
                ro_auto = false;
            }
            "readwrite" | "rw" => {
                m.readonly = !parse_bool(&value).ok_or_else(bad)?;
                ro_auto = false;
            }
            "required" if matches!(m.kind, MountType::Secret | MountType::Ssh) => m.required = parse_bool(&value).ok_or_else(bad)?,
            "size" if m.kind == MountType::Tmpfs => m.size = Some(ram_in_bytes(&value).ok_or_else(bad)?),
            "required" | "size" => return Err(format!("unexpected key '{key}' for mount type '{}'", m.kind.name())),
            "id" => m.id = value,
            "sharing" => {
                m.sharing = Some(match value.to_ascii_lowercase().as_str() {
                    "shared" => Sharing::Shared,
                    "private" => Sharing::Private,
                    "locked" => Sharing::Locked,
                    _ => return Err(format!("unsupported sharing value {value:?}")),
                })
            }
            "mode" => m.mode = Some(u32::from_str_radix(&value, 8).map_err(|_| format!("invalid value {value} for mode"))?),
            "uid" => m.uid = Some(value.parse().map_err(|_| format!("invalid value {value} for uid"))?),
            "gid" => m.gid = Some(value.parse().map_err(|_| format!("invalid value {value} for gid"))?),
            "env" => m.env = Some(value),
            _ => return Err(format!("unexpected key '{key}' in '{field}'")),
        }
    }
    if !matches!(m.kind, MountType::Secret | MountType::Ssh | MountType::Cache) {
        for (set, what) in [(m.mode.is_some(), "mode"), (m.uid.is_some(), "uid"), (m.gid.is_some(), "gid")] {
            if set {
                return Err(format!("{what} not allowed for {:?} type mounts", m.kind.name()));
            }
        }
    }
    if ro_auto {
        m.readonly = !matches!(m.kind, MountType::Cache | MountType::Tmpfs);
    }
    if m.kind == MountType::Secret {
        if !m.from.is_empty() {
            return Err("secret mount should not have a from".into());
        }
        if m.sharing.is_some() {
            return Err("secret mount should not define sharing".into());
        }
        if m.source.is_empty() && m.target.is_empty() && m.id.is_empty() {
            return Err("invalid secret mount. one of source, target required".into());
        }
        if !m.source.is_empty() && !m.id.is_empty() {
            return Err("both source and id can't be set".into());
        }
    }
    if m.sharing.is_some() && m.kind != MountType::Cache {
        return Err(format!("invalid cache sharing set for {} mount", m.kind.name()));
    }
    Ok(m)
}

/// A size such as `64m` or `1.5g`, in bytes, the units binary (docker/go-units' `RAMInBytes`).
fn ram_in_bytes(s: &str) -> Option<u64> {
    let num_len = s.bytes().take_while(|b| b.is_ascii_digit() || *b == b'.').count();
    let num = s.get(..num_len)?;
    if num.is_empty() || num.starts_with('.') || num.ends_with('.') || num.contains("..") {
        return None;
    }
    let mut rest = s.get(num_len..)?.chars().peekable();
    if rest.peek() == Some(&' ') {
        rest.next();
    }
    let mult: u64 = match rest.peek().map(char::to_ascii_lowercase) {
        Some('k') => 1 << 10,
        Some('m') => 1 << 20,
        Some('g') => 1 << 30,
        Some('t') => 1 << 40,
        Some('p') => 1 << 50,
        _ => 1,
    };
    if mult > 1 {
        rest.next();
    }
    if rest.peek().is_some_and(|c| c.eq_ignore_ascii_case(&'i')) {
        rest.next();
    }
    if rest.peek().is_some_and(|c| c.eq_ignore_ascii_case(&'b')) {
        rest.next();
    }
    if rest.next().is_some() {
        return None;
    }
    let value: f64 = num.parse().ok()?;
    let bytes = value * mult as f64;
    (0.0..1.8e19).contains(&bytes).then_some(bytes as u64)
}

/// `EXPOSE`'s ports, as `port/protocol` (a range is each of its ports).
fn parse_port(raw: &str) -> Result<Vec<String>, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (ip, host, container) = match parts.as_slice() {
        [c] => ("", "", *c),
        [h, c] => ("", *h, *c),
        [ip, h, c] => (*ip, *h, *c),
        [.., h, c] => (raw.get(..raw.len().saturating_sub(h.len() + c.len() + 2)).unwrap_or_default(), *h, *c),
        [] => ("", "", ""),
    };
    let (port, proto) = container.split_once('/').unwrap_or((container, ""));
    if port.is_empty() {
        return Err(format!("invalid port: {raw:?}: no port specified"));
    }
    let proto = match proto.to_ascii_lowercase().as_str() {
        "" => "tcp".to_owned(),
        p @ ("tcp" | "udp" | "sctp") => p.to_owned(),
        _ => return Err(format!("invalid port: {raw:?}: invalid proto: {proto}")),
    };
    if !ip.is_empty() && ip.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_err() {
        return Err(format!("invalid IP address: {ip}"));
    }
    let range = |p: &str| -> Result<(u32, u32), String> {
        let num = |n: &str| n.parse::<u32>().ok().filter(|n| *n <= 65_535).ok_or_else(|| format!("invalid containerPort: {p}"));
        match p.split_once('-') {
            None => num(p).map(|n| (n, n)),
            Some((a, b)) if a == b => num(a).map(|n| (n, n)),
            Some((a, b)) => {
                let (a, b) = (num(a)?, num(b)?);
                if b < a { Err(format!("invalid port range: {p}")) } else { Ok((a, b)) }
            }
        }
    };
    let (start, end) = range(port)?;
    if !host.is_empty() {
        let (hs, he) = range(host).map_err(|_| format!("invalid hostPort: {host}"))?;
        if end - start != he - hs && end != start {
            return Err(format!("invalid ranges specified for container and host Ports: {port} and {host}"));
        }
    }
    Ok((start..=end).map(|p| format!("{p}/{proto}")).collect())
}

/// Signals as moby's `ParseSignal` takes them: a number, or a name with or without `SIG`.
fn check_signal(s: &str) -> Result<(), String> {
    const NAMES: &[&str] = &[
        "ABRT", "ALRM", "BUS", "CHLD", "CLD", "CONT", "FPE", "HUP", "ILL", "INT", "IO", "IOT", "KILL", "PIPE", "POLL", "PROF", "PWR", "QUIT", "SEGV", "STKFLT", "STOP", "SYS", "TERM", "TRAP", "TSTP", "TTIN", "TTOU", "URG", "USR1", "USR2", "VTALRM", "WINCH", "XCPU", "XFSZ", "RTMIN", "RTMAX",
    ];
    if let Ok(n) = s.parse::<i64>() {
        return if n == 0 { Err(format!("invalid signal: {s}")) } else { Ok(()) };
    }
    let up = s.to_ascii_uppercase();
    let name = up.strip_prefix("SIG").unwrap_or(&up);
    let realtime = |prefix: &str, max: u32| name.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok()).is_some_and(|n| (1..=max).contains(&n));
    if NAMES.contains(&name) || realtime("RTMIN+", 15) || realtime("RTMAX-", 14) { Ok(()) } else { Err(format!("invalid signal: {s}")) }
}

/// `--chmod`: octal up to `07777`, or symbolic (`u+x`, `go=rx`).
fn check_chmod(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Ok(());
    }
    let octal_err = || format!("invalid chmod parameter: '{s}'. it should be octal string and between 0 and 07777");
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return match u32::from_str_radix(s, 8) {
            Ok(n) if n <= 0o7777 => Ok(()),
            _ => Err(octal_err()),
        };
    }
    let symbolic = s.split(',').all(|clause| {
        let rest = clause.trim_start_matches(['u', 'g', 'o', 'a']);
        let mut chars = rest.chars().peekable();
        let mut any = false;
        while let Some(op) = chars.next() {
            if !matches!(op, '+' | '-' | '=') {
                return false;
            }
            any = true;
            if matches!(chars.peek(), Some('u' | 'g' | 'o')) {
                chars.next();
                continue;
            }
            while matches!(chars.peek(), Some('r' | 'w' | 'x' | 'X' | 's' | 't')) {
                chars.next();
            }
        }
        any
    });
    if symbolic { Ok(()) } else { Err(format!("invalid chmod parameter: '{s}'")) }
}

/// Whether `--chown` names a user or group rather than numbering it, which `--link` can't resolve.
fn chown_by_name(chown: &str) -> bool {
    let (user, group) = chown.split_once(':').unwrap_or((chown, ""));
    [user, group].iter().any(|p| !p.is_empty() && !p.bytes().all(|b| b.is_ascii_digit()))
}

/// An HTTP checksum, as go-digest takes one.
fn check_digest(d: &str) -> Result<(), String> {
    let (algo, hex) = d.split_once(':').ok_or("invalid checksum digest format")?;
    let len = match algo {
        "sha256" => 64,
        "sha384" => 96,
        "sha512" => 128,
        _ => return Err("unsupported digest algorithm".into()),
    };
    if hex.len() != len || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err("invalid checksum digest length".into());
    }
    Ok(())
}

fn wildcards(s: &str) -> bool {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' | '[' => return true,
            '\\' => {
                chars.next();
            }
            _ => {}
        }
    }
    false
}

/// An HTTP source that isn't a git repository.
fn is_http(src: &str) -> bool {
    (src.starts_with("http://") || src.starts_with("https://")) && !matches!(git_ref(src), Ok(Some(_)))
}

/// The name a fetched file gets: its URL path's last part, as BuildKit names it.
fn url_filename(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, r)| r);
    let path = after_scheme.find('/').and_then(|i| after_scheme.get(i..)).unwrap_or_default();
    let path = path.split(['?', '#']).next().unwrap_or_default();
    match base(path).as_str() {
        "." | "/" => "__unnamed__".into(),
        b => b.to_owned(),
    }
}

/// A git source, as BuildKit's `ParseGitRef` reads one.
struct GitRef {
    remote: String,
    reference: String,
    subdir: String,
    checksum: Option<String>,
    keep_git_dir: Option<bool>,
    submodules: Option<bool>,
    /// Queries kiln doesn't do yet.
    unsupported: Vec<String>,
}

/// `Ok(None)` when `s` isn't a git source; an error when it is one, badly written.
fn git_ref(s: &str) -> Result<Option<GitRef>, String> {
    if s.starts_with("./") || s.starts_with("../") || s.starts_with("github.com/") {
        return Ok(None);
    }
    let proto_len = s.bytes().take_while(u8::is_ascii_alphanumeric).count();
    let (remote, fragment, query) = if proto_len > 0 && s.get(proto_len..).is_some_and(|r| r.starts_with("://")) {
        let scheme = s.get(..proto_len).unwrap_or_default().to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "git" | "ssh") {
            return Ok(None);
        }
        let (no_fragment, fragment) = s.split_once('#').unwrap_or((s, ""));
        let (url, query) = no_fragment.split_once('?').unwrap_or((no_fragment, ""));
        if matches!(scheme.as_str(), "http" | "https") {
            let after = url.get(proto_len + 3..).unwrap_or_default();
            let path = after.find('/').and_then(|i| after.get(i..)).unwrap_or_default();
            if !path.ends_with(".git") {
                return Ok(None);
            }
        }
        (url.to_owned(), fragment, query)
    } else {
        // `user@host:path`, as scp writes it.
        let Some((user, rest)) = s.split_once('@') else { return Ok(None) };
        let Some((host, _)) = rest.split_once(':') else { return Ok(None) };
        let user_ok = !user.is_empty() && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        let host_ok = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if !user_ok || !host_ok {
            return Ok(None);
        }
        let (url, fragment) = s.split_once('#').unwrap_or((s, ""));
        (url.to_owned(), fragment, "")
    };
    let (reference, subdir) = fragment.split_once(':').unwrap_or((fragment, ""));
    let mut g = GitRef { remote, reference: reference.to_owned(), subdir: subdir.to_owned(), checksum: None, keep_git_dir: None, submodules: None, unsupported: Vec::new() };
    if !query.is_empty() {
        git_query(&mut g, query)?;
    }
    Ok(Some(g))
}

fn git_query(g: &mut GitRef, query: &str) -> Result<(), String> {
    let mut values: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for kv in query.split('&').filter(|kv| !kv.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        values.entry(k).or_default().push(v);
    }
    let (mut tag, mut branch) = (String::new(), String::new());
    for (k, v) in values {
        let [v] = v.as_slice() else { return Err(format!("query {k:?} has multiple values")) };
        let switch = |v: &str, what: &str| if v.is_empty() { Ok(true) } else { parse_bool(v).ok_or_else(|| format!("invalid {what} value: {v:?}")) };
        if v.is_empty() && !matches!(k, "submodules" | "keep-git-dir" | "fetch-by-commit") {
            return Err(format!("query {k:?} has no value"));
        }
        match k {
            "ref" => {
                if !g.reference.is_empty() && g.reference != *v {
                    return Err(format!("ref conflicts: {:?} vs {v:?}", g.reference));
                }
                g.reference = (*v).to_owned();
            }
            "tag" => tag = (*v).to_owned(),
            "branch" => branch = (*v).to_owned(),
            "subdir" => {
                if !g.subdir.is_empty() && g.subdir != *v {
                    return Err(format!("subdir conflicts: {:?} vs {v:?}", g.subdir));
                }
                g.subdir = (*v).to_owned();
            }
            "checksum" | "commit" => g.checksum = Some((*v).to_owned()),
            "keep-git-dir" => g.keep_git_dir = Some(switch(v, "keep-git-dir")?),
            "submodules" => g.submodules = Some(switch(v, "submodules")?),
            "mtime" => {
                if !matches!(*v, "checkout" | "commit") {
                    return Err(format!("invalid mtime value: {v:?} (must be \"checkout\" or \"commit\")"));
                }
                g.unsupported.push(format!("mtime={v}"));
            }
            "fetch-by-commit" => {
                if switch(v, "fetch-by-commit")? {
                    g.unsupported.push("fetch-by-commit".into());
                }
            }
            _ => return Err(format!("unexpected query {k:?}")),
        }
    }
    if !tag.is_empty() {
        let tag = if tag.starts_with("refs/tags/") { tag } else { format!("refs/tags/{tag}") };
        if !g.reference.is_empty() && g.reference != tag {
            return Err(format!("ref conflicts: {:?} vs {tag:?}", g.reference));
        }
        g.reference = tag;
    }
    if !branch.is_empty() {
        if g.reference.starts_with("refs/tags/") {
            return Err("branch conflicts with tag".into());
        }
        let branch = if branch.starts_with("refs/heads/") { branch } else { format!("refs/heads/{branch}") };
        if !g.reference.is_empty() && g.reference != branch {
            return Err(format!("ref conflicts: {:?} vs {branch:?}", g.reference));
        }
        g.reference = branch;
    }
    Ok(())
}

/// An image name, normalized as Docker normalizes one (`alpine` is
/// `docker.io/library/alpine`).
struct ImageName {
    /// Its registry and path.
    name: String,
    tag: Option<String>,
    digest: Option<String>,
}

impl ImageName {
    fn parse(s: &str) -> Result<ImageName, String> {
        if s.is_empty() || s.contains(char::is_whitespace) {
            return Err("invalid reference format".into());
        }
        if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid repository name ({s}), cannot specify 64-byte hexadecimal strings"));
        }
        let (domain, remainder) = match s.split_once('/') {
            Some((first, rest)) if first.contains(['.', ':']) || first == "localhost" || first.to_ascii_lowercase() != first => (first, rest),
            _ => ("docker.io", s),
        };
        let domain = if domain == "index.docker.io" { "docker.io" } else { domain };
        let (rest, digest) = match remainder.split_once('@') {
            Some((r, d)) => (r, Some(d)),
            None => (remainder, None),
        };
        let (path, tag) = match rest.rsplit_once(':') {
            Some((p, t)) => (p, Some(t)),
            None => (rest, None),
        };
        if path.to_ascii_lowercase() != path {
            return Err(format!("invalid reference format: repository name ({path}) must be lowercase"));
        }
        let path = if domain == "docker.io" && !path.contains('/') { format!("library/{path}") } else { path.to_owned() };
        let domain_ok = !domain.is_empty() && domain.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
        if !domain_ok || !path.split('/').all(component) {
            return Err("invalid reference format".into());
        }
        if let Some(t) = tag {
            let mut b = t.bytes();
            let ok = t.len() <= 128 && b.next().is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'));
            if !ok {
                return Err("invalid reference format".into());
            }
        }
        if let Some(d) = digest {
            let (algo, hex) = d.split_once(':').unwrap_or_default();
            let ok = algo.bytes().next().is_some_and(|c| c.is_ascii_alphabetic()) && algo.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'+' | b'.')) && hex.len() >= 32 && hex.bytes().all(|c| c.is_ascii_hexdigit());
            if !ok {
                return Err("invalid reference format".into());
            }
        }
        Ok(ImageName { name: format!("{domain}/{path}"), tag: tag.map(str::to_owned), digest: digest.map(str::to_owned) })
    }

    /// Its name and tag; `:latest` when nothing pins it.
    fn reference(&self) -> String {
        match (&self.tag, &self.digest) {
            (Some(t), _) => format!("{}:{t}", self.name),
            (None, Some(_)) => self.name.clone(),
            (None, None) => format!("{}:latest", self.name),
        }
    }

    /// Its reference and digest.
    fn full(&self) -> String {
        match &self.digest {
            Some(d) => format!("{}@{d}", self.reference()),
            None => self.reference(),
        }
    }
}

/// A path component of an image name: lower-case letters and digits, separated by `.`, `_`, `__`
/// or runs of `-`.
fn component(c: &str) -> bool {
    let b = c.as_bytes();
    let alnum = |x: &u8| x.is_ascii_lowercase() || x.is_ascii_digit();
    if !b.first().is_some_and(alnum) || !b.last().is_some_and(alnum) {
        return false;
    }
    let mut sep = String::new();
    for x in b {
        if alnum(x) {
            if !(sep.is_empty() || sep == "." || sep == "_" || sep == "__" || sep.bytes().all(|s| s == b'-')) {
                return false;
            }
            sep.clear();
        } else if matches!(x, b'.' | b'_' | b'-') {
            sep.push(char::from(*x));
        } else {
            return false;
        }
    }
    true
}

// ------------------------------------------------------------------------------------ paths

/// Go's `path.Clean`.
fn clean(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let rooted = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let joined = parts.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// Go's `path.Join`: the non-empty parts, joined and cleaned.
fn join(parts: &[&str]) -> String {
    let parts: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return String::new();
    }
    clean(&parts.join("/"))
}

/// Go's `path.Base`.
fn base(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return "/".into();
    }
    p.rsplit('/').next().unwrap_or(p).to_owned()
}

/// BuildKit's `NormalizePath`: `new` against `parent` when it's relative (cleaned then), as it is
/// when it's absolute; with `keep_slash`, a trailing `/` or `/.` stays.
fn normalize(parent: &str, new: &str, keep_slash: bool) -> String {
    let parent = if parent.is_empty() { "/" } else { parent };
    let parent = if parent.starts_with('/') { parent.to_owned() } else { join(&["/", parent]) };
    let mut p = if new.is_empty() { parent.clone() } else { new.to_owned() };
    if !p.starts_with('/') {
        p = join(&[&parent, &p]);
    }
    if keep_slash {
        if new.ends_with('/') && !p.ends_with('/') {
            p.push('/');
        } else if new.ends_with("/.") {
            if p != "/" {
                p.push('/');
            }
            p.push('.');
        }
    }
    p
}

/// A `COPY` destination against the working folder.
fn relative_to(dir: &str, p: &str) -> String {
    if p.starts_with('/') {
        return normalize("/", p, true);
    }
    normalize(dir, if p == "." || p.is_empty() { "./" } else { p }, true)
}
