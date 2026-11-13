//! Goose's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 23; plan
//! 01a1ce705e3d72af83ef15ae83b62f45, step 2): what its pin, its harness.json and its Agentfile say of one
//! another, of kiln's Kind `goose`, and of credentials. Each check reads texts, so each test also hands
//! it changed copies, and each change must fail it.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{ALL, Builtin};
use crate::harness::kinds::{Kind, Protocol};

fn goose() -> anyhow::Result<&'static Builtin> {
    ALL.iter().find(|b| b.harness == "goose").ok_or_else(|| anyhow::anyhow!("no built-in Agentfile runs goose"))
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

/// A built-in's harness file, as JSON.
fn harness_file(b: &Builtin) -> anyhow::Result<Value> {
    let text = b.files.iter().find(|(n, _)| *n == "harness.json").map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("{}: no harness.json", b.name))?;
    Ok(serde_json::from_str(text)?)
}

/// The command a harness file starts its harness with.
fn command_of(h: &Value) -> Vec<String> {
    h.pointer("/run/command").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// What kiln runs for `kind` when it's an ACP harness: its program, then its arguments.
fn acp_command(kind: &Kind) -> Option<Vec<String>> {
    match kind.protocol {
        Protocol::Acp(args) => Some(std::iter::once(kind.program).chain(args.iter().copied()).map(str::to_owned).collect()),
        _ => None,
    }
}

fn is_sha256(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A path relative to a folder that stays inside it: no root, and no empty, `.` or `..` part.
fn is_inside(p: &str) -> bool {
    !p.starts_with('/') && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// What in Goose's pin would mislead a build or its smoke:
/// - a version not x.y.z, a base not pinned by digest, a platform missing or twice;
/// - per file, no https URL, SHA-256 or size, a path outside its folder, a mode that isn't octal
///   permissions, an archive's `unpack` not naming its one member with that member's own SHA-256
///   and size;
/// - a smoke that isn't `bin/goose --version` printing its version after a space (`stdout` is that
///   line without its newline, as the other harnesses' pins write theirs).
fn pin_problems(text: &str) -> Vec<String> {
    let pin: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return vec![format!("not JSON: {e}")],
    };
    let mut found = Vec::new();
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    if !(version.split('.').count() == 3 && version.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))) {
        found.push(format!("version `{version}` isn't x.y.z"));
    }
    let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
    if !base.rsplit_once("@sha256:").is_some_and(|(name, d)| !name.is_empty() && is_sha256(d)) {
        found.push(format!("base `{base}` isn't pinned by digest"));
    }
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    for platform in ["linux/amd64", "linux/arm64"] {
        if !builds.iter().any(|b| b.get("platform").and_then(Value::as_str) == Some(platform)) {
            found.push(format!("no build for {platform}"));
        }
    }
    let mut seen = BTreeSet::new();
    for b in &builds {
        let at = format!("{} {}", b.get("platform").and_then(Value::as_str).unwrap_or_default(), b.get("variant").and_then(Value::as_str).unwrap_or_default());
        if !seen.insert(at.clone()) {
            found.push(format!("two builds for {at}"));
        }
        let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        if files.is_empty() {
            found.push(format!("{at}: no files"));
        }
        for f in &files {
            let s = |k: &str| f.get(k).and_then(Value::as_str);
            if !s("url").is_some_and(|u| u.starts_with("https://")) {
                found.push(format!("{at}: no https URL"));
            }
            if !s("sha256").is_some_and(is_sha256) || f.get("size").and_then(Value::as_u64).is_none() {
                found.push(format!("{at}: a file without its SHA-256 and size"));
            }
            if !s("path").is_some_and(is_inside) {
                found.push(format!("{at}: a path outside its folder"));
            }
            if !s("mode").is_some_and(|m| (3..=4).contains(&m.len()) && m.bytes().all(|b| (b'0'..=b'7').contains(&b))) {
                found.push(format!("{at}: a mode that isn't octal permissions"));
            }
            if let Some(u) = f.get("unpack") {
                let us = |k: &str| u.get(k).and_then(Value::as_str);
                if us("format") != Some("tar.gz") {
                    found.push(format!("{at}: an unpack that isn't tar.gz"));
                }
                if !us("member").map(|m| m.strip_prefix("./").unwrap_or(m)).is_some_and(is_inside) {
                    found.push(format!("{at}: a member outside the folder"));
                }
                if !us("sha256").is_some_and(is_sha256) || u.get("size").and_then(Value::as_u64).is_none() {
                    found.push(format!("{at}: a member without its own SHA-256 and size"));
                }
            }
        }
    }
    let smoke = |k: &str| pin.get("smoke").and_then(|s| s.get(k));
    let program = smoke("program").and_then(Value::as_str).unwrap_or_default();
    let is_file = |b: &Value| b.get("files").and_then(Value::as_array).is_some_and(|fs| fs.iter().any(|f| f.get("path").and_then(Value::as_str) == Some(program)));
    if !builds.iter().all(is_file) {
        found.push(format!("the smoke runs `{program}`, which isn't a file of every build"));
    }
    if smoke("args") != Some(&Value::from(vec!["--version"])) || smoke("env").is_some() {
        found.push("the smoke isn't `--version` alone".to_owned());
    }
    let (line, want) = (smoke("stdout").and_then(Value::as_str).unwrap_or_default(), format!(" {version}"));
    if line != want {
        found.push(format!("the smoke's line {line:?} isn't Goose's {want:?}"));
    }
    found
}

/// What in `text` holds or names a credential: an ENV or ARG named like one, a variable named like
/// one given a value (`OPENAI_API_KEY=…`, in a harness file's env too), a provider's key (`sk-…`), or
/// Goose's own secrets file, secrets.yaml, which belongs on its owner's volume alone.
fn credentials_in(text: &str) -> Vec<String> {
    const WORDS: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    let named = |name: &str| WORDS.iter().any(|w| name.to_ascii_uppercase().contains(w));
    let mut found = Vec::new();
    for l in instructions(text) {
        let Some(rest) = l.strip_prefix("ENV ").or_else(|| l.strip_prefix("ARG ")) else { continue };
        for name in rest.split_whitespace().map(|w| w.split('=').next().unwrap_or(w)) {
            if named(name) {
                found.push(format!("{l}: names {name}"));
            }
        }
    }
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (is_word(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let word = text.get(s..i).unwrap_or_default();
                if c == '=' && named(word) {
                    found.push(format!("{word}= gives a value"));
                }
                let tail = text.get(i..).unwrap_or_default();
                if word == "sk" && tail.starts_with('-') && tail.chars().skip(1).take_while(|c| is_word(*c) || *c == '-').count() >= 20 {
                    found.push("a provider key (sk-…)".to_owned());
                }
                start = None;
            }
            _ => {}
        }
    }
    if text.contains("secrets.yaml") {
        found.push("Goose's secrets.yaml".to_owned());
    }
    found
}

/// Goose's pin holds (both platforms, every digest a SHA-256, the archive's member with its own, the
/// smoke Goose's), its harness file is an OSI config of Goose at the pin's version, and its Agentfile
/// builds on the base its pin was checked with by default.
#[test]
fn step23_goose_pin_and_harness_file_hold() -> anyhow::Result<()> {
    let goose = goose()?;
    let problems = pin_problems(goose.pin);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    let pin: Value = serde_json::from_str(goose.pin)?;
    let (version, base) = (pin.get("version").and_then(Value::as_str), pin.get("base").and_then(Value::as_str));
    anyhow::ensure!(base_of(goose.text) == base, "the Agentfile's BASE isn't its pin's {base:?}");
    let h = harness_file(goose)?;
    anyhow::ensure!(h.get("schemaVersion") == Some(&Value::from(1)), "{h}");
    anyhow::ensure!(h.get("name").and_then(Value::as_str) == Some(goose.harness), "{h}");
    anyhow::ensure!(h.get("version").and_then(Value::as_str) == version, "{h}");
    anyhow::ensure!(!command_of(&h).is_empty(), "no run.command");
    Ok(())
}

/// A pin no build or smoke could trust is caught: a digest one digit short, a platform missing, an
/// archive's member without its own digest, a member path that climbs out, a base by tag, and a
/// smoke that runs another program, asks something else or expects another line ('goose 1.59.0',
/// or '1.59.0' without Goose's leading space).
#[test]
fn step23_a_pin_that_would_mislead_a_build_is_caught() -> anyhow::Result<()> {
    let goose = goose()?;
    let real: Value = serde_json::from_str(goose.pin)?;
    let edited = |path: &str, to: Option<Value>| -> anyhow::Result<String> {
        let mut v = real.clone();
        let (parent, key) = path.rsplit_once('/').ok_or_else(|| anyhow::anyhow!("{path}"))?;
        let at = v.pointer_mut(parent).and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("{path}"))?;
        match to {
            Some(to) => at.insert(key.to_owned(), to),
            None => at.remove(key),
        };
        Ok(v.to_string())
    };
    let short = real.pointer("/builds/0/files/0/sha256").and_then(Value::as_str).and_then(|d| d.get(1..)).map(Value::from);
    let says = |text: String, what: &str| pin_problems(&text).iter().any(|p| p.contains(what));
    anyhow::ensure!(says(edited("/builds/0/files/0/sha256", short)?, "without its SHA-256"));
    anyhow::ensure!(says(edited("/builds/1/platform", Some(Value::from("linux/amd64")))?, "no build for linux/arm64"));
    anyhow::ensure!(says(edited("/builds/0/files/0/unpack/sha256", None)?, "member without its own SHA-256"));
    anyhow::ensure!(says(edited("/builds/0/files/0/unpack/member", Some(Value::from("../goose")))?, "member outside"));
    anyhow::ensure!(says(edited("/base", Some(Value::from("buildpack-deps:trixie-scm")))?, "pinned by digest"));
    let version = real.get("version").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(says(edited("/smoke/stdout", Some(Value::from(version)))?, "the smoke's line"));
    anyhow::ensure!(says(edited("/smoke/stdout", Some(Value::from(format!("goose {version}"))))?, "the smoke's line"));
    anyhow::ensure!(says(edited("/smoke/program", Some(Value::from("goose")))?, "isn't a file of every build"));
    anyhow::ensure!(says(edited("/smoke/args", Some(Value::from(vec!["info"])))?, "`--version` alone"));
    anyhow::ensure!(says(edited("/smoke", None)?, "the smoke"));
    anyhow::ensure!(pin_problems(goose.pin).is_empty());
    Ok(())
}

/// Nothing kiln ships for Goose holds a credential or names an ENV or ARG like one; adding one is
/// caught.
#[test]
fn step23_no_goose_file_holds_a_credential() -> anyhow::Result<()> {
    let goose = goose()?;
    for (name, text) in [("Agentfile", goose.text), ("pin.json", goose.pin)].into_iter().chain(goose.files.iter().copied()) {
        let found = credentials_in(text);
        anyhow::ensure!(found.is_empty(), "{name}: {found:?}");
    }
    anyhow::ensure!(!credentials_in(&format!("{}ENV OPENAI_API_KEY=x\n", goose.text)).is_empty());
    anyhow::ensure!(!credentials_in("ARG GITHUB_TOKEN\n").is_empty());
    anyhow::ensure!(!credentials_in(r#"{"env":["ANTHROPIC_API_KEY=y"]}"#).is_empty());
    anyhow::ensure!(!credentials_in("# sk-proj-abcdefghijklmnopqrstuvwx\n").is_empty());
    anyhow::ensure!(!credentials_in(&format!("{}COPY secrets.yaml /home/goose/.local/share/goose/config/\n", goose.text)).is_empty());
    Ok(())
}

/// Goose runs as kiln's Goose kind starts it (`goose acp`), its HOME the volume the Agentfile gives it
/// (step 18), its root beneath it and its caches on the scratch; a kind or a volume that drifts from
/// the files is caught.
#[test]
fn step23_goose_runs_as_kilns_goose_kind_with_its_root_on_its_volume() -> anyhow::Result<()> {
    let goose = goose()?;
    let kind = Kind::get("goose").ok_or_else(|| anyhow::anyhow!("no goose kind"))?;
    let h = harness_file(goose)?;
    anyhow::ensure!(Some(command_of(&h)) == acp_command(kind), "{h} isn't {:?}", acp_command(kind));
    let drifted = Kind { protocol: Protocol::Acp(&["--acp"]), ..*kind };
    anyhow::ensure!(Some(command_of(&h)) != acp_command(&drifted));

    let env: Vec<&str> = h.pointer("/run/env").and_then(Value::as_array).map(|e| e.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let root = env.iter().find_map(|e| e.strip_prefix("GOOSE_PATH_ROOT=")).ok_or_else(|| anyhow::anyhow!("no GOOSE_PATH_ROOT in {env:?}"))?;
    let home = env.iter().find_map(|e| e.strip_prefix("HOME=")).ok_or_else(|| anyhow::anyhow!("no HOME in {env:?}"))?;
    anyhow::ensure!(env == ["HOME=/home/goose", "GOOSE_PATH_ROOT=/home/goose/.local/share/goose", "XDG_CACHE_HOME=/tmp/.cache"], "{env:?}");
    anyhow::ensure!(root.starts_with(&format!("{home}/")), "GOOSE_PATH_ROOT={root} isn't beneath HOME={home}");
    let volumes = harness_volumes(goose.text, "goose");
    anyhow::ensure!(volumes == [home], "{volumes:?}");
    let moved = goose.text.replace("/home/goose FOR goose", "/home/goose/.goose FOR goose");
    anyhow::ensure!(harness_volumes(&moved, "goose").first() != Some(&home));
    Ok(())
}

/// Goose's Agentfile is the plan's shape: a base by digest, Goose's own build per platform with no
/// RUN, its providers' hosts by name and kiln's MCP (step 18's grants), its HOME as its one volume,
/// and no ENV; its pin's smoke line is what Goose prints.
#[test]
fn step23_goose_agentfile_is_the_plans_shape() -> anyhow::Result<()> {
    let goose = goose()?;
    let lines = instructions(goose.text);
    let want = [
        "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
        "FROM ${BASE}",
        "ARG TARGETARCH",
        "HARNESS goose FROM ./goose-${TARGETARCH}",
    ];
    let want: Vec<String> = want.iter().map(|l| (*l).to_owned()).chain(super::grants_tests::grant_lines("goose")).chain([super::grants_tests::volume_line("goose")]).collect();
    anyhow::ensure!(lines == want, "{lines:#?}");
    anyhow::ensure!(!lines.iter().any(|l| l.starts_with("RUN ") || l.starts_with("ENV ")));
    // The bytes 1.59.0's `goose --version` wrote (step 23's research, f12): the pin's line, then its
    // newline.
    let pin: Value = serde_json::from_str(goose.pin)?;
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    let line = pin.pointer("/smoke/stdout").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(version != "1.59.0" || format!("{line}\n").as_bytes() == [0x20, 0x31, 0x2e, 0x35, 0x39, 0x2e, 0x30, 0x0a], "{line:?}");
    Ok(())
}
