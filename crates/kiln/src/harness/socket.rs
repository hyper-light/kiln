//! The local socket `kiln spawn` and `kiln agents` talk to from inside a kiln shell. Every kiln
//! terminal carries `KILN_HARNESS_SOCK` (this socket) and `KILN_TERM_SESSION` (its own id), so an
//! agent started from a shell runs in that shell's directory and environment and shows up under
//! it in the UI. One JSON request per connection, one JSON answer. The socket is the owner's alone
//! (mode 0600 in a 0700 directory).

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::host::Host;
use super::model::{Command, SendMode};
use super::session::Spec;

/// A request from the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Spawn {
        kind: String,
        #[serde(default)]
        count: Option<usize>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        shell: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        /// Run this executable instead of the harness's usual one.
        #[serde(default)]
        program: Option<String>,
    },
    List {
        #[serde(default)]
        shell: Option<String>,
    },
    /// Stop agents: one id, or every agent of `shell` (all of them without one).
    Stop {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        shell: Option<String>,
    },
    Interrupt {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        shell: Option<String>,
    },
    Send {
        id: String,
        text: String,
        #[serde(default)]
        mode: SendMode,
    },
}

/// Where this process's socket lives: the user's runtime directory (else a private one in /tmp),
/// named after the agent so a server and runners on one machine do not collide.
pub fn path(name: &str) -> PathBuf {
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(40).collect();
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .map(|d| d.join("kiln"))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("kiln-{}", crate::os::uid())));
    dir.join(format!("harness-{safe}.sock"))
}

/// Answer one request against `host`.
pub fn handle(host: &Host, req: Request) -> Value {
    let pick = |id: &Option<String>, shell: &Option<String>| -> Vec<String> {
        host.list()
            .into_iter()
            .filter(|i| id.as_ref().is_none_or(|x| &i.id == x || i.id.starts_with(x.as_str())))
            .filter(|i| id.is_some() || shell.is_none() || i.shell == *shell)
            .map(|i| i.id)
            .collect()
    };
    match req {
        Request::Spawn { kind, count, prompt, model, mode, name, shell, cwd, env, program } => {
            let count = count.unwrap_or(1).clamp(1, 16);
            let mut made = Vec::new();
            for i in 0..count {
                let spec = Spec {
                    kind: kind.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    mode: mode.clone(),
                    prompt: prompt.clone(),
                    name: match (&name, count) {
                        (Some(n), 1) => Some(n.clone()),
                        (Some(n), _) => Some(format!("{n} {}", i + 1)),
                        (None, 1) => None,
                        (None, _) => super::kinds::Kind::get(&kind).map(|k| format!("{} {}", k.name, i + 1)),
                    },
                    shell: shell.clone(),
                    env: env.clone(),
                    program: program.clone(),
                    ..Spec::default()
                };
                match host.open(None, spec) {
                    Ok(s) => {
                        let info = s.info();
                        made.push(json!({"id": info.id, "name": info.name, "kind": info.kind}));
                    }
                    Err(e) => return json!({"ok": false, "error": format!("{e:#}"), "sessions": made}),
                }
            }
            json!({"ok": true, "sessions": made})
        }
        Request::List { shell } => {
            let list: Vec<_> = host.list().into_iter().filter(|i| shell.is_none() || i.shell == shell).collect();
            json!({"ok": true, "sessions": list})
        }
        Request::Stop { id, shell } => {
            let ids = pick(&id, &shell);
            let n = ids.iter().filter(|i| host.close(i)).count();
            json!({"ok": true, "stopped": n})
        }
        Request::Interrupt { id, shell } => {
            let ids = pick(&id, &shell);
            let n = ids.iter().filter(|i| host.command(i, Command::Interrupt)).count();
            json!({"ok": true, "interrupted": n})
        }
        Request::Send { id, text, mode } => {
            let ids = pick(&Some(id), &None);
            let Some(target) = ids.first() else { return json!({"ok": false, "error": "no such agent"}) };
            let ok = host.command(target, Command::Send { id: crate::util::new_id(), text, mode });
            json!({"ok": ok})
        }
    }
}

/// Serve `host` on the socket at `path` until this process exits. Unix only.
#[cfg(unix)]
pub fn serve(host: &Arc<Host>, path: PathBuf) -> anyhow::Result<PathBuf> {
    use std::os::fd::{AsRawFd as _, OwnedFd};
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixListener;

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // A socket left by a previous process of ours (it cannot be in use: the name is ours).
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let host = Arc::clone(host);
    crate::rt::spawn(async move {
        let fd = listener.as_raw_fd();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let host = Arc::clone(&host);
                    crate::rt::spawn(async move {
                        if stream.set_nonblocking(true).is_err() {
                            return;
                        }
                        let fd = OwnedFd::from(stream);
                        let mut buf = Vec::new();
                        let mut chunk = vec![0u8; 16 * 1024];
                        // One line, at most 1 MiB (an environment can be large, not larger).
                        while !buf.contains(&b'\n') && buf.len() < (1 << 20) {
                            match crate::rt::timeout(std::time::Duration::from_secs(10), crate::os::proc::read_chunk(&fd, &mut chunk)).await {
                                Ok(Ok(Some(n))) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                                _ => break,
                            }
                        }
                        let answer = match serde_json::from_slice::<Request>(buf.split(|b| *b == b'\n').next().unwrap_or_default()) {
                            Ok(req) => handle(&host, req),
                            Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
                        };
                        let mut out = answer.to_string().into_bytes();
                        out.push(b'\n');
                        let _ = crate::os::proc::write_all(&fd, &out).await;
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if hyper_rt::readiness::readable(fd).await.is_err() {
                        crate::rt::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    tracing::warn!(error = %e, "agent socket accept failed");
                    crate::rt::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    });
    Ok(path)
}

/// No local socket on Windows yet: `kiln spawn` is not available there.
#[cfg(not(unix))]
pub fn serve(_host: &Arc<Host>, _path: PathBuf) -> anyhow::Result<PathBuf> {
    anyhow::bail!("kiln spawn needs Unix sockets")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_route() -> anyhow::Result<()> {
        let r: Request = serde_json::from_str(r#"{"op":"spawn","kind":"claude","count":2,"prompt":"hi","shell":"t1","env":[["A","b"]]}"#)?;
        assert!(matches!(&r, Request::Spawn { count: Some(2), shell: Some(s), .. } if s == "t1"));
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let host = Host::new(tx, None, Default::default());
            let dir = tempfile::tempdir()?;
            let missing = dir.path().join("no-claude").to_string_lossy().into_owned();
            // Two sessions in shell t1 (their program is missing: they exist and say so).
            let spawned = handle(&host, Request::Spawn {
                kind: "claude".into(),
                count: Some(2),
                prompt: None,
                model: None,
                mode: None,
                name: None,
                shell: Some("t1".into()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                env: vec![],
                program: Some(missing),
            });
            assert_eq!(spawned.get("ok"), Some(&json!(true)));
            assert_eq!(spawned.get("sessions").and_then(Value::as_array).map(Vec::len), Some(2));
            let listed = handle(&host, Request::List { shell: Some("t1".into()) });
            assert_eq!(listed.get("sessions").and_then(Value::as_array).map(Vec::len), Some(2));
            assert_eq!(handle(&host, Request::List { shell: Some("other".into()) }).get("sessions").and_then(Value::as_array).map(Vec::len), Some(0));
            let stopped = handle(&host, Request::Stop { id: None, shell: Some("t1".into()) });
            assert_eq!(stopped.get("stopped"), Some(&json!(2)));
            assert!(host.list().is_empty());
            Ok(())
        })
    }
}
