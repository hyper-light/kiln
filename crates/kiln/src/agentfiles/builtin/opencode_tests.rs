//! opencode's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 22; plan
//! 01a1ce705e2f7773af54b299f1de42f5, step 2, whose research 01a1ce705e307593bd11f26bf9f7b9a9 the fN
//! are findings of): what its pin, its harness.json and its Agentfile say of one another, of kiln's Kind
//! `opencode`, and of credentials. Each check reads texts, so each test also hands it changed copies,
//! and each change must fail it.

use serde_json::Value;

use super::{ALL, Builtin};
use crate::agentfiles::unpack::{Member, Spec};
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::session::Spec as Session;

/// The repository whose release workflow builds opencode's archives and signs their provenance (pin.sh's
/// REPO and SIGNER, f9), and the provenance's predicate.
const REPO: &str = "anomalyco/opencode";
const SIGNER: &str = "anomalyco/opencode/.github/workflows/release.yml";
const SLSA: &str = "https://slsa.dev/provenance/v1";

/// harness.json's env: its HOME the person's volume, whole (step 18's rule for every built-in: what the
/// programs it runs keep in a home lands there too); opencode's four XDG roots beneath it, where it keeps
/// its settings, its sessions and sign-in (opencode.db) and its cache (f17, f19); and its update check
/// off (f20).
const ENV: [&str; 6] = [
    "HOME=/home/opencode",
    "XDG_CONFIG_HOME=/home/opencode/.config",
    "XDG_DATA_HOME=/home/opencode/.local/share",
    "XDG_STATE_HOME=/home/opencode/.local/state",
    "XDG_CACHE_HOME=/home/opencode/.cache",
    "OPENCODE_DISABLE_AUTOUPDATE=1",
];

/// opencode's entry, the one of kiln's one table that runs it.
fn opencode() -> anyhow::Result<&'static Builtin> {
    let mut runs = ALL.iter().filter(|b| b.harness == "opencode");
    let b = runs.next().ok_or_else(|| anyhow::anyhow!("no built-in Agentfile runs opencode"))?;
    anyhow::ensure!(b.name == "harness-opencode" && runs.next().is_none(), "opencode's entry isn't harness-opencode alone");
    Ok(b)
}

/// The instructions of an Agentfile, comments and blank lines aside.
fn instructions(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

/// The paths of the Agentfile's volumes for opencode alone.
fn volumes(text: &str) -> Vec<&str> {
    instructions(text).into_iter().filter_map(|l| l.strip_prefix("VOLUME --target-kind=harness ")?.strip_suffix(" FOR opencode")).collect()
}

/// A built-in's harness file, as its text.
fn harness_text(b: &Builtin) -> anyhow::Result<&'static str> {
    b.files.iter().find(|(n, _)| *n == "harness.json").map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("{}: no harness.json", b.name))
}

/// A JSON array of strings, as strings; none when it's no array of strings alone.
fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?.iter().map(|s| s.as_str().map(str::to_owned)).collect()
}

fn is_sha256(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A git commit's full name, as the pin records the tag's and the provenance's.
fn is_commit(c: &str) -> bool {
    c.len() == 40 && c.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// X.Y.Z with X 2 or more: OpenCode 2, which has the opencode/child-session-updates kiln's ACP adapter
/// asks for (f6).
fn is_opencode_2(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())) && parts.first().and_then(|major| major.parse::<u32>().ok()).is_some_and(|major| major >= 2)
}

/// What in opencode's pin would mislead a build or its smoke:
/// - a version that isn't OpenCode 2's X.Y.Z (f6), a tag other than v<version> at a commit, or a base not
///   pinned by digest or not the one its Agentfile builds on;
/// - checks recorded as other than pin.sh's (f9): the release's SHA256SUMS by its SHA-256, GitHub's own
///   digests, and SLSA provenance signed by opencode's release workflow at a commit;
/// - builds other than linux/amd64 and linux/arm64, each in gnu and musl;
/// - per build, other than two files: the release's archive for it (amd64's the baseline build, f11) with
///   its SHA-256 and size, which the one unpacker reads as its one entry `opencode` held alone (that
///   entry's own SHA-256 and size) and writes at bin/opencode, 0755 (f8); then the tag's LICENSE at
///   LICENSE, 0644, with nothing to unpack (f29);
/// - a smoke that isn't `bin/opencode --version` printing `opencode v<version>` (f32), in the shape every
///   pin gives its own: its program, args and stdout alone.
fn pin_problems(text: &str, agentfile: &str) -> Vec<String> {
    let pin: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return vec![format!("not JSON: {e}")],
    };
    let s = |p: &str| pin.pointer(p).and_then(Value::as_str).unwrap_or_default();
    let mut found = Vec::new();
    let version = s("/version");
    if !is_opencode_2(version) {
        found.push(format!("version `{version}` isn't OpenCode 2's X.Y.Z"));
    }
    let tag_commit = s("/tag/commit");
    if s("/tag/name") != format!("v{version}") || !is_commit(tag_commit) {
        found.push(format!("its tag isn't v{version} at a commit"));
    }
    let base = s("/base");
    if !base.rsplit_once("@sha256:").is_some_and(|(name, d)| !name.is_empty() && is_sha256(d)) {
        found.push(format!("base `{base}` isn't pinned by digest"));
    }
    if instructions(agentfile).into_iter().find_map(|l| l.strip_prefix("ARG BASE=")) != Some(base) {
        found.push(format!("the Agentfile doesn't build on the pin's base `{base}`"));
    }
    let sums = format!("https://github.com/{REPO}/releases/download/v{version}/SHA256SUMS");
    if s("/verified/sha256sums/url") != sums || !is_sha256(s("/verified/sha256sums/sha256")) || pin.pointer("/verified/github_digest") != Some(&Value::Bool(true)) {
        found.push("its archives' checks aren't recorded as the release's SHA256SUMS and GitHub's digests".to_owned());
    }
    if s("/verified/provenance/repo") != REPO || s("/verified/provenance/signer_workflow") != SIGNER || s("/verified/provenance/predicate") != SLSA || !is_commit(s("/verified/provenance/source_commit")) {
        found.push(format!("its provenance isn't recorded as signed by {SIGNER} at a commit"));
    }
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    let name = |b: &Value, k: &str| b.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut seen: Vec<String> = builds.iter().map(|b| format!("{} {}", name(b, "platform"), name(b, "variant"))).collect();
    seen.sort();
    if seen != ["linux/amd64 gnu", "linux/amd64 musl", "linux/arm64 gnu", "linux/arm64 musl"] {
        found.push(format!("its builds are {seen:?}"));
    }
    let license = format!("https://raw.githubusercontent.com/{REPO}/{tag_commit}/LICENSE");
    for b in &builds {
        let (platform, variant) = (name(b, "platform"), name(b, "variant"));
        let at = format!("{platform} {variant}");
        let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        let [archive, notice] = files.as_slice() else {
            found.push(format!("{at}: {} files, not its archive and its LICENSE", files.len()));
            continue;
        };
        for f in [archive, notice] {
            if !f.get("sha256").and_then(Value::as_str).is_some_and(is_sha256) || f.get("size").and_then(Value::as_u64).is_none_or(|n| n == 0) {
                found.push(format!("{at}: a file without its SHA-256 and size"));
            }
        }
        let arch = if platform == "linux/amd64" { "x64-baseline" } else { "arm64" };
        let libc = if variant == "musl" { "-musl" } else { "" };
        let want = format!("https://github.com/{REPO}/releases/download/v{version}/opencode-linux-{arch}{libc}.tar.gz");
        let of = |file: &Value, k: &str| file.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        if of(archive, "url") != want {
            found.push(format!("{at}: its archive isn't {want}"));
        }
        if of(archive, "path") != "bin/opencode" || of(archive, "mode") != "0755" {
            found.push(format!("{at}: its archive doesn't go to bin/opencode, 0755"));
        }
        // As the one unpacker reads it (`unpack::Spec`): the member opencode, held alone, as pin.sh found it
        // the archive's only entry (f8), with that entry's own SHA-256 and size.
        let unpack = archive.get("unpack");
        let (sha256, size) = (unpack.and_then(|u| u.get("sha256")).and_then(Value::as_str).unwrap_or_default(), unpack.and_then(|u| u.get("size")).and_then(Value::as_u64).unwrap_or_default());
        let held = Spec::Member(Member { member: "opencode".to_owned(), sha256: sha256.to_owned(), size, alone: true });
        let read = unpack.cloned().map(serde_json::from_value::<Spec>);
        if !is_sha256(sha256) || size == 0 || !matches!(read, Some(Ok(ref spec)) if *spec == held) {
            found.push(format!("{at}: its archive isn't unpacked from its one entry opencode, held alone, with that entry's SHA-256 and size"));
        }
        if of(notice, "url") != license || of(notice, "path") != "LICENSE" || of(notice, "mode") != "0644" || notice.get("unpack").is_some() {
            found.push(format!("{at}: its second file isn't the tag's LICENSE at LICENSE, 0644"));
        }
    }
    let smoke = serde_json::json!({"program": "bin/opencode", "args": ["--version"], "stdout": format!("opencode v{version}")});
    if pin.get("smoke") != Some(&smoke) {
        found.push(format!("the smoke isn't {smoke}"));
    }
    found
}

/// The fields of the object `v` that aren't among `known`.
fn unknown_fields(v: Option<&Value>, known: &[&str]) -> Vec<String> {
    v.and_then(Value::as_object).into_iter().flat_map(|o| o.keys()).filter(|k| !known.contains(&k.as_str())).cloned().collect()
}

/// What Shards' OSI reader refuses of a harness file (`Config::parse` in crates/image/src/osi.rs,
/// Shards main 7ad28fab1, as cursor_tests.rs reads it): a schemaVersion other than 1; a field it doesn't
/// know, as it denies unknown fields; a name no Agentfile can give; a version that's no string; a run
/// without its command, or a command or env not strings alone; and a run.env entry that isn't NAME=value
/// or sets SHARDS_DOMAIN, the runtime's own.
fn osi_problems(h: &Value) -> Vec<String> {
    const FIELDS: [&str; 9] = ["schemaVersion", "name", "version", "description", "platform", "run", "skills", "mcp", "asks"];
    const RUN: [&str; 3] = ["command", "env", "workdir"];
    let mut found = Vec::new();
    if h.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        found.push("its schemaVersion isn't 1".to_owned());
    }
    found.extend(unknown_fields(Some(h), &FIELDS).into_iter().map(|k| format!("a field `{k}` Shards' reader refuses")));
    found.extend(unknown_fields(h.get("run"), &RUN).into_iter().map(|k| format!("a field `run.{k}` Shards' reader refuses")));
    let name = h.get("name").and_then(Value::as_str).unwrap_or_default();
    let mut chars = name.chars();
    if !(chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))) {
        found.push(format!("its name `{name}` is no name an Agentfile can give"));
    }
    if h.get("version").is_some_and(|v| !v.is_string()) {
        found.push("its version is no string".to_owned());
    }
    if !strings(h.pointer("/run/command")).is_some_and(|c| !c.is_empty()) {
        found.push("its run.command isn't a command".to_owned());
    }
    match (h.pointer("/run/env"), strings(h.pointer("/run/env"))) {
        (Some(_), None) => found.push("its run.env isn't strings alone".to_owned()),
        (_, entries) => {
            for e in entries.unwrap_or_default() {
                if !e.split_once('=').is_some_and(|(n, _)| !n.is_empty() && n != "SHARDS_DOMAIN") {
                    found.push(format!("its run.env entry `{e}` isn't NAME=value of a variable an image may set"));
                }
            }
        }
    }
    found
}

/// Where a harness file departs from opencode's own: its env must be exactly [`ENV`], its HOME `home`,
/// the person's volume, each XDG root on it, and no workdir, as kiln gives each ACP session its own
/// folder.
fn env_problems(h: &Value, home: &str) -> Vec<String> {
    let mut found = Vec::new();
    let env = strings(h.pointer("/run/env")).unwrap_or_default();
    if env != ENV {
        found.push(format!("its env {env:?} isn't {ENV:?}"));
    }
    if !env.iter().any(|e| e.strip_prefix("HOME=") == Some(home)) {
        found.push(format!("its HOME isn't the person's volume {home}"));
    }
    for (var, root) in env.iter().filter_map(|e| e.split_once('=')).filter(|(n, _)| n.starts_with("XDG_")) {
        if !root.strip_prefix(home).is_some_and(|rest| rest.starts_with('/')) {
            found.push(format!("its {var} {root} isn't on the person's volume {home}"));
        }
    }
    if h.pointer("/run/workdir").is_some() {
        found.push("it sets a workdir".to_owned());
    }
    found
}

/// What kiln's Kind `kind` launches, for a default spec naming only its program and folder: its
/// arguments, after the program.
fn launched(kind: &Kind) -> anyhow::Result<Vec<String>> {
    let cwd = tempfile::tempdir()?;
    let spec = Session { kind: kind.id.to_owned(), program: Some(format!("/nowhere/{}", kind.program)), cwd: Some(cwd.path().to_string_lossy().into_owned()), ..Session::default() };
    let p = kind.launch(&spec, "session")?;
    Ok(p.get_args().map(|a| a.to_string_lossy().into_owned()).collect())
}

/// Where opencode's harness file and pin depart from how `kind` launches opencode: its command must be
/// the kind's program and then exactly the arguments the kind launches with (`acp`, f4), and the pin's
/// binary must be bin/<program>, which the harness's PATH finds as that command (f16).
fn kind_problems(h: &Value, pin: &Value, kind: &Kind) -> anyhow::Result<Vec<String>> {
    let mut found = Vec::new();
    let want: Vec<String> = std::iter::once(kind.program.to_owned()).chain(launched(kind)?).collect();
    let command = strings(h.pointer("/run/command")).unwrap_or_default();
    if command != want {
        found.push(format!("its command {command:?} isn't {want:?}"));
    }
    let bin = format!("bin/{}", kind.program);
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    if builds.is_empty() || !builds.iter().all(|b| b.pointer("/files/0/path").and_then(Value::as_str) == Some(bin.as_str())) {
        found.push(format!("the pin's binary isn't {bin} in every build"));
    }
    Ok(found)
}

/// What in a built-in looks like a credential: an ENV; an ARG or a harness's env named like one; a
/// credential's file (opencode's legacy auth.json, its opencode.db, f19) among its files or brought in by
/// an instruction; or a token's value in any of its texts.
fn credential_problems(text: &str, pin: &str, files: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
    let named = regex::Regex::new(r"(?i)key|token|secret|passw|credential")?;
    let value = regex::Regex::new(r"sk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----")?;
    const FILES: [&str; 2] = ["auth.json", "opencode.db"];
    let mut found = Vec::new();
    for line in instructions(text) {
        let mut words = line.split_whitespace();
        let (Some(op), Some(arg)) = (words.next(), words.next()) else { continue };
        if op.eq_ignore_ascii_case("ENV") {
            found.push(format!("an ENV: {line}"));
        }
        if op.eq_ignore_ascii_case("ARG") && arg.split('=').next().is_some_and(|name| named.is_match(name)) {
            found.push(format!("an ARG named like a credential: {line}"));
        }
        if let Some(file) = FILES.into_iter().find(|f| line.contains(f)) {
            found.push(format!("`{line}` brings in {file}"));
        }
    }
    for (name, body) in files {
        if FILES.contains(name) {
            found.push(format!("a credential's file: {name}"));
        }
        if let Ok(h) = serde_json::from_str::<Value>(body) {
            for e in strings(h.pointer("/run/env")).unwrap_or_default() {
                if e.split('=').next().is_some_and(|n| named.is_match(n)) {
                    found.push(format!("{name}: env {e} is named like a credential"));
                }
            }
        }
    }
    for t in [text, pin].into_iter().chain(files.iter().map(|(_, body)| *body)) {
        if let Some(m) = value.find(t) {
            found.push(format!("a credential's value: {}...", m.as_str().get(..6).unwrap_or_default()));
        }
    }
    Ok(found)
}

/// A copy of `text` (JSON) with the value at `path` set to `to`, or removed without one. In a list, a
/// place one past its end adds `to` there.
fn edited(text: &str, path: &str, to: Option<Value>) -> anyhow::Result<String> {
    let mut v: Value = serde_json::from_str(text)?;
    let (parent, key) = path.rsplit_once('/').ok_or_else(|| anyhow::anyhow!("{path}"))?;
    match (v.pointer_mut(parent), to) {
        (Some(Value::Object(o)), Some(to)) => {
            o.insert(key.to_owned(), to);
        }
        (Some(Value::Object(o)), None) => {
            o.remove(key);
        }
        (Some(Value::Array(a)), to) => {
            let i: usize = key.parse()?;
            match to {
                Some(to) if i == a.len() => a.push(to),
                Some(to) => *a.get_mut(i).ok_or_else(|| anyhow::anyhow!("{path}"))? = to,
                None => {
                    anyhow::ensure!(i < a.len(), "{path}");
                    a.remove(i);
                }
            }
        }
        _ => anyhow::bail!("no {parent} in it"),
    }
    Ok(v.to_string())
}

/// opencode's pin holds (OpenCode 2's four builds, each its release archive's one entry, held alone, to
/// bin/opencode with its LICENSE, checked as pin.sh checks them, the base its Agentfile builds on, its
/// smoke); its harness file is an OSI config of opencode at the pin's version whose env is exactly
/// [`ENV`], each XDG root on the person's volume; and it has no other context file.
#[test]
fn step22_opencode_pin_and_harness_file_hold() -> anyhow::Result<()> {
    let b = opencode()?;
    let problems = pin_problems(b.pin, b.text);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    anyhow::ensure!(b.files.iter().map(|(n, _)| *n).eq(["harness.json"]), "opencode's context files aren't its harness.json alone");
    let pin: Value = serde_json::from_str(b.pin)?;
    let h: Value = serde_json::from_str(harness_text(b)?)?;
    let problems = osi_problems(&h);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    anyhow::ensure!(h.get("name").and_then(Value::as_str) == Some(b.harness), "{h}");
    anyhow::ensure!(h.get("version").is_some() && h.get("version") == pin.get("version"), "{h} isn't at the pin's version");
    let vs = volumes(b.text);
    anyhow::ensure!(vs == ["/home/opencode"], "{vs:?}");
    let problems = env_problems(&h, "/home/opencode");
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    Ok(())
}

/// A pin or harness file no build or smoke could trust is caught: in the pin, a digest one digit short,
/// a variant or a platform missing, OpenCode 1, a tag elsewhere, a base by tag or not the Agentfile's,
/// another workflow's provenance, an archive of another release or over plain HTTP, the x64 build where
/// amd64 takes the baseline one, an archive not unpacked or unpacked as a tree, its entry without its
/// digest, not held alone or in a form the unpacker refuses, a binary elsewhere, the LICENSE missing or
/// at another commit, and a smoke that runs another program, asks something else, expects another line
/// or carries an env; in the harness file, a schema it isn't, a field Shards refuses, a name no
/// Agentfile gives, no command, an env entry with no value or the runtime's own, an XDG root off the
/// person's volume, a HOME on the scratch, the update check on, and a workdir.
#[test]
fn step22_an_opencode_pin_or_harness_file_that_would_mislead_is_caught() -> anyhow::Result<()> {
    let b = opencode()?;
    let real: Value = serde_json::from_str(b.pin)?;
    let field = |p: &str| real.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    let version = field("/version");
    let says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(pin_problems(&edited(b.pin, path, to)?, b.text).iter().any(|p| p.contains(what))) };
    let short = field("/builds/0/files/0/sha256").get(1..).map(Value::from);
    anyhow::ensure!(says("/builds/0/files/0/sha256", short, "without its SHA-256")?);
    anyhow::ensure!(says("/builds/1/variant", Some(Value::from("gnu")), "its builds are")?);
    anyhow::ensure!(says("/builds/3", None, "its builds are")?);
    anyhow::ensure!(says("/version", Some(Value::from("1.19.5")), "OpenCode 2's")?);
    anyhow::ensure!(says("/tag/name", Some(Value::from("v2.2.6")), "its tag")?);
    anyhow::ensure!(says("/base", Some(Value::from("buildpack-deps:trixie-scm")), "pinned by digest")?);
    anyhow::ensure!(says("/base", Some(Value::from(format!("debian@sha256:{}", "0".repeat(64)))), "doesn't build on")?);
    anyhow::ensure!(says("/verified/github_digest", Some(Value::Bool(false)), "GitHub's digests")?);
    anyhow::ensure!(says("/verified/provenance/signer_workflow", Some(Value::from("anomalyco/opencode/.github/workflows/check.yml")), "its provenance")?);
    let url = field("/builds/0/files/0/url");
    anyhow::ensure!(says("/builds/0/files/0/url", Some(Value::from(url.replace(&format!("/v{version}/"), "/v2.2.6/"))), "its archive isn't")?);
    anyhow::ensure!(says("/builds/0/files/0/url", Some(Value::from(url.replacen("https://", "http://", 1))), "its archive isn't")?);
    anyhow::ensure!(says("/builds/0/files/0/url", Some(Value::from(url.replace("x64-baseline", "x64"))), "its archive isn't")?);
    anyhow::ensure!(says("/builds/0/files/0/unpack", None, "one entry opencode")?);
    anyhow::ensure!(says("/builds/0/files/0/unpack/member", Some(Value::from("./opencode")), "one entry opencode")?);
    anyhow::ensure!(says("/builds/0/files/0/unpack/sha256", None, "one entry opencode")?);
    anyhow::ensure!(says("/builds/0/files/0/unpack/strip", Some(Value::from(0)), "one entry opencode")?);
    anyhow::ensure!(says("/builds/0/files/0/unpack/alone", Some(Value::Bool(false)), "held alone")?);
    anyhow::ensure!(says("/builds/2/files/0/unpack/alone", None, "held alone")?);
    anyhow::ensure!(says("/builds/1/files/0/unpack/format", Some(Value::from("zip")), "held alone")?);
    anyhow::ensure!(says("/builds/3/files/0/unpack/skip", Some(serde_json::json!(["opencode"])), "held alone")?);
    anyhow::ensure!(says("/builds/0/files/0/path", Some(Value::from("opencode")), "bin/opencode, 0755")?);
    anyhow::ensure!(says("/builds/0/files/0/mode", Some(Value::from("0644")), "bin/opencode, 0755")?);
    anyhow::ensure!(says("/builds/0/files/1", None, "not its archive and its LICENSE")?);
    let notice = field("/builds/0/files/1/url");
    anyhow::ensure!(says("/builds/0/files/1/url", Some(Value::from(notice.replace(&field("/tag/commit"), &"0".repeat(40)))), "the tag's LICENSE")?);
    anyhow::ensure!(says("/smoke/program", Some(Value::from("opencode")), "the smoke")?);
    anyhow::ensure!(says("/smoke/args", Some(serde_json::json!(["version"])), "the smoke")?);
    anyhow::ensure!(says("/smoke/stdout", Some(Value::from(version.clone())), "the smoke")?);
    anyhow::ensure!(says("/smoke/stdout", Some(Value::from(format!("opencode v{version}\n"))), "the smoke")?);
    anyhow::ensure!(says("/smoke/env", Some(serde_json::json!(["HOME=/tmp"])), "the smoke")?);
    anyhow::ensure!(says("/smoke", None, "the smoke")?);
    anyhow::ensure!(pin_problems(b.pin, b.text).is_empty());

    let h = harness_text(b)?;
    let osi_says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(osi_problems(&serde_json::from_str(&edited(h, path, to)?)?).iter().any(|p| p.contains(what))) };
    anyhow::ensure!(osi_says("/schemaVersion", Some(Value::from(2)), "schemaVersion")?);
    anyhow::ensure!(osi_says("/command", Some(serde_json::json!(["opencode"])), "`command`")?);
    anyhow::ensure!(osi_says("/run/cwd", Some(Value::from("/workspace")), "`run.cwd`")?);
    anyhow::ensure!(osi_says("/name", Some(Value::from("OpenCode")), "no name")?);
    anyhow::ensure!(osi_says("/run/command", Some(serde_json::json!([])), "run.command")?);
    anyhow::ensure!(osi_says("/run/env/5", Some(Value::from("OPENCODE_DISABLE_AUTOUPDATE")), "NAME=value")?);
    anyhow::ensure!(osi_says("/run/env/5", Some(Value::from("SHARDS_DOMAIN=opencode")), "NAME=value")?);
    let env_says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(env_problems(&serde_json::from_str(&edited(h, path, to)?)?, "/home/opencode").iter().any(|p| p.contains(what))) };
    anyhow::ensure!(env_says("/run/env/2", Some(Value::from("XDG_DATA_HOME=/tmp/.local/share")), "isn't on the person's volume")?);
    anyhow::ensure!(env_says("/run/env/1", Some(Value::from("XDG_CONFIG_HOME=/home/opencode2/.config")), "isn't on the person's volume")?);
    anyhow::ensure!(env_says("/run/env/0", Some(Value::from("HOME=/tmp")), "its HOME isn't the person's volume")?);
    anyhow::ensure!(env_says("/run/env/5", None, "its env")?);
    anyhow::ensure!(env_says("/run/workdir", Some(Value::from("/workspace")), "a workdir")?);
    anyhow::ensure!(osi_problems(&serde_json::from_str(h)?).is_empty() && env_problems(&serde_json::from_str(h)?, "/home/opencode").is_empty());
    Ok(())
}

/// opencode's harness file runs opencode as kiln's opencode kind launches it, for a default spec: its
/// command is the kind's program and then exactly the kind's launch arguments (`acp`), and the pin puts
/// that program at bin/opencode in every build. A kind whose arguments or program change alone fails it,
/// and so does a harness file that drifts.
#[test]
fn step22_opencode_runs_as_kilns_opencode_kind_launches_it() -> anyhow::Result<()> {
    let b = opencode()?;
    let kind = Kind::get(b.harness).ok_or_else(|| anyhow::anyhow!("no {} kind", b.harness))?;
    anyhow::ensure!(matches!(kind.protocol, Protocol::Acp(_)), "kiln's opencode kind doesn't speak ACP");
    let pin: Value = serde_json::from_str(b.pin)?;
    let h: Value = serde_json::from_str(harness_text(b)?)?;
    let problems = kind_problems(&h, &pin, kind)?;
    anyhow::ensure!(problems.is_empty(), "{problems:?}");

    let other_args = Kind { protocol: Protocol::Acp(&["acp", "--print-logs"]), ..*kind };
    anyhow::ensure!(kind_problems(&h, &pin, &other_args)?.len() == 1, "a kind launching other arguments passed");
    let other_program = Kind { program: "opencode-cli", ..*kind };
    anyhow::ensure!(kind_problems(&h, &pin, &other_program)?.len() == 2, "a kind launching another program passed");
    let mut drifted = h.clone();
    if let Some(c) = drifted.pointer_mut("/run/command").and_then(Value::as_array_mut) {
        c.retain(|a| a.as_str() != Some("acp"));
    }
    anyhow::ensure!(kind_problems(&drifted, &pin, kind)?.len() == 1, "a harness file without acp passed");
    let moved: Value = serde_json::from_str(&edited(b.pin, "/builds/2/files/0/path", Some(Value::from("opencode")))?)?;
    anyhow::ensure!(kind_problems(&h, &moved, kind)?.len() == 1, "a build with its binary elsewhere passed");
    Ok(())
}

/// opencode's Agentfile is the plan's shape (step 19's layout): the pin's base by digest; a stage for
/// each libc, a RUN only in musl's (libstdc++, which brings libgcc, and the distribution's ripgrep, as
/// the one opencode fetches for arm64 is a glibc build: f13, f22); opencode's own build per platform and
/// libc from the directory kiln writes (f26); its providers' hosts by name (step 18's grants; its ACP
/// server is on the harness's own loopback, which needs no grant, f15, f16); its HOME
/// as its one volume, the harness's alone; and no ENV.
#[test]
fn step22_opencode_agentfile_is_the_plans_shape() -> anyhow::Result<()> {
    let b = opencode()?;
    let pin: Value = serde_json::from_str(b.pin)?;
    let base = format!("ARG BASE={}", pin.get("base").and_then(Value::as_str).unwrap_or_default());
    let lines = instructions(b.text);
    let want = [
        base.as_str(),
        "ARG OPENCODE_LIBC=gnu",
        "FROM ${BASE} AS base-gnu",
        "FROM ${BASE} AS base-musl",
        "RUN apk add --no-cache libstdc++ ripgrep",
        "FROM base-${OPENCODE_LIBC}",
        "ARG TARGETARCH",
        "ARG OPENCODE_LIBC",
        "HARNESS opencode FROM ./opencode-${TARGETARCH}-${OPENCODE_LIBC}",
    ];
    let want: Vec<String> = want.iter().map(|l| (*l).to_owned()).chain(super::grants_tests::grant_lines("opencode")).chain([super::grants_tests::volume_line("opencode")]).collect();
    anyhow::ensure!(lines == want, "{lines:#?}");
    Ok(())
}

/// Nothing kiln ships for opencode holds or brings in a credential, nor an ENV, ARG or env named like
/// one; each way of adding one is caught, an ENV OPENAI_API_KEY=x in the embedded Agentfile first (f19:
/// a person's sign-in lives in their own volume).
#[test]
fn step22_no_opencode_file_holds_a_credential() -> anyhow::Result<()> {
    let b = opencode()?;
    let found = credential_problems(b.text, b.pin, b.files)?;
    anyhow::ensure!(found.is_empty(), "{found:?}");
    let caught_in_text = |text: &str| -> anyhow::Result<bool> { Ok(!credential_problems(text, b.pin, b.files)?.is_empty()) };
    anyhow::ensure!(caught_in_text(&format!("{}ENV OPENAI_API_KEY=x\n", b.text))?, "ENV OPENAI_API_KEY passed");
    anyhow::ensure!(caught_in_text(&format!("{}ARG GITHUB_TOKEN\n", b.text))?, "ARG GITHUB_TOKEN passed");
    anyhow::ensure!(caught_in_text(&format!("{}COPY auth.json /home/opencode/.local/share/opencode/\n", b.text))?, "a COPY of auth.json passed");
    let h = harness_text(b)?;
    let caught_in_harness = |text: &str| -> anyhow::Result<bool> { Ok(!credential_problems(b.text, b.pin, &[("harness.json", text)])?.is_empty()) };
    anyhow::ensure!(caught_in_harness(&h.replace("OPENCODE_DISABLE_AUTOUPDATE=1", "ANTHROPIC_API_KEY=x"))?, "env ANTHROPIC_API_KEY passed");
    anyhow::ensure!(caught_in_harness(&h.replace("OPENCODE_DISABLE_AUTOUPDATE=1", "OPENCODE_PASSWORD=x"))?, "env OPENCODE_PASSWORD passed");
    anyhow::ensure!(!credential_problems(b.text, b.pin, &[("harness.json", h), ("opencode.db", "")])?.is_empty(), "a context file opencode.db passed");
    let leaky_pin = b.pin.replacen("\"harness\"", "\"key\": \"sk-ant-api03-0123456789abcdef\", \"harness\"", 1);
    anyhow::ensure!(leaky_pin != b.pin && !credential_problems(b.text, &leaky_pin, b.files)?.is_empty(), "a key's value in the pin passed");
    Ok(())
}
