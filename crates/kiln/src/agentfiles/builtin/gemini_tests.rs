//! Gemini CLI's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 21; plan
//! 01a1ce705e2374dfbac201fe6ba98184, research 01a1ce705e2472df9f3dd81be0531c9f): what its pin, its
//! harness.json, its system settings and its Agentfile say of one another, of kiln's Kind `gemini`, and
//! of credentials. Each check reads texts, so each test also hands it changed copies, and each change
//! must fail it.

use std::collections::BTreeSet;
use std::ffi::OsStr;

use serde_json::{Value, json};

use super::{ALL, Builtin};
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::session::Spec;

/// Where Shards puts the harness's directory in the image: HARNESS gemini lands at /harness/<name> (f28).
const HARNESS: &str = "/harness/gemini";
/// Node's archive for each of kiln's platforms, by Node's name for its architecture.
const ARCHES: [(&str, &str); 2] = [("linux/amd64", "x64"), ("linux/arm64", "arm64")];
/// Node's two members: where each is in its archive's top directory, where it goes, and its mode. Under
/// node/, off the PATH Shards starts a harness with (its directory, then its bin/: f28, f37).
const MEMBERS: [(&str, &str, &str); 2] = [("bin/node", "node/bin/node", "0755"), ("LICENSE", "node/LICENSE", "0644")];

/// Gemini's entry, the one of kiln's one table that runs it.
fn gemini() -> anyhow::Result<&'static Builtin> {
    let mut runs = ALL.iter().filter(|b| b.harness == "gemini");
    let b = runs.next().ok_or_else(|| anyhow::anyhow!("no built-in Agentfile runs gemini"))?;
    anyhow::ensure!(b.name == "harness-gemini" && runs.next().is_none(), "Gemini's entry isn't harness-gemini alone");
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

/// One of a built-in's own files, by name, as its text.
fn file(b: &Builtin, name: &str) -> anyhow::Result<&'static str> {
    b.files.iter().find(|(n, _)| *n == name).map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("{}: no {name}", b.name))
}

/// A JSON array of strings, as strings; none when it's no array of strings alone.
fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?.iter().map(|s| s.as_str().map(str::to_owned)).collect()
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

fn is_sha256(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_x_y_z(v: &str) -> bool {
    v.split('.').count() == 3 && v.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A relative path inside the directory it's in: no root, and no empty, `.` or `..` part.
fn is_inside(p: &str) -> bool {
    !p.starts_with('/') && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// A copy of the JSON `text` with the value at `path` (an object's field, or an array's item) put in
/// place, or removed for none.
fn edit(text: &str, path: &str, to: Option<Value>) -> anyhow::Result<Value> {
    let mut v: Value = serde_json::from_str(text)?;
    let (parent, key) = path.rsplit_once('/').ok_or_else(|| anyhow::anyhow!("{path}"))?;
    match (v.pointer_mut(parent), to) {
        (Some(Value::Object(o)), Some(to)) => {
            o.insert(key.to_owned(), to);
        }
        (Some(Value::Object(o)), None) => {
            o.remove(key);
        }
        (Some(Value::Array(a)), Some(to)) => {
            let slot = key.parse::<usize>().ok().and_then(|i| a.get_mut(i)).ok_or_else(|| anyhow::anyhow!("{path}: no such item"))?;
            *slot = to;
        }
        (Some(Value::Array(a)), None) => {
            let i = key.parse::<usize>().ok().filter(|i| *i < a.len()).ok_or_else(|| anyhow::anyhow!("{path}: no such item"))?;
            a.remove(i);
        }
        _ => anyhow::bail!("{path}: nothing to change at {parent}"),
    }
    Ok(v)
}

/// The path in the harness's directory the pin puts Node's executable at: the first build's file that
/// is its archive's bin/node.
fn node_path(pin: &Value) -> Option<&str> {
    let files = pin.pointer("/builds/0/files")?.as_array()?;
    files.iter().find(|f| str_at(f, "/unpack/member").ends_with("/bin/node")).and_then(|f| f.get("path")?.as_str())
}

/// What in Gemini's pin would mislead a build or its smoke:
/// - a version not x.y.z, a base not pinned by digest, a record of the npm package's check or of how
///   its Node was derived that isn't whole;
/// - builds for other than exactly linux/amd64 and linux/arm64, once each, or with a libc variant;
/// - per build, other than three files: the npm registry's tarball of the pin's version over HTTPS, a
///   tree at the harness's root with its package/ stripped and both totals the unpacker holds a tree
///   to, the same in both builds; and Node's bin/node and LICENSE, members of its own archive for the
///   platform at the pinned Node version, at node/ (off the harness's PATH: f37) with their modes, each
///   with its own SHA-256 and size;
/// - a smoke other than that node on the package's entry at the harness's path, `--version`, printing
///   the bare version (f19).
fn pin_problems(text: &str) -> Vec<String> {
    let pin: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return vec![format!("not JSON: {e}")],
    };
    let mut found = Vec::new();
    let version = str_at(&pin, "/version");
    if !is_x_y_z(version) {
        found.push(format!("version `{version}` isn't x.y.z"));
    }
    let base = str_at(&pin, "/base");
    if !base.rsplit_once("@sha256:").is_some_and(|(name, d)| !name.is_empty() && is_sha256(d)) {
        found.push(format!("base `{base}` isn't pinned by digest"));
    }
    let integrity = str_at(&pin, "/verified/gemini/integrity");
    let keyids = pin.pointer("/verified/gemini/keyids").and_then(Value::as_array).map_or(0, Vec::len);
    if str_at(&pin, "/verified/gemini/package") != "@google/gemini-cli" || !(integrity.starts_with("sha512-") && integrity.len() == 95) || keyids == 0 {
        found.push("its record of the npm package's signed integrity isn't whole".to_owned());
    }
    let entry = str_at(&pin, "/verified/gemini/entry");
    if !is_inside(entry) {
        found.push(format!("the package's entry `{entry}` isn't a path inside it"));
    }
    let node = str_at(&pin, "/verified/node/version");
    let major = pin.pointer("/verified/node/major").and_then(Value::as_u64);
    let tested: Vec<u64> = pin.pointer("/verified/node/tested_majors").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).collect()).unwrap_or_default();
    let derived = node.strip_prefix('v').filter(|v| is_x_y_z(v)).and_then(|v| v.split('.').next()?.parse::<u64>().ok());
    if derived.is_none() || derived != major || !major.is_some_and(|m| tested.contains(&m)) {
        found.push(format!("Node.js {node} isn't of a major Gemini's Linux CI tests (major {major:?}, tested {tested:?})"));
    }
    let fingerprint = str_at(&pin, "/verified/node/signer");
    if fingerprint.len() != 40 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) || !is_sha256(str_at(&pin, "/verified/node/shasums/sha256")) {
        found.push("its record of Node's signed SHASUMS256.txt isn't whole".to_owned());
    }
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    let platforms: Vec<&str> = builds.iter().map(|b| str_at(b, "/platform")).collect();
    let mut seen = BTreeSet::new();
    for (want, _) in ARCHES {
        if !platforms.contains(&want) {
            found.push(format!("no build for {want}"));
        }
    }
    for p in &platforms {
        if !seen.insert(*p) {
            found.push(format!("two builds for {p}"));
        }
    }
    let package = format!("https://registry.npmjs.org/@google/gemini-cli/-/gemini-cli-{version}.tgz");
    let mut trees = BTreeSet::new();
    for b in &builds {
        let at = str_at(b, "/platform");
        let Some((_, arch)) = ARCHES.iter().find(|(p, _)| *p == at) else {
            found.push(format!("a build for {at}, which Gemini's pin doesn't build"));
            continue;
        };
        if b.get("variant").is_some() {
            found.push(format!("{at}: a libc variant, where Node's Linux builds are glibc's alone"));
        }
        let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        if files.len() != 1 + MEMBERS.len() {
            found.push(format!("{at}: {} files, where Gemini's build is the npm tarball and Node's two members", files.len()));
        }
        for f in &files {
            if !is_sha256(str_at(f, "/sha256")) || f.get("size").and_then(Value::as_u64).is_none_or(|s| s == 0) {
                found.push(format!("{at}: a file without its SHA-256 and size"));
            }
        }
        match files.iter().filter(|f| str_at(f, "/path") == ".").collect::<Vec<_>>().as_slice() {
            [tree] => {
                if str_at(tree, "/url") != package {
                    found.push(format!("{at}: its tree isn't {package}, the npm registry's tarball of the pin's version"));
                }
                if tree.get("mode").is_some() {
                    found.push(format!("{at}: a mode for the tree, whose entries keep their own"));
                }
                let u = |k: &str| tree.pointer(&format!("/unpack/{k}"));
                if u("format").and_then(Value::as_str) != Some("tar.gz") || u("strip").and_then(Value::as_u64) != Some(1) {
                    found.push(format!("{at}: its tree isn't tar.gz with its package/ stripped"));
                }
                if !["entries", "unpacked_bytes"].into_iter().all(|k| u(k).and_then(Value::as_u64).is_some_and(|n| n > 0)) {
                    found.push(format!("{at}: its tree lacks one of the totals the unpacker holds a tree to"));
                }
                if ["member", "sha256", "size", "alone"].into_iter().any(|k| u(k).is_some()) {
                    found.push(format!("{at}: one member's fields for the tree, which is taken whole"));
                }
                trees.insert(tree.to_string());
            }
            other => found.push(format!("{at}: {} trees at the harness's root, not the npm package alone", other.len())),
        }
        let archive = format!("https://nodejs.org/dist/{node}/node-{node}-linux-{arch}.tar.gz");
        for (member, path, mode) in MEMBERS {
            let Some(f) = files.iter().find(|f| str_at(f, "/path") == path) else {
                found.push(format!("{at}: no {path}, off the harness's PATH, from Node's archive"));
                continue;
            };
            if str_at(f, "/url") != archive {
                found.push(format!("{at}: its {path} isn't from {archive}, Node's own build for the platform"));
            }
            if str_at(f, "/mode") != mode {
                found.push(format!("{at}: its {path} isn't mode {mode}"));
            }
            let named = format!("node-{node}-linux-{arch}/{member}");
            if str_at(f, "/unpack/format") != "tar.gz" || str_at(f, "/unpack/member") != named {
                found.push(format!("{at}: its {path} isn't the archive's {named}"));
            }
            if !is_sha256(str_at(f, "/unpack/sha256")) || f.pointer("/unpack/size").and_then(Value::as_u64).is_none_or(|s| s == 0) {
                found.push(format!("{at}: its {path} is a member without its own SHA-256 and size"));
            }
            if ["strip", "entries", "unpacked_bytes"].into_iter().any(|k| f.pointer(&format!("/unpack/{k}")).is_some()) {
                found.push(format!("{at}: a tree's fields for its {path}, a member"));
            }
        }
        for f in &files {
            let path = str_at(f, "/path");
            if path != "." && !MEMBERS.iter().any(|(_, p, _)| *p == path) {
                found.push(format!("{at}: a file at {path}, which Gemini's build doesn't pin"));
            }
        }
    }
    if trees.len() > 1 {
        found.push("the builds' trees differ, where the npm package is one for both platforms".to_owned());
    }
    let smoke = json!({"program": MEMBERS.first().map(|(_, p, _)| *p), "args": [format!("{HARNESS}/{entry}"), "--version"], "stdout": version});
    if pin.get("smoke") != Some(&smoke) {
        found.push(format!("the smoke isn't {smoke}: its node on the package's entry, printing the bare version"));
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
/// arguments, and the variables it sets of its own (the PATH it was found on and kiln's own `KILN_*`
/// sockets aside, which are the host's).
fn launched(kind: &Kind) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let cwd = tempfile::tempdir()?;
    let spec = Spec { kind: kind.id.to_owned(), program: Some("/nowhere/agent".to_owned()), cwd: Some(cwd.path().to_string_lossy().into_owned()), ..Spec::default() };
    let p = kind.launch(&spec, "session")?;
    let args = p.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    let own = |k: &OsStr| k == OsStr::new("PATH") || k.to_string_lossy().starts_with("KILN_");
    let env = p.get_envs().filter(|(k, _)| !own(*k)).filter_map(|(k, v)| Some(format!("{}={}", k.to_string_lossy(), v?.to_string_lossy()))).collect();
    Ok((args, env))
}

/// Where a harness file departs from how `kind` launches Gemini CLI, given its pin: its command must be
/// the pin's node and the package's entry, each at the harness's path, then exactly the arguments the
/// kind launches with; and each variable the kind's launch sets of its own must be in its env.
fn kind_problems(h: &Value, pin: &Value, kind: &Kind) -> anyhow::Result<Vec<String>> {
    let (args, env) = launched(kind)?;
    let node = node_path(pin).ok_or_else(|| anyhow::anyhow!("the pin puts no bin/node in the harness's directory"))?;
    let entry = str_at(pin, "/verified/gemini/entry");
    let want: Vec<String> = [format!("{HARNESS}/{node}"), format!("{HARNESS}/{entry}")].into_iter().chain(args).collect();
    let mut found = Vec::new();
    let command = strings(h.pointer("/run/command")).unwrap_or_default();
    if command != want {
        found.push(format!("its command {command:?} isn't {want:?}"));
    }
    let given = strings(h.pointer("/run/env")).unwrap_or_default();
    found.extend(env.into_iter().filter(|e| !given.contains(e)).map(|e| format!("it lacks {e}, which the kind's launch sets")));
    Ok(found)
}

/// Where the system settings file and the harness file's pointer to it fail to turn Gemini's update
/// check off: the settings must be exactly general.enableAutoUpdate and enableAutoUpdateNotification,
/// both false (f23); and GEMINI_CLI_SYSTEM_SETTINGS_PATH must name, at the harness's path, a file kiln
/// writes into the harness's directory, which is this one.
fn settings_problems(settings: &str, h: &Value, files: &[(&str, &str)]) -> Vec<String> {
    let mut found = Vec::new();
    match serde_json::from_str::<Value>(settings) {
        Ok(s) if s == json!({"general": {"enableAutoUpdate": false, "enableAutoUpdateNotification": false}}) => {}
        Ok(s) => found.push(format!("the system settings {s} aren't the update check's two switches, off")),
        Err(e) => found.push(format!("the system settings aren't JSON: {e}")),
    }
    let env = strings(h.pointer("/run/env")).unwrap_or_default();
    let at = env.iter().find_map(|e| e.strip_prefix("GEMINI_CLI_SYSTEM_SETTINGS_PATH="));
    let named = at.and_then(|p| p.strip_prefix(HARNESS)?.strip_prefix('/'));
    match named.and_then(|n| files.iter().find(|(f, _)| *f == n)) {
        Some((_, text)) if *text == settings => {}
        _ => found.push(format!("GEMINI_CLI_SYSTEM_SETTINGS_PATH {at:?} names no file kiln writes with these settings")),
    }
    found
}

/// Words that bring in a credential of Gemini CLI's (research f21 and its bundle's own names): its key
/// and token variables, a service account's file, and the files it keeps a sign-in in.
const GEMINI_CREDENTIALS: [&str; 9] = [
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_ACCESS_TOKEN",
    "oauth_creds.json",
    "gemini-credentials.json",
    "mcp-oauth-tokens.json",
    "a2a-oauth-tokens.json",
    "application_default_credentials.json",
];

/// A variable named as a credential is: key, token, secret, password or credential, in any case.
fn named_like_a_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"].into_iter().any(|w| upper.contains(w))
}

/// What in `text` holds or names a credential: one of Gemini's words anywhere, comments included; a
/// `.env` file, which Gemini reads its keys from; a Google API key (`AIza` and 35 more of its
/// characters, a word of its own); an ENV or ARG named like a credential; and such a variable given a
/// value (`NAME=`, not inside base64 such as the pin's integrity), as a harness file's run.env would.
fn credentials_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = GEMINI_CREDENTIALS.into_iter().filter(|w| text.contains(w)).map(|w| format!("names {w}")).collect();
    if [" .env", "/.env", "\".env"].into_iter().any(|w| text.match_indices(w).any(|(i, m)| text.get(i + m.len()..).and_then(|r| r.chars().next()).is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_')))) {
        found.push("names a .env file".to_owned());
    }
    for l in instructions(text) {
        if let Some(rest) = l.strip_prefix("ENV ").or_else(|| l.strip_prefix("ARG ")) {
            found.extend(rest.split_whitespace().filter_map(|w| w.split('=').next()).filter(|n| named_like_a_credential(n)).map(|n| format!("`{l}` names {n}")));
        }
    }
    let key_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = None;
    let mut before = ' ';
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let word = text.get(s..i).unwrap_or_default();
                if c == '=' && !matches!(before, '+' | '/' | '-') && named_like_a_credential(word) {
                    found.push(format!("{word}= gives one a value"));
                }
                start = None;
            }
            _ => {}
        }
        if start.is_none() {
            before = c;
        }
    }
    for (i, _) in text.match_indices("AIza") {
        let lead = text.get(..i).and_then(|t| t.chars().next_back()).is_none_or(|c| !(key_char(c) || c == '+' || c == '/'));
        let tail: String = text.get(i + 4..).unwrap_or_default().chars().take_while(|c| key_char(*c)).collect();
        if lead && tail.len() == 35 {
            found.push("a Google API key (AIza…)".to_owned());
        }
    }
    found
}

/// Gemini's pin holds (its version, the npm package as one tree for both platforms, Node's own build
/// for each as two members off PATH, its checks recorded, its smoke); its harness file is an OSI config
/// of Gemini CLI at the pin's version whose env is exactly the plan's, HOME on the person's volume; and
/// its Agentfile builds on the base its pin was checked with by default.
#[test]
fn step21_gemini_pin_and_harness_file_hold() -> anyhow::Result<()> {
    let gemini = gemini()?;
    let problems = pin_problems(gemini.pin);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    let pin: Value = serde_json::from_str(gemini.pin)?;
    anyhow::ensure!(base_of(gemini.text) == pin.get("base").and_then(Value::as_str), "the Agentfile's BASE isn't its pin's");
    anyhow::ensure!(gemini.files.iter().map(|(n, _)| *n).eq(["harness.json", "system-settings.json"]), "Gemini's context files aren't its harness.json and system settings");
    let h: Value = serde_json::from_str(file(gemini, "harness.json")?)?;
    let problems = osi_problems(&h);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    anyhow::ensure!(h.get("name").and_then(Value::as_str) == Some(gemini.harness), "{h}");
    anyhow::ensure!(h.get("version") == pin.get("version"), "{h} isn't at the pin's version");
    let env = strings(h.pointer("/run/env")).unwrap_or_default();
    anyhow::ensure!(env == ["HOME=/home/gemini", "GEMINI_CLI_SYSTEM_SETTINGS_PATH=/harness/gemini/system-settings.json"], "{env:?}");
    let home = env.iter().find_map(|e| e.strip_prefix("HOME=")).unwrap_or_default();
    anyhow::ensure!(harness_volumes(gemini.text, "gemini") == ["/workspace", home], "HOME {home} isn't Gemini's second volume");
    let moved = gemini.text.replace("harness /home/gemini FOR", "harness /home/gemini/.gemini FOR");
    anyhow::ensure!(harness_volumes(&moved, "gemini") != ["/workspace", home]);
    Ok(())
}

/// A pin or harness file no build could trust is caught: a digest one digit short; a platform missing,
/// twice or another, or a libc variant; the npm tree not stripped of package/, not tar.gz, a total
/// missing or 0, a member's fields, a mode, another place, another version's tarball, a URL elsewhere
/// or over plain HTTP; a Node member at bin/node (on PATH), from another platform's archive or another
/// Node, without its own digest, another mode, or gone; a base by tag; a version that isn't x.y.z; a
/// Node of a major Gemini doesn't test; and in the harness file a schema it isn't, a field Shards
/// refuses, a name no Agentfile gives, no command, and an env entry with no value or the runtime's own.
#[test]
fn step21_a_gemini_pin_or_harness_file_that_would_mislead_a_build_is_caught() -> anyhow::Result<()> {
    let gemini = gemini()?;
    let pin_says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(pin_problems(&edit(gemini.pin, path, to)?.to_string()).iter().any(|p| p.contains(what))) };
    let real: Value = serde_json::from_str(gemini.pin)?;
    let field = |p: &str| str_at(&real, p).to_owned();
    let short = field("/builds/0/files/0/sha256").get(1..).map(Value::from);
    anyhow::ensure!(pin_says("/builds/0/files/0/sha256", short, "without its SHA-256")?);
    anyhow::ensure!(pin_says("/builds/1/platform", Some(Value::from("linux/amd64")), "no build for linux/arm64")?);
    anyhow::ensure!(pin_says("/builds/1/platform", Some(Value::from("linux/amd64")), "two builds")?);
    anyhow::ensure!(pin_says("/builds/1/platform", Some(Value::from("linux/arm/v7")), "doesn't build")?);
    anyhow::ensure!(pin_says("/builds/0/variant", Some(Value::from("gnu")), "libc variant")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/strip", Some(Value::from(0)), "package/ stripped")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/format", Some(Value::from("zip")), "isn't tar.gz")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/entries", None, "totals")?);
    anyhow::ensure!(pin_says("/builds/1/files/0/unpack/unpacked_bytes", Some(Value::from(0)), "totals")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/unpack/member", Some(Value::from("package/bundle/gemini.js")), "member's fields")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/mode", Some(Value::from("0755")), "a mode for the tree")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/path", Some(Value::from("lib")), "trees at the harness's root")?);
    anyhow::ensure!(pin_says("/builds/1/files/0/unpack/entries", Some(Value::from(450)), "trees differ")?);
    let tarball = field("/builds/0/files/0/url");
    anyhow::ensure!(pin_says("/builds/0/files/0/url", Some(Value::from(tarball.replacen("https://", "http://", 1))), "npm registry's tarball")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/url", Some(Value::from(tarball.replacen("registry.npmjs.org", "registry.example.com", 1))), "npm registry's tarball")?);
    anyhow::ensure!(pin_says("/builds/0/files/0/url", Some(Value::from(tarball.replace(&field("/version"), "0.67.0"))), "of the pin's version")?);
    anyhow::ensure!(pin_says("/builds/0/files/1/path", Some(Value::from("bin/node")), "no node/bin/node, off the harness's PATH")?);
    anyhow::ensure!(pin_says("/builds/1/files/1/url", Some(Value::from(field("/builds/0/files/1/url"))), "Node's own build for the platform")?);
    let other_node = field("/builds/0/files/1/unpack/member").replacen(&field("/verified/node/version"), "v24.22.2", 1);
    anyhow::ensure!(pin_says("/builds/0/files/1/unpack/member", Some(Value::from(other_node)), "isn't the archive's")?);
    anyhow::ensure!(pin_says("/builds/0/files/1/unpack/sha256", None, "without its own SHA-256")?);
    anyhow::ensure!(pin_says("/builds/0/files/2/mode", Some(Value::from("0755")), "isn't mode 0644")?);
    anyhow::ensure!(pin_says("/builds/1/files/2", None, "no node/LICENSE")?);
    anyhow::ensure!(pin_says("/builds/0/files/1/unpack/strip", Some(Value::from(1)), "a tree's fields")?);
    anyhow::ensure!(pin_says("/base", Some(Value::from("buildpack-deps:trixie-scm")), "pinned by digest")?);
    anyhow::ensure!(pin_says("/version", Some(Value::from("0.68")), "isn't x.y.z")?);
    anyhow::ensure!(pin_says("/verified/node/major", Some(Value::from(26)), "a major Gemini's Linux CI tests")?);
    anyhow::ensure!(pin_says("/verified/gemini/integrity", Some(Value::from("sha1-x")), "signed integrity")?);

    let h = file(gemini, "harness.json")?;
    let osi_says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(osi_problems(&edit(h, path, to)?).iter().any(|p| p.contains(what))) };
    anyhow::ensure!(osi_says("/schemaVersion", Some(Value::from(2)), "schemaVersion")?);
    anyhow::ensure!(osi_says("/command", Some(Value::from(vec!["gemini"])), "`command`")?);
    anyhow::ensure!(osi_says("/run/cwd", Some(Value::from("/workspace")), "`run.cwd`")?);
    anyhow::ensure!(osi_says("/name", Some(Value::from("Gemini")), "no name")?);
    anyhow::ensure!(osi_says("/run/command", Some(Value::from(Vec::<String>::new())), "run.command")?);
    anyhow::ensure!(osi_says("/run/env", Some(Value::from(vec!["HOME"])), "NAME=value")?);
    anyhow::ensure!(osi_says("/run/env", Some(Value::from(vec!["SHARDS_DOMAIN=gemini"])), "NAME=value")?);
    anyhow::ensure!(pin_problems(gemini.pin).is_empty() && osi_problems(&serde_json::from_str::<Value>(h)?).is_empty());
    Ok(())
}

/// Gemini's harness file runs Gemini CLI as kiln's Gemini kind launches it, for a default spec: its
/// command is the pin's node on the package's entry, at the harness's path, then exactly the kind's
/// launch arguments (`--acp`); and it sets each variable the kind's launch sets of its own (none). A
/// kind whose arguments change alone, a launch that sets a variable of its own, a harness file that
/// drifts, and a pin whose node moves each fail it.
#[test]
fn step21_gemini_runs_as_kilns_gemini_kind_launches_it() -> anyhow::Result<()> {
    let gemini = gemini()?;
    let kind = Kind::get("gemini").ok_or_else(|| anyhow::anyhow!("no gemini kind"))?;
    anyhow::ensure!(kind.protocol == Protocol::Acp(&["--acp"]), "kiln no longer starts Gemini CLI as `--acp`");
    let pin: Value = serde_json::from_str(gemini.pin)?;
    let h: Value = serde_json::from_str(file(gemini, "harness.json")?)?;
    let problems = kind_problems(&h, &pin, kind)?;
    anyhow::ensure!(problems.is_empty(), "{problems:?}");

    let other_args = Kind { protocol: Protocol::Acp(&["--experimental-acp"]), ..*kind };
    anyhow::ensure!(!kind_problems(&h, &pin, &other_args)?.is_empty());
    // kiln's launch sets NO_OPEN_BROWSER for the kind whose id is cursor's alone: that id is a launch
    // that sets a variable of its own, nothing else changed.
    let other_env = Kind { id: "cursor", ..*kind };
    anyhow::ensure!(!launched(&other_env)?.1.is_empty() && !kind_problems(&h, &pin, &other_env)?.is_empty());
    let mut drifted = h.clone();
    if let Some(c) = drifted.pointer_mut("/run/command").and_then(Value::as_array_mut) {
        c.retain(|a| a.as_str() != Some("--acp"));
    }
    anyhow::ensure!(kind_problems(&drifted, &pin, kind)?.len() == 1);
    let moved = edit(gemini.pin, "/builds/0/files/1/path", Some(Value::from("lib/node")))?;
    anyhow::ensure!(node_path(&moved) == Some("lib/node") && !kind_problems(&h, &moved, kind)?.is_empty());
    let entry = edit(gemini.pin, "/verified/gemini/entry", Some(Value::from("dist/index.js")))?;
    anyhow::ensure!(!kind_problems(&h, &entry, kind)?.is_empty());
    Ok(())
}

/// Gemini's system settings turn its update check and its installs off in every mode (f23), and the
/// harness file points Gemini at them where kiln writes them; settings that turn either back on, add
/// another, or a pointer elsewhere or gone each fail it.
#[test]
fn step21_gemini_system_settings_turn_its_update_check_off() -> anyhow::Result<()> {
    let gemini = gemini()?;
    let settings = file(gemini, "system-settings.json")?;
    let h: Value = serde_json::from_str(file(gemini, "harness.json")?)?;
    let problems = settings_problems(settings, &h, gemini.files);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    let caught = |settings: &str, h: &Value| !settings_problems(settings, h, gemini.files).is_empty();
    anyhow::ensure!(caught(&settings.replace("\"enableAutoUpdate\": false", "\"enableAutoUpdate\": true"), &h));
    anyhow::ensure!(caught(&edit(settings, "/general/enableAutoUpdateNotification", None)?.to_string(), &h));
    anyhow::ensure!(caught(&edit(settings, "/security", Some(json!({"folderTrust": {"enabled": false}})))?.to_string(), &h));
    anyhow::ensure!(caught("{", &h));
    let elsewhere = edit(&h.to_string(), "/run/env", Some(json!(["HOME=/home/gemini", "GEMINI_CLI_SYSTEM_SETTINGS_PATH=/etc/gemini-cli/settings.json"])))?;
    anyhow::ensure!(caught(settings, &elsewhere));
    let unwritten = edit(&h.to_string(), "/run/env", Some(json!(["HOME=/home/gemini", "GEMINI_CLI_SYSTEM_SETTINGS_PATH=/harness/gemini/settings.json"])))?;
    anyhow::ensure!(caught(settings, &unwritten));
    let gone = edit(&h.to_string(), "/run/env", Some(json!(["HOME=/home/gemini"])))?;
    anyhow::ensure!(caught(settings, &gone));
    Ok(())
}

/// Gemini's Agentfile is the plan's shape: ARG BASE by digest and ARG TARGETARCH alone, the pinned
/// harness per platform, exactly the bring-up's three grants, exactly its two harness volumes, and no
/// ENV or RUN.
#[test]
fn step21_gemini_agentfile_is_the_plans_shape() -> anyhow::Result<()> {
    let want = [
        "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
        "FROM ${BASE}",
        "ARG TARGETARCH",
        "HARNESS gemini FROM ./gemini-${TARGETARCH}",
        "NETWORK --dns --egress=443/tcp provider FOR gemini",
        "EXPOSE --harnesses=gemini 443/tcp AS egress FOR provider",
        "CONNECT gemini WITH gemini ON provider",
        "VOLUME --target-kind=harness /workspace FOR gemini",
        "VOLUME --target-kind=harness /home/gemini FOR gemini",
    ];
    let text = gemini()?.text;
    anyhow::ensure!(instructions(text) == want, "{:#?}", instructions(text));
    for changed in [format!("{text}RUN npm install -g @google/gemini-cli\n"), format!("{text}ENV PATH=/harness/gemini/node/bin:$PATH\n"), text.replace("443/tcp provider", "80/tcp provider")] {
        anyhow::ensure!(instructions(&changed) != want, "{changed}");
    }
    Ok(())
}

/// Nothing kiln embeds for Gemini holds or names a credential, nor an ENV or ARG named like one; each
/// way of adding one is caught, the plan's ENV GEMINI_API_KEY=x in the embedded Agentfile first.
#[test]
fn step21_no_gemini_file_holds_a_credential() -> anyhow::Result<()> {
    let gemini = gemini()?;
    for (name, text) in [("Agentfile", gemini.text), ("pin.json", gemini.pin)].into_iter().chain(gemini.files.iter().copied()) {
        let found = credentials_in(text);
        anyhow::ensure!(found.is_empty(), "{name}: {found:?}");
    }
    let caught = |text: &str| !credentials_in(text).is_empty();
    anyhow::ensure!(caught(&format!("{}ENV GEMINI_API_KEY=x\n", gemini.text)));
    anyhow::ensure!(caught(&format!("{}ARG GOOGLE_API_KEY\n", gemini.text)));
    anyhow::ensure!(caught(&format!("{}ENV GOOGLE_APPLICATION_CREDENTIALS /home/gemini/sa.json\n", gemini.text)));
    anyhow::ensure!(caught(&format!("{}ARG NPM_PASSWORD\n", gemini.text)));
    anyhow::ensure!(caught(&format!("{}COPY oauth_creds.json /home/gemini/.gemini/\n", gemini.text)));
    anyhow::ensure!(caught(&format!("{}COPY .env /home/gemini/.gemini/.env\n", gemini.text)));
    anyhow::ensure!(caught(&format!("{}# AIzaSyA0123456789abcdefghijklmnopqrstuv\n", gemini.text)));
    let h = file(gemini, "harness.json")?;
    anyhow::ensure!(caught(&h.replace("\"HOME=/home/gemini\"", "\"MY_SECRET=s\"")));
    anyhow::ensure!(caught(&h.replace("\"HOME=/home/gemini\"", "\"GOOGLE_CLOUD_ACCESS_TOKEN=ya29\"")));
    let settings = file(gemini, "system-settings.json")?;
    anyhow::ensure!(caught(&settings.replace("\"general\"", "\"apiKey\": \"AIzaSyA0123456789abcdefghijklmnopqrstuv\", \"general\"")));
    anyhow::ensure!(caught(&gemini.pin.replace("\"harness\"", "\"sign_in\": \"gemini-credentials.json\", \"harness\"")));
    Ok(())
}

/// The pin's smoke is the harness's own node on the package's entry at the harness's path, `--version`,
/// printing the bare version and nothing else (f19); another program, Node's own version, a version
/// with Gemini's name before it, `--version` alone, or no smoke each fail it.
#[test]
fn step21_gemini_smoke_runs_its_node_on_its_entry() -> anyhow::Result<()> {
    let gemini = gemini()?;
    let pin: Value = serde_json::from_str(gemini.pin)?;
    anyhow::ensure!(str_at(&pin, "/smoke/stdout") == str_at(&pin, "/version") && str_at(&pin, "/smoke/program") == "node/bin/node", "{:?}", pin.get("smoke"));
    let version = str_at(&pin, "/version").to_owned();
    let says = |path: &str, to: Option<Value>| -> anyhow::Result<bool> { Ok(pin_problems(&edit(gemini.pin, path, to)?.to_string()).iter().any(|p| p.contains("the smoke"))) };
    anyhow::ensure!(says("/smoke/program", Some(Value::from("bin/gemini")))?);
    anyhow::ensure!(says("/smoke/stdout", Some(Value::from(str_at(&pin, "/verified/node/version"))))?);
    anyhow::ensure!(says("/smoke/stdout", Some(Value::from(format!("gemini {version}"))))?);
    anyhow::ensure!(says("/smoke/args", Some(json!(["--version"])))?);
    anyhow::ensure!(says("/smoke/env", Some(json!(["HOME=/tmp"])))?);
    anyhow::ensure!(says("/smoke", None)?);
    Ok(())
}

/// Every path under `dir`, relative and `/`-separated, with its metadata (links not followed), sorted.
fn walk(dir: &std::path::Path) -> anyhow::Result<Vec<(String, std::fs::Metadata)>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d)? {
            let path = e?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                todo.push(path.clone());
            }
            out.push((path.strip_prefix(dir)?.to_string_lossy().replace('\\', "/"), meta));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// A file's SHA-256, in hex, read a MiB at a time.
fn sha256_of(path: &std::path::Path) -> anyhow::Result<String> {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hash.finalize()));
        }
        hash.update(buf.get(..n).unwrap_or_default());
    }
}

/// Live, where a test names a folder for it (`KILN_TEST_LIVE_CONTEXT`, under `~/.cache` on kiln's Linux
/// machines): the fetcher makes Gemini's context for linux/amd64 and linux/arm64 from npm's registry and
/// nodejs.org, through the one unpacker. Each harness directory holds the npm package's files at its
/// root, as many and as large in all as the pin's tree says; node/bin/node and node/LICENSE alone under
/// node/, each hashing to its pinned member; and kiln's harness.json and system settings as embedded.
/// Nothing in it is a link, every directory is 0755 and no file is writable by its group or others, as
/// Gemini needs of its system settings and the directories above them (f24). Gemini isn't run.
#[test]
fn step21_a_live_gemini_context_holds_the_pinned_builds() -> anyhow::Result<()> {
    let Some(root) = std::env::var_os("KILN_TEST_LIVE_CONTEXT") else {
        eprintln!("skipped: KILN_TEST_LIVE_CONTEXT names no folder");
        return Ok(());
    };
    let gemini = gemini()?;
    let pin: Value = serde_json::from_str(gemini.pin)?;
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir_in(&root)?;
        let platforms: Vec<String> = ARCHES.iter().map(|(p, _)| (*p).to_owned()).collect();
        crate::agentfiles::context::materialize(gemini, &platforms, None, dir.path()).await?;
        let top: Vec<String> = walk(dir.path())?.into_iter().map(|(p, _)| p).filter(|p| !p.contains('/')).collect();
        anyhow::ensure!(top == ["Agentfile", "gemini-amd64", "gemini-arm64"], "the context holds {top:?}");
        let entry = str_at(&pin, "/verified/gemini/entry");
        let mut matched = 0;
        for (i, (platform, _)) in ARCHES.iter().enumerate() {
            let harness = dir.path().join(format!("gemini-{}", platform.trim_start_matches("linux/")));
            let all = walk(&harness)?;
            let links: Vec<&String> = all.iter().filter(|(_, m)| m.file_type().is_symlink()).map(|(p, _)| p).collect();
            anyhow::ensure!(links.is_empty(), "{platform}: links {links:?}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let root = std::fs::metadata(&harness)?.permissions().mode() & 0o7777;
                anyhow::ensure!(root == 0o755, "{platform}: the harness's directory is {root:o}");
                let loose: Vec<String> = all
                    .iter()
                    .filter(|(_, m)| if m.is_dir() { m.permissions().mode() & 0o7777 != 0o755 } else { m.permissions().mode() & 0o022 != 0 })
                    .map(|(p, m)| format!("{p} {:o}", m.permissions().mode() & 0o7777))
                    .collect();
                anyhow::ensure!(loose.is_empty(), "{platform}: a directory not 0755 or a file others may write: {loose:?}");
            }
            let under_node: Vec<&str> = all.iter().map(|(p, _)| p.as_str()).filter(|p| *p == "node" || p.starts_with("node/")).collect();
            anyhow::ensure!(under_node == ["node", "node/LICENSE", "node/bin", "node/bin/node"], "{platform}: node/ holds {under_node:?}");
            let kiln_files = ["harness.json", "system-settings.json"];
            for name in kiln_files {
                anyhow::ensure!(std::fs::read_to_string(harness.join(name))? == file(gemini, name)?, "{platform}: {name} isn't the embedded one");
            }
            let tree: Vec<&(String, std::fs::Metadata)> = all.iter().filter(|(p, m)| m.is_file() && !p.starts_with("node/") && !kiln_files.contains(&p.as_str())).collect();
            let bytes: u64 = tree.iter().map(|(_, m)| m.len()).sum();
            let (want_files, want_bytes) = (pin.pointer("/verified/gemini/files").and_then(Value::as_u64), pin.pointer(&format!("/builds/{i}/files/0/unpack/unpacked_bytes")).and_then(Value::as_u64));
            anyhow::ensure!(u64::try_from(tree.len()).ok() == want_files && Some(bytes) == want_bytes, "{platform}: the package's {} files of {bytes} bytes, pinned {want_files:?} of {want_bytes:?}", tree.len());
            anyhow::ensure!(tree.iter().any(|(p, _)| p == entry), "{platform}: no {entry}");
            for (member, path, _) in MEMBERS {
                let pinned = pin.pointer(&format!("/builds/{i}/files")).and_then(Value::as_array).and_then(|fs| fs.iter().find(|f| str_at(f, "/path") == path)).ok_or_else(|| anyhow::anyhow!("{platform}: no {path} pinned"))?;
                let got = sha256_of(&harness.join(path))?;
                anyhow::ensure!(got == str_at(pinned, "/unpack/sha256"), "{platform}: {path} SHA-256 {got}, the pin's {member} {}", str_at(pinned, "/unpack/sha256"));
                println!("MATCH {platform} {path} {got}");
                matched += 1;
            }
            println!("TREE {platform} {} files {bytes} bytes", tree.len());
        }
        anyhow::ensure!(matched == 2 * MEMBERS.len(), "{matched} of {} members match their pins", 2 * MEMBERS.len());
        Ok(())
    })
}
