//! A terminal beside an agent, in its folder on its machine: the machine's shell, for what the
//! agent can't or won't run, or the harness's own terminal interface, for what only that interface
//! does (Claude Code's /permissions editor, /config, /doctor ...). Each is an ordinary kiln
//! terminal, kept off the Terminal page and found again by the agent it belongs to. When the
//! harness's own interface closes, the agent starts again (once it is idle) to load what changed;
//! its conversation continues.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::kinds::Kind;
use super::model::Command;
use crate::hub::Hub;
use crate::termsess::{Info, Input};

/// Marks a terminal beside an agent, in its command line: `kiln-agent:<agent>:<what>[:<command>]`.
pub const MARK: &str = "kiln-agent:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    /// The machine's shell, in the agent's folder.
    Shell,
    /// The harness's own terminal interface, in the agent's folder.
    Screen,
}

impl What {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Screen => "screen",
        }
    }
}

/// What to open.
#[derive(Debug, Deserialize)]
pub struct Open {
    pub what: What,
    /// The screen: the slash command to start on ("/permissions"). The shell: a command line to
    /// run in it (typed, as if by you).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// A terminal beside an agent.
#[derive(Debug, Clone, Serialize)]
pub struct Beside {
    pub terminal: Info,
    pub what: What,
    /// The screen's slash command, when it was opened on one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// That command is for you to type there (the harness can't start on one).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub to_type: bool,
}

/// The terminals beside `agent`, live ones first, newest first.
pub fn list(hub: &Hub, agent: &str) -> Vec<Beside> {
    let starts_on = hub.harness.list().into_iter().find(|i| i.id == agent).and_then(|i| Kind::get(&i.kind)).is_some_and(|k| k.screen_takes_command);
    let mut out: Vec<Beside> = hub.terminals.list().into_iter().filter_map(|t| beside(t, agent, starts_on)).collect();
    out.sort_by_key(|b| (b.terminal.exited, std::cmp::Reverse(b.terminal.created_at)));
    out
}

fn beside(t: Info, agent: &str, starts_on: bool) -> Option<Beside> {
    let mark = t.command.as_deref()?.rsplit_once(MARK)?.1.to_owned();
    let mut parts = mark.splitn(3, ':');
    if parts.next()? != agent {
        return None;
    }
    let what = match parts.next()? {
        "shell" => What::Shell,
        "screen" => What::Screen,
        _ => return None,
    };
    let command = parts.next().map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned);
    let to_type = command.is_some() && !starts_on;
    Some(Beside { terminal: t, what, command, to_type })
}

/// Opens a terminal beside `agent` (or, for its shell, returns the one already open, running
/// `command` in it).
pub async fn open(hub: &Arc<Hub>, agent: &str, o: Open) -> anyhow::Result<Beside> {
    let info = hub.harness.list().into_iter().find(|i| i.id == agent).ok_or_else(|| anyhow::anyhow!("no such agent"))?;
    let kind = Kind::get(&info.kind).ok_or_else(|| anyhow::anyhow!("unknown harness {}", info.kind))?;
    let command = o.command.as_deref().map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned);
    if o.what == What::Shell
        && let Some(b) = list(hub, agent).into_iter().find(|b| b.what == What::Shell && !b.terminal.exited)
    {
        if let (Some(c), Some(s)) = (&command, hub.terminals.get(&b.terminal.id)) {
            s.send(Input::Data(format!("{c}\r").into_bytes()));
        }
        return Ok(b);
    }
    let machine = info.target.strip_prefix("agent:").map(str::to_owned);
    let windows = match &machine {
        None => cfg!(windows),
        Some(m) => {
            let agents = hub.agents.lock();
            let a = agents.get(m).filter(|a| a.connected).ok_or_else(|| anyhow::anyhow!("{m} is not connected"))?;
            if !a.terminal {
                anyhow::bail!("{m} doesn't open terminals (its runner was started without them)");
            }
            a.os == "windows"
        }
    };
    if hub.terminals.live() >= hub.cfg.terminal.max_sessions {
        anyhow::bail!("too many terminals are open on this server");
    }
    // The screen starts on its command when the harness takes one at start; else it's typed there.
    let screen_command = command.clone().filter(|_| o.what == What::Screen);
    let start = screen_command.clone().filter(|_| kind.screen_takes_command);
    let mark = format!("{MARK}{agent}:{}{}", o.what.as_str(), screen_command.as_deref().map(|c| format!(":{c}")).unwrap_or_default());
    let program: Vec<&str> = match o.what {
        What::Shell => Vec::new(),
        What::Screen => kind.screen.to_vec(),
    };
    let line = command_line(windows, &info.cwd, &program, start.as_deref(), &mark);
    let (cols, rows) = (o.cols.unwrap_or(120).clamp(20, 400), o.rows.unwrap_or(32).clamp(6, 200));
    let session = match &machine {
        None => crate::termsess::open_local(hub, Some(line), cols, rows)?,
        Some(m) => crate::termsess::open_agent(hub, m.clone(), Some(line), cols, rows),
    };
    let what = match o.what {
        What::Shell => "a shell".to_owned(),
        What::Screen => format!("{}'s own screen{}", kind.name, command.as_deref().map(|c| format!(" ({c})")).unwrap_or_default()),
    };
    hub.store.audit("admin".into(), "agent.terminal".into(), format!("{what} beside {} ({agent}) on {}", info.name, info.target)).await;
    match o.what {
        What::Shell => {
            if let Some(c) = command.clone() {
                // Typed once the shell has drawn its prompt (or after a moment), so it reads as typed.
                let s = Arc::clone(&session);
                crate::rt::spawn(async move {
                    for _ in 0..60 {
                        if !s.is_empty() {
                            break;
                        }
                        crate::rt::sleep(Duration::from_millis(50)).await;
                    }
                    crate::rt::sleep(Duration::from_millis(150)).await;
                    s.send(Input::Data(format!("{c}\r").into_bytes()));
                });
            }
        }
        // When it ends, `watch` starts the agent again to load what changed.
        What::Screen => {}
    }
    let to_type = screen_command.is_some() && !kind.screen_takes_command;
    Ok(Beside { terminal: session.info(), what: o.what, command: screen_command, to_type })
}

/// Watches every screen beside an agent (opened here, or taken back from its machine after a
/// restart): when one ends, its agent starts again, once idle, to load what was changed there.
pub async fn watch(hub: Arc<Hub>) -> anyhow::Result<()> {
    // Screens seen running: terminal id -> (agent, machine). Ended ones stay listed for a while
    // (see `crate::termsess`): each is acted on once, even one that came and went between looks.
    let mut running: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut done: HashSet<String> = HashSet::new();
    loop {
        if !hub.shutdown.sleep(Duration::from_secs(2)).await {
            return Ok(());
        }
        let now: Vec<(Info, String)> = hub.terminals.list().into_iter().filter_map(|t| screen_of(&t).map(|agent| (t, agent))).collect();
        let mut ended: Vec<String> = Vec::new();
        for (t, agent) in &now {
            if !t.exited {
                running.entry(t.id.clone()).or_insert_with(|| (agent.clone(), t.target.strip_prefix("agent:").map(str::to_owned)));
            } else if done.insert(t.id.clone()) {
                running.remove(&t.id);
                ended.push(agent.clone());
            }
        }
        // Running ones gone from the list (swept) ended too; unless their machine is away: then they
        // run on there, and are taken back when it returns.
        let gone: Vec<String> = running.keys().filter(|id| !now.iter().any(|(t, _)| &t.id == *id)).cloned().collect();
        for id in gone {
            let away = running.get(&id).and_then(|(_, m)| m.as_ref()).is_some_and(|m| !hub.agents.lock().get(m).is_some_and(|a| a.connected));
            if !away && let Some((agent, _)) = running.remove(&id) {
                ended.push(agent);
            }
        }
        done.retain(|id| now.iter().any(|(t, _)| &t.id == id));
        for agent in ended {
            let harness = hub.harness.list().into_iter().find(|i| i.id == agent).and_then(|i| Kind::get(&i.kind)).map_or("the harness", |k| k.name);
            let value = json!({"what": format!("What you changed in {harness}'s own screen")});
            let _ = hub.harness.command(&hub, &agent, Command::Control { id: "reload_config".into(), value: Some(value) }).await;
        }
    }
}

/// The agent a terminal is the harness's own screen beside, if it is one.
fn screen_of(t: &Info) -> Option<String> {
    let mark = t.command.as_deref()?.rsplit_once(MARK)?.1;
    let mut parts = mark.splitn(3, ':');
    let agent = parts.next()?;
    (parts.next()? == "screen").then(|| agent.to_owned())
}

/// The terminal's command line: into the folder, then the shell (or the program, started on
/// `start`), with the mark after it as a comment.
fn command_line(windows: bool, cwd: &str, program: &[&str], start: Option<&str>, mark: &str) -> String {
    if windows {
        let q = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let cd = if cwd.is_empty() { String::new() } else { format!("Set-Location -LiteralPath {}; ", q(cwd)) };
        let run = if program.is_empty() {
            // Another of the same PowerShell, which stays (this one ends with its command).
            "& (Get-Process -Id $PID).Path -NoLogo".to_owned()
        } else {
            let mut words: Vec<String> = program.iter().map(|w| q(w)).collect();
            words.extend(start.map(q));
            format!("& {}", words.join(" "))
        };
        return format!("{cd}{run} # {mark}");
    }
    let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    if program.is_empty() {
        let cd = if cwd.is_empty() { String::new() } else { format!("cd -- {} 2>/dev/null; ", q(cwd)) };
        return format!("{cd}exec \"$SHELL\" -l # {mark}");
    }
    let cd = if cwd.is_empty() { String::new() } else { format!("cd -- {} && ", q(cwd)) };
    let mut words: Vec<String> = program.iter().map(|w| q(w)).collect();
    words.extend(start.map(q));
    format!("{cd}exec {} # {mark}", words.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_go_into_the_folder_and_carry_the_mark() {
        assert_eq!(command_line(false, "/home/a/it's", &[], None, "kiln-agent:x:shell"), r#"cd -- '/home/a/it'\''s' 2>/dev/null; exec "$SHELL" -l # kiln-agent:x:shell"#);
        assert_eq!(command_line(false, "/w", &["claude"], Some("/permissions"), "m"), "cd -- '/w' && exec 'claude' '/permissions' # m");
        assert_eq!(command_line(false, "", &["goose", "session"], None, "m"), "exec 'goose' 'session' # m");
        assert_eq!(command_line(true, r"C:\it's", &["codex"], None, "m"), r"Set-Location -LiteralPath 'C:\it''s'; & 'codex' # m");
        assert_eq!(command_line(true, r"C:\w", &[], None, "m"), r"Set-Location -LiteralPath 'C:\w'; & (Get-Process -Id $PID).Path -NoLogo # m");
    }

    #[test]
    fn a_terminal_is_found_by_its_mark() {
        let t = |command: &str| Info {
            id: "t".into(),
            target: "local".into(),
            command: Some(command.into()),
            title: String::new(),
            created_at: 0,
            attached: 0,
            cols: 80,
            rows: 24,
            idle_seconds: 0,
            exited: false,
            exit_code: None,
            error: None,
        };
        let b = beside(t("cd -- '/w' && exec 'codex' # kiln-agent:a1:screen:/experimental"), "a1", false);
        assert_eq!(b.as_ref().map(|b| (b.what, b.command.as_deref(), b.to_type)), Some((What::Screen, Some("/experimental"), true)));
        let b = beside(t("cd -- '/w' && exec 'claude' '/doctor' # kiln-agent:a1:screen:/doctor"), "a1", true);
        assert_eq!(b.as_ref().map(|b| (b.command.as_deref(), b.to_type)), Some((Some("/doctor"), false)));
        assert_eq!(beside(t("exec \"$SHELL\" -l # kiln-agent:a1:shell"), "a1", true).map(|b| b.what), Some(What::Shell));
        assert!(beside(t("exec \"$SHELL\" -l # kiln-agent:a1:shell"), "a2", true).is_none());
        assert!(beside(t("htop"), "a1", true).is_none());
        // What the watcher reloads after: screens only.
        assert_eq!(screen_of(&t("exec 'claude' '/doctor' # kiln-agent:a1:screen:/doctor")).as_deref(), Some("a1"));
        assert_eq!(screen_of(&t("exec \"$SHELL\" -l # kiln-agent:a1:shell")), None);
    }
}
