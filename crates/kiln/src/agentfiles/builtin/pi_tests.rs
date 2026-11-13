//! pi's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 28; plan
//! 01a1ce705e62732b83f16295d74ee263, step 2; research 01a1ce705e62732b86d3534d788ae242): what its pin,
//! its harness.json and its Agentfile say of one another, of the one unpacker, of kiln's Kind `pi`, and
//! of credentials. Each check reads texts, so each test also hands it changed copies, and each change
//! must fail it; and, live, the context kiln's fetcher makes of them.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{ALL, Builtin};
use crate::agentfiles::unpack;
use crate::harness::kinds::Kind;
use crate::harness::session::Spec;

/// pi's entry, the one of kiln's table that runs it.
fn pi() -> anyhow::Result<&'static Builtin> {
    let mut runs = ALL.iter().filter(|b| b.harness == "pi");
    let b = runs.next().ok_or_else(|| anyhow::anyhow!("no built-in Agentfile runs pi"))?;
    anyhow::ensure!(b.name == "harness-pi" && runs.next().is_none(), "pi's entry isn't harness-pi alone");
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

/// A built-in's harness file, as JSON.
fn harness_file(b: &Builtin) -> anyhow::Result<Value> {
    let text = b.files.iter().find(|(n, _)| *n == "harness.json").map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("{}: no harness.json", b.name))?;
    Ok(serde_json::from_str(text)?)
}

/// The command a harness file starts its harness with.
fn command_of(h: &Value) -> Vec<String> {
    h.pointer("/run/command").and_then(Value::as_array).map(|c| c.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// `text` (JSON) with the value at `path` (a JSON pointer) set to `to`, or taken out without one.
fn edited(text: &str, path: &str, to: Option<Value>) -> anyhow::Result<String> {
    let mut v: Value = serde_json::from_str(text)?;
    let (parent, key) = path.rsplit_once('/').ok_or_else(|| anyhow::anyhow!("{path}"))?;
    let at = v.pointer_mut(parent).and_then(Value::as_object_mut).ok_or_else(|| anyhow::anyhow!("{path}"))?;
    match to {
        Some(to) => at.insert(key.to_owned(), to),
        None => at.remove(key),
    };
    Ok(v.to_string())
}

fn is_sha256(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A path relative to a folder that stays inside it: no root, and no empty, `.` or `..` part.
fn is_inside(p: &str) -> bool {
    !p.starts_with('/') && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Permissions as the fetcher takes a pinned file's mode: three octal digits, or four led by 0, so no
/// set-ID or sticky bit, which the unpacker refuses too.
fn is_mode(m: &str) -> bool {
    m.bytes().all(|b| (b'0'..=b'7').contains(&b)) && (m.len() == 3 || (m.len() == 4 && m.starts_with('0')))
}

/// What in a pinned file `f` of the build `at` would mislead a build, pushed onto `found`: no https URL,
/// SHA-256 or size; an `unpack` the one unpacker refuses as it reads pin.json's (step 26's
/// [`unpack::Spec`]: a member's fields or a tree's, every one its form needs and none of the other's,
/// and no other field); and one it takes that would mislead pi's directory:
/// - a tree outside the harness directory (`.`) or a folder inside it; one with a mode, which the
///   unpacker refuses as it unpacks, its entries keeping their own; or one without both its totals above
///   0, which pin.sh measures from the archive;
/// - a member that isn't a file inside the directory with its permissions, or whose entry is outside
///   the archive's folder or has no SHA-256 of its own.
///
/// A file with no `unpack` is a file inside the directory with its permissions.
fn file_problems(at: &str, f: &Value, found: &mut Vec<String>) {
    let s = |k: &str| f.get(k).and_then(Value::as_str);
    if !s("url").is_some_and(|u| u.starts_with("https://")) {
        found.push(format!("{at}: no https URL"));
    }
    if !s("sha256").is_some_and(is_sha256) || f.get("size").and_then(Value::as_u64).is_none() {
        found.push(format!("{at}: a file without its SHA-256 and size"));
    }
    let path = s("path").unwrap_or_default();
    let as_file = |found: &mut Vec<String>| {
        if !is_inside(path) {
            found.push(format!("{at}: a path outside its folder"));
        }
        if !s("mode").is_some_and(is_mode) {
            found.push(format!("{at}: a mode that isn't permissions"));
        }
    };
    match f.get("unpack").map(|u| serde_json::from_value::<unpack::Spec>(u.clone())) {
        None => as_file(found),
        Some(Err(e)) => found.push(format!("{at}: an unpack the one unpacker refuses: {e}")),
        Some(Ok(unpack::Spec::Tree(tree))) => {
            if path != "." && !is_inside(path) {
                found.push(format!("{at}: a tree outside the harness directory"));
            }
            if f.get("mode").is_some() {
                found.push(format!("{at}: a mode for a whole tree, whose entries keep their own"));
            }
            if tree.entries == 0 || tree.unpacked_bytes == 0 {
                found.push(format!("{at}: a tree without both its totals above 0"));
            }
        }
        Some(Ok(unpack::Spec::Member(m))) => {
            as_file(found);
            if !is_inside(m.member.strip_prefix("./").unwrap_or(&m.member)) {
                found.push(format!("{at}: a member outside the archive's folder"));
            }
            if !is_sha256(&m.sha256) {
                found.push(format!("{at}: a member without its own SHA-256"));
            }
        }
    }
}

/// What in pi's pin would mislead a build or its smoke:
/// - a version not x.y.z, a base not pinned by digest, builds other than linux/amd64 and linux/arm64
///   once each, a build without files, and each file as [`file_problems`] finds it;
/// - a smoke that isn't `pi --version` printing pi's bare version: `pi` is the executable at the root
///   of every build's tree (its tarball's `pi/pi`, which pin.sh requires, with `pi/` stripped), and
///   `stdout` is the line without its newline, as the other harnesses' pins write theirs.
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
            found.push(format!("a build for {p}, which pi's pin doesn't build"));
        }
    }
    for b in &builds {
        let at = b.get("platform").and_then(Value::as_str).unwrap_or_default();
        let files = b.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        if files.is_empty() {
            found.push(format!("{at}: no files"));
        }
        for f in &files {
            file_problems(at, f, &mut found);
        }
    }
    let smoke = |k: &str| pin.get("smoke").and_then(|s| s.get(k));
    let program = smoke("program").and_then(Value::as_str).unwrap_or_default();
    let roots_its_tree = |b: &Value| b.get("files").and_then(Value::as_array).is_some_and(|fs| fs.iter().any(|f| f.get("path").and_then(Value::as_str) == Some(".") && f.pointer("/unpack/strip").and_then(Value::as_u64) == Some(1)));
    if program != "pi" || !builds.iter().all(roots_its_tree) {
        found.push(format!("the smoke runs `{program}`, which isn't pi's executable at the root of every build's tree"));
    }
    if smoke("args") != Some(&Value::from(vec!["--version"])) || smoke("env").is_some() {
        found.push("the smoke isn't `--version` alone".to_owned());
    }
    let line = smoke("stdout").and_then(Value::as_str).unwrap_or_default();
    if line != version {
        found.push(format!("the smoke's line {line:?} isn't pi's bare version {version:?}"));
    }
    found
}

/// Where pi's harness file departs from its pin and its Agentfile from the base its pin was checked
/// with: an OSI config (schemaVersion 1) of pi at the pin's version, with a command; and the
/// Agentfile's BASE by default the pin's.
fn harness_problems(text: &str, pin: &Value, h: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let (version, base) = (pin.get("version").and_then(Value::as_str), pin.get("base").and_then(Value::as_str));
    if base_of(text) != base {
        found.push(format!("the Agentfile's BASE isn't its pin's {base:?}"));
    }
    if h.get("schemaVersion") != Some(&Value::from(1)) {
        found.push("its harness file's schemaVersion isn't 1".to_owned());
    }
    if h.get("name").and_then(Value::as_str) != Some("pi") {
        found.push("its harness file names another harness".to_owned());
    }
    if h.get("version").and_then(Value::as_str) != version {
        found.push(format!("its harness file isn't of the pin's version {version:?}"));
    }
    if command_of(h).is_empty() {
        found.push("its harness file has no run.command".to_owned());
    }
    found
}

/// What in `text` holds or names a credential: an ENV or ARG named like one, a variable named like one
/// given a value (`OPENAI_API_KEY=…`, in a harness file's env too), a provider's key (`sk-…`), or pi's
/// sign-in file, auth.json, outside a comment: it belongs on its owner's config volume alone (f27).
fn credentials_in(text: &str) -> Vec<String> {
    const WORDS: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    let named = |name: &str| WORDS.iter().any(|w| name.to_ascii_uppercase().contains(w));
    let mut found = Vec::new();
    for l in instructions(text) {
        if l.contains("auth.json") {
            found.push(format!("{l}: pi's auth.json"));
        }
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
    found
}

/// What kiln starts for `kind` ahead of a session's own flags: its program, then what `pi::args` gives
/// every session before `--session-id` (`--mode rpc`, f14).
fn started(kind: &Kind) -> Vec<String> {
    let args = crate::harness::pi::args(&Spec { kind: kind.id.to_owned(), ..Spec::default() });
    std::iter::once(kind.program.to_owned()).chain(args.into_iter().take_while(|a| a != "--session-id")).collect()
}

/// Each platform's files, each as `path mode what`: a tree's strip, or its member's file name.
fn layout(pin: &Value) -> Vec<String> {
    let file = |f: &Value| {
        let s = |p: &str| f.pointer(p).and_then(Value::as_str).unwrap_or("-");
        let member = || s("/unpack/member").rsplit('/').next().unwrap_or_default().to_owned();
        let what = f.pointer("/unpack/strip").and_then(Value::as_u64).map_or_else(member, |n| format!("strip {n}"));
        format!("{} {} {what}", s("/path"), s("/mode"))
    };
    let builds = pin.get("builds").and_then(Value::as_array).cloned().unwrap_or_default();
    builds.iter().map(|b| b.get("files").and_then(Value::as_array).map(|f| f.iter().map(&file).collect::<Vec<_>>().join(", ")).unwrap_or_default()).collect()
}

/// The plan's shape of pi's Agentfile: a base by digest, pi's own build per platform with no RUN, its
/// providers' hosts by name (step 18's grants, f20), its HOME as its one volume (step 18, f22), and no
/// ENV.
fn shape() -> Vec<String> {
    [
        "ARG BASE=buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10",
        "FROM ${BASE}",
        "ARG TARGETARCH",
        "HARNESS pi FROM ./pi-${TARGETARCH}",
    ]
    .iter()
    .map(|l| (*l).to_owned())
    .chain(super::grants_tests::grant_lines("pi"))
    .chain([super::grants_tests::volume_line("pi")])
    .collect()
}

/// Where pi's Agentfile and pin depart from the plan's shape: the Agentfile's instructions are
/// [`shape`]'s; each platform's directory is laid out as pi needs it, its tarball whole at the root less
/// its `pi/` (pi reads its package files beside its executable, f11) and ripgrep's and fd's builds at
/// bin/rg and bin/fd, 0755, so pi downloads neither (f17, f18); and the smoke's line is the bytes
/// 1.5.2's x64 `pi --version` wrote, 31 2e 35 2e 32 0a, less its newline (step 28's record,
/// docs/research/s28-pi-agentfile/step2/).
fn shape_problems(text: &str, pin: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let lines = instructions(text);
    if lines != shape() {
        found.push(format!("its Agentfile is {lines:#?}"));
    }
    let each = ". - strip 1, bin/rg 0755 rg, bin/fd 0755 fd";
    let files = layout(pin);
    if files != [each, each] {
        found.push(format!("its builds lay out {files:?}"));
    }
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    let line = pin.pointer("/smoke/stdout").and_then(Value::as_str).unwrap_or_default();
    if version == "1.5.2" && format!("{line}\n").as_bytes() != [0x31, 0x2e, 0x35, 0x2e, 0x32, 0x0a] {
        found.push(format!("the smoke's line {line:?} isn't what pi 1.5.2 printed"));
    }
    found
}

/// pi's pin holds (both platforms, every digest a SHA-256, its tarball a tree with its totals, rg and
/// fd members with their own, its smoke pi's), its harness file is an OSI config of pi at the pin's
/// version, and its Agentfile builds on the base its pin was checked with by default; a BASE, a
/// version, a name, a schema or a command that drifts is caught.
#[test]
fn step28_pi_pin_and_harness_file_hold() -> anyhow::Result<()> {
    let pi = pi()?;
    let problems = pin_problems(pi.pin);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");
    let pin: Value = serde_json::from_str(pi.pin)?;
    let h = harness_file(pi)?;
    let problems = harness_problems(pi.text, &pin, &h);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");

    let says = |text: &str, h: &Value, what: &str| harness_problems(text, &pin, h).iter().any(|p| p.contains(what));
    let other_base = pi.text.replace("@sha256:4f34e1d6", "@sha256:0000e1d6");
    anyhow::ensure!(says(&other_base, &h, "BASE"));
    let h_text = h.to_string();
    for (path, to, what) in [
        ("/version", Some(Value::from("1.5.1")), "pin's version"),
        ("/name", Some(Value::from("pi-coding-agent")), "another harness"),
        ("/schemaVersion", Some(Value::from(2)), "schemaVersion"),
        ("/run/command", None, "no run.command"),
    ] {
        let changed: Value = serde_json::from_str(&edited(&h_text, path, to)?)?;
        anyhow::ensure!(says(pi.text, &changed, what), "a harness file with {path} changed passed");
    }
    Ok(())
}

/// A pin no build or smoke could trust is caught:
/// - a digest one digit short, a platform missing, a URL over plain HTTP, a base by tag, a version
///   that's a tag;
/// - as the one unpacker refuses them, in its own words: pi's tarball given a member's digest beside
///   its tree's fields, a total or its strip left out or negative, another format or a field the
///   unpacker doesn't know; rg's member given a tree's strip or no size of its own; fd's unpack with
///   neither form's fields;
/// - as pi's directory takes them: the tree given a mode, a total of 0 or a path that climbs out; rg's
///   member at the folder itself, with set-ID permissions, or from an entry that climbs out;
/// - a smoke that runs rg or the unstripped `pi/pi`, or pi where a build's tree keeps its `pi/`, that
///   asks something else, sets an env, or expects another line ('1.5.2' and a newline, 'pi 1.5.2',
///   'v1.5.2'), and a pin with no smoke.
#[test]
fn step28_a_pin_that_would_mislead_a_build_is_caught() -> anyhow::Result<()> {
    let pi = pi()?;
    let real: Value = serde_json::from_str(pi.pin)?;
    let says = |path: &str, to: Option<Value>, what: &str| -> anyhow::Result<bool> { Ok(pin_problems(&edited(pi.pin, path, to)?).iter().any(|p| p.contains(what))) };
    // In each build, files/0 is pi's tarball, unpacked whole; files/1 is rg and files/2 fd, a member each.
    let short = real.pointer("/builds/0/files/0/sha256").and_then(Value::as_str).and_then(|d| d.get(1..)).map(Value::from);
    let http = real.pointer("/builds/1/files/2/url").and_then(Value::as_str).map(|u| Value::from(u.replacen("https://", "http://", 1)));
    let cases = [
        ("/builds/0/files/0/sha256", short, "without its SHA-256"),
        ("/builds/1/platform", Some(Value::from("linux/amd64")), "no build for linux/arm64"),
        ("/builds/1/files/2/url", http, "no https URL"),
        ("/base", Some(Value::from("buildpack-deps:trixie-scm")), "pinned by digest"),
        ("/version", Some(Value::from("v1.5.2")), "isn't x.y.z"),
        ("/builds/0/files/0/unpack/sha256", Some(Value::from("0".repeat(64))), "a member's fields and a tree's together"),
        ("/builds/0/files/0/unpack/entries", None, "no entries, which its form needs"),
        ("/builds/1/files/0/unpack/unpacked_bytes", Some(Value::from(-1)), "the one unpacker refuses"),
        ("/builds/1/files/0/unpack/strip", None, "no strip, which its form needs"),
        ("/builds/0/files/0/unpack/format", Some(Value::from("zip")), "only tar.gz unpacks"),
        ("/builds/0/files/0/unpack/members", Some(Value::from(vec!["pi/pi"])), "unknown field `members`"),
        ("/builds/0/files/1/unpack/strip", Some(Value::from(1)), "a member's fields and a tree's together"),
        ("/builds/1/files/1/unpack/size", None, "no size, which its form needs"),
        ("/builds/1/files/2/unpack", Some(serde_json::json!({"format": "tar.gz"})), "neither a member's fields nor a tree's"),
        ("/builds/0/files/0/mode", Some(Value::from("0755")), "a mode for a whole tree"),
        ("/builds/1/files/0/unpack/entries", Some(Value::from(0)), "both its totals above 0"),
        ("/builds/0/files/0/path", Some(Value::from("../pi")), "a tree outside"),
        ("/builds/0/files/1/path", Some(Value::from(".")), "a path outside"),
        ("/builds/0/files/1/mode", Some(Value::from("4755")), "isn't permissions"),
        ("/builds/1/files/1/unpack/member", Some(Value::from("../rg")), "member outside"),
        ("/smoke/program", Some(Value::from("bin/rg")), "isn't pi's executable"),
        ("/smoke/program", Some(Value::from("pi/pi")), "isn't pi's executable"),
        ("/builds/1/files/0/unpack/strip", Some(Value::from(0)), "isn't pi's executable"),
        ("/smoke/args", Some(Value::from(vec!["-v"])), "`--version` alone"),
        ("/smoke/env", Some(Value::from(vec!["PI_OFFLINE=1"])), "`--version` alone"),
        ("/smoke/stdout", Some(Value::from("1.5.2\n")), "the smoke's line"),
        ("/smoke/stdout", Some(Value::from("pi 1.5.2")), "the smoke's line"),
        ("/smoke/stdout", Some(Value::from("v1.5.2")), "the smoke's line"),
        ("/smoke", None, "the smoke"),
    ];
    for (path, to, what) in cases {
        anyhow::ensure!(says(path, to.clone(), what)?, "a pin with {path} at {to:?} passed, or not for {what:?}");
    }
    anyhow::ensure!(pin_problems(pi.pin).is_empty());
    Ok(())
}

/// Nothing kiln ships for pi holds a credential, names an ENV or ARG like one, or brings in pi's
/// sign-in file; adding one is caught.
#[test]
fn step28_no_pi_file_holds_a_credential() -> anyhow::Result<()> {
    let pi = pi()?;
    for (name, text) in [("Agentfile", pi.text), ("pin.json", pi.pin)].into_iter().chain(pi.files.iter().copied()) {
        let found = credentials_in(text);
        anyhow::ensure!(found.is_empty(), "{name}: {found:?}");
    }
    let h = harness_file(pi)?.to_string();
    for text in [
        format!("{}ENV OPENAI_API_KEY=x\n", pi.text),
        format!("{}ARG GITHUB_TOKEN\n", pi.text),
        format!("{}COPY auth.json /home/pi/.pi/agent/\n", pi.text),
        h.replace("PI_SKIP_VERSION_CHECK=1", "ANTHROPIC_API_KEY=y"),
        h.replace("PI_SKIP_VERSION_CHECK=1", "PI_AUTH=/home/pi/.pi/agent/auth.json"),
        format!("{}# sk-proj-abcdefghijklmnopqrstuvwx\n", pi.text),
    ] {
        anyhow::ensure!(!credentials_in(&text).is_empty(), "{text}");
    }
    Ok(())
}

/// pi runs as kiln's Pi kind starts it: harness.json's command is the kind's program and what
/// `pi::args` gives every session before its own flags (`--mode rpc`, f14). Its env is exactly its HOME,
/// the volume the Agentfile gives it (step 18), its config directory beneath it, the version check off
/// (f15, f19) and its caches on the scratch, where they were, so of what shards-init sets first it sets
/// HOME alone (not PATH, nor SHARDS_DOMAIN; f16); and the one file beside the Agentfile is harness.json,
/// with no sign-in, as pi's auth.json lives on that volume (f27). A kind, an env, a volume or a file
/// that drifts from them is caught.
#[test]
fn step28_pi_runs_as_kilns_pi_kind_with_its_config_on_its_volume() -> anyhow::Result<()> {
    let pi = pi()?;
    let kind = Kind::get("pi").ok_or_else(|| anyhow::anyhow!("no pi kind"))?;
    let h = harness_file(pi)?;
    anyhow::ensure!(command_of(&h) == started(kind), "{h} isn't {:?}", started(kind));
    anyhow::ensure!(command_of(&h) != started(&Kind { program: "pi-rpc", ..*kind }));

    let env: Vec<&str> = h.pointer("/run/env").and_then(Value::as_array).map(|e| e.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let config = env.iter().find_map(|e| e.strip_prefix("PI_CODING_AGENT_DIR=")).ok_or_else(|| anyhow::anyhow!("no PI_CODING_AGENT_DIR in {env:?}"))?;
    let home = env.iter().find_map(|e| e.strip_prefix("HOME=")).ok_or_else(|| anyhow::anyhow!("no HOME in {env:?}"))?;
    let want = ["HOME=/home/pi".to_owned(), format!("PI_CODING_AGENT_DIR={config}"), "PI_SKIP_VERSION_CHECK=1".to_owned(), "XDG_CACHE_HOME=/tmp/.cache".to_owned()];
    anyhow::ensure!(env == want, "{env:?}");
    anyhow::ensure!(config.starts_with(&format!("{home}/")), "PI_CODING_AGENT_DIR={config} isn't beneath HOME={home}");
    let homeless: Vec<&str> = env.iter().copied().filter(|e| !e.starts_with("HOME=")).collect();
    anyhow::ensure!(homeless != want);
    let volumes = harness_volumes(pi.text, "pi");
    anyhow::ensure!(volumes == [home], "{volumes:?}");
    let moved = pi.text.replace("/home/pi FOR pi", "/home/pi/.pi FOR pi");
    anyhow::ensure!(harness_volumes(&moved, "pi").first() != Some(&home));

    let names = |files: &[(&str, &str)]| files.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>();
    anyhow::ensure!(names(pi.files) == ["harness.json"], "{:?}", names(pi.files));
    anyhow::ensure!(names([pi.files, &[("auth.json", "{}")]].concat().as_slice()) != ["harness.json"]);
    Ok(())
}

/// pi's Agentfile and pin are the plan's shape ([`shape_problems`]); an Agentfile with a RUN or its
/// HOME volume moved, a platform without rg, and a smoke line with its newline are each caught.
#[test]
fn step28_pi_agentfile_and_pin_are_the_plans_shape() -> anyhow::Result<()> {
    let pi = pi()?;
    let pin: Value = serde_json::from_str(pi.pin)?;
    let problems = shape_problems(pi.text, &pin);
    anyhow::ensure!(problems.is_empty(), "{problems:?}");

    for text in [format!("{}RUN apt-get install -y ripgrep\n", pi.text), pi.text.replace("/home/pi FOR pi", "/home/pi/.pi FOR pi")] {
        anyhow::ensure!(text != pi.text && !shape_problems(&text, &pin).is_empty(), "{text}");
    }
    let mut no_rg = pin.clone();
    no_rg.pointer_mut("/builds/1/files").and_then(Value::as_array_mut).ok_or_else(|| anyhow::anyhow!("no arm64 files"))?.retain(|f| f.get("path") != Some(&Value::from("bin/rg")));
    anyhow::ensure!(!shape_problems(pi.text, &no_rg).is_empty());
    let newline: Value = serde_json::from_str(&edited(pi.pin, "/smoke/stdout", Some(Value::from("1.5.2\n")))?)?;
    anyhow::ensure!(!shape_problems(pi.text, &newline).is_empty());
    Ok(())
}

/// Live, where a test names a folder for it (`KILN_TEST_LIVE_CONTEXT`, under `~/.cache` on kiln's Linux
/// machines): kiln's fetcher makes pi's context for linux/amd64 and linux/arm64 from pi's, ripgrep's
/// and fd's releases, each archive held to its pin and unpacked by the one unpacker, pi's tree to its
/// strip and totals and rg and fd to their members' own digests. It holds the Agentfile and the two
/// harness directories alone, each with the embedded harness.json, pi at its root with its package.json
/// at the pin's version beside it (f11), and bin/rg and bin/fd at 0755 hashing to their members' SHA-256.
/// This machine's own pi, run with `--version` alone, an empty HOME and nothing else in its
/// environment (step 2's method), writes the pin's smoke line and its newline to its standard output.
#[test]
fn step28_a_live_pi_context_holds_the_pinned_builds() -> anyhow::Result<()> {
    use sha2::Digest as _;
    let Some(root) = std::env::var_os("KILN_TEST_LIVE_CONTEXT") else {
        eprintln!("skipped: KILN_TEST_LIVE_CONTEXT names no folder");
        return Ok(());
    };
    let pi = pi()?;
    let pin: Value = serde_json::from_str(pi.pin)?;
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default().to_owned();
    let line = pin.pointer("/smoke/stdout").and_then(Value::as_str).unwrap_or_default().to_owned();
    let harness_json = pi.files.iter().find(|(n, _)| *n == "harness.json").map(|(_, t)| *t).ok_or_else(|| anyhow::anyhow!("no harness.json"))?;
    let names = |dir: &std::path::Path| -> anyhow::Result<Vec<String>> {
        let mut names = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned())).collect::<Result<Vec<_>, _>>()?;
        names.sort();
        Ok(names)
    };
    crate::rt::block_on(async move {
        let dir = tempfile::tempdir_in(&root)?;
        let platforms = ["linux/amd64".to_owned(), "linux/arm64".to_owned()];
        let made = crate::agentfiles::context::materialize(pi, &platforms, None, dir.path()).await?;
        anyhow::ensure!(made == [dir.path().join("pi-amd64"), dir.path().join("pi-arm64")], "made {made:?}");
        anyhow::ensure!(names(dir.path())? == ["Agentfile", "pi-amd64", "pi-arm64"], "the context holds {:?}", names(dir.path())?);
        anyhow::ensure!(std::fs::read_to_string(dir.path().join("Agentfile"))? == pi.text, "the Agentfile isn't the embedded one");
        for (n, arch) in ["amd64", "arm64"].into_iter().enumerate() {
            let at = dir.path().join(format!("pi-{arch}"));
            let held = names(&at)?;
            anyhow::ensure!(["bin", "harness.json", "package.json", "pi"].iter().all(|f| held.iter().any(|h| h == f)), "pi-{arch} holds {held:?}");
            anyhow::ensure!(std::fs::read_to_string(at.join("harness.json"))? == harness_json, "pi-{arch}: harness.json isn't the embedded one");
            let package: Value = serde_json::from_str(&std::fs::read_to_string(at.join("package.json"))?)?;
            anyhow::ensure!(package.get("version").and_then(Value::as_str) == Some(version.as_str()), "pi-{arch}: package.json is {:?}", package.get("version"));
            for (i, tool) in [(1, "rg"), (2, "fd")] {
                let want = pin.pointer(&format!("/builds/{n}/files/{i}/unpack/sha256")).and_then(Value::as_str).unwrap_or_default();
                let binary = at.join("bin").join(tool);
                let got = hex::encode(sha2::Sha256::digest(std::fs::read(&binary)?));
                anyhow::ensure!(got == want, "{}: SHA-256 {got}, the pin's member {want}", binary.display());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(&binary)?.permissions().mode() & 0o7777;
                    anyhow::ensure!(mode == 0o755, "{}: mode {mode:o}, not 755", binary.display());
                }
                println!("MATCH linux/{arch} bin/{tool} {got}");
            }
        }
        let own = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some("amd64"),
            ("linux", "aarch64") => Some("arm64"),
            _ => None,
        };
        if let Some(arch) = own {
            let home = tempfile::tempdir_in(&root)?;
            let out = std::process::Command::new(dir.path().join(format!("pi-{arch}/pi"))).arg("--version").env_clear().env("HOME", home.path()).output()?;
            anyhow::ensure!(out.status.success() && out.stdout == format!("{line}\n").into_bytes(), "pi-{arch}/pi --version: {} {:?} {:?}", out.status, out.stdout, String::from_utf8_lossy(&out.stderr));
            println!("VERSION linux/{arch} stdout {:02x?}, stderr {} bytes", out.stdout, out.stderr.len());
        }
        Ok(())
    })
}
