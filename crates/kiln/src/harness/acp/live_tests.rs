use std::sync::Arc;
use std::time::Duration;

use crate::harness::core::Core;
use crate::harness::model::{Body, Command, Delivery, SendMode, Status};
use crate::harness::session::{self, Files, Session, Spec};

struct Running(Arc<Session>);

impl Drop for Running {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn open(spec: Spec, dir: &std::path::Path) -> anyhow::Result<Running> {
    let (tx, _rx) = async_channel::unbounded();
    let files = Files { dir: dir.join("attachments"), take: Arc::new(|_| None), spawn: Arc::new(|_| {}), release: Arc::new(|_| {}), carried: Arc::new(|_: String| Box::pin(async { Ok(Vec::new()) })), fence: Arc::new(|_: String, _: bool| Box::pin(async { Ok(()) })), settle: Arc::new(|_: String, _: String, _: bool| Box::pin(async { Ok(None) })) };
    Ok(Running(session::start(crate::util::new_id(), spec, tx, Arc::new(|| {}), files)?))
}

async fn until(s: &Session, label: &str, ready: impl Fn(&Core) -> bool + Send + Sync) -> anyhow::Result<()> {
    crate::rt::timeout(Duration::from_secs(60), async {
        loop {
            {
                let core = s.core.lock();
                if ready(&core) {
                    return Ok(());
                }
                anyhow::ensure!(core.state.status != Status::Exited, "Devin exited before {label}: {:?}", core.state.exit);
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Devin timed out before {label}"))?
}

fn send(s: &Session, id: &str, text: &str) {
    assert!(s.command(Command::Send { id: id.into(), text: text.into(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None}));
}

#[test]
#[ignore = "requires an installed, signed-in Devin CLI"]
fn installed_devin_uses_the_existing_session_controls_and_resumes() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("config/devin");
        std::fs::create_dir_all(&config)?;
        // Imports could launch the user's configured MCP servers.
        std::fs::write(config.join("config.json"), serde_json::json!({
            "auto_update": false, "notify": "never",
            "read_config_from": {"agents_standard": false, "cursor": false, "windsurf": false, "claude": false, "copilot": false, "opencode": false, "zed": false}
        }).to_string())?;
        let spec = Spec {
            kind: "devin".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), mode: Some("ask".into()),
            env: vec![("XDG_CONFIG_HOME".into(), dir.path().join("config").to_string_lossy().into_owned())],
            reach: Some("off".into()), ..Spec::default()
        };
        let running = open(spec, dir.path())?;
        let s = &running.0;
        until(s, "opening the session", |c| c.state.status == Status::Idle && c.state.mode == "ask" && !c.meta.models.is_empty()).await?;
        let model = {
            let core = s.core.lock();
            core.meta.models.iter().find(|m| m.id != core.state.model && m.id != "adaptive").map(|m| m.id.clone()).ok_or_else(|| anyhow::anyhow!("no other Devin model"))?
        };
        assert!(s.command(Command::SetModel { model: model.clone(), kind: None }));
        until(s, "switching models", |c| c.state.model == model).await?;
        let effort = {
            let core = s.core.lock();
            core.meta.efforts.iter().find(|e| e.id != core.state.effort).map(|e| e.id.clone()).ok_or_else(|| anyhow::anyhow!("no other Devin effort"))?
        };
        assert!(s.command(Command::SetEffort { effort: effort.clone() }));
        until(s, "switching effort", |c| c.state.effort == effort).await?;
        assert!(s.command(Command::SetMode { mode: "plan".into() }));
        until(s, "switching modes", |c| c.state.mode == "plan").await?;
        assert!(s.command(Command::SetMode { mode: "ask".into() }));
        until(s, "returning to Ask", |c| c.state.mode == "ask").await?;

        send(s, "cancel", "Reply only KILN_CANCEL_PROBE. Do not use tools or access files.");
        until(s, "starting a cancellable turn", |c| c.state.delivery.get("cancel") == Some(&Delivery::Started)).await?;
        send(s, "queued", "/mcp");
        assert!(s.command(Command::Interrupt));
        until(s, "cancelling and draining the queue", |c| c.state.delivery.get("cancel") == Some(&Delivery::Cancelled) && c.state.delivery.get("queued") == Some(&Delivery::Completed) && c.state.status == Status::Idle).await?;

        send(s, "answer", "Reply with exactly KILN_DEVIN_SMOKE_OK. Do not use tools or access files.");
        until(s, "completing the answer", |c| c.state.delivery.get("answer") == Some(&Delivery::Completed) && c.state.status == Status::Idle).await?;
        assert!(s.core.lock().events().any(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text.trim() == "KILN_DEVIN_SMOKE_OK")));
        let resumed_spec = s.resume_spec();
        let own = resumed_spec.resume.clone().ok_or_else(|| anyhow::anyhow!("Devin did not supply a session ID"))?;
        s.close();
        until(s, "closing the first process", |c| c.state.status == Status::Exited).await?;

        let resumed = open(resumed_spec, dir.path())?;
        until(&resumed.0, "resuming the conversation", |c| c.state.status == Status::Idle && c.state.harness_session.as_deref() == Some(own.as_str())).await?;
        {
            let core = resumed.0.core.lock();
            assert_eq!(core.state.model, model);
            assert_eq!(core.state.effort, effort);
            assert_eq!(core.events().filter(|e| matches!(&e.body, Body::Assistant { text, .. } if text.trim() == "KILN_DEVIN_SMOKE_OK")).count(), 1);
        }
        resumed.0.close();
        until(&resumed.0, "closing the resumed process", |c| c.state.status == Status::Exited).await?;
        Ok(())
    })
}

#[test]
#[ignore = "requires an installed, signed-in Devin CLI with mid-turn prompts"]
fn installed_devin_steers_running_and_queued_messages_into_one_turn() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let config = dir.path().join("config/devin");
        std::fs::create_dir_all(&config)?;
        std::fs::write(
            config.join("config.json"),
            serde_json::json!({
                "auto_update": false, "notify": "never",
                "read_config_from": {"agents_standard": false, "cursor": false, "windsurf": false, "claude": false, "copilot": false, "opencode": false, "zed": false}
            })
            .to_string(),
        )?;
        let spec = Spec {
            kind: "devin".into(),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            mode: Some("ask".into()),
            env: vec![("XDG_CONFIG_HOME".into(), dir.path().join("config").to_string_lossy().into_owned())],
            reach: Some("off".into()),
            ..Spec::default()
        };
        let running = open(spec, dir.path())?;
        let s = &running.0;
        until(s, "opening a session with native steering", |c| c.state.status == Status::Idle && c.meta.features.iter().any(|f| f == "steer") && c.meta.features.iter().any(|f| f == "queue_steer")).await?;
        send(s, "main", "This is a small live steering test. Write the numbers 1 through 80, each on its own line. Do not use tools or access files.");
        until(s, "streaming the original turn", |c| {
            c.state.delivery.get("main") == Some(&Delivery::Started) && c.events().any(|e| matches!(&e.body, Body::Assistant { streaming: true, .. } | Body::Thinking { streaming: true, .. }))
        })
        .await?;
        assert!(s.command(Command::Send {
            id: "steer".into(),
            text: "Steering update: stop listing numbers and reply KILN_NATIVE_STEERING_OK. Do not use tools or access files.".into(),
            mode: SendMode::Steer,
            attachments: Vec::new(),
            overrides: None,
            reply_to: None,
            task: None
        }));
        send(s, "queued-steer", "Another steering update for the same test: your final reply must end with KILN_NATIVE_STEERING_OK. Do not use tools or access files.");
        assert!(s.command(Command::QueueSteer { id: "queued-steer".into() }));
        until(s, "finishing the shared steered turn", |c| ["main", "steer", "queued-steer"].iter().all(|id| c.state.delivery.get(*id) == Some(&Delivery::Completed)) && c.state.status == Status::Idle).await?;
        {
            let core = s.core.lock();
            assert_eq!(core.state.usage.turns, 1);
            assert!(core.state.queue.is_empty());
            assert!(core.events().any(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text.trim().ends_with("KILN_NATIVE_STEERING_OK"))));
            for id in ["main", "steer", "queued-steer"] {
                assert_eq!(core.events().filter(|e| matches!(&e.body, Body::User { client_id: Some(client), .. } if client == id)).count(), 1);
            }
        }
        s.close();
        until(s, "closing the steered session", |c| c.state.status == Status::Exited).await?;
        Ok(())
    })
}

#[cfg(unix)]
#[test]
#[ignore = "requires an installed, signed-in Devin CLI"]
fn installed_devin_reloads_credentials_without_restarting_or_losing_queued_work() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        anyhow::ensure!(std::env::var_os("WINDSURF_API_KEY").is_none(), "unset WINDSURF_API_KEY for the isolated credential-refresh test");
        let dir = tempfile::tempdir()?;
        let data = dir.path().join("data");
        let config = dir.path().join("config");
        let source = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map_or_else(|| crate::os::home_dir().join(".local/share"), std::path::PathBuf::from).join("devin/credentials.toml");
        anyhow::ensure!(source.is_file(), "sign in to the installed Devin CLI first");
        std::fs::create_dir_all(data.join("devin"))?;
        std::fs::create_dir_all(config.join("devin"))?;
        std::fs::write(config.join("devin/config.json"), serde_json::json!({
            "auto_update": false, "notify": "never",
            "read_config_from": {"agents_standard": false, "cursor": false, "windsurf": false, "claude": false, "copilot": false, "opencode": false, "zed": false}
        }).to_string())?;
        let spec = Spec {
            kind: "devin".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), mode: Some("ask".into()),
            env: vec![
                ("XDG_DATA_HOME".into(), data.to_string_lossy().into_owned()),
                ("XDG_CONFIG_HOME".into(), config.to_string_lossy().into_owned()),
                ("XDG_CACHE_HOME".into(), dir.path().join("cache").to_string_lossy().into_owned()),
            ],
            reach: Some("off".into()), ..Spec::default()
        };
        let running = open(spec, dir.path())?;
        let s = &running.0;
        until(s, "opening without credentials", |c| c.state.status == Status::Idle && c.state.mode == "ask" && c.state.harness_session.is_some()).await?;
        assert!(s.core.lock().meta.models.is_empty());
        let pid = s.pid.lock().ok_or_else(|| anyhow::anyhow!("no Devin process"))?;

        std::os::unix::fs::symlink(source, data.join("devin/credentials.toml"))?;
        assert!(s.command(Command::Control { id: "account".into(), value: None }));
        send(s, "after-sign-in", "Reply with exactly KILN_AUTH_REFRESH_OK. Do not use tools or access files.");
        until(s, "refreshing credentials and delivering queued work", |c| c.state.status == Status::Idle && c.state.delivery.get("after-sign-in") == Some(&Delivery::Completed) && !c.meta.models.is_empty()).await?;
        assert_eq!(*s.pid.lock(), Some(pid));
        {
            let c = s.core.lock();
            assert_eq!(c.state.mode, "ask");
            assert!(c.events().any(|e| matches!(&e.body, Body::Assistant { text, streaming: false } if text.trim() == "KILN_AUTH_REFRESH_OK")));
            assert_eq!(c.events().filter(|e| matches!(&e.body, Body::User { client_id: Some(id), .. } if id == "after-sign-in")).count(), 1);
        }
        s.close();
        until(s, "closing after credential refresh", |c| c.state.status == Status::Exited).await?;
        Ok(())
    })
}
