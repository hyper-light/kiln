//! Antigravity's built-in Agentfile (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26): what its
//! pin, its harness.json and its Agentfile say of one another, of kiln's Kind `antigravity`, and of
//! sign-ins. Each check reads texts, so each test also hands it changed copies, and each change alone
//! must fail it.

use anyhow::{Context, anyhow, bail, ensure};
use serde_json::Value;

use crate::harness::antigravity;
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::session::Spec;

/// Where Google's updater sends each archive of a release: under `<version>-<execution id>/`.
const STORAGE: &str = "https://storage.googleapis.com/antigravity-public/antigravity-cli/";

fn agy() -> anyhow::Result<&'static super::Builtin> {
    super::ALL.iter().find(|b| b.harness == "antigravity").ok_or_else(|| anyhow!("no built-in Agentfile runs antigravity"))
}

fn harness_json(b: &super::Builtin) -> anyhow::Result<&'static str> {
    b.files.iter().find(|(name, _)| *name == "harness.json").map(|(_, text)| *text).ok_or_else(|| anyhow!("{} carries no harness.json", b.name))
}

/// What kiln starts for Kind `antigravity` in `mode`, for a new session with no model or effort
/// chosen: its program, then `antigravity::args`.
fn kind_command(mode: &str) -> anyhow::Result<Vec<String>> {
    let kind = Kind::get("antigravity").ok_or_else(|| anyhow!("no Kind antigravity"))?;
    ensure!(kind.protocol == Protocol::AgyStreamJson, "Kind antigravity doesn't speak agy's stream-json");
    let spec = Spec { kind: kind.id.to_owned(), mode: Some(mode.to_owned()), ..Spec::default() };
    Ok(std::iter::once(kind.program.to_owned()).chain(antigravity::args(&spec)).collect())
}

/// agy's version form: three dot-separated numbers (1.5.6).
fn agy_version(v: &str) -> bool {
    v.split('.').count() == 3 && v.split('.').all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn lowercase_hex(s: &str, digits: usize) -> bool {
    s.len() == digits && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The pin: agy's version and its release's execution id, the base by digest, the two manifests
/// read, and linux/amd64 and linux/arm64 once each, each one .tar.gz from that release's directory
/// by HTTPS URL, SHA-256, SHA-512 and size, whose one member, antigravity, becomes bin/agy (0755)
/// by its own SHA-256 and size: the shared unpack's member form alone, never its tree's.
fn check_pin(pin: &str) -> anyhow::Result<Value> {
    let p: Value = serde_json::from_str(pin).context("pin.json doesn't parse")?;
    ensure!(p.get("harness").and_then(Value::as_str) == Some("antigravity"), "the pin isn't antigravity's");
    let version = p.get("version").and_then(Value::as_str).unwrap_or_default();
    ensure!(agy_version(version), "the pin's version `{version}` isn't agy's form");
    let execution = p.get("execution_id").and_then(Value::as_str).unwrap_or_default();
    ensure!(!execution.is_empty() && execution.bytes().all(|b| b.is_ascii_digit()), "the pin's execution id `{execution}` isn't a number");
    let base = p.get("base").and_then(Value::as_str).unwrap_or_default();
    ensure!(base.contains("@sha256:"), "the pin's base `{base}` isn't pinned by digest");
    let manifests = p.pointer("/verified/manifests").and_then(Value::as_array).map_or(0, Vec::len);
    ensure!(manifests == 2, "the pin records {manifests} manifests read, not one per platform");
    let builds = p.get("builds").and_then(Value::as_array).ok_or_else(|| anyhow!("the pin has no builds"))?;
    let mut platforms: Vec<&str> = builds.iter().filter_map(|b| b.get("platform").and_then(Value::as_str)).collect();
    platforms.sort_unstable();
    ensure!(platforms == ["linux/amd64", "linux/arm64"], "the pin's builds are {platforms:?}, not linux/amd64 and linux/arm64 once each");
    let release = format!("{STORAGE}{version}-{execution}/");
    for build in builds {
        let platform = build.get("platform").and_then(Value::as_str).unwrap_or_default();
        let files = build.get("files").and_then(Value::as_array).ok_or_else(|| anyhow!("{platform}: no files"))?;
        let [file] = files.as_slice() else { bail!("{platform}: {} files, not agy's one archive", files.len()) };
        ensure!(file.get("path").and_then(Value::as_str) == Some("bin/agy"), "{platform}: its file isn't bin/agy");
        ensure!(file.get("mode").and_then(Value::as_str) == Some("0755"), "{platform}: bin/agy isn't mode 0755");
        let url = file.get("url").and_then(Value::as_str).unwrap_or_default();
        ensure!(url.starts_with(&release) && url.ends_with(".tar.gz"), "{platform}: {url} isn't a .tar.gz of release {version}-{execution} ({release})");
        let sha256 = file.get("sha256").and_then(Value::as_str).unwrap_or_default();
        ensure!(lowercase_hex(sha256, 64), "{platform}: `{sha256}` isn't a SHA-256 in 64 lowercase hex digits");
        let sha512 = file.get("sha512").and_then(Value::as_str).unwrap_or_default();
        ensure!(lowercase_hex(sha512, 128), "{platform}: `{sha512}` isn't a SHA-512 in 128 lowercase hex digits");
        ensure!(file.get("size").and_then(Value::as_u64).is_some_and(|s| s > 0), "{platform}: the archive has no size");
        let unpack = file.get("unpack").and_then(Value::as_object).ok_or_else(|| anyhow!("{platform}: the archive isn't unpacked"))?;
        let mut keys: Vec<&str> = unpack.keys().map(String::as_str).collect();
        keys.sort_unstable();
        ensure!(keys == ["format", "member", "sha256", "size"], "{platform}: unpack has {keys:?}, not the member form's format, member, sha256 and size");
        ensure!(unpack.get("format").and_then(Value::as_str) == Some("tar.gz"), "{platform}: unpack isn't of a tar.gz");
        ensure!(unpack.get("member").and_then(Value::as_str) == Some("antigravity"), "{platform}: unpack's member isn't antigravity");
        let member = unpack.get("sha256").and_then(Value::as_str).unwrap_or_default();
        ensure!(lowercase_hex(member, 64), "{platform}: the member's `{member}` isn't a SHA-256 in 64 lowercase hex digits");
        ensure!(unpack.get("size").and_then(Value::as_u64).is_some_and(|s| s > 0), "{platform}: the member has no size");
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

/// agy's harness-scoped volumes, as the Agentfile declares them.
fn harness_volumes(agentfile: &str) -> Vec<&str> {
    instructions(agentfile)
        .into_iter()
        .filter(|(keyword, _)| keyword == "VOLUME")
        .filter_map(|(_, rest)| rest.strip_prefix("--target-kind=harness ")?.strip_suffix(" FOR antigravity"))
        .collect()
}

/// The Agentfile: the pin's base, one stage, the harness from each platform's directory, the
/// bring-up's three provider grants and nothing more, the project and the person's whole home as
/// volumes, and nothing that installs or configures beside the pin and harness.json.
fn check_agentfile(agentfile: &str, pin: &Value) -> anyhow::Result<()> {
    let all = instructions(agentfile);
    let has = |keyword: &str, rest: &str| all.iter().any(|(k, r)| k == keyword && *r == rest);
    let count = |keyword: &str| all.iter().filter(|(k, _)| k == keyword).count();
    let base = pin.get("base").and_then(Value::as_str).unwrap_or_default();
    ensure!(has("ARG", &format!("BASE={base}")), "the Agentfile's ARG BASE isn't the pin's base {base}");
    ensure!(has("FROM", "${BASE}") && count("FROM") == 1, "the Agentfile isn't one stage FROM ${{BASE}}");
    ensure!(has("ARG", "TARGETARCH") && count("ARG") == 2, "the Agentfile's ARGs aren't BASE and TARGETARCH");
    ensure!(has("HARNESS", "antigravity FROM ./antigravity-${TARGETARCH}") && count("HARNESS") == 1, "the Agentfile's harness isn't antigravity FROM ./antigravity-${{TARGETARCH}}");
    let grants = [
        ("NETWORK", "--dns --egress=443/tcp provider FOR antigravity"),
        ("EXPOSE", "--harnesses=antigravity 443/tcp AS egress FOR provider"),
        ("CONNECT", "antigravity WITH antigravity ON provider"),
    ];
    for (keyword, rest) in grants {
        ensure!(has(keyword, rest), "the Agentfile lacks `{keyword} {rest}`");
    }
    ensure!(count("NETWORK") + count("EXPOSE") + count("CONNECT") == grants.len(), "the Agentfile grants more than the bring-up's three lines");
    ensure!(count("VOLUME") == 2 && harness_volumes(agentfile) == ["/workspace", "/home/antigravity"], "the Agentfile's volumes aren't /workspace and /home/antigravity, agy's alone");
    let known = ["ARG", "FROM", "HARNESS", "NETWORK", "EXPOSE", "CONNECT", "VOLUME"];
    if let Some((keyword, rest)) = all.iter().find(|(k, _)| !known.contains(&k.as_str())) {
        bail!("the Agentfile has `{keyword} {rest}`: only the pin and harness.json install and configure agy");
    }
    Ok(())
}

/// harness.json: agy's (schema 1, the pin's version), starting what kiln starts for Kind
/// `antigravity` in `command`, saying nothing more than that and its environment. Its environment
/// is three variables, each once: HOME, the Agentfile's home volume itself (agy keeps everything
/// under $HOME/.gemini, and a harness sees the image read-only: a shards at or after 997f4e47e
/// gives run.env's HOME the harness, not init's /tmp); agy's self-updater off; and the XDG cache on
/// the harness's /tmp scratch, in no volume.
fn check_harness(harness: &str, pin: &Value, agentfile: &str, command: &[String]) -> anyhow::Result<()> {
    let h: Value = serde_json::from_str(harness).context("harness.json doesn't parse")?;
    let mut keys: Vec<&str> = h.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    keys.sort_unstable();
    ensure!(keys == ["name", "run", "schemaVersion", "version"], "harness.json has {keys:?}, not schemaVersion, name, version and run alone");
    ensure!(h.get("schemaVersion").and_then(Value::as_u64) == Some(1), "harness.json isn't an OSI config of schema 1");
    ensure!(h.get("name").and_then(Value::as_str) == Some("antigravity"), "harness.json isn't antigravity's");
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    ensure!(h.get("version").and_then(Value::as_str) == Some(version), "harness.json's version isn't the pin's {version}");
    let mut run: Vec<&str> = h.get("run").and_then(Value::as_object).map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    run.sort_unstable();
    ensure!(run == ["command", "env"], "harness.json's run has {run:?}, not command and env alone");
    let run_command = strings(&h, "/run/command");
    ensure!(run_command == command, "harness.json runs {run_command:?}, not {command:?} as kiln starts its Kind");
    let env = strings(&h, "/run/env");
    let mut names: Vec<&str> = env.iter().filter_map(|e| e.split_once('=').map(|(name, _)| name)).collect();
    names.sort_unstable();
    ensure!(env.len() == 3 && names == ["AGY_CLI_DISABLE_AUTO_UPDATE", "HOME", "XDG_CACHE_HOME"], "harness.json's env is {env:?}, not HOME, AGY_CLI_DISABLE_AUTO_UPDATE and XDG_CACHE_HOME once each");
    let value = |name: &str| env.iter().find_map(|e| e.strip_prefix(name)?.strip_prefix('=')).unwrap_or_default();
    let homes: Vec<&str> = harness_volumes(agentfile).into_iter().filter(|v| *v != "/workspace").collect();
    let home = value("HOME");
    ensure!(homes == [home], "HOME={home} isn't the Agentfile's home volume (its volumes beside /workspace are {homes:?})");
    ensure!(value("AGY_CLI_DISABLE_AUTO_UPDATE") == "true", "harness.json doesn't turn agy's self-updater off");
    let cache = value("XDG_CACHE_HOME");
    ensure!(cache.starts_with("/tmp/") && !cache.starts_with(&format!("{home}/")), "XDG_CACHE_HOME={cache} isn't on the harness's /tmp scratch");
    Ok(())
}

/// The strings of the array at `pointer` in `v` (none where it's missing).
fn strings<'a>(v: &'a Value, pointer: &str) -> Vec<&'a str> {
    v.pointer(pointer).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

/// No sign-in, nor a name for one, in an embedded file: agy's key variables and its two
/// documented headless routes (a Gemini API key, Application Default Credentials), and Google's
/// credential files.
fn check_no_signin(name: &str, text: &str) -> anyhow::Result<()> {
    for word in ["GEMINI_API_KEY", "GOOGLE_API_KEY", "AGY_ADC_AUTH", "GOOGLE_APPLICATION_CREDENTIALS", "application_default_credentials", "oauth_creds"] {
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

/// The pin's smoke, in the shape every harness's pin gives its own (step 19's layout): `bin/agy
/// --version`, whose line is the version alone (`stdout`, the line without its newline). agy writes
/// the version and a newline: 1.5.7's builds wrote 31 2e 35 2e 37 0a in a microVM on both platforms
/// (docs/research/s26-agy-agentfile/evidence/step4/), as 1.5.6's wrote 31 2e 35 2e 36 0a (3423c8df).
fn check_smoke(pin: &Value) -> anyhow::Result<()> {
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default();
    let smoke = pin.get("smoke").and_then(Value::as_object).ok_or_else(|| anyhow!("the pin has no smoke"))?;
    let mut keys: Vec<&str> = smoke.keys().map(String::as_str).collect();
    keys.sort_unstable();
    ensure!(keys == ["args", "program", "stdout"], "the pin's smoke has {keys:?}, not program, args and stdout");
    ensure!(smoke.get("program").and_then(Value::as_str) == Some("bin/agy"), "the smoke runs another program than bin/agy");
    let args: Vec<&str> = smoke.get("args").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    ensure!(args == ["--version"], "the smoke runs agy {args:?}, not --version");
    let stdout = smoke.get("stdout").and_then(Value::as_str).unwrap_or_default();
    ensure!(stdout == version, "the smoke expects {stdout:?}, not the version {version} alone");
    ensure!(version != "1.5.7" || format!("{stdout}\n").as_bytes() == [0x31, 0x2e, 0x35, 0x2e, 0x37, 0x0a], "the smoke expects {stdout:?}, not the line 1.5.7 wrote");
    Ok(())
}

/// The pin with one edit.
fn edited(pin: &str, edit: impl FnOnce(&mut Value) -> Option<()>) -> anyhow::Result<String> {
    let mut p: Value = serde_json::from_str(pin)?;
    edit(&mut p).ok_or_else(|| anyhow!("the edit found nothing to change"))?;
    Ok(p.to_string())
}

/// The pin's file of `platform`.
fn file_of<'a>(p: &'a mut Value, platform: &str) -> Option<&'a mut Value> {
    p.get_mut("builds")?.as_array_mut()?.iter_mut().find(|b| b.get("platform").and_then(Value::as_str) == Some(platform))?.pointer_mut("/files/0")
}

#[test]
fn antigravity_is_listed_once_with_its_harness_file() -> anyhow::Result<()> {
    let listed = super::ALL.iter().filter(|b| b.harness == "antigravity").count();
    ensure!(listed == 1, "{listed} built-in Agentfiles run antigravity");
    let b = agy()?;
    ensure!(b.name == "harness-antigravity", "antigravity's built-in Agentfile is named {}", b.name);
    let names: Vec<&str> = b.files.iter().map(|(name, _)| *name).collect();
    ensure!(names == ["harness.json"], "its build context's files are {names:?}, not harness.json alone");
    Ok(())
}

#[test]
fn antigravity_pin_names_agy_for_both_platforms() -> anyhow::Result<()> {
    let b = agy()?;
    let pin = check_pin(b.pin)?;
    let short_digest = edited(b.pin, |p| {
        let sha = file_of(p, "linux/amd64")?.get_mut("sha256")?;
        let shorter = sha.as_str()?.get(..63)?.to_owned();
        *sha = Value::from(shorter);
        Some(())
    })?;
    ensure!(check_pin(&short_digest).is_err(), "a 63-character digest passed");
    let short_sha512 = edited(b.pin, |p| {
        let sha = file_of(p, "linux/arm64")?.get_mut("sha512")?;
        let shorter = sha.as_str()?.get(..127)?.to_owned();
        *sha = Value::from(shorter);
        Some(())
    })?;
    ensure!(check_pin(&short_sha512).is_err(), "a 127-character SHA-512 passed");
    let other_member = edited(b.pin, |p| {
        *file_of(p, "linux/arm64")?.pointer_mut("/unpack/member")? = Value::from("agy");
        Some(())
    })?;
    ensure!(check_pin(&other_member).is_err(), "a member other than antigravity passed");
    let both_forms = edited(b.pin, |p| {
        file_of(p, "linux/amd64")?.get_mut("unpack")?.as_object_mut()?.insert("strip".into(), Value::from(1));
        Some(())
    })?;
    ensure!(check_pin(&both_forms).is_err(), "an unpack naming a member and a strip passed");
    let execution = pin.get("execution_id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let other_release = edited(b.pin, |p| {
        let url = file_of(p, "linux/amd64")?.get_mut("url")?;
        let other = url.as_str()?.replacen(&format!("-{execution}/"), "-1/", 1);
        *url = Value::from(other);
        Some(())
    })?;
    ensure!(other_release != pin.to_string() && check_pin(&other_release).is_err(), "an archive of another release's directory passed");
    let plain_http = edited(b.pin, |p| {
        let url = file_of(p, "linux/arm64")?.get_mut("url")?;
        let other = url.as_str()?.replacen("https://", "http://", 1);
        *url = Value::from(other);
        Some(())
    })?;
    ensure!(check_pin(&plain_http).is_err(), "an archive over plain HTTP passed");
    let other_path = edited(b.pin, |p| {
        *file_of(p, "linux/amd64")?.get_mut("path")? = Value::from("bin/antigravity");
        Some(())
    })?;
    ensure!(check_pin(&other_path).is_err(), "a harness file other than bin/agy passed");
    let second_file = edited(b.pin, |p| {
        let file = file_of(p, "linux/amd64")?.clone();
        p.get_mut("builds")?.as_array_mut()?.first_mut()?.get_mut("files")?.as_array_mut()?.push(file);
        Some(())
    })?;
    ensure!(check_pin(&second_file).is_err(), "a platform with two files passed");
    let one_platform = edited(b.pin, |p| {
        p.get_mut("builds")?.as_array_mut()?.retain(|b| b.get("platform").and_then(Value::as_str) != Some("linux/amd64"));
        Some(())
    })?;
    ensure!(check_pin(&one_platform).is_err(), "a pin without linux/amd64 passed");
    let other_version = edited(b.pin, |p| {
        *p.get_mut("version")? = Value::from("1.5");
        Some(())
    })?;
    ensure!(check_pin(&other_version).is_err(), "a version not of agy's form passed");
    Ok(())
}

#[test]
fn antigravity_harness_file_runs_agy_in_ask_mode_with_its_home_in_the_volume() -> anyhow::Result<()> {
    let b = agy()?;
    let pin = check_pin(b.pin)?;
    let harness = harness_json(b)?;
    let command = kind_command(antigravity::ASK)?;
    check_harness(harness, &pin, b.text, &command)?;
    // What kiln starts in its default mode adds `--mode accept-edits`: harness.json's isn't that.
    let default_mode = kind_command(antigravity::DEFAULT_MODE)?;
    ensure!(default_mode != command && check_harness(harness, &pin, b.text, &default_mode).is_err(), "harness.json passed as the default mode's command too");
    let changes = [
        ("\"stream-json\"]", "\"stream-json\", \"--mode\", \"accept-edits\"]"),
        ("\"HOME=/home/antigravity\"", "\"HOME=/tmp\""),
        ("\"HOME=/home/antigravity\"", "\"HOME=/home/antigravity/.gemini\""),
        ("\"AGY_CLI_DISABLE_AUTO_UPDATE=true\"", "\"AGY_CLI_DISABLE_AUTO_UPDATE=false\""),
        ("\"XDG_CACHE_HOME=/tmp/.cache\"", "\"XDG_CACHE_HOME=/home/antigravity/.cache\""),
        ("\"XDG_CACHE_HOME=/tmp/.cache\"", "\"XDG_CACHE_HOME=/tmp/.cache\", \"PATH=/usr/bin:/bin\""),
        ("\"XDG_CACHE_HOME=/tmp/.cache\"", "\"XDG_CACHE_HOME=/tmp/.cache\", \"HOME=/tmp\""),
        ("\"XDG_CACHE_HOME=/tmp/.cache\"", "\"XDG_CACHE_HOME=/tmp/.cache\", \"SHARDS_DOMAIN=antigravity\""),
        ("\"run\": {", "\"run\": {\n    \"workdir\": \".\","),
        ("\"name\": \"antigravity\"", "\"name\": \"agy\""),
    ];
    for (from, to) in changes {
        ensure!(harness.contains(from), "harness.json no longer has {from}");
        ensure!(check_harness(&harness.replacen(from, to, 1), &pin, b.text, &command).is_err(), "harness.json with {to} passed");
    }
    // Its home must be the Agentfile's volume.
    let other_home = b.text.replace("/home/antigravity FOR antigravity", "/home/agy FOR antigravity");
    ensure!(other_home != b.text && check_harness(harness, &pin, &other_home, &command).is_err(), "HOME outside the Agentfile's volumes passed");
    // Its version and the pin's must be one (the pin unchecked here: its URLs name its own release).
    let other: Value = serde_json::from_str(&edited(b.pin, |p| {
        *p.get_mut("version")? = Value::from("1.5.5");
        Some(())
    })?)?;
    ensure!(check_harness(harness, &other, b.text, &command).is_err(), "harness.json of another version than the pin's passed");
    Ok(())
}

#[test]
fn antigravity_agentfile_grants_its_harness_what_it_lists_alone() -> anyhow::Result<()> {
    let b = agy()?;
    let pin = check_pin(b.pin)?;
    check_agentfile(b.text, &pin)?;
    let additions = [
        "EXPOSE --harnesses=antigravity 80/tcp AS egress FOR provider",
        "VOLUME --target-kind=harness /tmp/agy FOR antigravity",
        "RUN apt-get install -y ca-certificates",
        "ENV HOME=/home/antigravity",
    ];
    for line in additions {
        ensure!(check_agentfile(&format!("{}\n{line}\n", b.text), &pin).is_err(), "the Agentfile with `{line}` passed");
    }
    let only_gemini = b.text.replace("/home/antigravity FOR antigravity", "/home/antigravity/.gemini FOR antigravity");
    ensure!(only_gemini != b.text && check_agentfile(&only_gemini, &pin).is_err(), "a volume of .gemini alone, not the whole home, passed");
    let other_directory = b.text.replace("FROM ./antigravity-${TARGETARCH}", "FROM ./agy");
    ensure!(other_directory != b.text && check_agentfile(&other_directory, &pin).is_err(), "the harness from one directory for every platform passed");
    let other_base = edited(b.pin, |p| {
        *p.get_mut("base")? = Value::from("debian:trixie@sha256:0000000000000000000000000000000000000000000000000000000000000000");
        Some(())
    })?;
    ensure!(check_agentfile(b.text, &check_pin(&other_base)?).is_err(), "an Agentfile on another base than the pin's passed");
    Ok(())
}

#[test]
fn antigravity_smoke_expects_what_agy_prints() -> anyhow::Result<()> {
    let b = agy()?;
    let pin = check_pin(b.pin)?;
    check_smoke(&pin)?;
    let version = pin.get("version").and_then(Value::as_str).unwrap_or_default().to_owned();
    let changes: [(&str, Value); 5] = [
        ("/smoke/stdout", Value::from(format!(" {version}"))),
        ("/smoke/stdout", Value::from(format!("{version}\n"))),
        ("/smoke/stdout", Value::from("1.5.5")),
        ("/smoke/args", serde_json::json!(["version"])),
        ("/smoke/program", Value::from("bin/antigravity")),
    ];
    for (at, to) in changes {
        let changed = edited(b.pin, |p| {
            *p.pointer_mut(at)? = to.clone();
            Some(())
        })?;
        ensure!(check_smoke(&serde_json::from_str(&changed)?).is_err(), "a smoke with {at} {to} passed");
    }
    let none = edited(b.pin, |p| p.as_object_mut()?.remove("smoke").map(|_| ()))?;
    ensure!(check_smoke(&serde_json::from_str(&none)?).is_err(), "a pin without its smoke passed");
    Ok(())
}

#[test]
fn antigravity_files_carry_no_signin() -> anyhow::Result<()> {
    let b = agy()?;
    check_no_signin("the Agentfile", b.text)?;
    check_no_signin("pin.json", b.pin)?;
    for (name, text) in b.files {
        check_no_signin(name, text)?;
    }
    ensure!(check_no_signin("the Agentfile", &format!("{}\nENV GEMINI_API_KEY=x\n", b.text)).is_err(), "ENV GEMINI_API_KEY passed");
    ensure!(check_no_signin("the Agentfile", &format!("{}\nENV AGY_ADC_AUTH=true\n", b.text)).is_err(), "ENV AGY_ADC_AUTH passed");
    ensure!(check_no_signin("the Agentfile", &format!("{}\nARG GOOGLE_CLOUD_ACCESS_TOKEN\n", b.text)).is_err(), "ARG GOOGLE_CLOUD_ACCESS_TOKEN passed");
    let harness = harness_json(b)?;
    let keyed = harness.replacen("\"XDG_CACHE_HOME=/tmp/.cache\"", "\"XDG_CACHE_HOME=/tmp/.cache\", \"GEMINI_API_KEY=x\"", 1);
    ensure!(keyed != harness && check_no_signin("harness.json", &keyed).is_err(), "harness.json with GEMINI_API_KEY passed");
    Ok(())
}

/// Shards' own check of the Agentfile, where a test names a `shards` (`KILN_TEST_SHARDS`, as
/// `check`'s tests do): it parses, its names and grants hold, and Shards finds nothing to say. Its
/// CONNECT joining the harness to another, with no port, grants nothing (default deny): Shards
/// refuses it at that line.
#[test]
fn antigravity_agentfile_passes_shards_own_check() -> anyhow::Result<()> {
    let Some(bin) = std::env::var_os("KILN_TEST_SHARDS") else {
        eprintln!("skipped: KILN_TEST_SHARDS names no shards binary");
        return Ok(());
    };
    let text = agy()?.text;
    crate::rt::block_on(async move {
        let root = tempfile::tempdir()?;
        let shards = std::path::Path::new(&bin);
        let c = super::super::check::run(shards, root.path(), text).await;
        ensure!(c.checked && c.diagnostics.is_empty(), "{c:?}");
        let broken = text.replace("CONNECT antigravity WITH antigravity ON provider", "CONNECT antigravity WITH nobody ON provider");
        let line = broken.lines().position(|l| l.starts_with("CONNECT ")).map(|i| i + 1).ok_or_else(|| anyhow!("the Agentfile has no CONNECT line"))?;
        ensure!(broken != text, "the Agentfile's CONNECT line isn't the harness's own");
        let c = super::super::check::run(shards, root.path(), &broken).await;
        ensure!(c.refusal().is_some_and(|r| r.contains(&format!("line {line}"))), "{c:?}");
        Ok(())
    })
}
