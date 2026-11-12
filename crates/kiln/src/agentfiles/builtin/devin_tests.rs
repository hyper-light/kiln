//! Devin's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 25; plan
//! 01a1ce705e4d7008993f5c7291ef4476, step 3; research 01a1ce705e4d70089c04cee721525c8f, fN): what its
//! pin, its harness.json and its Agentfile say of one another, of kiln's Kind `devin`, of Shards' OSI
//! config, and of credentials. Each check that reads a text is also handed changed copies, and each
//! change must fail it.

use serde_json::Value;

use super::{ALL, Builtin};
use crate::agentfiles::unpack::{Fields, Spec, Tree};
use crate::harness::kinds::{Kind, Protocol};

fn devin() -> anyhow::Result<&'static Builtin> {
    ALL.iter().find(|b| b.name == "harness-devin").ok_or_else(|| anyhow::anyhow!("no harness-devin"))
}

/// One of a built-in's own files, by name.
fn file(b: &Builtin, name: &str) -> anyhow::Result<&'static str> {
    b.files.iter().find(|(n, _)| *n == name).map(|(_, text)| *text).ok_or_else(|| anyhow::anyhow!("{}: no {name}", b.name))
}

/// An Agentfile's instructions: its lines that are neither blank nor comments.
fn instructions(text: &str) -> Vec<&str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect()
}

fn text<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

fn strings(v: Option<&Value>) -> Vec<&str> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// What's wrong with Devin's pin (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 1): its version
/// X.Y.Z, read from Cognition's manifest of that version (its SHA-256 kept); the base by digest; and
/// exactly one build for each of linux/amd64 and linux/arm64, each that version's bundle for its
/// architecture, with its size and SHA-256, unpacked at its harness's directory as kiln's one unpacker
/// takes a tree: read as the fetcher reads it, stripped of nothing (its bin/ and share/ are the
/// directory's), its entries and bytes pinned (research f4, f6).
fn devin_pin_problems(pin: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let version = text(pin, "/version");
    if version.split('.').count() != 3 || !version.split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())) {
        problems.push(format!("version {version:?} is no X.Y.Z"));
    }
    let at = format!("https://static.devin.ai/cli/{version}/");
    if text(pin, "/manifest/url") != format!("{at}manifest.json") || !is_sha256(text(pin, "/manifest/sha256")) {
        problems.push("not read from the version's manifest, its SHA-256 kept".into());
    }
    if text(pin, "/base/ref").is_empty() || !text(pin, "/base/digest").strip_prefix("sha256:").is_some_and(is_sha256) {
        problems.push("the base is not by digest".into());
    }
    let builds = pin.get("builds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let platforms: Vec<&str> = builds.iter().map(|b| text(b, "/platform")).collect();
    if platforms != ["linux/amd64", "linux/arm64"] {
        problems.push(format!("builds for {platforms:?}, not linux/amd64 and linux/arm64"));
    }
    for build in builds {
        let arch = match text(build, "/platform") {
            "linux/amd64" => "x86_64",
            "linux/arm64" => "aarch64",
            _ => continue,
        };
        let [file] = build.get("files").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default() else {
            problems.push(format!("{build}: not one file"));
            continue;
        };
        if text(file, "/url") != format!("{at}devin-{version}-{arch}-unknown-linux.tar.gz") {
            problems.push(format!("{file}: not the version's bundle for {arch}"));
        }
        if !is_sha256(text(file, "/sha256")) {
            problems.push(format!("{:?} is no SHA-256", text(file, "/sha256")));
        }
        if file.get("size").and_then(Value::as_u64).unwrap_or(0) == 0 {
            problems.push(format!("{file}: no size"));
        }
        if text(file, "/path") != "." {
            problems.push(format!("{file}: not at the harness's directory"));
        }
        let unpack = file.get("unpack").cloned().map(serde_json::from_value::<Fields>).and_then(Result::ok).map(Spec::try_from);
        if !matches!(unpack, Some(Ok(Spec::Tree(Tree { strip: 0, unpacked_bytes: 1.., entries: 1.. })))) {
            problems.push(format!("{file}: not a tar.gz tree the one unpacker takes, stripped of nothing"));
        }
    }
    problems
}

/// Devin's pin names Cognition's two Linux bundles of one version, each a tree the one unpacker takes;
/// a pin with a short digest, a platform missing, a bundle not unpacked, an unpack the unpacker
/// refuses or a bundle stripped of a name is refused (plan step 3's test; step 2's, once the one
/// unpacker landed).
#[test]
fn step25_devins_pin_is_cognitions_two_linux_bundles() -> anyhow::Result<()> {
    let pin: Value = serde_json::from_str(devin()?.pin)?;
    assert_eq!(devin_pin_problems(&pin), Vec::<String>::new());
    let mutants: [(&str, fn(&mut Value)); 5] = [
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
        ("a bundle not unpacked", |p| {
            if let Some(file) = p.pointer_mut("/builds/1/files/0").and_then(Value::as_object_mut) {
                file.remove("unpack");
            }
        }),
        ("an unpack the one unpacker refuses (no entries)", |p| {
            if let Some(unpack) = p.pointer_mut("/builds/0/files/0/unpack").and_then(Value::as_object_mut) {
                unpack.remove("entries");
            }
        }),
        ("a bundle stripped of a name", |p| {
            if let Some(strip) = p.pointer_mut("/builds/1/files/0/unpack/strip") {
                *strip = Value::from(1);
            }
        }),
    ];
    for (what, mutate) in mutants {
        let mut p = pin.clone();
        mutate(&mut p);
        assert!(!devin_pin_problems(&p).is_empty(), "a pin with {what} passed");
    }
    Ok(())
}

/// The fields of `h` Shards' OSI config reader doesn't know, at its top and in `run`: it refuses any
/// (Shards crates/image/src/osi.rs at main ac87190d3: `Config` and `Run`, `deny_unknown_fields`).
fn unknown_osi_fields(h: &Value) -> Vec<String> {
    let unknown = |v: Option<&Value>, known: &[&str]| -> Vec<String> {
        v.and_then(Value::as_object).into_iter().flat_map(|o| o.keys()).filter(|k| !known.contains(&k.as_str())).cloned().collect()
    };
    let mut fields = unknown(Some(h), &["schemaVersion", "name", "version", "description", "platform", "run", "skills", "mcp", "asks"]);
    fields.extend(unknown(h.get("run"), &["command", "env", "workdir"]));
    fields
}

/// The command kiln starts a harness that speaks ACP with: its program, then its ACP arguments.
fn acp_command(kind: &Kind) -> anyhow::Result<Vec<&'static str>> {
    let Protocol::Acp(args) = kind.protocol else {
        anyhow::bail!("{} doesn't speak ACP", kind.id);
    };
    Ok(std::iter::once(kind.program).chain(args.iter().copied()).collect())
}

/// harness.json is Shards' full OSI form, every field one its reader knows: schema version 1, Devin's
/// name, the pin's version; the command kiln starts Devin with (its Kind's program and ACP arguments,
/// f9), which Shards' init finds on the harness's PATH; and its env exactly the two XDG roots its
/// volumes hold (f10, f12), set after init's PATH, HOME and SHARDS_DOMAIN. No update setting: Devin's
/// updater stays off without the installer's marker (f26).
#[test]
fn step25_devins_harness_json_starts_it_as_kiln_does() -> anyhow::Result<()> {
    let b = devin()?;
    let pin: Value = serde_json::from_str(b.pin)?;
    let h: Value = serde_json::from_str(file(b, "harness.json")?)?;
    assert_eq!(unknown_osi_fields(&h), Vec::<String>::new());
    assert_eq!(h.get("schemaVersion").and_then(Value::as_u64), Some(1));
    assert_eq!(text(&h, "/name"), b.harness);
    assert_eq!(h.get("version"), pin.get("version"));
    let kind = Kind::get(b.harness).ok_or_else(|| anyhow::anyhow!("no harness {}", b.harness))?;
    let command = strings(h.pointer("/run/command"));
    assert_eq!(command, acp_command(kind)?);
    // Changing the Kind's ACP arguments alone tells them apart.
    assert_ne!(command, acp_command(&Kind { protocol: Protocol::Acp(&["acp", "--stdio"]), ..*kind })?);
    assert_eq!(strings(h.pointer("/run/env")), ["XDG_CONFIG_HOME=/home/devin/.config", "XDG_DATA_HOME=/home/devin/.local/share"]);
    Ok(())
}

/// Devin's Agentfile is the layout's: the pin's base by digest and no RUN, as both builds are static
/// (f5); TARGETARCH naming the directory kiln writes; exactly the bring-up's three grants, until step
/// 9's (f14); exactly the project's and Devin's two XDG volumes, the harness's alone (f10, f12); no ENV,
/// and no ARG but those two.
#[test]
fn step25_devins_agentfile_is_the_layouts() -> anyhow::Result<()> {
    let b = devin()?;
    let pin: Value = serde_json::from_str(b.pin)?;
    assert!(text(&pin, "/base/digest").strip_prefix("sha256:").is_some_and(is_sha256), "the pin's base is not by digest");
    let base = format!("ARG BASE={}@{}", text(&pin, "/base/ref"), text(&pin, "/base/digest"));
    assert_eq!(
        instructions(b.text),
        [
            base.as_str(),
            "FROM ${BASE}",
            "ARG TARGETARCH",
            "HARNESS devin FROM ./devin-${TARGETARCH}",
            "NETWORK --dns --egress=443/tcp provider FOR devin",
            "EXPOSE --harnesses=devin 443/tcp AS egress FOR provider",
            "CONNECT devin WITH devin ON provider",
            "VOLUME --target-kind=harness /workspace FOR devin",
            "VOLUME --target-kind=harness /home/devin/.config/devin FOR devin",
            "VOLUME --target-kind=harness /home/devin/.local/share/devin FOR devin",
        ]
    );
    Ok(())
}

/// What in Devin's built-in looks like a credential (research f13): an ENV or ARG named like one (key,
/// token, secret, password, credential), or a harness's env entry so named; Devin's own
/// (WINDSURF_API_KEY, DEVIN_API_KEY, its sign-in's credentials.toml) in an instruction, the pin or a
/// file; or a Cognition key's prefix, `cog_` (docs/ci.md's Devin row), anywhere.
fn credential_problems(text: &str, pin: &str, files: &[(&str, &str)]) -> anyhow::Result<Vec<String>> {
    let named = regex::Regex::new(r"(?i)key|token|secret|passw|credential")?;
    let devins = ["WINDSURF_API_KEY", "DEVIN_API_KEY", "credentials.toml"];
    let mut problems = Vec::new();
    for line in instructions(text) {
        let mut words = line.split_whitespace();
        let op = words.next().unwrap_or_default();
        if op.eq_ignore_ascii_case("ENV") || op.eq_ignore_ascii_case("ARG") {
            for name in words.map(|w| w.split('=').next().unwrap_or_default()).filter(|n| named.is_match(n)) {
                problems.push(format!("{op} {name} is named like a credential"));
            }
        }
        problems.extend(devins.iter().filter(|d| line.contains(**d)).map(|d| format!("{d} in the Agentfile: {line}")));
    }
    for (name, body) in files {
        if let Ok(h) = serde_json::from_str::<Value>(body) {
            for e in strings(h.pointer("/run/env")).into_iter().filter(|e| e.split('=').next().is_some_and(|n| named.is_match(n))) {
                problems.push(format!("{name}: env {e} is named like a credential"));
            }
        }
        problems.extend(devins.iter().filter(|d| name.contains(**d) || body.contains(**d)).map(|d| format!("{d} in {name}")));
    }
    problems.extend(devins.iter().filter(|d| pin.contains(**d)).map(|d| format!("{d} in pin.json")));
    for t in [text, pin].into_iter().chain(files.iter().map(|(_, body)| *body)) {
        if t.contains("cog_") {
            problems.push("a Cognition key's prefix, cog_".into());
        }
    }
    Ok(problems)
}

/// Devin's files carry no credential, and the scan finds one put in.
#[test]
fn step25_devins_files_carry_no_credential() -> anyhow::Result<()> {
    let b = devin()?;
    assert_eq!(credential_problems(b.text, b.pin, b.files)?, Vec::<String>::new());
    let leaky = format!("{}ENV WINDSURF_API_KEY=x\n", b.text);
    assert!(!credential_problems(&leaky, b.pin, b.files)?.is_empty(), "ENV WINDSURF_API_KEY passed");
    let leaky_env = [("harness.json", r#"{"run":{"command":["devin","acp"],"env":["DEVIN_API_KEY=x"]}}"#)];
    assert!(!credential_problems(b.text, b.pin, &leaky_env)?.is_empty(), "env DEVIN_API_KEY passed");
    let signed_in = [("credentials.toml", "")];
    assert!(!credential_problems(b.text, b.pin, &signed_in)?.is_empty(), "a credentials.toml passed");
    let key = format!("{}# cog_0123456789abcdef\n", b.text);
    assert!(!credential_problems(&key, b.pin, b.files)?.is_empty(), "a cog_ key passed");
    Ok(())
}
