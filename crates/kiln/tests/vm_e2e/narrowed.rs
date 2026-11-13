//! Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17 part 6, in a microVM of its own: a Goose session
//! of kiln's own Agentfile reaches past its microVM only the hosts of the provider its person's home
//! names, read as Goose reads it, and kiln says so on the session (`/runs/reach`). A session whose home
//! can't be read keeps every destination its image names, and says why, in a notice and on
//! `/runs/reach`.
//!
//! No vendor's CLI runs and no prompt leaves the machine. The stand-in (`tests/fixtures/vm-stub`) takes
//! Goose's place in kiln's image:
//! - Before kiln starts a session, the test builds kiln's own Goose Agentfile with the stand-in's
//!   `HARNESS` (every grant, volume and command as kiln ships them) into the runner's Shards home
//!   (`kiln::sandbox::shards_home`), under the tag kiln finds Goose's image by
//!   (`kiln::sandbox::image::tag_of`). kiln then boots it as its own.
//! - Each session starts with no prompt. The stand-in gets its words only once kiln says it runs that
//!   very image (`/runs/image`, by digest). A session of any other image is stopped, unasked.
//!
//! What it reads:
//! - `/runs/reach`.
//! - The stand-in's `CONNECT=` lines: its resolver's answer for the name, then a TCP connection. A
//!   name no grant names is REFUSED by the microVM's network, RCODE 5 (Shards' D236).
//! - The session's notices.
//!
//! It skips until its runner starts microVMs; with `VM_E2E=1` it fails instead, as step 34's runs do.
//! It always runs a server and a runner of its own, whose folder it writes the person's home in.
//!
//!   cargo test -p kiln --test vm_harnesses narrowed:: -- --nocapture --test-threads 1

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::vm_e2e::{self, Fixture};

/// The runner's name, and so its folder's (`vm_e2e::own_state`).
const RUNNER: &str = "narrow";

/// The person an admin's session acts for (`harness::lineage::person_of`): whose home kiln reads.
const PERSON: &str = "owner";

/// The provider the person's home names, and one Goose serves that it doesn't, both granted by kiln's
/// Goose Agentfile (`agentfiles::builtin::destinations::BY_USE`).
const KEPT: (&str, &str) = ("anthropic", "api.anthropic.com:443");
const DROPPED: &str = "api.groq.com:443";

fn goose() -> anyhow::Result<&'static kiln::agentfiles::builtin::Builtin> {
    kiln::agentfiles::builtin::ALL.iter().find(|b| b.harness == "goose").ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for goose"))
}

/// The hosts a run of `harness` keeps for `uses`, as kiln's table names them: its harness's own, then
/// each use's, sorted, each once.
pub fn kept_for(harness: &str, uses: &[&str]) -> anyhow::Result<Vec<String>> {
    let table = kiln::agentfiles::builtin::destinations::BY_USE.iter().find(|t| t.harness == harness).ok_or_else(|| anyhow::anyhow!("kiln names no hosts by use for {harness}"))?;
    let mut hosts: Vec<String> = table.always.iter().map(|h| (*h).to_owned()).collect();
    for u in uses {
        let (_, of) = table.by.iter().find(|(id, _)| id == u).ok_or_else(|| anyhow::anyhow!("{harness} serves no {u} in kiln's table"))?;
        hosts.extend(of.iter().map(|h| (*h).to_owned()));
    }
    hosts.sort();
    hosts.dedup();
    Ok(hosts)
}

/// kiln's Agentfile for Goose (`text`) with the stand-in as its harness: its `HARNESS` line the stand-in's
/// at `commit` of `repository` (as `vm_e2e::stub_agentfile` names it), speaking Goose's interface, and
/// every other line kiln's.
pub fn stand_in_as_goose(text: &str, commit: &str, repository: &str) -> anyhow::Result<String> {
    let (dir, at) = vm_e2e::STUB_HARNESS;
    let harness = |l: &str| l.trim_start().starts_with("HARNESS goose FROM ");
    anyhow::ensure!(text.lines().filter(|l| harness(l)).count() == 1, "kiln's Goose Agentfile has no one HARNESS goose line");
    let from = |l: &str| l.trim_start().starts_with("FROM ");
    anyhow::ensure!(text.lines().filter(|l| from(l)).count() == 1, "kiln's Goose Agentfile has no one FROM line");
    let mut out = Vec::new();
    for line in text.lines() {
        if harness(line) {
            out.push(format!("HARNESS goose FROM {repository}.git#{commit}:{dir} TO {at}"));
        } else {
            out.push(line.to_owned());
            if from(line) {
                out.push("ENV VM_STUB_PROTOCOL=goose".to_owned());
            }
        }
    }
    Ok(out.join("\n") + "\n")
}

/// What the stand-in said of `to` (`host:port`): its `CONNECT_*` line's word and what follows `to`.
pub fn connect_said<'a>(lines: &'a [String], to: &str) -> Option<(&'a str, &'a str)> {
    lines.iter().find_map(|l| {
        let mut parts = l.splitn(3, ' ');
        let (word, said_to, rest) = (parts.next()?, parts.next()?, parts.next().unwrap_or_default());
        (said_to == to && matches!(word, "CONNECT_OK" | "CONNECT_REFUSED" | "CONNECT_FAIL")).then_some((word, rest.trim()))
    })
}

/// The notices in a session's timeline at `level`, their words.
pub fn notices(events: &[Value], level: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.get("kind").and_then(Value::as_str) == Some("notice") && e.get("level").and_then(Value::as_str) == Some(level))
        .filter_map(|e| e.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// This machine's guests' platform, as kiln builds their images (`sandbox::image`).
fn platform() -> anyhow::Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("linux/amd64"),
        "aarch64" => Ok("linux/arm64"),
        other => anyhow::bail!("Shards builds no images for {other}"),
    }
}

/// Goose's config in the person's home on the runner, as kiln's Goose image reads it (its harness.json's
/// `GOOSE_PATH_ROOT`, on the person's volume: `harness::isolated::home`): its path on the host, and in
/// the microVM.
fn config_of(f: &Fixture) -> anyhow::Result<(PathBuf, String)> {
    let b = goose()?;
    let (volumes, env) = kiln::harness::isolated::home::of_builtin(b);
    let root = env.iter().find(|(k, _)| k == "GOOSE_PATH_ROOT").map(|(_, v)| v.clone()).ok_or_else(|| anyhow::anyhow!("kiln's Goose image roots no GOOSE_PATH_ROOT"))?;
    let guest = format!("{root}/config/config.yaml");
    let homes = kiln::harness::isolated::home::root(&vm_e2e::own_state(&f.dir, RUNNER));
    let layout = kiln::harness::isolated::home::Layout::new(&homes, PERSON, b.harness, &volumes, &env).map_err(|e| anyhow::anyhow!("{PERSON}'s home for goose: {e}"))?;
    let host = layout.host_of(&guest).ok_or_else(|| anyhow::anyhow!("{guest} is on no volume of the person's"))?;
    Ok((host, guest))
}

/// The stand-in as kiln's Goose image, built into the runner's Shards home under the tag kiln finds
/// Goose's image by: its digest (`sha256:…`).
fn plant(f: &Fixture) -> anyhow::Result<String> {
    let b = goose()?;
    let text = stand_in_as_goose(b.text, &vm_e2e::this_commit()?, &vm_e2e::kiln_repository()?)?;
    let tag = kiln::sandbox::image::tag_of(kiln::sandbox::image::Source::Builtin(b)).map_err(|e| anyhow::anyhow!("goose's image tag: {e}"))?;
    let held = kiln::shards::write_out(&f.dir.join("shards"))?;
    let (context, config, iid) = (f.dir.join("planted"), f.dir.join("planted-docker-config"), f.dir.join("planted.iid"));
    std::fs::create_dir_all(&context)?;
    std::fs::create_dir_all(&config)?;
    std::fs::write(context.join("Agentfile"), &text)?;
    let out = std::process::Command::new(&held.shards)
        .args(["build", "--progress=quiet", "--platform", platform()?, "--file"])
        .arg(context.join("Agentfile"))
        .arg("--tag")
        .arg(&tag)
        .arg("--iidfile")
        .arg(&iid)
        .arg(&context)
        .env("SHARDS_HOME", kiln::sandbox::shards_home(&vm_e2e::own_state(&f.dir, RUNNER)))
        .env("DOCKER_CONFIG", &config)
        .output()?;
    anyhow::ensure!(out.status.success(), "the stand-in as kiln's Goose image ({tag}) wasn't built: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Ok(std::fs::read_to_string(&iid)?.trim().to_owned())
}

/// A Goose session of kiln's own Agentfile in a microVM, started with no prompt: its id, and kiln's
/// word on it once it runs the stand-in's image `digest`. A session of any other image is stopped
/// before it's given a word.
fn started(f: &Fixture, project: &str, digest: &str) -> anyhow::Result<(String, Value)> {
    let id = vm_e2e::open(f, &json!({"kind": "goose", "cwd": project, "isolation": {"kind": "microvm"}}))?;
    let ran = vm_e2e::until(f, &id, vm_e2e::turn_within(), "its microVM", |i, _| i.pointer("/runs/image").and_then(Value::as_str).is_some() || vm_e2e::status(i) == "exited");
    let (info, _) = match ran {
        Ok(seen) => seen,
        Err(e) => {
            let _ = vm_e2e::stop(f, &id);
            return Err(e);
        }
    };
    let image = info.pointer("/runs/image").and_then(Value::as_str).unwrap_or_default();
    if image != format!("harness-goose@{digest}") {
        let _ = vm_e2e::stop(f, &id);
        anyhow::bail!("kiln booted {image:?}, not the stand-in built as its Goose image (harness-goose@{digest}): it was stopped, given no word");
    }
    Ok((id, info))
}

/// The stand-in's `CONNECT=` turn for each of `to`, through kiln: what it said once its turn ran to its end.
fn probed(f: &Fixture, id: &str, client: &str, to: &[&str]) -> anyhow::Result<(Vec<Value>, Vec<String>)> {
    let words: Vec<String> = to.iter().map(|t| format!("CONNECT={t}")).collect();
    vm_e2e::command(f, id, &json!({"type": "send", "id": client, "text": format!("{} do the task", words.join(" ")), "mode": "queue"}))?;
    let (_, events) = vm_e2e::until(f, id, vm_e2e::turn_within(), "its turn", |_, e| vm_e2e::said_marker(&vm_e2e::said_after(e, client), vm_e2e::STUB_DONE))?;
    let lines = vm_e2e::said_after(&events, client);
    Ok((events, lines))
}

/// That the stand-in ran in a microVM's kernel, said as `WHERE`.
fn in_guest(lines: &[String]) -> anyhow::Result<()> {
    vm_e2e::check_in_guest(vm_e2e::where_said(lines).as_ref(), std::env::consts::OS, vm_e2e::host_boot_id().as_deref()).map_err(anyhow::Error::msg)
}

#[test]
fn s17_a_goose_session_reaches_its_persons_providers_hosts_alone_and_says_so_where_it_cant_tell() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let f = vm_e2e::up_own(dir.path(), RUNNER, &[])?;
    if !vm_e2e::isolation_ready(&f)? {
        return Ok(());
    }
    let project = vm_e2e::project(dir.path())?;
    let digest = plant(&f)?;
    let (config, guest_config) = config_of(&f)?;
    let parent = config.parent().ok_or_else(|| anyhow::anyhow!("{} is in no folder", config.display()))?;
    std::fs::create_dir_all(parent)?;

    // Narrowed: the person's home names Anthropic, so the run keeps Anthropic's hosts and Goose's own
    // alone, and the microVM's network refuses another provider Goose's image grants.
    std::fs::write(&config, format!("GOOSE_PROVIDER: {}\n", KEPT.0))?;
    let (id, info) = started(&f, &project, &digest)?;
    let what = "a session whose home names anthropic";
    anyhow::ensure!(info.pointer("/isolation/person").and_then(Value::as_str) == Some(PERSON), "{what}: kiln says it acts for {:?}", info.pointer("/isolation/person"));
    let hosts = kept_for("goose", &[KEPT.0])?;
    anyhow::ensure!(hosts.iter().any(|h| h == KEPT.1) && !hosts.iter().any(|h| h == DROPPED), "kiln's table for goose keeps {hosts:?} for {}", KEPT.0);
    let reach = info.pointer("/runs/reach").cloned().unwrap_or(Value::Null);
    anyhow::ensure!(reach == json!({"as": "narrowed", "uses": [KEPT.0], "hosts": hosts}), "{what}: its reach is {reach}, not narrowed to {hosts:?}");
    let (events, lines) = probed(&f, &id, "narrowed-1", &[KEPT.1, DROPPED])?;
    in_guest(&lines).map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    anyhow::ensure!(matches!(connect_said(&lines, KEPT.1), Some(("CONNECT_OK", _))), "{what}: {} wasn't reached from its microVM: {lines:?}", KEPT.1);
    anyhow::ensure!(connect_said(&lines, DROPPED) == Some(("CONNECT_REFUSED", "dns rcode 5")), "{what}: {DROPPED} wasn't refused by its network (REFUSED, RCODE 5): {lines:?}");
    anyhow::ensure!(notices(&events, "warn").iter().all(|n| !n.contains("isn't narrowed")), "{what}: it says it isn't narrowed: {:?}", notices(&events, "warn"));
    vm_e2e::stop(&f, &id)?;

    // Whole, and said: the person's config can't be read (a folder where its file is), so kiln can't
    // tell the provider; the run keeps every destination its image names, and the session says why.
    std::fs::remove_file(&config)?;
    std::fs::create_dir_all(&config)?;
    let (id, info) = started(&f, &project, &digest)?;
    let what = "a session whose home can't be read";
    let reach = info.pointer("/runs/reach").cloned().unwrap_or(Value::Null);
    anyhow::ensure!(
        reach.get("as").and_then(Value::as_str) == Some("whole") && reach.pointer("/why/unread/file").and_then(Value::as_str) == Some(guest_config.as_str()),
        "{what}: its reach is {reach}, not whole for want of reading {guest_config}"
    );
    let (events, lines) = probed(&f, &id, "whole-1", &[KEPT.1, DROPPED])?;
    in_guest(&lines).map_err(|e| anyhow::anyhow!("{what}: {e}"))?;
    let warned = notices(&events, "warn");
    anyhow::ensure!(warned.iter().any(|n| n.contains("isn't narrowed") && n.contains(&guest_config)), "{what}: no notice says it isn't narrowed, and why: {warned:?}");
    for to in [KEPT.1, DROPPED] {
        anyhow::ensure!(matches!(connect_said(&lines, to), Some(("CONNECT_OK", _))), "{what}: {to}, which its image grants, wasn't reached: {lines:?}");
    }
    vm_e2e::stop(&f, &id)?;
    Ok(())
}

/// The checks above, against samples.
mod checks {
    use super::*;

    #[test]
    fn s17_the_stand_in_replaces_gooses_harness_alone() -> anyhow::Result<()> {
        let text = goose()?.text;
        let planted = stand_in_as_goose(text, "c0ffee", "https://github.com/hyper-light/kiln")?;
        let (dir, at) = vm_e2e::STUB_HARNESS;
        assert!(planted.contains(&format!("HARNESS goose FROM https://github.com/hyper-light/kiln.git#c0ffee:{dir} TO {at}\n")), "{planted}");
        assert!(planted.contains("ENV VM_STUB_PROTOCOL=goose\n"));
        let kiln_lines: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with("HARNESS ")).collect();
        let planted_lines: Vec<&str> = planted.lines().filter(|l| !l.trim_start().starts_with("HARNESS ") && !l.starts_with("ENV VM_STUB_PROTOCOL=")).collect();
        assert_eq!(planted_lines, kiln_lines, "every other line kiln's: its grants, volume and base");
        assert!(stand_in_as_goose("FROM x\n", "c", "r").is_err(), "an Agentfile with no Goose harness isn't one to plant");
        Ok(())
    }

    #[test]
    fn s17_what_the_stand_in_said_of_a_destination_is_read_by_its_word() {
        let lines: Vec<String> = ["CWD /p", "CONNECT_OK api.anthropic.com:443 160.79.104.10", "CONNECT_REFUSED api.groq.com:443 dns rcode 5", "STUB_DONE"].map(String::from).to_vec();
        assert_eq!(connect_said(&lines, "api.anthropic.com:443"), Some(("CONNECT_OK", "160.79.104.10")));
        assert_eq!(connect_said(&lines, "api.groq.com:443"), Some(("CONNECT_REFUSED", "dns rcode 5")));
        assert_eq!(connect_said(&lines, "api.openai.com:443"), None);
        assert_eq!(connect_said(&["CONNECT_OK api.anthropic.com:4430 1.2.3.4".to_owned()], "api.anthropic.com:443"), None, "a destination is read whole");
    }

    #[test]
    fn s17_goose_keeps_anthropics_hosts_and_not_groqs() -> anyhow::Result<()> {
        let hosts = kept_for("goose", &[KEPT.0])?;
        assert!(hosts.iter().any(|h| h == KEPT.1), "{hosts:?}");
        assert!(!hosts.iter().any(|h| h == DROPPED), "{hosts:?}");
        assert!(kept_for("goose", &["no-such-provider"]).is_err());
        Ok(())
    }

    #[test]
    fn s17_a_warning_notice_is_read_from_the_timeline() {
        let events = vec![
            json!({"kind": "notice", "level": "warn", "text": "Its network isn't narrowed to its own provider or sign-in, as /h/x couldn't be read: it keeps every destination its image names."}),
            json!({"kind": "notice", "level": "info", "text": "isn't narrowed, but info"}),
            json!({"kind": "assistant", "text": "isn't narrowed"}),
        ];
        assert_eq!(notices(&events, "warn").len(), 1);
    }
}
