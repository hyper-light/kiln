use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::harness::model::{Command, Delivery, SendMode, Status};
use crate::harness::session::{self, Files, Session, Spec};

use super::{Scanner, devin};

struct Running(Arc<Session>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.close();
    }
}

async fn until(mut ready: impl FnMut() -> bool + Send) -> anyhow::Result<()> {
    crate::rt::timeout(Duration::from_secs(60), async {
        while !ready() {
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
    }).await.map_err(|_| anyhow::anyhow!("Devin's native conversation did not become ready"))
}

#[test]
#[ignore = "requires an installed, signed-in Devin CLI"]
fn installed_devin_native_history_and_repl_ownership() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let config = dir.path().join("config");
        let source = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map_or_else(|| crate::os::home_dir().join(".local/share"), std::path::PathBuf::from).join("devin/credentials.toml");
        anyhow::ensure!(source.is_file(), "sign in to the installed Devin CLI first");
        std::fs::create_dir_all(data.join("devin"))?;
        std::fs::create_dir_all(config.join("devin"))?;
        std::os::unix::fs::symlink(source, data.join("devin/credentials.toml"))?;
        std::fs::write(config.join("devin/config.json"), serde_json::json!({
            "auto_update": false, "notify": "never", "theme_mode": "nocolor",
            "read_config_from": {"agents_standard": false, "cursor": false, "windsurf": false, "claude": false, "copilot": false, "opencode": false, "zed": false}
        }).to_string())?;
        let env = vec![("XDG_DATA_HOME".into(), data.to_string_lossy().into_owned()), ("XDG_CONFIG_HOME".into(), config.to_string_lossy().into_owned()), ("XDG_CACHE_HOME".into(), dir.path().join("cache").to_string_lossy().into_owned())];
        let spec = Spec { kind: "devin".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), mode: Some("ask".into()), env: env.clone(), reach: Some("off".into()), ..Spec::default() };
        let (tx, _rx) = async_channel::unbounded();
        let files = Files { dir: dir.path().join("attachments"), take: Arc::new(|_| None), spawn: Arc::new(|_| {}), release: Arc::new(|_| {}), carried: Arc::new(|_: String| Box::pin(async { Ok(Vec::new()) })), fence: Arc::new(|_: String, _: bool| Box::pin(async { Ok(()) })), settle: Arc::new(|_: String, _: String, _: bool| Box::pin(async { Ok(None) })) };
        let running = Running(session::start(crate::util::new_id(), spec, tx, Arc::new(|| {}), files)?);
        let s = &running.0;
        until(|| { let c = s.core.lock(); c.state.status == Status::Idle && !c.meta.models.is_empty() }).await?;
        let prompt = "Reply with exactly KILN_DEVIN_NATIVE_OK. Do not use tools or access files.";
        assert!(s.command(Command::Send { id: "native-answer".into(), text: prompt.into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None }));
        until(|| s.core.lock().state.delivery.get("native-answer") == Some(&Delivery::Completed)).await?;
        let (own, model) = {
            let c = s.core.lock();
            (c.state.harness_session.clone().ok_or_else(|| anyhow::anyhow!("no Devin session ID"))?, c.state.model.clone())
        };
        let mut scanner = Scanner::new(dir.path().to_path_buf());
        scanner.claude = dir.path().join("claude");
        scanner.codex = dir.path().join("codex");
        scanner.devin = data.join("devin/cli");
        assert!(!scanner.scan(&HashMap::new()).iter().any(|r| r.id == own));
        assert!(!scanner.history(Some("devin"), 10, &HashSet::new()).iter().any(|r| r.id == own));
        s.close();
        until(|| s.core.lock().state.status == Status::Exited).await?;
        until(|| scanner.history(Some("devin"), 10, &HashSet::new()).iter().any(|r| r.id == own)).await?;
        let past = scanner.history(Some("devin"), 10, &HashSet::new());
        assert_eq!(past.len(), 1);
        let past = past.first().ok_or_else(|| anyhow::anyhow!("no native history"))?;
        assert_eq!((past.id.as_str(), past.kind.as_str(), past.last_prompt.as_str(), past.model.as_str()), (own.as_str(), "devin", prompt, model.as_str()));
        assert_eq!(past.cwd, dir.path().to_string_lossy());
        let entries = devin::transcript(&scanner.devin, &own, 10).ok_or_else(|| anyhow::anyhow!("no native transcript"))?;
        assert_eq!(entries.iter().map(|e| (e.role.as_str(), e.text.as_str())).collect::<Vec<_>>(), [("user", prompt), ("assistant", "KILN_DEVIN_NATIVE_OK")]);
        assert!(entries.iter().all(|e| !e.at.is_empty()));

        let program = crate::os::which("devin").ok_or_else(|| anyhow::anyhow!("Devin is not installed"))?;
        let args = vec![program.to_string_lossy().into_owned(), "--respect-workspace-trust".into(), "false".into(), "-r".into(), own.clone()];
        let env: Vec<(&str, String)> = env.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let (pty, output) = crate::terminal::spawn_at(None, None, Some(&args), Some(dir.path()), 100, 30, &env)?;
        let pid = pty.pid().ok_or_else(|| anyhow::anyhow!("no native REPL pid"))?;
        let ended = crate::rt::spawn(async move {
            while let Ok(out) = output.recv().await {
                if matches!(out, crate::terminal::PtyOut::Exit(_)) { break; }
            }
        });
        until(|| scanner.scan(&HashMap::new()).iter().any(|r| r.pid == pid && r.id == own && r.last_reply == "KILN_DEVIN_NATIVE_OK")).await?;
        assert!(scanner.transcript(pid).is_some_and(|e| e.iter().any(|e| e.role == "assistant" && e.text == "KILN_DEVIN_NATIVE_OK")));
        assert!(scanner.history(Some("devin"), 10, &HashSet::new()).is_empty());
        pty.write(vec![4]);
        crate::rt::timeout(Duration::from_secs(10), ended).await??;
        Ok(())
    })
}
