//! What an agent's harness started, kept track of across restarts of what holds it (the runner
//! upgrading, crashing, stopped by the system or joined again, or the harness itself starting
//! again). Every few seconds the machine's harness processes and everything under them (their
//! tools' commands and what those left running: a dev server, a test run, a benchmark queue) are
//! noted in the session and kept in the machine's record of its sessions. What still runs once the
//! process that started it is gone is taken back over: shown with the agent's background tasks
//! ("left running"), stoppable from kiln, and named to the agent when kiln tells it to pick up where
//! it left off, so it uses them rather than starting them again. A harness that outlived its
//! runner is stopped before another resumes its conversation (what it started is kept), so two
//! never work one conversation.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A process, as last seen.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ran {
    pub pid: u32,
    /// Its parent when seen (a harness, or another of these).
    #[serde(default)]
    pub ppid: u32,
    /// When it started (seconds since the epoch, as the system says): a pid given out again is
    /// another process.
    pub started: u64,
    pub command: String,
}

/// Most processes noted per session.
const MAX_RAN: usize = 64;
/// Longest command kept.
const COMMAND_MAX: usize = 400;
/// How long a stopped process gets to end before it's killed.
const GRACE: Duration = Duration::from_secs(3);
/// The longest pause between looks at whether stopped processes have ended (the first is 1 ms,
/// each twice the last: one that ends at once is seen ending at once).
const LOOK_MAX: Duration = Duration::from_millis(100);

/// The pause after `pause`, between looks at whether processes have ended.
pub(crate) fn next_look(pause: Duration) -> Duration {
    pause.saturating_mul(2).min(LOOK_MAX)
}
/// A survivor's task id: `left-<pid>`.
pub const LEFT: &str = "left-";

fn command_of(cmd: &[std::ffi::OsString]) -> String {
    let joined = cmd.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" ");
    super::model::clip_middle(joined.trim(), COMMAND_MAX).0
}

/// kiln's own helpers and the harness's MCP servers: they belong to the harness, end with it, and
/// are started again with the next one.
fn helper(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let program = words.next().map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p)).unwrap_or("");
    let lower = command.to_lowercase();
    program == "kiln" || program == "kiln.exe" || lower.contains("mcp") || lower.contains("modelcontextprotocol")
}

/// An `ssh-agent` as a shell's startup file starts it (`eval "$(ssh-agent -s)"`, fish's
/// `eval (ssh-agent -c)`): one more for every shell that reads the file (a harness's shell
/// snapshots, its tools' login shells), each running on after its shell, so a session left dozens.
/// No work of the agent's: never left running, and stopped once [`AGENT_KEPT`] old.
pub fn login_agent(command: &str) -> bool {
    let mut words = command.split_whitespace();
    let program = words.next().map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p)).unwrap_or("");
    program == "ssh-agent" && words.all(|w| matches!(w, "-s" | "-c"))
}

/// How long a shell's `ssh-agent` in a session is left alone: a command that started one on purpose
/// (`eval "$(ssh-agent -s)" && ssh-add … && git push`) has had its use of it by then.
pub const AGENT_KEPT: Duration = Duration::from_secs(30 * 60);

/// Of the shells' ssh-agents a look found, the ones to stop at `now_s` (seconds since the epoch):
/// those [`AGENT_KEPT`] old.
pub fn agents_to_stop(seen: &HashMap<String, Seen>, now_s: u64) -> Vec<u32> {
    seen.values().flat_map(|s| &s.agents).filter(|r| now_s.saturating_sub(r.started) >= AGENT_KEPT.as_secs()).map(|r| r.pid).collect()
}

fn system(pids: Option<&[u32]>) -> sysinfo::System {
    refreshed(pids, false)
}

fn refreshed(pids: Option<&[u32]>, environ: bool) -> sysinfo::System {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let mut kind = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).without_tasks();
    if environ {
        kind = kind.with_environ(UpdateKind::Always);
    }
    match pids {
        Some(p) => {
            let p: Vec<Pid> = p.iter().map(|x| Pid::from_u32(*x)).collect();
            sys.refresh_processes_specifics(ProcessesToUpdate::Some(&p), true, kind);
        }
        None => {
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        }
    }
    sys
}

/// Not a zombie waiting for its parent to read how it ended.
fn running(p: &sysinfo::Process) -> bool {
    p.status() != sysinfo::ProcessStatus::Zombie
}

fn ran_of(sys: &sysinfo::System, pid: sysinfo::Pid) -> Option<Ran> {
    sys.process(pid).map(|p| Ran { pid: pid.as_u32(), ppid: p.parent().map_or(0, sysinfo::Pid::as_u32), started: p.start_time(), command: command_of(p.cmd()) })
}

/// The environment variable every harness kiln starts carries (its session's id), and so does
/// everything it starts, however that's parented later (a command that daemonized, a shell that
/// exited under what it left running).
pub const MARK: &str = "KILN_AGENT";

/// The agent session process `pid` belongs to, of `sessions` (each with its harness's process):
/// the one whose harness is that process or one of its ancestors, else the one its environment
/// marks it as ([`MARK`]). `None`: none of them (a person's shell). Blocking.
pub fn session_of(pid: u32, sessions: &[(String, Option<u32>)]) -> Option<String> {
    use sysinfo::Pid;
    if sessions.is_empty() {
        return None;
    }
    // In a session's group of processes: one look says whose it is.
    if let Some(id) = crate::os::contain::session_of(pid)
        && sessions.iter().any(|(s, _)| *s == id)
    {
        return Some(id);
    }
    let sys = system(None);
    let mut at = Pid::from_u32(pid);
    // Up the tree: a tool's command runs under its harness.
    for _ in 0..64 {
        if let Some((id, _)) = sessions.iter().find(|(_, h)| *h == Some(at.as_u32())) {
            return Some(id.clone());
        }
        match sys.process(at).and_then(sysinfo::Process::parent) {
            Some(p) if p != at => at = p,
            _ => break,
        }
    }
    // Started by one and parented elsewhere since (it daemonized): its mark says whose it is.
    let env = refreshed(Some(&[pid]), true);
    let prefix = format!("{MARK}=");
    let marked = env.process(Pid::from_u32(pid))?.environ().iter().find_map(|kv| kv.to_str().and_then(|s| s.strip_prefix(prefix.as_str())).map(str::to_owned))?;
    sessions.iter().any(|(id, _)| *id == marked).then_some(marked)
}

/// What one look at a session's processes found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// Its harness, when it runs.
    pub harness: Option<Ran>,
    /// What runs under the harness now.
    pub under: Vec<Ran>,
    /// What it started that runs apart from it (marked as its, not under it): left running.
    pub apart: Vec<Ran>,
    /// The `ssh-agent`s shells' startup files started for it, apart from it ([`login_agent`]).
    pub agents: Vec<Ran>,
}

/// One look at the sessions' processes (blocking): for each session (its id, and its harness's
/// process when it runs), what runs under its harness and what it started that runs apart from
/// it, its helpers (and what runs under them) left out. A session with a group of processes
/// (`crate::os::contain`) is what's in its group, and only those processes are looked at; one
/// without is found by its mark, which takes looking at every process on the machine.
pub fn scan(sessions: &[(String, Option<u32>)]) -> HashMap<String, Seen> {
    use sysinfo::Pid;
    if sessions.is_empty() {
        return HashMap::new();
    }
    let groups: HashMap<&str, Vec<Pid>> =
        sessions.iter().filter_map(|(id, _)| crate::os::contain::Group::existing(id).map(|g| (id.as_str(), g.pids().into_iter().map(Pid::from_u32).collect()))).collect();
    let marks = sessions.iter().any(|(id, _)| !groups.contains_key(id.as_str()));
    let sys = if marks {
        refreshed(None, true)
    } else {
        let pids: Vec<u32> = groups.values().flatten().map(|p| p.as_u32()).chain(sessions.iter().filter_map(|(_, h)| *h)).collect();
        refreshed(Some(&pids), false)
    };
    let mut kids: HashMap<Pid, Vec<Pid>> = HashMap::new();
    let mut marked: HashMap<String, Vec<Pid>> = HashMap::new();
    let wanted: HashSet<&str> = sessions.iter().map(|(id, _)| id.as_str()).filter(|id| !groups.contains_key(id)).collect();
    let prefix = format!("{MARK}=");
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            kids.entry(parent).or_default().push(*pid);
        }
        if marks
            && let Some(id) = p.environ().iter().find_map(|kv| kv.to_str().and_then(|s| s.strip_prefix(prefix.as_str())))
            && wanted.contains(id)
        {
            marked.entry(id.to_owned()).or_default().push(*pid);
        }
    }
    for (id, pids) in &groups {
        marked.insert((*id).to_owned(), pids.clone());
    }
    // PID 1 may be kiln itself in a container: what it adopted needn't be a helper's child.
    let skip = |pid: Pid| {
        sys.process(pid).is_none_or(|p| helper(&command_of(p.cmd())) || p.parent().filter(|pp| pp.as_u32() != 1).and_then(|pp| sys.process(pp)).is_some_and(|pp| helper(&command_of(pp.cmd()))))
    };
    sessions
        .iter()
        .map(|(id, harness)| {
            let mut seen = Seen { harness: harness.and_then(|h| ran_of(&sys, Pid::from_u32(h))), ..Seen::default() };
            let mut below: HashSet<Pid> = HashSet::new();
            if let Some(h) = harness {
                let root = Pid::from_u32(*h);
                let mut todo = vec![root];
                while let Some(p) = todo.pop() {
                    for c in kids.get(&p).into_iter().flatten() {
                        if !below.insert(*c) || skip(*c) {
                            continue;
                        }
                        todo.push(*c);
                        if seen.under.len() < MAX_RAN
                            && let Some(r) = ran_of(&sys, *c)
                        {
                            seen.under.push(r);
                        }
                    }
                }
            }
            let me = harness.map(Pid::from_u32);
            for pid in marked.get(id).into_iter().flatten() {
                if Some(*pid) == me || below.contains(pid) || skip(*pid) {
                    continue;
                }
                let Some(r) = ran_of(&sys, *pid) else { continue };
                let into = if login_agent(&r.command) { &mut seen.agents } else { &mut seen.apart };
                if into.len() < MAX_RAN {
                    into.push(r);
                }
            }
            (id.clone(), seen)
        })
        .collect()
}

/// Of what was seen, what still runs: the same processes, by pid and start (blocking).
pub fn alive(ran: &[Ran]) -> Vec<Ran> {
    if ran.is_empty() {
        return Vec::new();
    }
    let pids: Vec<u32> = ran.iter().map(|r| r.pid).collect();
    let sys = system(Some(&pids));
    ran.iter().filter(|r| sys.process(sysinfo::Pid::from_u32(r.pid)).is_some_and(|p| p.start_time() == r.started && running(p))).cloned().collect()
}

/// Survivors as trees: each one whose parent isn't among them, with the pids of all under it.
pub fn trees(alive: &[Ran]) -> Vec<(Ran, Vec<u32>)> {
    let pids: HashSet<u32> = alive.iter().map(|r| r.pid).collect();
    let mut under: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in alive {
        under.entry(r.ppid).or_default().push(r.pid);
    }
    alive
        .iter()
        .filter(|r| !pids.contains(&r.ppid))
        .map(|root| {
            let mut all = vec![root.pid];
            let mut i = 0;
            while let Some(p) = all.get(i).copied() {
                all.extend(under.get(&p).into_iter().flatten().filter(|c| **c != p));
                i += 1;
            }
            (root.clone(), all)
        })
        .collect()
}

/// What a process runs, as a person reads it: a harness's shell wrapper (Claude Code's
/// `zsh -c source <snapshot> … && eval '<command>' < /dev/null …`, a `bash -lc <command>`) gives way
/// to the command it runs.
pub fn shown(command: &str) -> String {
    let inner = if let Some((_, rest)) = command.split_once("eval '") {
        let end = ["' < /dev/null", "' && pwd -P"].iter().filter_map(|m| rest.find(m)).min().unwrap_or_else(|| rest.rfind('\'').unwrap_or(rest.len()));
        rest.get(..end).unwrap_or(rest).replace("'\\''", "'")
    } else {
        let words: Vec<&str> = command.split_whitespace().collect();
        let shell = words.first().map(|p| p.rsplit('/').next().unwrap_or(p)).is_some_and(|p| matches!(p, "bash" | "zsh" | "sh" | "dash" | "fish"));
        match words.iter().position(|w| matches!(*w, "-c" | "-lc" | "-ic")) {
            Some(i) if shell => words.get(i + 1..).map(|w| w.join(" ")).unwrap_or_default(),
            _ => command.to_owned(),
        }
    };
    super::model::clip_middle(inner.trim(), 160).0
}

/// Stop processes (blocking): asked to end, then killed if they haven't in a few seconds, and
/// waited for (a few seconds more at most). Only these: a stopped harness's children stay.
pub fn stop(pids: &[u32]) {
    use sysinfo::{Pid, Signal};
    if pids.is_empty() {
        return;
    }
    let sys = system(Some(pids));
    for p in pids {
        if let Some(proc_) = sys.process(Pid::from_u32(*p))
            && proc_.kill_with(Signal::Term).is_none()
        {
            proc_.kill();
        }
    }
    let end = std::time::Instant::now() + GRACE;
    let mut pause = Duration::from_millis(1);
    loop {
        let left: Vec<u32> = alive_pids(pids);
        if left.is_empty() {
            return;
        }
        if std::time::Instant::now() >= end {
            let sys = system(Some(&left));
            for p in &left {
                if let Some(proc_) = sys.process(Pid::from_u32(*p)) {
                    proc_.kill();
                }
            }
            // Gone once each is next given the CPU: on a busy machine, not at once.
            let end = std::time::Instant::now() + GRACE;
            let mut pause = Duration::from_millis(1);
            while !alive_pids(&left).is_empty() && std::time::Instant::now() < end {
                std::thread::sleep(pause);
                pause = next_look(pause);
            }
            return;
        }
        std::thread::sleep(pause);
        pause = next_look(pause);
    }
}

/// Everything under a process now (blocking).
fn under(root: u32) -> Vec<u32> {
    use sysinfo::Pid;
    let sys = system(None);
    let mut kids: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            kids.entry(parent).or_default().push(*pid);
        }
    }
    let mut out = Vec::new();
    let mut todo = vec![Pid::from_u32(root)];
    let mut seen = HashSet::new();
    while let Some(p) = todo.pop() {
        for c in kids.get(&p).into_iter().flatten() {
            if seen.insert(*c) {
                todo.push(*c);
                out.push(c.as_u32());
            }
        }
    }
    out
}

fn alive_pids(pids: &[u32]) -> Vec<u32> {
    let sys = system(Some(pids));
    pids.iter().copied().filter(|p| sys.process(sysinfo::Pid::from_u32(*p)).is_some_and(running)).collect()
}

/// Stop a survivor and what runs under it now (blocking).
pub fn stop_tree(root: &Ran) {
    if alive(std::slice::from_ref(root)).is_empty() {
        return;
    }
    let mut pids = vec![root.pid];
    pids.extend(under(root.pid));
    stop(&pids);
}

/// A survivor as one of the agent's background tasks.
pub fn task(root: &Ran) -> super::model::Task {
    let shown = shown(&root.command);
    super::model::Task {
        id: format!("{LEFT}{}", root.pid),
        description: shown.clone(),
        kind: "shell".to_owned(),
        status: "running".to_owned(),
        background: true,
        summary: Some(format!("Running since before kiln restarted (process {})", root.pid)),
        started: i64::try_from(root.started).unwrap_or(0).saturating_mul(1000),
        class: Some("shell".to_owned()),
        command: Some(shown),
        ..super::model::Task::default()
    }
}

/// The lines kiln adds to its word to an agent whose turn a restart cut short: what still runs.
pub fn said(trees: &[(Ran, Vec<u32>)], now_s: u64) -> String {
    if trees.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\nStill running from before the restart (you started them; use them rather than starting them again, and stop them when you're done with them):");
    for (root, _) in trees {
        let for_ = now_s.saturating_sub(root.started);
        let ago = if for_ >= 3600 { format!("{}h {:02}m", for_ / 3600, (for_ % 3600) / 60) } else { format!("{}m", for_ / 60) };
        s.push_str(&format!("\n- `{}` (process {}, running {ago})", shown(&root.command), root.pid));
    }
    s
}

/// A shell command that sleeps `secs` as a process whose environment this machine lets kiln read
/// (how a process that runs apart is found by its mark). macOS hides the environment of Apple's own
/// programs (`/bin/sleep`), and copies of them can't run (arm64e), so there it is Python's. `None`
/// on a Mac without Python.
#[cfg(all(test, unix))]
pub(crate) fn sleeper(secs: u32) -> Option<String> {
    if cfg!(target_os = "macos") {
        let python = std::process::Command::new("python3").args(["-c", "pass"]).output().is_ok_and(|o| o.status.success());
        return python.then(|| format!("python3 -c 'import time; time.sleep({secs})'"));
    }
    Some(format!("sleep {secs}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ssh_agents_shells_start_are_no_work_of_the_agents() {
        assert!(login_agent("ssh-agent -s") && login_agent("ssh-agent -c") && login_agent("/usr/bin/ssh-agent"));
        assert!(!login_agent("ssh-agent -a /run/user/1000/agent.sock") && !login_agent("ssh-agent -D") && !login_agent("ssh-agent bash") && !login_agent("ssh-add -l"));
        let agent = |pid, started| Ran { pid, ppid: 1, started, command: "ssh-agent -s".into() };
        let kept = AGENT_KEPT.as_secs();
        let seen = HashMap::from([("s".to_owned(), Seen { agents: vec![agent(7, 1_000), agent(8, 1_001)], ..Seen::default() })]);
        assert_eq!(agents_to_stop(&seen, 1_000 + kept), vec![7], "only those kept long enough");
    }

    #[test]
    fn helpers_are_left_out_and_wrappers_give_way() {
        assert!(helper("/Users/a/.local/bin/kiln reach mcp"));
        assert!(helper("npx -y @modelcontextprotocol/server-filesystem /w"));
        assert!(helper("uvx mcp-server-fetch"));
        assert!(!helper("/bin/zsh -c source /x.sh && eval 'npm run dev' < /dev/null"));
        let claude = "/bin/zsh -c source /Users/a/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && setopt NO_EXTENDED_GLOB 2>/dev/null || true && eval 'cd /w && PORT=3000 npm run dev' < /dev/null && pwd -P >| /tmp/claude-a-cwd";
        assert_eq!(shown(claude), "cd /w && PORT=3000 npm run dev");
        assert_eq!(shown("bash -lc cargo watch -x test"), "cargo watch -x test");
        assert_eq!(shown("/usr/bin/python3 -m http.server 8000"), "/usr/bin/python3 -m http.server 8000");
        assert_eq!(shown("zsh -c echo 'it'\\''s'"), "echo 'it'\\''s'");
    }

    #[test]
    fn survivors_group_into_trees() {
        let r = |pid, ppid| Ran { pid, ppid, started: 1, command: format!("p{pid}") };
        // 10 (its parent gone) runs 11, which runs 12; 20 stands alone.
        let t = trees(&[r(10, 1), r(11, 10), r(12, 11), r(20, 99)]);
        let mut got: Vec<(u32, Vec<u32>)> = t.into_iter().map(|(root, all)| (root.pid, all)).collect();
        got.sort();
        assert_eq!(got, vec![(10, vec![10, 11, 12]), (20, vec![20])]);
    }

    #[test]
    fn what_kiln_says_of_them() {
        let root = Ran { pid: 6405, ppid: 1, started: 1_000, command: "bash /w/queue.sh".into() };
        let s = said(&[(root, vec![6405])], 1_000 + 2 * 3600 + 5 * 60);
        assert!(s.contains("`bash /w/queue.sh` (process 6405, running 2h 05m)"), "{s}");
        assert!(said(&[], 0).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn what_a_session_started_is_found_under_it_or_by_its_mark_until_it_is_stopped() -> anyhow::Result<()> {
        // One under this process; one marked as session "s1"'s that runs apart (its shell exited).
        let mut child = std::process::Command::new("sleep").arg("31").spawn()?;
        let Some(sleep_32) = sleeper(32) else {
            eprintln!("skipped: nothing here sleeps as a process whose environment can be read");
            let _ = child.kill();
            return Ok(());
        };
        let out = std::process::Command::new("sh").arg("-c").arg(format!("{sleep_32} >/dev/null 2>&1 & echo $!")).env(MARK, "s1").output()?;
        let apart: u32 = String::from_utf8_lossy(&out.stdout).trim().parse()?;
        let me = std::process::id();
        let seen = scan(&[("s1".to_owned(), Some(me)), ("s2".to_owned(), None)]);
        let s1 = seen.get("s1").cloned().unwrap_or_default();
        let sleep = s1.under.iter().find(|r| r.pid == child.id()).cloned().ok_or_else(|| anyhow::anyhow!("the child isn't seen under its harness: {:?}", s1.under))?;
        assert!(sleep.command.contains("sleep 31"));
        assert!(s1.apart.iter().any(|r| r.pid == apart && r.command.contains("32")), "{:?}", s1.apart);
        assert!(seen.get("s2").is_some_and(|s| s.apart.is_empty() && s.under.is_empty()));
        assert_eq!(alive(std::slice::from_ref(&sleep)).len(), 1);
        // Another start time is another process.
        assert!(alive(&[Ran { started: sleep.started + 1, ..sleep.clone() }]).is_empty());
        stop_tree(&sleep);
        let _ = child.wait();
        assert!(alive(std::slice::from_ref(&sleep)).is_empty());
        let marked = s1.apart.iter().find(|r| r.pid == apart).cloned().ok_or_else(|| anyhow::anyhow!("not found apart"))?;
        stop_tree(&marked);
        assert!(alive(&[marked]).is_empty());
        Ok(())
    }
}
