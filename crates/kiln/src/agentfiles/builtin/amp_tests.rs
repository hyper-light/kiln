//! Amp's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 24): what its pin, its
//! harness.json and its Agentfile say of one another, of kiln's Kind `amp`, and of sign-ins. Each
//! check reads texts, so each test also hands it changed copies, and each change alone must fail it.

use anyhow::{Context, anyhow, bail, ensure};
use serde_json::Value;

use crate::harness::kinds::{Kind, Protocol};

fn amp() -> anyhow::Result<&'static super::Builtin> {
    super::ALL.iter().find(|b| b.harness == "amp").ok_or_else(|| anyhow!("no built-in Agentfile runs amp"))
}

fn harness_json(b: &super::Builtin) -> anyhow::Result<&'static str> {
    b.files.iter().find(|(name, _)| *name == "harness.json").map(|(_, text)| *text).ok_or_else(|| anyhow!("{} carries no harness.json", b.name))
}

/// What kiln starts for Kind `id`: its program, then its ACP arguments (`Kind::launch`).
fn kind_command(id: &str) -> anyhow::Result<Vec<String>> {
    let kind = Kind::get(id).ok_or_else(|| anyhow!("no Kind {id}"))?;
    let Protocol::Acp(args) = kind.protocol else { bail!("Kind {id} doesn't speak ACP") };
    Ok(std::iter::once(kind.program).chain(args.iter().copied()).map(str::to_owned).collect())
}

/// Amp's version form: three dot-separated numbers, then its own `-g<commit>` (0.0.1794456033-gb2ff22).
fn amp_version(v: &str) -> bool {
    let (numbers, tail) = v.split_once('-').map_or((v, None), |(n, t)| (n, Some(t)));
    numbers.split('.').count() == 3
        && numbers.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && tail.is_none_or(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
}

fn lowercase_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The pin: Amp's version, the base by digest, and linux/amd64 and linux/arm64 once each, each with
/// bin/amp and bin/amp-acp (no variant: both are glibc builds alone) by HTTPS URL, SHA-256 and size.
fn check_pin(pin: &str) -> anyhow::Result<Value> {
    let p: Value = serde_json::from_str(pin).context("pin.json doesn't parse")?;
    let version = p.get("version").and_then(Value::as_str).unwrap_or_default();
    ensure!(amp_version(version), "the pin's version `{version}` isn't Amp's form");
    let base = p.get("base").and_then(Value::as_str).unwrap_or_default();
    ensure!(base.contains("@sha256:"), "the pin's base `{base}` isn't pinned by digest");
    ensure!(p.pointer("/verified/amp-acp/version").and_then(Value::as_str).is_some_and(|v| !v.is_empty()), "the pin names no amp-acp version");
    let builds = p.get("builds").and_then(Value::as_array).ok_or_else(|| anyhow!("the pin has no builds"))?;
    let mut platforms: Vec<&str> = builds.iter().filter_map(|b| b.get("platform").and_then(Value::as_str)).collect();
    platforms.sort_unstable();
    ensure!(platforms == ["linux/amd64", "linux/arm64"], "the pin's builds are {platforms:?}, not linux/amd64 and linux/arm64 once each");
    for build in builds {
        let platform = build.get("platform").and_then(Value::as_str).unwrap_or_default();
        ensure!(build.get("variant").is_none(), "{platform}: Amp's builds have no variant");
        let files = build.get("files").and_then(Value::as_array).ok_or_else(|| anyhow!("{platform}: no files"))?;
        let mut paths: Vec<&str> = files.iter().filter_map(|f| f.get("path").and_then(Value::as_str)).collect();
        paths.sort_unstable();
        ensure!(paths == ["bin/amp", "bin/amp-acp"], "{platform}: its files are {paths:?}, not bin/amp and bin/amp-acp");
        for file in files {
            let path = file.get("path").and_then(Value::as_str).unwrap_or_default();
            let sha = file.get("sha256").and_then(Value::as_str).unwrap_or_default();
            ensure!(lowercase_sha256(sha), "{platform} {path}: `{sha}` isn't a SHA-256 in 64 lowercase hex digits");
            ensure!(file.get("size").and_then(Value::as_u64).is_some_and(|s| s > 0), "{platform} {path}: no size");
            ensure!(file.get("mode").and_then(Value::as_str) == Some("0755"), "{platform} {path}: not mode 0755");
            ensure!(file.get("url").and_then(Value::as_str).is_some_and(|u| u.starts_with("https://")), "{platform} {path}: its URL isn't HTTPS");
        }
    }
    Ok(p)
}

/// The Agentfile's instructions, each a keyword (upper-cased) and the rest of its line, without
/// blank lines and comments.
fn instructions(text: &str) -> Vec<(String, &str)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (keyword, rest) = line.split_once(' ').unwrap_or((line, ""));
            (keyword.to_ascii_uppercase(), rest.trim())
        })
        .collect()
}

/// Amp's harness-scoped volumes, as the Agentfile declares them.
fn harness_volumes(agentfile: &str) -> Vec<&str> {
    instructions(agentfile)
        .into_iter()
        .filter(|(keyword, _)| keyword == "VOLUME")
        .filter_map(|(_, rest)| rest.strip_prefix("--target-kind=harness ")?.strip_suffix(" FOR amp"))
        .collect()
}

/// The Agentfile: the pin's base, one stage, the harness from each platform's directory, the
/// bring-up's three provider grants and nothing more, the project and /home/amp volumes, and nothing
/// that installs or configures beside the pin and harness.json.
fn check_agentfile(agentfile: &str, pin: &Value) -> anyhow::Result<()> {
    let all = instructions(agentfile);
    let has = |keyword: &str, rest: &str| all.iter().any(|(k, r)| k == keyword && *r == rest);
    let count = |keyword: &str| all.iter().filter(|(k, _)| k == keyword).count();
    let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
    ensure!(has("ARG", &format!("BASE={base}")), "the Agentfile's ARG BASE isn't the pin's base {base}");
    ensure!(has("FROM", "${BASE}") && count("FROM") == 1, "the Agentfile isn't one stage FROM ${{BASE}}");
    ensure!(has("ARG", "TARGETARCH") && count("ARG") == 2, "the Agentfile's ARGs aren't BASE and TARGETARCH");
    ensure!(has("HARNESS", "amp FROM ./amp-${TARGETARCH}") && count("HARNESS") == 1, "the Agentfile's harness isn't amp FROM ./amp-${{TARGETARCH}}");
    let grants = [("NETWORK", "--dns --egress=443/tcp provider FOR amp"), ("EXPOSE", "--harnesses=amp 443/tcp AS egress FOR provider"), ("CONNECT", "amp WITH amp ON provider")];
    for (keyword, rest) in grants {
        ensure!(has(keyword, rest), "the Agentfile lacks `{keyword} {rest}`");
    }
    ensure!(count("NETWORK") + count("EXPOSE") + count("CONNECT") == grants.len(), "the Agentfile grants more than the bring-up's three lines");
    ensure!(count("VOLUME") == 2 && harness_volumes(agentfile) == ["/workspace", "/home/amp"], "the Agentfile's volumes aren't /workspace and /home/amp, Amp's alone");
    let known = ["ARG", "FROM", "HARNESS", "NETWORK", "EXPOSE", "CONNECT", "VOLUME"];
    if let Some((keyword, rest)) = all.iter().find(|(k, _)| !known.contains(&k.as_str())) {
        bail!("the Agentfile has `{keyword} {rest}`: only the pin and harness.json install and configure Amp");
    }
    Ok(())
}

/// harness.json: Amp's (schema 1, the pin's versions), starting what kiln starts for Kind `amp`
/// (`command`), with Amp's and amp-acp's state in the Agentfile's /home/amp volume through the XDG
/// variables (a HOME would sit beside init's HOME=/tmp, not replace it) and no update checks.
fn check_harness(harness: &str, pin: &Value, agentfile: &str, command: &[String]) -> anyhow::Result<()> {
    let h: Value = serde_json::from_str(harness).context("harness.json doesn't parse")?;
    ensure!(h.get("schemaVersion").and_then(Value::as_u64) == Some(1), "harness.json isn't an OSI config of schema 1");
    ensure!(h.get("name").and_then(Value::as_str) == Some("amp"), "harness.json isn't amp's");
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    ensure!(h.get("version").and_then(Value::as_str) == Some(version), "harness.json's version isn't the pin's {version}");
    let acp = pin.pointer("/verified/amp-acp/version").and_then(Value::as_str).unwrap_or_default();
    ensure!(h.get("description").and_then(Value::as_str).is_some_and(|d| d.contains(&format!("amp-acp {acp}"))), "harness.json's description doesn't name amp-acp {acp}");
    let run_command = strings(&h, "/run/command");
    ensure!(run_command == command, "harness.json runs {run_command:?}, not {command:?} as kiln starts its Kind");
    ensure!(h.pointer("/run/workdir").is_none(), "harness.json sets a workdir");
    let env = strings(&h, "/run/env");
    let mut names: Vec<&str> = env.iter().filter_map(|e| e.split_once('=').map(|(name, _)| name)).collect();
    names.sort_unstable();
    ensure!(names == ["AMP_SKIP_UPDATE_CHECK", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"], "harness.json's env names {names:?}");
    ensure!(env.contains(&"AMP_SKIP_UPDATE_CHECK=1"), "harness.json doesn't turn Amp's update checks off");
    let state = harness_volumes(agentfile).into_iter().filter(|v| *v != "/workspace").collect::<Vec<_>>();
    ensure!(state == ["/home/amp"], "the Agentfile has no /home/amp volume for Amp's state");
    for e in env.iter().filter(|e| e.starts_with("XDG_")) {
        let path = e.split_once('=').map(|(_, path)| path).unwrap_or_default();
        ensure!(state.iter().any(|v| path.starts_with(&format!("{v}/"))), "{e} is outside the Agentfile's /home/amp volume");
    }
    Ok(())
}

/// The strings of the array at `pointer` in `v` (none where it's missing).
fn strings<'a>(v: &'a Value, pointer: &str) -> Vec<&'a str> {
    v.pointer(pointer).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

/// No sign-in, nor a name for one, in an embedded file.
fn check_no_signin(name: &str, text: &str) -> anyhow::Result<()> {
    for word in ["AMP_API_KEY", "sgamp_", "secrets.json", "credentials.json"] {
        ensure!(!text.contains(word), "{name} names `{word}`");
    }
    for (keyword, rest) in instructions(text) {
        if keyword == "ENV" || keyword == "ARG" {
            let variable = rest.split(['=', ' ']).next().unwrap_or_default().to_ascii_lowercase();
            for word in ["key", "token", "secret", "password", "credential"] {
                ensure!(!variable.contains(word), "{name}: `{keyword} {rest}` is named like a sign-in");
            }
        }
    }
    Ok(())
}

/// The pin with one edit.
fn edited(pin: &str, edit: impl FnOnce(&mut Value) -> Option<()>) -> anyhow::Result<String> {
    let mut p: Value = serde_json::from_str(pin)?;
    edit(&mut p).ok_or_else(|| anyhow!("the edit found nothing to change"))?;
    Ok(p.to_string())
}

#[test]
fn amp_pin_names_both_programs_for_both_platforms() -> anyhow::Result<()> {
    let b = amp()?;
    check_pin(b.pin)?;
    let short_digest = edited(b.pin, |p| {
        let sha = p.pointer_mut("/builds/0/files/0/sha256")?;
        let shorter = sha.as_str()?.get(..63)?.to_owned();
        *sha = Value::from(shorter);
        Some(())
    })?;
    ensure!(check_pin(&short_digest).is_err(), "a 63-character digest passed");
    let no_adapter = edited(b.pin, |p| {
        let build = p.get_mut("builds")?.as_array_mut()?.iter_mut().find(|b| b.get("platform").and_then(Value::as_str) == Some("linux/arm64"))?;
        build.get_mut("files")?.as_array_mut()?.retain(|f| f.get("path").and_then(Value::as_str) != Some("bin/amp-acp"));
        Some(())
    })?;
    ensure!(check_pin(&no_adapter).is_err(), "linux/arm64 without bin/amp-acp passed");
    let one_platform = edited(b.pin, |p| {
        p.get_mut("builds")?.as_array_mut()?.retain(|b| b.get("platform").and_then(Value::as_str) != Some("linux/amd64"));
        Some(())
    })?;
    ensure!(check_pin(&one_platform).is_err(), "a pin without linux/amd64 passed");
    let other_version = edited(b.pin, |p| {
        *p.get_mut("version")? = Value::from("1.2");
        Some(())
    })?;
    ensure!(check_pin(&other_version).is_err(), "a version not of Amp's form passed");
    Ok(())
}

#[test]
fn amp_harness_file_starts_kinds_program_with_its_state_in_the_volume() -> anyhow::Result<()> {
    let b = amp()?;
    let pin = check_pin(b.pin)?;
    let harness = harness_json(b)?;
    let command = kind_command("amp")?;
    check_harness(harness, &pin, b.text, &command)?;
    // The Kind starting another program alone fails it.
    ensure!(check_harness(harness, &pin, b.text, &["amp".to_owned()]).is_err(), "a Kind starting another program passed");
    // One XDG path out of the volume, a HOME, an AMP_CLI_PATH, update checks left on: each alone fails it.
    let changes = [
        ("XDG_STATE_HOME=/home/amp/.local/state", "XDG_STATE_HOME=/tmp/state"),
        ("\"AMP_SKIP_UPDATE_CHECK=1\"", "\"AMP_SKIP_UPDATE_CHECK=1\", \"HOME=/home/amp\""),
        ("\"AMP_SKIP_UPDATE_CHECK=1\"", "\"AMP_SKIP_UPDATE_CHECK=1\", \"AMP_CLI_PATH=/harness/amp/bin/amp\""),
        ("\"AMP_SKIP_UPDATE_CHECK=1\"", "\"AMP_SKIP_UPDATE_CHECK=0\""),
    ];
    for (from, to) in changes {
        ensure!(harness.contains(from), "harness.json no longer has {from}");
        ensure!(check_harness(&harness.replacen(from, to, 1), &pin, b.text, &command).is_err(), "harness.json with {to} passed");
    }
    // Its version and the pin's must be one.
    let other = edited(b.pin, |p| {
        *p.get_mut("version")? = Value::from("0.0.1-gabc");
        Some(())
    })?;
    ensure!(check_harness(harness, &check_pin(&other)?, b.text, &command).is_err(), "harness.json of another version than the pin's passed");
    Ok(())
}

#[test]
fn amp_agentfile_grants_its_harness_what_it_lists_alone() -> anyhow::Result<()> {
    let b = amp()?;
    let pin = check_pin(b.pin)?;
    check_agentfile(b.text, &pin)?;
    let additions = [
        "EXPOSE --harnesses=amp 80/tcp AS egress FOR provider",
        "VOLUME --target-kind=harness /root FOR amp",
        "RUN apt-get install -y curl",
        "ENV AMP_SKIP_UPDATE_CHECK=1",
    ];
    for line in additions {
        ensure!(check_agentfile(&format!("{}\n{line}\n", b.text), &pin).is_err(), "the Agentfile with `{line}` passed");
    }
    let other_base = edited(b.pin, |p| {
        *p.get_mut("base")? = Value::from("debian:trixie@sha256:0000000000000000000000000000000000000000000000000000000000000000");
        Some(())
    })?;
    ensure!(check_agentfile(b.text, &check_pin(&other_base)?).is_err(), "an Agentfile on another base than the pin's passed");
    Ok(())
}

#[test]
fn amp_files_carry_no_signin() -> anyhow::Result<()> {
    let b = amp()?;
    check_no_signin("the Agentfile", b.text)?;
    check_no_signin("pin.json", b.pin)?;
    for (name, text) in b.files {
        check_no_signin(name, text)?;
    }
    ensure!(check_no_signin("the Agentfile", &format!("{}\nENV AMP_API_KEY=x\n", b.text)).is_err(), "ENV AMP_API_KEY passed");
    ensure!(check_no_signin("the Agentfile", &format!("{}\nARG GITHUB_TOKEN\n", b.text)).is_err(), "ARG GITHUB_TOKEN passed");
    Ok(())
}
