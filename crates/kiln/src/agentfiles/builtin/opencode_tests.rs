//! opencode's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 22; plan
//! 01a1ce705e2f7773af54b299f1de42f5, step 2, whose research 01a1ce705e307593bd11f26bf9f7b9a9 the fN
//! are findings of): what its pin, its harness.json and its Agentfile say of one another, of kiln's Kind
//! `opencode`, and of credentials. Each check reads texts, so each test also hands it changed copies,
//! and each change must fail it.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{ALL, Builtin};
use crate::harness::kinds::{Kind, Protocol};

fn opencode() -> anyhow::Result<&'static Builtin> {
    ALL.iter().find(|b| b.name == "harness-opencode").ok_or_else(|| anyhow::anyhow!("no harness-opencode"))
}

fn harness_json(b: &Builtin) -> anyhow::Result<Value> {
    let (_, text) = b.files.iter().find(|(name, _)| *name == "harness.json").ok_or_else(|| anyhow::anyhow!("{}: no harness.json", b.name))?;
    Ok(serde_json::from_str(text)?)
}

/// An Agentfile's instructions: its lines that are neither blank nor comments.
fn instructions(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

fn strings(v: Option<&Value>) -> Vec<&str> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// X.Y.Z with X 2 or more: OpenCode 2, which has the opencode/child-session-updates kiln's ACP adapter
/// asks for (f6).
fn is_opencode_2(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
        && parts.first().and_then(|major| major.parse::<u32>().ok()).is_some_and(|major| major >= 2)
}

/// What's wrong with opencode's pin (plan 01a1ce705e2f7773af54b299f1de42f5, steps 1 and 2): its version
/// OpenCode 2's; one build for each of linux/amd64 and linux/arm64 with gnu and musl, each its release
/// archive unpacked from its one entry opencode to bin/opencode, and its LICENSE; every digest a SHA-256;
/// and the smoke that version prints.
fn opencode_pin_problems(pin: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    if !is_opencode_2(version) {
        problems.push(format!("version {version:?} is no X.Y.Z of OpenCode 2"));
    }
    let builds = pin.get("builds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    for platform in ["linux/amd64", "linux/arm64"] {
        for variant in ["gnu", "musl"] {
            let n = builds
                .iter()
                .filter(|b| b.get("platform").and_then(Value::as_str) == Some(platform) && b.get("variant").and_then(Value::as_str) == Some(variant))
                .count();
            if n != 1 {
                problems.push(format!("{n} builds for {platform} {variant}"));
            }
        }
    }
    if builds.len() != 4 {
        problems.push(format!("{} builds, not 4", builds.len()));
    }
    let releases = format!("https://github.com/anomalyco/opencode/releases/download/v{version}/");
    for build in builds {
        let files = build.get("files").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let at = |path: &str| files.iter().filter(|f| f.get("path").and_then(Value::as_str) == Some(path)).collect::<Vec<_>>();
        let (bin, license) = (at("bin/opencode"), at("LICENSE"));
        let ([bin], [license]) = (bin.as_slice(), license.as_slice()) else {
            problems.push(format!("{build}: not one bin/opencode and one LICENSE"));
            continue;
        };
        if files.len() != 2 {
            problems.push(format!("{build}: {} files", files.len()));
        }
        for f in files {
            let sha = f.get("sha256").and_then(Value::as_str).unwrap_or_default();
            if !is_sha256(sha) {
                problems.push(format!("{sha:?} is no SHA-256"));
            }
            if f.get("size").and_then(Value::as_u64).unwrap_or(0) == 0 {
                problems.push(format!("{f}: no size"));
            }
            if !f.get("url").and_then(Value::as_str).unwrap_or_default().starts_with("https://") {
                problems.push(format!("{f}: no HTTPS URL"));
            }
        }
        if !bin.get("url").and_then(Value::as_str).unwrap_or_default().starts_with(&releases) {
            problems.push(format!("{bin}: not an archive of release v{version}"));
        }
        let unpack = bin.get("unpack");
        let member_sha = unpack.and_then(|u| u.get("sha256")).and_then(Value::as_str).unwrap_or_default();
        if unpack.and_then(|u| u.get("format")).and_then(Value::as_str) != Some("tar.gz")
            || unpack.and_then(|u| u.get("member")).and_then(Value::as_str) != Some("opencode")
            || !is_sha256(member_sha)
            || unpack.and_then(|u| u.get("size")).and_then(Value::as_u64).unwrap_or(0) == 0
        {
            problems.push(format!("{bin}: not unpacked from its one entry opencode"));
        }
        if bin.get("mode").and_then(Value::as_str) != Some("0755") || license.get("mode").and_then(Value::as_str) != Some("0644") || license.get("unpack").is_some() {
            problems.push(format!("{build}: modes or unpack not as the layout's"));
        }
    }
    let smoke = pin.get("smoke");
    if smoke.and_then(|s| s.get("program")).and_then(Value::as_str) != Some("bin/opencode")
        || strings(smoke.and_then(|s| s.get("args"))) != ["--version"]
        || strings(smoke.and_then(|s| s.get("env"))) != ["HOME=/tmp"]
        || smoke.and_then(|s| s.get("stdout")).and_then(Value::as_str) != Some(format!("opencode v{version}").as_str())
    {
        problems.push(format!("the smoke is not `opencode --version` printing opencode v{version}"));
    }
    problems
}

/// opencode's pin names OpenCode 2's four builds, each its release archive unpacked to bin/opencode with
/// its LICENSE, and the smoke its version prints; a pin with a short digest, a platform missing, an
/// archive not unpacked or OpenCode 1 is refused (f6-f11, f29, f32).
#[test]
fn step22_opencodes_pin_is_opencode_2s_four_builds() -> anyhow::Result<()> {
    let pin: Value = serde_json::from_str(opencode()?.pin)?;
    assert_eq!(opencode_pin_problems(&pin), Vec::<String>::new());
    let mutants: [(&str, fn(&mut Value)); 4] = [
        ("a 63-character digest", |p| {
            if let Some(s) = p.pointer_mut("/builds/0/files/0/sha256") {
                *s = Value::from("0".repeat(63));
            }
        }),
        ("linux/arm64 missing", |p| {
            if let Some(builds) = p.get_mut("builds").and_then(Value::as_array_mut) {
                builds.retain(|b| b.get("platform").and_then(Value::as_str) != Some("linux/arm64"));
            }
        }),
        ("an archive not unpacked", |p| {
            if let Some(file) = p.pointer_mut("/builds/0/files/0").and_then(Value::as_object_mut) {
                file.remove("unpack");
            }
        }),
        ("OpenCode 1", |p| {
            if let Some(v) = p.get_mut("version") {
                *v = Value::from("1.19.5");
            }
        }),
    ];
    for (what, mutate) in mutants {
        let mut p = pin.clone();
        mutate(&mut p);
        assert!(!opencode_pin_problems(&p).is_empty(), "a pin with {what} passed");
    }
    Ok(())
}

/// harness.json starts opencode as kiln does (kinds.rs: its program and its ACP arguments), its four XDG
/// roots on the per-user volume and its self-update off, and is a config Shards reads: schema version 1,
/// its name, the pin's version and no field Shards' OSI config refuses (Shards crates/image/src/osi.rs,
/// `deny_unknown_fields`). f4, f17, f20, f26.
#[test]
fn step22_opencodes_harness_json_starts_it_as_kiln_does() -> anyhow::Result<()> {
    let b = opencode()?;
    let pin: Value = serde_json::from_str(b.pin)?;
    let h = harness_json(b)?;
    let kind = Kind::get(b.harness).ok_or_else(|| anyhow::anyhow!("no harness {}", b.harness))?;
    let Protocol::Acp(args) = kind.protocol else {
        anyhow::bail!("{} doesn't speak ACP", b.harness);
    };
    let kilns: Vec<&str> = std::iter::once(kind.program).chain(args.iter().copied()).collect();
    assert_eq!(strings(h.pointer("/run/command")), kilns);
    assert_eq!(
        strings(h.pointer("/run/env")),
        [
            "XDG_CONFIG_HOME=/home/opencode/.config",
            "XDG_DATA_HOME=/home/opencode/.local/share",
            "XDG_STATE_HOME=/home/opencode/.local/state",
            "XDG_CACHE_HOME=/home/opencode/.cache",
            "OPENCODE_DISABLE_AUTOUPDATE=1",
        ]
    );
    assert_eq!(h.get("schemaVersion").and_then(Value::as_u64), Some(1));
    assert_eq!(h.get("name").and_then(Value::as_str), Some(b.harness));
    assert_eq!(h.get("version"), pin.get("version"));
    let fields: BTreeSet<&str> = h.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    assert!(fields.is_subset(&BTreeSet::from(["schemaVersion", "name", "version", "description", "platform", "run", "skills", "mcp"])), "{fields:?}");
    let run: BTreeSet<&str> = h.get("run").and_then(Value::as_object).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    assert!(run.is_subset(&BTreeSet::from(["command", "env", "workdir"])), "{run:?}");
    Ok(())
}

/// opencode's Agentfile is the shared layout's (plan 01a1cdf3cc7a70e4b3ca2ec610352383): the pin's base by
/// digest; gnu with nothing added and musl with libstdc++ and ripgrep; the harness from the directory
/// kiln writes; exactly the bring-up's three grants; exactly the project and per-user volumes, the
/// harness's alone; and no ENV. f12, f13, f21, f22, f26.
#[test]
fn step22_opencodes_agentfile_is_the_layouts() -> anyhow::Result<()> {
    let b = opencode()?;
    let pin: Value = serde_json::from_str(b.pin)?;
    let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
    assert!(base.rsplit_once("@sha256:").is_some_and(|(_, digest)| is_sha256(digest)), "base {base:?} is not by digest");
    let base_arg = format!("ARG BASE={base}");
    assert_eq!(
        instructions(b.text),
        [
            base_arg.as_str(),
            "ARG OPENCODE_LIBC=gnu",
            "FROM ${BASE} AS base-gnu",
            "FROM ${BASE} AS base-musl",
            "RUN apk add --no-cache libstdc++ ripgrep",
            "FROM base-${OPENCODE_LIBC}",
            "ARG TARGETARCH",
            "ARG OPENCODE_LIBC",
            "HARNESS opencode FROM ./opencode-${TARGETARCH}-${OPENCODE_LIBC}",
            "NETWORK --dns --egress=443/tcp provider FOR opencode",
            "EXPOSE --harnesses=opencode 443/tcp AS egress FOR provider",
            "CONNECT opencode WITH opencode ON provider",
            "VOLUME --target-kind=harness /workspace FOR opencode",
            "VOLUME --target-kind=harness /home/opencode FOR opencode",
        ]
    );
    Ok(())
}

/// What in a built-in looks like a credential: an ENV; an ARG or a harness's env named like one; a
/// credential's file among its files; or a token's value in any of its texts.
fn credential_problems(text: &str, pin: &str, files: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
    let named = regex::Regex::new(r"(?i)key|token|secret|passw|credential")?;
    let value = regex::Regex::new(r"sk-[A-Za-z0-9_-]{16,}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----")?;
    let mut problems = Vec::new();
    for line in instructions(text) {
        let mut words = line.split_whitespace();
        let (Some(op), Some(arg)) = (words.next(), words.next()) else { continue };
        if op.eq_ignore_ascii_case("ENV") {
            problems.push(format!("an ENV: {line}"));
        }
        if op.eq_ignore_ascii_case("ARG") && arg.split('=').next().is_some_and(|name| named.is_match(name)) {
            problems.push(format!("an ARG named like a credential: {line}"));
        }
    }
    for (name, body) in files {
        if matches!(*name, "auth.json" | "opencode.db" | ".credentials.json") {
            problems.push(format!("a credential's file: {name}"));
        }
        if let Ok(h) = serde_json::from_str::<Value>(body) {
            for e in strings(h.pointer("/run/env")) {
                if e.split('=').next().is_some_and(|n| named.is_match(n)) {
                    problems.push(format!("{name}: env {e} is named like a credential"));
                }
            }
        }
    }
    for t in [text, pin].into_iter().chain(files.iter().map(|(_, body)| *body)) {
        if let Some(m) = value.find(t) {
            problems.push(format!("a credential's value: {}...", m.as_str().get(..6).unwrap_or_default()));
        }
    }
    Ok(problems)
}

/// Nothing kiln ships for opencode carries a credential, and the check finds one put in (f19).
#[test]
fn step22_no_opencode_file_holds_a_credential() -> anyhow::Result<()> {
    let b = opencode()?;
    assert_eq!(credential_problems(b.text, b.pin, b.files)?, Vec::<String>::new());
    let leaky = format!("{}ENV OPENAI_API_KEY=x\n", b.text);
    assert!(!credential_problems(&leaky, b.pin, b.files)?.is_empty(), "ENV OPENAI_API_KEY passed");
    let leaky_env = [("harness.json", r#"{"run":{"command":["opencode"],"env":["ANTHROPIC_API_KEY=x"]}}"#)];
    assert!(!credential_problems(b.text, b.pin, &leaky_env)?.is_empty(), "env ANTHROPIC_API_KEY passed");
    Ok(())
}
