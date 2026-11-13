//! The installed Devin's catalog for the existing start dialog, before a person starts a session.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Acp, Command, Core};
use crate::harness::kinds::Kind;
use crate::harness::model::ModelOption;
use crate::harness::session::Spec;

#[derive(Default)]
struct Warmup {
    running: bool,
    done: bool,
    retry_at: Option<Instant>,
}

impl Warmup {
    fn begin(&mut self, now: Instant) -> bool {
        if self.running || self.done || self.retry_at.is_some_and(|at| now < at) {
            return false;
        }
        self.running = true;
        true
    }

    fn finish(&mut self, now: Instant, success: bool) {
        self.running = false;
        self.done = success;
        self.retry_at = (!success).then(|| now + Duration::from_secs(60));
    }
}

#[cfg(not(test))]
pub(super) fn warm_models() {
    static STATE: std::sync::LazyLock<parking_lot::Mutex<Warmup>> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(Warmup::default()));
    if !STATE.lock().begin(Instant::now()) {
        return;
    }
    let Some(program) = Kind::get("devin").and_then(Kind::find) else {
        STATE.lock().finish(Instant::now(), false);
        return;
    };
    /// The slowest answer a Devin catalog has given in this process, ms, plus one (none: no answer yet):
    /// [`discover`]'s window, which every catalog after the one that measured it is held to.
    static SLOWEST: AtomicU64 = AtomicU64::new(0);
    crate::rt::spawn(async move {
        let config = crate::os::config_home().join("devin/config.json");
        let models = discover(program, &SLOWEST, &config).await.ok().filter(|m| !m.is_empty());
        if let Some(models) = &models {
            crate::harness::kinds::note_models("devin", models);
        }
        STATE.lock().finish(Instant::now(), models.is_some());
    });
}

/// What ended a catalog ask before it had what it asked for, typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Its process closed its output (it ended) with a word of kiln's unanswered.
    Exited,
    /// Asked something, it said nothing for a whole window of the slowest answer a catalog has given in this
    /// process (`slowest_ms`), and its CPU time (`cpu_us`) was as it was at the window's start.
    Stalled { slowest_ms: u64, cpu_us: Option<u64> },
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited => f.write_str("Devin's catalog ended before it answered"),
            Self::Stalled { slowest_ms, cpu_us } => write!(f, "Devin's catalog said nothing for a whole window of the slowest answer a catalog has given ({slowest_ms} ms), its CPU time {cpu_us:?} µs as it was at the window's start"),
        }
    }
}

impl std::error::Error for Ended {}

/// Asks the installed Devin for its catalog, in a session of its own, and ends it once done or once it ends,
/// typed ([`Ended`]). How long it may take follows its liveness and progress alone, as the capacity keeper's
/// waits do (`kiln_ci::place::keeper::Waits`): no time of kiln's own, no multiplier (R03, the person's word,
/// 2026-11-13). Each word kiln says is timed to its answer, and the slowest answer a catalog has given in this
/// process (`slowest`, ms plus one; none before the first) is the window: a catalog asked something that says
/// nothing for a whole window is asked whether its process's CPU time moved in it, and the window starts again
/// if it did; if it didn't, the catalog is ended. Until the first answer there is no such window (the cold
/// start: a process starting on a loaded machine can use no CPU time for longer than any answer seen), so the
/// wait follows its liveness alone and ends, typed, when it does ([`Ended::Exited`]). What it had listed when a
/// later word stalled is kept. Its whole session had a fixed 20 s deadline, and the tests' stand-in 200 ms,
/// which a loaded machine's stand-in outran before its first write (adas-mac-studio-3.local, 2026-11-13).
async fn discover(program: PathBuf, slowest: &AtomicU64, user_config: &Path) -> anyhow::Result<Vec<ModelOption>> {
    let dir = std::env::temp_dir().join(format!("kiln-devin-models-{}", crate::util::new_id()));
    let result = async {
        std::fs::create_dir(&dir)?;
        let config = dir.join("config.json");
        std::fs::write(&config, probe_config(user_config).to_string())?;
        let spec = Spec { kind: "devin".into(), program: Some(program.to_string_lossy().into_owned()), cwd: Some(dir.to_string_lossy().into_owned()), reach: Some("off".into()), ..Spec::default() };
        let kind = Kind::get("devin").ok_or_else(|| anyhow::anyhow!("no Devin harness"))?;
        let mut process = crate::proc::spawn_duplex(kind.cli(&spec, &["--config".into(), config.to_string_lossy().into_owned(), "acp".into()])?)?;
        let stderr = process.stderr.clone();
        crate::rt::spawn(async move { while stderr.recv().await.is_ok() {} });
        let mut learned = Vec::new();
        let result = async {
            use futures::FutureExt as _;
            let clock = crate::rt::clock();
            let began = clock.now();
            let ms = || i64::try_from(clock.now().saturating_duration_since(began).as_millis()).unwrap_or(i64::MAX);
            let pid = process.pid;
            let cpu = || crate::os::metrics::cpu_us_of(&[pid]);
            let mut waits = kiln_ci::place::keeper::Waits::default();
            // The ids of kiln's words not yet answered, oldest first, and the process's CPU time when it last moved.
            let (mut owed, mut spent) = (Vec::<u64>::new(), cpu());
            let mut adapter = Acp::from_spec(&spec);
            let mut core = Core::new();
            core.state.cwd = spec.cwd.clone().unwrap_or_default();
            adapter.start(&mut core);
            let mut buffer = Vec::new();
            let mut received = 0usize;
            let mut models = None;
            loop {
                for write in core.take_writes() {
                    let id = serde_json::from_str::<Value>(&write).ok().and_then(|v| v.get("id").and_then(Value::as_u64));
                    if let Some(id) = id.filter(|id| adapter.pending.contains_key(id)) {
                        waits.said(ms());
                        owed.push(id);
                    }
                    process.stdin.send(format!("{write}\n").into_bytes()).await?;
                }
                core.take_out();
                if adapter.ready && adapter.pending.is_empty() {
                    if models.is_none() {
                        for model in &mut core.meta.models {
                            if model.id != core.state.model {
                                model.efforts.clear();
                            }
                        }
                        models = Some(core.meta.models.iter().filter(|m| m.id != core.state.model).take(120).map(|m| m.id.clone()).collect::<std::collections::VecDeque<_>>());
                    }
                    learned.clone_from(&core.meta.models);
                    let Some(model) = models.as_mut().and_then(|m| m.pop_front()) else { return Ok::<_, anyhow::Error>(()) };
                    adapter.command(&mut core, Command::SetModel { model, kind: None });
                    continue;
                }
                anyhow::ensure!(!adapter.pending.is_empty(), "Devin did not open its catalog session");
                let bytes = loop {
                    if let Some(s) = slowest.load(Ordering::Relaxed).checked_sub(1) {
                        waits.slowest_at_least(s);
                    }
                    let still = waits.due().map(|due| clock.sleep(Duration::from_millis(u64::try_from(due.saturating_sub(ms())).unwrap_or(0))));
                    futures::select_biased! {
                        bytes = process.stdout.recv().fuse() => break bytes.map_err(|_| Ended::Exited)?,
                        () = async move { match still { Some(s) => s.await, None => std::future::pending().await } }.fuse() => {
                            let now = cpu();
                            if now.is_some() && now != spent {
                                spent = now;
                                waits.heard(ms());
                                continue;
                            }
                            return Err(Ended::Stalled { slowest_ms: waits.slowest().unwrap_or(0), cpu_us: now }.into());
                        }
                    }
                };
                let now = ms();
                (spent, received) = (cpu(), received.saturating_add(bytes.len()));
                waits.heard(now);
                anyhow::ensure!(received <= 16 * 1024 * 1024, "Devin's catalog exceeded its output limit");
                buffer.extend_from_slice(&bytes);
                anyhow::ensure!(buffer.len() <= 512 * 1024, "Devin's catalog exceeded its line limit");
                while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=end).collect();
                    adapter.line(&mut core, &String::from_utf8_lossy(&line));
                }
                owed.retain(|id| {
                    if adapter.pending.contains_key(id) {
                        return true;
                    }
                    if let Some(took) = waits.answered(now) {
                        slowest.fetch_max(took.saturating_add(1), Ordering::Relaxed);
                    }
                    false
                });
            }
        }.await;
        // Ended at once (no grace: it holds nothing of the person's), so its exit comes: no wait of kiln's own on it.
        process.terminate(Duration::ZERO);
        let _ = (&mut process.exit).await;
        if learned.is_empty() {
            result?;
        }
        Ok(learned)
    }.await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn probe_config(user: &Path) -> Value {
    let mut config = serde_json::Map::from_iter([
        ("auto_update".into(), json!(false)), ("notify".into(), json!("never")),
        ("read_config_from".into(), json!({"agents_standard": false, "cursor": false, "windsurf": false, "claude": false, "copilot": false, "opencode": false, "zed": false})),
    ]);
    let model = std::fs::read_to_string(user)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&kiln_cd::jsonc::strip(&text)).ok())
        .and_then(|v| v.pointer("/agent/model").and_then(Value::as_str).map(str::to_owned));
    if let Some(model) = model {
        config.insert("agent".into(), json!({"model": model}));
    }
    Value::Object(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_retries_failures_after_a_cooldown_with_only_one_probe_in_flight() {
        let mut state = Warmup::default();
        let start = Instant::now();
        assert!(state.begin(start));
        assert!(!state.begin(start + Duration::from_secs(61)));
        state.finish(start + Duration::from_secs(10), false);
        assert!(!state.begin(start + Duration::from_secs(69)));
        assert!(state.begin(start + Duration::from_secs(70)));
        assert!(!state.begin(start + Duration::from_secs(71)));
        state.finish(start + Duration::from_secs(72), false);
        assert!(!state.begin(start + Duration::from_secs(131)));
        assert!(state.begin(start + Duration::from_secs(132)));
    }

    #[test]
    fn a_successful_catalog_finishes_warmup_permanently() {
        let mut state = Warmup::default();
        let start = Instant::now();
        assert!(state.begin(start));
        state.finish(start + Duration::from_secs(1), true);
        assert!(!state.begin(start + Duration::from_secs(60)));
        assert!(!state.begin(start + Duration::from_secs(86400)));
    }

    #[test]
    fn the_probe_keeps_only_the_users_default_model_from_commented_config() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("devin/config.json");
        std::fs::create_dir_all(dir.path().join("devin"))?;
        std::fs::write(&config, r#"{
            // Devin supports comments in the native user config.
            "agent": {"model": "gpt-6-sol-medium", "subagent_model": "other"},
            "auto_update": true, "notify": "always",
            "read_config_from": {"claude": true},
            "mcpServers": {"synthetic": {"command": "must-not-start"}},
            "hooks": {"session_start": "must-not-run"},
            "ignored": "https://example.test// /* stays quoted */",
            /* A block comment, too. */
            "permissions": {"allow": ["*"]}
        }"#)?;
        assert_eq!(probe_config(&config), json!({
            "agent": {"model": "gpt-6-sol-medium"}, "auto_update": false, "notify": "never",
            "read_config_from": {"agents_standard": false, "cursor": false, "windsurf": false, "claude": false, "copilot": false, "opencode": false, "zed": false}
        }));
        Ok(())
    }

    #[test]
    fn absent_invalid_or_non_string_default_models_keep_the_native_fallback() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("config.json");
        let default = probe_config(&config);
        assert!(default.get("agent").is_none());
        for text in ["not json", "{}", r#"{"agent": {"model": null}}"#, r#"{"agent": {"model": 42}}"#, r#"{"model": "other", "mcpServers": {"model": "decoy"}}"#] {
            std::fs::write(&config, text)?;
            assert_eq!(probe_config(&config), default);
        }
        Ok(())
    }

    #[cfg(unix)]
    /// A stand-in Devin: it writes its files first (before any answer, so a test reads them whatever ended it), then
    /// gives `answers` in order, one per word it reads, and then reads on, answering nothing. `before[i]` is shell
    /// run before the `i`th answer. Its first answer comes after `sleep 0.2`: the judge learns its window from the
    /// answers it times, and a stand-in that answers in the few µs a shell takes would hold the next word to a window
    /// a loaded test machine's scheduler outruns; this one's pace is a slow CLI's starting.
    fn agent(dir: &std::path::Path, answers: &[serde_json::Value], before: &[&str]) -> anyhow::Result<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        let mut script = format!("#!/bin/sh\nprintf '%s\\n' \"$2\" > '{}/config-path'\nprintf '%s\\n' \"$$\" > '{}/pid'\ncat \"$2\" > '{}/config'\n", dir.display(), dir.display(), dir.display());
        for (i, answer) in answers.iter().enumerate() {
            let pace = if i == 0 { "sleep 0.2\n" } else { "" };
            let before = before.get(i).copied().unwrap_or_default();
            script.push_str(&format!("IFS= read -r line || exit\nprintf '%s\\n' \"$line\" >> '{}/requests'\n{pace}{before}\nprintf '%s\\n' '{}'\n", dir.display(), answer));
        }
        script.push_str(&format!("while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}/requests'; done\n", dir.display()));
        let program = dir.join("agent");
        std::fs::write(&program, script)?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
        Ok(program)
    }

    #[cfg(unix)]
    fn cleaned(dir: &std::path::Path) -> anyhow::Result<()> {
        let config = PathBuf::from(std::fs::read_to_string(dir.join("config-path"))?.trim());
        assert!(!config.parent().is_some_and(std::path::Path::exists));
        let pid: i32 = std::fs::read_to_string(dir.join("pid"))?.trim().parse()?;
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn catalog_learns_each_models_efforts_without_prompts_and_cleans_up() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let options = |model: &str| json!([
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model, "options": [{"value": "default", "name": "Default"}, {"value": "careful", "name": "Careful"}]},
                {"id": "effort", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": "high", "options": if model == "default" { json!([{"value": "high", "name": "High"}]) } else { json!([{"value": "high", "name": "High"}, {"value": "max", "name": "Max"}]) }}
            ]);
            let program = agent(dir.path(), &[
                json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "catalog", "configOptions": options("default")}}),
                json!({"jsonrpc": "2.0", "id": 3, "result": {"configOptions": options("careful")}}),
            ], &[])?;
            let models = discover(program, &AtomicU64::new(0), &dir.path().join("user-config.json")).await?;
            assert_eq!(models.iter().map(|m| (m.id.as_str(), m.efforts.clone(), m.default)).collect::<Vec<_>>(), [
                ("default", vec!["high".to_owned()], true),
                ("careful", vec!["high".to_owned(), "max".to_owned()], false),
            ]);
            let requests: Vec<serde_json::Value> = std::fs::read_to_string(dir.path().join("requests"))?.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
            assert_eq!(requests.iter().filter_map(|r| r.get("method").and_then(serde_json::Value::as_str)).collect::<Vec<_>>(), ["initialize", "session/new", "session/set_config_option"]);
            assert_eq!(requests.get(1).and_then(|r| r.pointer("/params/mcpServers")), Some(&json!([])));
            assert_eq!(requests.get(2).and_then(|r| r.pointer("/params/value")), Some(&json!("careful")));
            let config: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("config"))?)?;
            assert_eq!(config.get("auto_update"), Some(&json!(false)));
            assert!(config.get("read_config_from").and_then(serde_json::Value::as_object).is_some_and(|imports| !imports.is_empty() && imports.values().all(|v| v == &json!(false))));
            cleaned(dir.path())?;
            Ok(())
        })
    }

    /// A catalog that starts slowly, as a loaded machine starts one, then answers `initialize` and goes truly
    /// unresponsive: blocked reading, writing nothing, answering nothing. It is ended, typed, a window of its
    /// slowest answer after its last word, and cleaned up. Its files, written before its first answer, show it: the
    /// fixed deadline this replaces ended it before it had started, leaving no `config-path` (the reported
    /// "No such file or directory", adas-mac-studio-3.local, 2026-11-13; this test's red, 789194a80).
    #[cfg(unix)]
    #[test]
    fn a_catalog_slow_to_start_then_unresponsive_is_ended_and_cleaned_up() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let d = dir.path().display();
            let answer = json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1}});
            let script = format!("#!/bin/sh\nsleep 1\nprintf '%s\\n' \"$2\" > '{d}/config-path'\nprintf '%s\\n' \"$$\" > '{d}/pid'\nIFS= read -r line || exit\nprintf '%s\\n' '{answer}'\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> '{d}/requests'; done\n");
            let program = dir.path().join("agent");
            std::fs::write(&program, script)?;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
            let slowest = AtomicU64::new(0);
            let ended = discover(program, &slowest, &dir.path().join("user-config.json")).await.err().ok_or_else(|| anyhow::anyhow!("an unresponsive catalog was not ended"))?;
            assert!(matches!(ended.downcast_ref::<Ended>(), Some(Ended::Stalled { .. })), "{ended:#}");
            assert!(slowest.load(Ordering::Relaxed) > 1000, "its slow start is the window it was held to");
            cleaned(dir.path())?;
            let requests = std::fs::read_to_string(dir.path().join("requests"))?;
            assert!(requests.contains("session/new"), "ended while owing session/new: {requests}");
            Ok(())
        })
    }

    /// Before its first answer a catalog has no window (its cold start), so its wait follows its liveness alone:
    /// one that ends without answering ends the ask, typed, and is cleaned up.
    #[cfg(unix)]
    #[test]
    fn a_catalog_that_ends_before_answering_ends_the_ask_typed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let program = agent(dir.path(), &[], &[])?;
            std::fs::write(&program, std::fs::read_to_string(&program)?.replace("while IFS= read -r line; do", "IFS= read -r line; exit 0\nwhile IFS= read -r line; do"))?;
            let ended = discover(program, &AtomicU64::new(0), &dir.path().join("user-config.json")).await.err().ok_or_else(|| anyhow::anyhow!("an ended catalog listed models"))?;
            assert_eq!(ended.downcast_ref::<Ended>(), Some(&Ended::Exited), "{ended:#}");
            cleaned(dir.path())?;
            Ok(())
        })
    }

    /// A catalog that keeps working (its CPU time moving) past a window of its slowest answer is waited on, and
    /// what it then answers is learned.
    #[cfg(unix)]
    #[test]
    fn a_catalog_busy_past_its_window_is_waited_on() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            // Busy in the shell itself (its own CPU time, not a child's) until two seconds pass by the clock's
            // second: a whole second at least, past a window of the 0.2 s its first answer took.
            let busy = r#"end=$(( $(date +%s) + 2 )); while [ "$(date +%s)" -lt "$end" ]; do i=0; while [ $i -lt 2000 ]; do i=$((i+1)); done; done"#;
            let program = agent(dir.path(), &[
                json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "catalog", "configOptions": [
                    {"id": "model", "category": "model", "type": "select", "currentValue": "default", "options": [{"value": "default", "name": "Default"}]},
                    {"id": "effort", "category": "thought_level", "type": "select", "currentValue": "high", "options": [{"value": "high", "name": "High"}]}
                ]}}),
            ], &["", busy])?;
            let slowest = AtomicU64::new(0);
            let models = discover(program, &slowest, &dir.path().join("user-config.json")).await?;
            assert_eq!(models.iter().map(|m| (m.id.as_str(), m.efforts.clone())).collect::<Vec<_>>(), [("default", vec!["high".to_owned()])]);
            assert!(slowest.load(Ordering::Relaxed) > 1000, "its busy answer took a second and more");
            cleaned(dir.path())?;
            Ok(())
        })
    }

    /// A word that stalls after the catalog listed its models keeps what was listed, and guesses no efforts for the
    /// model whose switch went unanswered.
    #[cfg(unix)]
    #[test]
    fn a_stall_keeps_the_catalog_without_guessing_unanswered_models_efforts() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let program = agent(dir.path(), &[
                json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1}}),
                json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "catalog", "configOptions": [
                    {"id": "model", "category": "model", "type": "select", "currentValue": "default", "options": [{"value": "default", "name": "Default"}, {"value": "unknown", "name": "Unknown"}]},
                    {"id": "effort", "category": "thought_level", "type": "select", "currentValue": "high", "options": [{"value": "high", "name": "High"}]}
                ]}}),
            ], &[])?;
            let models = discover(program, &AtomicU64::new(0), &dir.path().join("user-config.json")).await?;
            assert_eq!(models.iter().find(|m| m.default).map(|m| m.efforts.clone()), Some(vec!["high".into()]));
            assert!(models.iter().find(|m| m.id == "unknown").is_some_and(|m| m.efforts.is_empty()));
            cleaned(dir.path())?;
            Ok(())
        })
    }

    #[test]
    #[ignore = "requires an installed, signed-in Devin CLI"]
    fn installed_devin_lists_models_and_their_efforts_without_a_prompt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let program = crate::os::which("devin").ok_or_else(|| anyhow::anyhow!("no Devin CLI"))?;
            anyhow::ensure!(std::env::var_os("DEVIN_MODEL").is_none(), "unset DEVIN_MODEL to check the configured default");
            let dir = tempfile::tempdir()?;
            let config = dir.path().join("devin/config.json");
            let models = discover(program.clone(), &AtomicU64::new(0), &config).await?;
            anyhow::ensure!(!models.is_empty(), "Devin did not list any models");
            anyhow::ensure!(models.iter().any(|m| m.default && !m.efforts.is_empty()), "Devin did not list its default model's efforts");
            let chosen = models.iter().find(|m| !m.default && !m.efforts.is_empty()).ok_or_else(|| anyhow::anyhow!("Devin did not list another model with efforts"))?;
            std::fs::create_dir_all(dir.path().join("devin"))?;
            std::fs::write(&config, format!("// Synthetic user default.\n{}\n", json!({"agent": {"model": chosen.id}})))?;
            let configured = discover(program, &AtomicU64::new(0), &config).await?;
            anyhow::ensure!(configured.iter().any(|m| m.default && m.id == chosen.id && m.efforts == chosen.efforts), "Devin's configured default and its efforts were not kept");
            eprintln!("Devin listed {} models; {} have effort choices", models.len(), models.iter().filter(|m| !m.efforts.is_empty()).count());
            Ok(())
        })
    }
}
