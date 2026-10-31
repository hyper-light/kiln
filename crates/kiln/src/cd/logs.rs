//! The logs of what kiln's runners run for an app (docs/ci.md W26-R26): a Compose project's
//! containers and a host's service, read on each of their hosts by the runner there. The command
//! `kiln_cd::providers::logs` gives runs in a terminal of kiln's own on that runner (never listed
//! as anyone's, [`MARK`]), and its lines come in the same API and stream as a pod's: each line names
//! its host where a pod's names its pod, so the one log viewer reads them all.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{FutureExt as _, StreamExt as _};
use http::StatusCode;
use kiln_cd::obj::ResKey;
use kiln_cd::providers::logs::Ask;
use serde_json::{Value, json};

use super::Cd;
use super::ops::Fail;
use crate::http::ws;
use crate::hub::Hub;
use crate::termsess::Frame;
use crate::wire::ui::{CdLogDown, CdLogLine};

/// What a terminal kiln opens to read a log carries, so no one's list of terminals shows it.
pub const MARK: &str = "# kiln: a deployment's log";

/// How long reading a log (not following it) may take on one host.
const WITHIN: Duration = Duration::from_secs(20);
/// The most hosts one log is read from.
const MAX_HOSTS: usize = 8;

/// Whether `k` is an object a runner's job made, its logs read on its hosts.
pub fn on_runners(k: &ResKey) -> bool {
    matches!(k.group.as_ref(), kiln_cd::providers::compose::GROUP | kiln_cd::providers::host::GROUP)
}

/// What a log is asked for, from the API's input (`tail_lines`, `since_seconds`, `previous`, and a
/// Compose project's one service as its `container`).
pub fn ask_of(v: &Value, follow: bool) -> Ask {
    let num = |k: &str| v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
    Ask {
        tail: num("tail_lines").and_then(|n| u32::try_from(n).ok()),
        since_seconds: num("since_seconds"),
        follow,
        previous: v.get("previous").is_some_and(|p| p.as_bool() == Some(true) || p.as_str() == Some("true")),
        service: v.get("container").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned),
    }
}

/// The hosts app `app`'s object `k` runs on, as kiln last made it there.
pub(super) fn hosts_of(cd: &Cd, app: &str, k: &ResKey) -> Vec<String> {
    let mine = |o: &Value| {
        kiln_cd::obj::name(o) == k.name.as_ref() && kiln_cd::obj::kind(o) == k.kind.as_ref() && kiln_cd::obj::api_group(o.get("apiVersion").and_then(Value::as_str).unwrap_or_default()) == k.group.as_ref() && super::targets::app_of(o).as_deref() == Some(app)
    };
    cd.store
        .read(|d| d.applied.values().flat_map(|m| m.values()).find(|a| mine(&a.object)).and_then(|a| a.made.get("hosts").cloned()))
        .and_then(|h| serde_json::from_value(h).ok())
        .unwrap_or_default()
}

/// The command for `k`'s log on `host`, by the host's OS as its runner said.
fn command_on(hub: &Hub, host: &str, k: &ResKey, ask: &Ask) -> Result<String, String> {
    let os = hub.agents.lock().get(host).map(|a| a.os.clone()).ok_or_else(|| format!("{host} isn't one of kiln's runners now"))?;
    let os = if os == "macos" { "darwin".to_owned() } else { os };
    kiln_cd::providers::logs::command(k.group.as_ref(), k.kind.as_ref(), k.name.as_ref(), &os, ask).map(|c| format!("{c}{MARK}\n"))
}

/// What a terminal's bytes say as text: no colours, no carriage returns.
fn plain(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // A control sequence: up to its final byte.
                Some('[') => while chars.next().is_some_and(|x| !('@'..='~').contains(&x)) {},
                // An operating system command (a title): up to BEL, or ESC \.
                Some(']') => {
                    while let Some(x) = chars.next() {
                        if x == '\u{7}' || (x == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// One line of a runner's log as a pod's line is read: a Compose line (`web-1  | <time> <text>`) has
/// its time after its container's name, which stays at the start of its text.
fn runner_line(host: &str, raw: &str) -> CdLogLine {
    if let Some((who, rest)) = raw.split_once(" | ") {
        let l = super::live::line(host, rest);
        if !l.ts.is_empty() {
            return CdLogLine { pod: l.pod, ts: l.ts, text: format!("{} | {}", who.trim_end(), l.text) };
        }
    }
    super::live::line(host, raw)
}

/// `k`'s log on its hosts, read now: each line with its host, and why a host's couldn't be read.
pub(super) async fn read(hub: &Arc<Hub>, cd: &Cd, app: &str, k: &ResKey, ask: &Ask) -> Result<Value, Fail> {
    let hosts = hosts_of(cd, app, k);
    if hosts.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("kiln hasn't made {} on any runner yet", k.name)));
    }
    let mut lines: Vec<CdLogLine> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for host in hosts.iter().take(MAX_HOSTS) {
        let command = match command_on(hub, host, k, ask) {
            Ok(c) => c,
            Err(e) => {
                errors.push(json!({"pod": host, "message": e}));
                continue;
            }
        };
        let session = crate::termsess::open_agent(hub, host.clone(), Some(command), 400, 50);
        let mut att = session.attach();
        let mut out: Vec<u8> = Vec::new();
        let mut ended = att.ended.clone();
        let deadline = Instant::now() + WITHIN;
        while ended.is_none() {
            match crate::rt::timeout(deadline.saturating_duration_since(Instant::now()), att.live.recv()).await {
                Ok(Ok(Frame::Data(b))) => out.extend_from_slice(&b),
                Ok(Ok(Frame::Exit { code, error })) => ended = Some((code, error)),
                Ok(Ok(Frame::Size { .. })) => {}
                Ok(Err(_)) => ended = Some((None, Some("its terminal went".into()))),
                Err(_) => ended = Some((None, Some(format!("no end in {} s", WITHIN.as_secs())))),
            }
        }
        session.close();
        hub.terminals.remove(&session.id);
        let text = plain(&out);
        let got: Vec<CdLogLine> = text.lines().filter(|l| !l.trim().is_empty()).map(|l| runner_line(host, l)).collect();
        match ended {
            Some((Some(0), _)) | None => lines.extend(got),
            Some((code, error)) => {
                let why = error.or_else(|| got.last().map(|l| l.text.clone())).unwrap_or_else(|| format!("exited {}", code.map_or("without a code".to_owned(), |c| c.to_string())));
                errors.push(json!({"pod": host, "message": why}));
            }
        }
    }
    // Merged by time where the lines say it, each host's in order.
    lines.sort_by(|a, b| a.ts.cmp(&b.ts));
    Ok(json!({"pods": hosts, "more_pods": hosts.len().saturating_sub(MAX_HOSTS), "container": ask.service, "containers": Vec::<String>::new(), "lines": lines, "errors": errors}))
}

/// `k`'s log on its hosts followed on `socket`, as a pod's is ([`super::live::follow`]): lines as
/// they come, each with its host, until the page goes or every host's ends.
pub(super) async fn follow(hub: Arc<Hub>, cd: Arc<Cd>, app: String, k: ResKey, ask: Ask, sock: crate::wire::ui::Socket, socket: ws::WebSocket) -> anyhow::Result<()> {
    let (mut tx, mut rx) = socket.split();
    let hosts = hosts_of(&cd, &app, &k);
    if hosts.is_empty() {
        tx.send(ws::text(CdLogDown::Error { pod: String::new(), message: format!("kiln hasn't made {} on any runner yet", k.name) }.text())).await?;
        tx.close(None).await.ok();
        return Ok(());
    }
    let mut sessions = Vec::new();
    let mut streams = Vec::new();
    for host in hosts.iter().take(MAX_HOSTS) {
        match command_on(&hub, host, &k, &ask) {
            Ok(command) => {
                let session = crate::termsess::open_agent(&hub, host.clone(), Some(command), 400, 50);
                let att = session.attach();
                let h = host.clone();
                streams.push(att.live.map(move |f| (h.clone(), f)).boxed());
                sessions.push(session);
            }
            Err(e) => tx.send(ws::text(CdLogDown::Error { pod: host.clone(), message: e }.text())).await?,
        }
    }
    let mut open = streams.len();
    let mut all = futures::stream::select_all(streams);
    let mut partial: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let result = async {
        while open > 0 {
            futures::select_biased! {
                msg = rx.next().fuse() => {
                    if hub.unread_only(&sock, &mut tx, crate::wire::ui::came(msg), |r| CdLogDown::Refused(r).text()).await? {
                        return Ok(());
                    }
                }
                ev = all.next().fuse() => match ev {
                    Some((host, Frame::Data(b))) => {
                        let buf = partial.entry(host.clone()).or_default();
                        buf.push_str(&plain(&b));
                        let Some(end) = buf.rfind('\n') else { continue };
                        let done: String = buf.drain(..=end).collect();
                        let lines: Vec<CdLogLine> = done.lines().filter(|l| !l.trim().is_empty()).map(|l| runner_line(&host, l)).collect();
                        if !lines.is_empty() {
                            tx.send(ws::text(CdLogDown::Lines { lines }.text())).await?;
                        }
                    }
                    Some((host, Frame::Exit { code, error })) => {
                        open -= 1;
                        if code != Some(0) {
                            let message = error.unwrap_or_else(|| format!("its log ended ({})", code.map_or("no code".to_owned(), |c| format!("exit {c}"))));
                            tx.send(ws::text(CdLogDown::Error { pod: host, message }.text())).await?;
                        }
                    }
                    Some((_, Frame::Size { .. })) => {}
                    None => break,
                },
            }
        }
        tx.send(ws::text(CdLogDown::End.text())).await?;
        anyhow::Ok(())
    }
    .await;
    for s in &sessions {
        s.close();
        hub.terminals.remove(&s.id);
    }
    tx.close(None).await.ok();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sh -c command` here, as a runner's terminal runs it: what it printed, and how it ended.
    #[cfg(unix)]
    fn sh(command: &str) -> anyhow::Result<(String, bool)> {
        let out = std::process::Command::new("sh").args(["-c", command]).output()?;
        Ok((String::from_utf8_lossy(&out.stdout).into_owned(), out.status.success()))
    }

    /// W26-R26, on this machine's Docker with a Compose project, as a host's runner reads it: the
    /// last lines in the order they came, a service's own, the log followed as lines come, and a
    /// stopped container's log read as its previous one.
    #[cfg(unix)]
    #[test]
    fn w26_r26_a_compose_projects_log_reads_follows_and_reads_a_stopped_containers() -> anyhow::Result<()> {
        if !sh("docker version --format '{{.Server.Version}}'").is_ok_and(|(v, ok)| ok && !v.trim().is_empty()) {
            eprintln!("no Docker here: skipped");
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let project = format!("kiln-w26r26-{}", std::process::id());
        // No network of its own (Docker's address pools can be full): only its log matters.
        std::fs::write(
            dir.path().join("compose.yaml"),
            "services:\n  web:\n    image: alpine:latest\n    network_mode: none\n    command: [\"sh\", \"-c\", \"i=0; while :; do i=$$((i+1)); echo line-$$i; sleep 0.2; done\"]\n",
        )?;
        let file = dir.path().join("compose.yaml");
        let up = format!("docker compose -p {project} -f {} up -d 2>&1", file.display());
        let (said, ok) = sh(&up)?;
        anyhow::ensure!(ok, "compose up: {said}");
        let checked = (|| -> anyhow::Result<()> {
            std::thread::sleep(Duration::from_millis(1500));
            let read = |ask: &Ask| kiln_cd::providers::logs::command("compose.kiln.dev", "Project", &project, "linux", ask).map_err(anyhow::Error::msg);
            // The last lines, in order, each with its time.
            let (text, ok) = sh(&read(&Ask { tail: Some(5), service: Some("web".into()), ..Ask::default() })?)?;
            anyhow::ensure!(ok, "logs: {text}");
            let lines: Vec<CdLogLine> = plain(text.as_bytes()).lines().filter(|l| !l.trim().is_empty()).map(|l| runner_line("host-1", l)).collect();
            let n: Vec<u32> = lines.iter().filter_map(|l| l.text.rsplit("line-").next().and_then(|x| x.trim().parse().ok())).collect();
            anyhow::ensure!(n.len() == 5 && n.windows(2).all(|w| w.get(1) > w.first()), "the last five, in order: {lines:?}");
            anyhow::ensure!(lines.iter().all(|l| !l.ts.is_empty()), "each with its time: {lines:?}");
            // Followed: lines past those keep coming.
            let follow = read(&Ask { tail: Some(1), follow: true, ..Ask::default() })?;
            let mut child = std::process::Command::new("sh").args(["-c", &follow]).stdout(std::process::Stdio::piped()).spawn()?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                use std::io::BufRead as _;
                for l in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
            });
            let mut seen = Vec::new();
            let until = Instant::now() + Duration::from_secs(10);
            while seen.len() < 4 && Instant::now() < until {
                if let Ok(l) = rx.recv_timeout(Duration::from_millis(500)) {
                    seen.push(l);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::ensure!(seen.len() >= 4, "followed, lines kept coming: {seen:?}");
            // Stopped: its container's log is read as the previous one's.
            let (said, ok) = sh(&format!("docker compose -p {project} -f {} stop -t 1 2>&1", file.display()))?;
            anyhow::ensure!(ok, "compose stop: {said}");
            let (text, ok) = sh(&read(&Ask { tail: Some(3), previous: true, ..Ask::default() })?)?;
            anyhow::ensure!(ok && text.contains("line-"), "the stopped container's log: {text}");
            Ok(())
        })();
        let _ = sh(&format!("docker compose -p {project} -f {} down -t 1 2>&1", file.display()));
        checked
    }

    /// W26-R26: what a runner's terminal sends reads as plain lines: its colours and carriage
    /// returns gone.
    #[test]
    fn w26_r26_a_terminals_bytes_read_as_plain_lines() {
        assert_eq!(plain(b"\x1b[32m2026-10-30T15:00:00.1Z web | up\x1b[0m\r\nnext\r\n"), "2026-10-30T15:00:00.1Z web | up\nnext\n");
        assert_eq!(plain(b"\x1b]0;title\x07ok \x1b]0;t\x1b\\done"), "ok done");
        // Docker's: its time first; Compose's: its container's name, then its time.
        let l = runner_line("host-1", "2026-10-30T15:00:00.100Z listening on :8080");
        assert_eq!((l.pod.as_str(), l.ts.as_str(), l.text.as_str()), ("host-1", "2026-10-30T15:00:00.100Z", "listening on :8080"));
        let l = runner_line("host-1", "web-1  | 2026-10-30T15:00:00.100Z listening on :8080");
        assert_eq!((l.ts.as_str(), l.text.as_str()), ("2026-10-30T15:00:00.100Z", "web-1 | listening on :8080"));
        let l = runner_line("host-1", "Oct 30 15:00:00 host web[12]: up");
        assert_eq!((l.ts.as_str(), l.text.as_str()), ("", "Oct 30 15:00:00 host web[12]: up"));
        let ask = ask_of(&json!({"tail_lines": "20", "since_seconds": 60, "previous": true, "container": "web"}), false);
        assert_eq!(ask, Ask { tail: Some(20), since_seconds: Some(60), follow: false, previous: true, service: Some("web".into()) });
        assert!(on_runners(&ResKey::new("compose.kiln.dev", "Project", "", "shop")) && !on_runners(&ResKey::new("apps", "Deployment", "web", "web")));
    }
}
