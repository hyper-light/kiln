//! Cursor's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 27; plan
//! 01a1ce705e5c7504b88732f443d99288, step 3): what its pin, its harness.json and its Agentfile say of one
//! another, of kiln's Kind `cursor`, and of credentials. Each check reads texts, so each test also hands
//! it changed copies, and each change must fail it.

use std::collections::BTreeSet;
use std::ffi::OsStr;

use serde_json::Value;

use super::{ALL, Builtin};
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::session::Spec;

/// Cursor's entry, the one of kiln's one table that runs it.
fn cursor() -> anyhow::Result<&'static Builtin> {
    let mut runs = ALL.iter().filter(|b| b.harness == "cursor");
    let b = runs.next().ok_or_else(|| anyhow::anyhow!("no built-in Agentfile runs cursor"))?;
    anyhow::ensure!(b.name == "harness-cursor" && runs.next().is_none(), "Cursor's entry isn't harness-cursor alone");
    Ok(b)
}

/// The instructions of an Agentfile, comments and blank lines aside.
fn instructions(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

/// `ARG BASE=`'s default.
fn base_of(text: &str) -> Option<&str> {
    instructions(text).into_iter().find_map(|l| l.strip_prefix("ARG BASE="))
}

/// The paths of the Agentfile's volumes for `harness` alone.
fn harness_volumes<'a>(text: &'a str, harness: &str) -> Vec<&'a str> {
    let tail = format!(" FOR {harness}");
    instructions(text).into_iter().filter_map(|l| l.strip_prefix("VOLUME --target-kind=harness ")?.strip_suffix(tail.as_str())).collect()
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

/// One URL path segment that needs no escape, and not `.` or `..`: pin.sh puts the version in the
/// download URL's path.
fn is_segment(v: &str) -> bool {
    !v.is_empty() && v != "." && v != ".." && v.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// What in Cursor's pin would mislead a build:
/// - a version that isn't one URL path segment, an installer read without its SHA-256, a base not
///   pinned by digest, or a check recorded as other than Cursor's unsigned same-origin .sha256;
/// - builds for other than exactly linux/amd64 and linux/arm64, once each;
/// - per build, other than one file: Cursor's archive from its download host over HTTPS, with its
///   SHA-256 and size, at `.`, the harness directory the archive's tree fills;
/// - an unpack other than that tree: tar.gz with its one top directory stripped, both counts the
///   unpacker holds a tree to above 0, and no `member`.
fn pin_problems(text: &str) -> Vec<String> {
    let pin: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return vec![format!("not JSON: {e}")],
    };
    let mut found = Vec::new();
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    if !is_segment(version) {
        found.push(format!("version `{version}` isn't one URL path segment"));
    }
    if !pin.pointer("/version_read/sha256").and_then(Value::as_str).is_some_and(is_sha256) {
        found.push("the installer it read the version from has no SHA-256".to_owned());
    }
    let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
    if !base.rsplit_once("@sha256:").is_some_and(|(name, d)| !name.is_empty() && is_sha256(d)) {
        found.push(format!("base `{base}` isn't pinned by digest"));
    }
    if pin.pointer("/verified/checksum").and_then(Value::as_str) != Some("same-origin") || pin.pointer("/verified/signed") != Some(&Value::Bool(false)) {
        found.push("its check isn't recorded as Cursor's unsigned same-origin .sha256".to_owned());
    }
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    let platforms: Vec<&str> = builds.iter().filter_map(|b| b.get("platform").and_then(Value::as_str)).collect();
    for want in ["linux/amd64", "linux/arm64"] {
        if !platforms.contains(&want) {
            found.push(format!("no build for {want}"));
        }
    }
    let mut seen = BTreeSet::new();
    for p in &platforms {
        if !seen.insert(*p) {
            found.push(format!("two builds for {p}"));
        }
        if !["linux/amd64", "linux/arm64"].contains(p) {
            found.push(format!("a build for {p}, which Cursor's pin doesn't build"));
        }
    }
    for b in &builds {
        let at = b.get("platform").and_then(Value::as_str).unwrap_or_default();
        let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        if files.len() != 1 {
            found.push(format!("{at}: {} files, where Cursor's build is its one archive", files.len()));
        }
        for f in &files {
            let s = |k: &str| f.get(k).and_then(Value::as_str);
            if !s("url").is_some_and(|u| u.starts_with("https://downloads.cursor.com/")) {
                found.push(format!("{at}: no https URL on Cursor's download host"));
            }
            if !s("sha256").is_some_and(is_sha256) || f.get("size").and_then(Value::as_u64).is_none() {
                found.push(format!("{at}: a file without its SHA-256 and size"));
            }
            if s("path") != Some(".") {
                found.push(format!("{at}: a path other than `.`, the harness directory its tree fills"));
            }
            let u = |k: &str| f.get("unpack").and_then(|u| u.get(k));
            if u("format").and_then(Value::as_str) != Some("tar.gz") || u("strip").and_then(Value::as_u64) != Some(1) {
                found.push(format!("{at}: an unpack other than tar.gz with its one top directory stripped"));
            }
            if !["entries", "unpacked_bytes"].into_iter().all(|k| u(k).and_then(Value::as_u64).is_some_and(|n| n > 0)) {
                found.push(format!("{at}: an unpack without both counts the unpacker holds a tree to"));
            }
            if u("member").is_some() {
                found.push(format!("{at}: an unpack of one member, where Cursor's is its whole tree"));
            }
        }
    }
    found
}

/// The fields of the object `v` that aren't among `known`.
fn unknown_fields(v: Option<&Value>, known: &[&str]) -> Vec<String> {
    v.and_then(Value::as_object).into_iter().flat_map(|o| o.keys()).filter(|k| !known.contains(&k.as_str())).cloned().collect()
}

/// What Shards' OSI reader refuses of a harness file (`Config::parse` in crates/image/src/osi.rs,
/// Shards main 7ad28fab1): a schemaVersion other than 1; a field it doesn't know, as it denies
/// unknown fields; a name no Agentfile can give; a version that's no string; a run without its
/// command, or a command or env not strings alone; and a run.env entry that isn't NAME=value or sets
/// SHARDS_DOMAIN, the runtime's own.
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

/// What kiln's Kind `kind` launches, for a default spec naming only its program and folder: its
/// arguments, and the NO_OPEN_BROWSER it sets.
fn launched(kind: &Kind) -> anyhow::Result<(Vec<String>, Option<String>)> {
    let cwd = tempfile::tempdir()?;
    let spec = Spec { kind: kind.id.to_owned(), program: Some("/nowhere/agent".to_owned()), cwd: Some(cwd.path().to_string_lossy().into_owned()), ..Spec::default() };
    let p = kind.launch(&spec, "session")?;
    let args = p.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    let browser = p.get_envs().find(|(k, _)| *k == OsStr::new("NO_OPEN_BROWSER")).and_then(|(_, v)| v).map(|v| v.to_string_lossy().into_owned());
    Ok((args, browser))
}

/// Where a harness file departs from how `kind` launches Cursor: its command must be `cursor-agent
/// --disable-auto-update`, then exactly the arguments the kind launches with, and its env must set
/// NO_OPEN_BROWSER as the kind's launch sets it.
fn kind_problems(h: &Value, kind: &Kind) -> anyhow::Result<Vec<String>> {
    let (args, browser) = launched(kind)?;
    let mut found = Vec::new();
    let command = strings(h.pointer("/run/command")).unwrap_or_default();
    let want: Vec<String> = ["cursor-agent", "--disable-auto-update"].into_iter().map(str::to_owned).chain(args).collect();
    if command != want {
        found.push(format!("its command {command:?} isn't {want:?}"));
    }
    let env = strings(h.pointer("/run/env")).unwrap_or_default();
    let set = env.iter().find_map(|e| e.strip_prefix("NO_OPEN_BROWSER=")).map(str::to_owned);
    if set != browser {
        found.push(format!("its NO_OPEN_BROWSER {set:?} isn't the kind's {browser:?}"));
    }
    Ok(found)
}

/// Words that bring in a credential of Cursor's: its key and token variables and flags, its sign-in
/// file and the fields Cursor keeps in it, and the variable that moves where its sign-in is kept.
const CURSOR_CREDENTIALS: [&str; 8] = ["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN", "--api-key", "--auth-token", "auth.json", "accessToken", "refreshToken", "AGENT_CLI_CREDENTIAL_STORE"];

/// A variable named as a credential is: key, token, secret, password or credential, in any case.
fn named_like_a_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"].into_iter().any(|w| upper.contains(w))
}

/// What in `text` holds or names a credential: one of Cursor's words anywhere, comments included; an
/// ENV or ARG named like a credential; and such a variable given a value (`NAME=`), as a harness
/// file's run.env would give one.
fn credentials_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = CURSOR_CREDENTIALS.into_iter().filter(|w| text.contains(w)).map(|w| format!("names {w}")).collect();
    for l in instructions(text) {
        if let Some(rest) = l.strip_prefix("ENV ").or_else(|| l.strip_prefix("ARG ")) {
            found.extend(rest.split_whitespace().filter_map(|w| w.split('=').next()).filter(|n| named_like_a_credential(n)).map(|n| format!("`{l}` names {n}")));
        }
    }
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let word = text.get(s..i).unwrap_or_default();
                if c == '=' && named_like_a_credential(word) {
                    found.push(format!("{word}= gives one a value"));
                }
                start = None;
            }
            _ => {}
        }
    }
    found
}

/// Cursor's pin holds (its version, both platforms' archives by digest and size, each unpacked whole
/// as a tree to its counts, its check recorded as it is); its harness file is an OSI config of Cursor
/// at the pin's version whose env is exactly the plan's, HOME on the person's volume; and its
/// Agentfile builds on the base its pin was checked with by default.
#[test]
fn step27_cursor_pin_and_harness_file_hold() -> anyhow::Result<()> {
    let cursor = cursor()?;
    let problems = pin_problems(cursor.pin);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    let pin: Value = serde_json::from_str(cursor.pin)?;
    let (version, base) = (pin.get("version").and_then(Value::as_str), pin.get("base").and_then(Value::as_str));
    anyhow::ensure!(base_of(cursor.text) == base, "the Agentfile's BASE isn't its pin's {base:?}");
    anyhow::ensure!(cursor.files.iter().map(|(n, _)| *n).eq(["harness.json"]), "Cursor's context files aren't its harness.json alone");
    let h: Value = serde_json::from_str(harness_text(cursor)?)?;
    let problems = osi_problems(&h);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    anyhow::ensure!(h.get("name").and_then(Value::as_str) == Some(cursor.harness), "{h}");
    anyhow::ensure!(h.get("version").and_then(Value::as_str) == version, "{h} isn't at the pin's {version:?}");
    let env = strings(h.pointer("/run/env")).unwrap_or_default();
    anyhow::ensure!(env == ["HOME=/home/cursor", "XDG_CACHE_HOME=/tmp/.cache", "NO_OPEN_BROWSER=1"], "{env:?}");
    let home = env.iter().find_map(|e| e.strip_prefix("HOME=")).unwrap_or_default();
    anyhow::ensure!(harness_volumes(cursor.text, "cursor") == [home], "HOME {home} isn't Cursor's one volume");
    let moved = cursor.text.replace("harness /home/cursor FOR", "harness /home/cursor/.cursor FOR");
    anyhow::ensure!(harness_volumes(&moved, "cursor") != [home]);
    Ok(())
}

/// A pin or harness file no build could trust is caught: a digest one digit short, a platform
/// missing or another, a tree not stripped of its top directory or not tar.gz, a count missing or 0,
/// one member for the tree, a path into it, a URL elsewhere or over plain HTTP, a base by tag, a
/// version that climbs a path, an installer's digest not lowercase hex, a check recorded as signed;
/// and in the harness file a schema it isn't, a field Shards refuses, a name no Agentfile gives, no
/// command, and an env entry with no value or the runtime's own.
#[test]
fn step27_a_cursor_pin_or_harness_file_that_would_mislead_a_build_is_caught() -> anyhow::Result<()> {
    let cursor = cursor()?;
    let edit = |text: &str, path: &str, to: Option<Value>| -> anyhow::Result<Value> {
        let mut v: Value = serde_json::from_str(text)?;
        let (parent, key) = path.rsplit_once('/').ok_or_else(|| anyhow::anyhow!("{path}"))?;
        let object = v.pointer_mut(parent).and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("{path}"))?;
        match to {
            Some(to) => object.insert(key.to_owned(), to),
            None => object.remove(key),
        };
        Ok(v)
    };
    let pin_says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(pin_problems(&edit(cursor.pin, path, to)?.to_string()).iter().any(|p| p.contains(what))) };
    let real: Value = serde_json::from_str(cursor.pin)?;
    let field = |p: &str| real.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
    let short = field("/builds/0/files/0/sha256").get(1..).map(Value::from);
    anyhow::ensure!(pin_says("/builds/0/files/0/sha256", short, "without its SHA-256")?);
    anyhow::ensure!(pin_says("/builds/1/platform", Some(Value::from("linux/amd64")), "no build for linux/arm64")?);
    anyhow::ensure!(pin_says("/builds/1/platform", Some(Value::from("linux/arm/v7")), "doesn't build")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/strip", Some(Value::from(0)), "top directory stripped")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/format", Some(Value::from("zip")), "other than tar.gz")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/entries", None, "both counts")?);
    anyhow::ensure!(pin_says("/builds/1/files/0/unpack/unpacked_bytes", Some(Value::from(0)), "both counts")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/member", Some(Value::from("cursor-agent")), "one member")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/path", Some(Value::from("bin")), "a path other than")?);
    let url = field("/builds/0/files/0/url");
    anyhow::ensure!(pin_says("/builds/0/files/0/url", Some(Value::from(url.replacen("https://", "http://", 1))), "download host")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/url", Some(Value::from(url.replacen("downloads.cursor.com", "downloads.example.com", 1))), "download host")?);
    anyhow::ensure!(pin_says("/base", Some(Value::from("buildpack-deps:trixie-scm")), "pinned by digest")?);
    anyhow::ensure!(pin_says("/version", Some(Value::from("2026.11.11/../x")), "one URL path segment")?);
    anyhow::ensure!(pin_says("/version_read/sha256", Some(Value::from(field("/version_read/sha256").to_uppercase())), "installer")?);
    anyhow::ensure!(pin_says("/verified/signed", Some(Value::Bool(true)), "unsigned same-origin")?);

    let h = harness_text(cursor)?;
    let osi_says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(osi_problems(&edit(h, path, to)?).iter().any(|p| p.contains(what))) };
    anyhow::ensure!(osi_says("/schemaVersion", Some(Value::from(2)), "schemaVersion")?);
    anyhow::ensure!(osi_says("/command", Some(Value::from(vec!["cursor-agent"])), "`command`")?);
    anyhow::ensure!(osi_says("/run/cwd", Some(Value::from("/workspace")), "`run.cwd`")?);
    anyhow::ensure!(osi_says("/name", Some(Value::from("Cursor")), "no name")?);
    anyhow::ensure!(osi_says("/run/command", Some(Value::from(Vec::<String>::new())), "run.command")?);
    anyhow::ensure!(osi_says("/run/env", Some(Value::from(vec!["NO_OPEN_BROWSER"])), "NAME=value")?);
    anyhow::ensure!(osi_says("/run/env", Some(Value::from(vec!["SHARDS_DOMAIN=cursor"])), "NAME=value")?);
    anyhow::ensure!(pin_problems(cursor.pin).is_empty() && osi_problems(&serde_json::from_str::<Value>(h)?).is_empty());
    Ok(())
}

/// Cursor's harness file runs Cursor as kiln's Cursor kind launches it, for a default spec: its
/// command is `cursor-agent --disable-auto-update` and then exactly the kind's launch arguments
/// (`acp`), and its NO_OPEN_BROWSER is the kind's launch's. A kind whose arguments change alone, or
/// whose launch sets another NO_OPEN_BROWSER alone, fails it, and so does a harness file that drifts.
#[test]
fn step27_cursor_runs_as_kilns_cursor_kind_launches_it() -> anyhow::Result<()> {
    let cursor = cursor()?;
    let kind = Kind::get("cursor").ok_or_else(|| anyhow::anyhow!("no cursor kind"))?;
    let h: Value = serde_json::from_str(harness_text(cursor)?)?;
    let problems = kind_problems(&h, kind)?;
    anyhow::ensure!(problems.is_empty(), "{problems:?}");

    let other_args = Kind { protocol: Protocol::Acp(&["acp", "--trust"]), ..*kind };
    anyhow::ensure!(!kind_problems(&h, &other_args)?.is_empty());
    // kiln's launch sets NO_OPEN_BROWSER for the kind whose id is cursor's alone: another id is a
    // kind whose launch env differs, nothing else changed.
    let other_env = Kind { id: "cursor-elsewhere", ..*kind };
    anyhow::ensure!(launched(&other_env)?.1.is_none() && !kind_problems(&h, &other_env)?.is_empty());
    let mut drifted = h.clone();
    if let Some(env) = drifted.pointer_mut("/run/env").and_then(Value::as_array_mut) {
        env.retain(|e| e.as_str() != Some("NO_OPEN_BROWSER=1"));
    }
    anyhow::ensure!(kind_problems(&drifted, kind)?.len() == 1);
    if let Some(c) = drifted.pointer_mut("/run/command").and_then(Value::as_array_mut) {
        c.retain(|a| a.as_str() != Some("--disable-auto-update"));
    }
    anyhow::ensure!(kind_problems(&drifted, kind)?.len() == 2);
    Ok(())
}

/// Cursor's Agentfile is the plan's shape: ARG BASE by digest and ARG TARGETARCH alone, Cursor's own
/// package per platform, its hosts by name and kiln's MCP (step 18's grants), its HOME as its one
/// harness volume, and no ENV or RUN.
#[test]
fn step27_cursor_agentfile_is_the_plans_shape() -> anyhow::Result<()> {
    let lines = instructions(cursor()?.text);
    let want = [
        "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
        "FROM ${BASE}",
        "ARG TARGETARCH",
        "HARNESS cursor FROM ./cursor-${TARGETARCH}",
    ];
    let want: Vec<String> = want.iter().map(|l| (*l).to_owned()).chain(super::grants_tests::grant_lines("cursor")).chain([super::grants_tests::volume_line("cursor")]).collect();
    anyhow::ensure!(lines == want, "{lines:#?}");
    Ok(())
}

/// Nothing kiln embeds for Cursor holds or names a credential, nor an ENV or ARG named like one; each
/// way of adding one is caught, the plan's ENV CURSOR_API_KEY=x in the embedded Agentfile first.
#[test]
fn step27_no_cursor_file_holds_a_credential() -> anyhow::Result<()> {
    let cursor = cursor()?;
    for (name, text) in [("Agentfile", cursor.text), ("pin.json", cursor.pin)].into_iter().chain(cursor.files.iter().copied()) {
        let found = credentials_in(text);
        anyhow::ensure!(found.is_empty(), "{name}: {found:?}");
    }
    let caught = |text: &str| !credentials_in(text).is_empty();
    anyhow::ensure!(caught(&format!("{}ENV CURSOR_API_KEY=x\n", cursor.text)));
    anyhow::ensure!(caught(&format!("{}ARG CURSOR_AUTH_TOKEN\n", cursor.text)));
    anyhow::ensure!(caught(&format!("{}ENV GITHUB_TOKEN ghp\n", cursor.text)));
    anyhow::ensure!(caught(&format!("{}ARG NPM_PASSWORD\n", cursor.text)));
    anyhow::ensure!(caught(&format!("{}COPY auth.json /home/cursor/.config/cursor/\n", cursor.text)));
    let h = harness_text(cursor)?;
    anyhow::ensure!(caught(&h.replace("NO_OPEN_BROWSER=1", "MY_SECRET=s")));
    anyhow::ensure!(caught(&h.replace("NO_OPEN_BROWSER=1", "AGENT_CLI_CREDENTIAL_STORE=file")));
    anyhow::ensure!(caught(&h.replace("\"acp\"", "\"--api-key\", \"k\", \"acp\"")));
    anyhow::ensure!(caught(&cursor.pin.replace("\"harness\"", "\"refreshToken\": \"r\", \"harness\"")));
    Ok(())
}
