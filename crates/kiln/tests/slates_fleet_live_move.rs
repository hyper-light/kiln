//! A live move across machines (docs/agentic.md W7; kiln goal 95101ef32ffb8b099942fe076619c310, K4 and K5, "the live
//! move across two Mac runners"; Slates' A-291 planned move). A server of the test's own and three runners, each its
//! own machine (`KILN_MACHINE_ID`) with its own Slates instance (`KILN_SLATES_INSTANCE`), listed in the server's
//! `[slates_fleet]` with `f = 1`, form kiln's Slates fleet. An agent of the stand-in harness (no model) is started on the
//! first runner in a workspace of its own (a clone of its folder's captured base, K5), writes, appends and removes in
//! its first turn, and is moved by kiln (`agents.move`) to the second runner. What the test reads, each step said
//! (`PROVEN: ...`) as it holds:
//!
//! 1. **Formed.** Every runner connected, and the first's node counts the fleet's three members.
//! 2. **Worked.** The agent's first turn did its three edits inside its volume's mount on the first runner.
//! 3. **Moved.** kiln's word on the session names the second runner (a refusal by kind fails it, saying the kind).
//! 4. **Carried on.** Its second turn, on the second runner, appends to the file its first turn wrote: the file came
//!    with it. It works inside its volume's mount there, and that mount holds exactly the first turn's work plus the
//!    second's, byte for byte.
//! 5. **The old runner let go.** Its mount of the volume is out of its host's table, a write attachment asked of its
//!    node is refused by kind, and a read through its node is never stale (the current bytes, or a typed refusal).
//! 6. **The end.** Ended for good, the session's mount on the second runner goes.
//!
//! Where the runners run, by `SLATES_FLEET_LIVE_MOVE`:
//!
//! - `local`: all three on this machine (nodes on loopback): the move's flow, debuggable on one Mac.
//! - `<runner B>,<runner C>`: the first runner on this machine and the other two on the production kiln runners named,
//!   each started there through `kiln run --on <runner> --no-sync` (this build's `kiln` and the stand-in fetched from a
//!   file server of the test's own; nodes at LAN addresses; `SLATES_FLEET_LIVE_MOVE_HOST` is this machine's address the
//!   others reach). The agent moves from this machine to runner B; runner C is the fleet's third node.
//!
//! Unset, the test skips, saying so. Teardown: the agent is stopped first, and no Slates process is signalled on a
//! machine while its host's mount table lists a mount under the test's folder there (a daemon killed under a live macOS
//! NFS mount has panicked the Mac: Slates' record of 2026-10-06); it says `LEFT RUNNING` instead.
//!
//!   SLATES_FLEET_LIVE_MOVE=local cargo test -p kiln --test slates_fleet_live_move -- --nocapture
//!   SLATES_FLEET_LIVE_MOVE=adas-mac-studio.local,adas-macbook-pro.local SLATES_FLEET_LIVE_MOVE_HOST=192.168.1.82 \
//!     cargo test -p kiln --test slates_fleet_live_move -- --nocapture

#![cfg(unix)]
#![allow(dead_code, reason = "vm_e2e's helpers are shared with tests that use more of them")]

mod common;
mod vm_e2e;

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The switch: `local`, or the two production runners the agent moves to and the third node runs on.
const SWITCH: &str = "SLATES_FLEET_LIVE_MOVE";
/// This machine's address the other machines reach (remote runs only).
const HOST: &str = "SLATES_FLEET_LIVE_MOVE_HOST";
/// `1` keeps the lab's folder (the server's and runners' logs) after the test, for reading why it failed.
const KEEP: &str = "SLATES_FLEET_LIVE_MOVE_KEEP";
/// The runners, by the names the test starts them as; the agent starts on the first and moves to the second.
const NAMES: [&str; 3] = ["live-a", "live-b", "live-c"];
/// How long the test waits at any one step on loaded machines.
const WITHIN: Duration = Duration::from_secs(600);
const LOOK: Duration = Duration::from_millis(500);
/// The first turn's words and the second's (the stand-in's, `tests/fixtures/vm-stub/harness/stub`).
const FIRST: &str = "WRITE=made.txt APPEND=README.md REMOVE=gone.txt";
/// The harness the stand-in plays: an ACP one, whose conversation kiln carries in words from its own timeline when the
/// agent moves (`harness::moving::record`). Claude Code's moves carry its own transcript file, which the stand-in (no
/// model) never writes, so a stand-in `claude` can't move ("Claude Code's record of session ... is not on this machine").
const HARNESS: &str = "opencode";
/// How long a remote machine's preparation may take, fetching this build's kiln over the LAN, in seconds.
const PREPARE_S: u32 = 1800;
/// How many times the test asks kiln to move the agent while Slates refuses it `unplaced`.
const MOVES: usize = 10;
/// How long the test waits on one ask to move before asking again.
const ASK_AGAIN: Duration = Duration::from_secs(30);
const SECOND: &str = "WRITE=on-b.txt APPEND=made.txt";

/// The pure checks: what the switch says, what a remote preparation printed, and the files the moved volume holds.
mod checks {
    /// Where the runners run.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Mode {
        Local,
        Remote { b: String, c: String },
    }

    /// The switch's word: `local`, or two runner names comma-separated; none, or anything else, says why.
    pub fn mode(said: Option<&str>) -> Result<Mode, String> {
        let said = said.map(str::trim).filter(|s| !s.is_empty()).ok_or("unset")?;
        if said == "local" {
            return Ok(Mode::Local);
        }
        match said.split(',').map(str::trim).collect::<Vec<_>>().as_slice() {
            [b, c] if !b.is_empty() && !c.is_empty() && b != c => Ok(Mode::Remote { b: (*b).to_owned(), c: (*c).to_owned() }),
            _ => Err(format!("{said:?} is neither `local` nor two runner names, comma-separated")),
        }
    }

    /// What a machine's preparation said: its node's base UDP port, a free TCP port for its runner's OTLP receiver and
    /// its own address toward this machine (`LIVE_MOVE_PREPARED <base> <tcp> <ip>`).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Prepared {
        pub base: u16,
        pub otlp: u16,
        pub ip: String,
    }

    pub fn prepared(out: &str) -> Result<Prepared, String> {
        let line = out.lines().rev().find(|l| l.starts_with("LIVE_MOVE_PREPARED ")).ok_or_else(|| format!("no preparation said: {out}"))?;
        match line.split_whitespace().collect::<Vec<_>>().as_slice() {
            [_, base, otlp, ip] => {
                let port = |s: &str| s.parse::<u16>().ok().filter(|p| *p > 0 && *p < u16::MAX).ok_or_else(|| format!("no port: {line}"));
                let ip: std::net::Ipv4Addr = ip.parse().map_err(|_| format!("no IPv4 address: {line}"))?;
                Ok(Prepared { base: port(base)?, otlp: port(otlp)?, ip: ip.to_string() })
            }
            _ => Err(format!("a preparation's line in no shape: {line}")),
        }
    }

    /// The files of the moved volume a listing printed (`== <path>` then its bytes, `-- absent <path>` where none), by
    /// path.
    pub fn listing(out: &str) -> std::collections::BTreeMap<String, Option<String>> {
        let mut files = std::collections::BTreeMap::new();
        let mut at: Option<String> = None;
        for line in out.split_inclusive('\n') {
            if let Some(p) = line.strip_prefix("== ") {
                at = Some(p.trim().to_owned());
                files.insert(p.trim().to_owned(), Some(String::new()));
            } else if let Some(p) = line.strip_prefix("-- absent ") {
                at = None;
                files.insert(p.trim().to_owned(), None);
            } else if let Some(Some(body)) = at.as_ref().and_then(|p| files.get_mut(p)) {
                body.push_str(line);
            }
        }
        files
    }

    /// What the moved volume holds: the project as it was, its first turn's three edits and its second's two.
    pub fn expected() -> std::collections::BTreeMap<String, Option<String>> {
        [
            // Appended on the first runner, and again by the hand-off turn on the second, which hears the first turn's words
            // (kiln carries an ACP agent's conversation in words): two lines prove the first append moved.
            ("README.md", Some("the project\nAPPENDED README.md\nAPPENDED README.md\n")),
            ("made.txt", Some("made.txt\nAPPENDED made.txt\n")),
            ("on-b.txt", Some("on-b.txt\n")),
            ("src/lib.rs", Some("pub fn f() {}\n")),
            ("gone.txt", None),
        ]
        .into_iter()
        .map(|(p, b)| (p.to_owned(), b.map(str::to_owned)))
        .collect()
    }

    /// The lines of the stand-in's turn `n` (from 0): those after its `n`th `STUB_DONE`, up to and with the next.
    pub fn turn(lines: &[String], n: usize) -> Vec<String> {
        let mut done = 0_usize;
        let mut out = Vec::new();
        for l in lines {
            if done == n {
                out.push(l.clone());
            }
            if l.contains("STUB_DONE") {
                done = done.saturating_add(1);
                if done > n {
                    break;
                }
            }
        }
        out
    }

    /// The kind a kiln notice refusing a move names (`... [kind: unplaced] ...`).
    pub fn refused_kind(notice: &str) -> Option<String> {
        let rest = notice.split("[kind: ").nth(1)?;
        let kind = rest.split(']').next()?.trim();
        (!kind.is_empty()).then(|| kind.to_owned())
    }

    /// Whether `kiln run` failed reaching its runner rather than running the script: kiln's own words for it.
    pub fn kiln_unreached(said: &str) -> bool {
        ["did not answer in", "the kiln server did not answer in time", "no longer has this run (its runner restarted)", "no runner suits"].iter().any(|w| said.contains(w))
    }

    /// Whether a move's refusal says to ask again: Slates' `unplaced`, or the workspace's mount found busy as the source let
    /// it go (holders macOS can't name, seen across the LAN 2026-11-20), the agent going on where it was either way.
    pub fn asked_again(notice: &str) -> bool {
        match refused_kind(notice).as_deref() {
            Some("unplaced") => true,
            Some("slates") => notice.contains("Slates' unmount:") && notice.contains("is busy"),
            _ => false,
        }
    }

    /// Whether a session's end says its workspace was refused only for now: its node hadn't imported its consensus groups
    /// yet (Slates' `ConsensusNotInitialized`), or its first mount was refused for now (Slates' `Retry`, such as a lease
    /// its members across the LAN hadn't confirmed yet, `LeaseUnconfirmed`, 2026-11-20), which kiln's first mount
    /// doesn't wait on as its mount of a moved volume does.
    pub fn refused_for_now(said: &str) -> bool {
        said.contains("no consensus group yet") || said.contains("Slates' mount: refused for now")
    }

    /// How many nodes a runner's word that it joins kiln's Slates fleet says it heard (`nodes=<n>`).
    pub fn nodes_heard(line: &str) -> Option<usize> {
        let plain: String = strip_ansi(line);
        plain.split_whitespace().find_map(|w| w.strip_prefix("nodes=")).and_then(|n| n.parse().ok())
    }

    /// `s` without its terminal colours (`ESC [ ... m`).
    pub fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\u{1b}' && it.peek() == Some(&'[') {
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Whether the mount table `mount` printed lists a mount at or under `dir`.
    pub fn mounted_under(table: &str, dir: &str) -> Vec<String> {
        let dir = dir.trim_end_matches('/');
        table
            .lines()
            .filter_map(|l| l.split(" on ").nth(1).and_then(|r| r.rsplit_once(" (")).map(|(at, _)| at.to_owned()))
            .filter(|at| at == dir || at.starts_with(&format!("{dir}/")))
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn k4_live_move_the_switch_says_where_the_runners_run() {
            assert_eq!(mode(Some("local")), Ok(Mode::Local));
            assert_eq!(mode(Some(" b.local , c.local ")), Ok(Mode::Remote { b: "b.local".into(), c: "c.local".into() }));
            assert!(mode(None).is_err());
            assert!(mode(Some("b.local")).is_err());
            assert!(mode(Some("b.local,b.local")).is_err());
            assert!(mode(Some("a,b,c")).is_err());
        }

        #[test]
        fn k4_live_move_a_preparation_names_its_ports_and_address() {
            let out = "fetched\nLIVE_MOVE_PREPARED 50100 50200 192.168.1.84\n";
            assert_eq!(prepared(out), Ok(Prepared { base: 50100, otlp: 50200, ip: "192.168.1.84".into() }));
            assert!(prepared("LIVE_MOVE_PREPARED 0 50200 192.168.1.84").is_err());
            assert!(prepared("LIVE_MOVE_PREPARED 65535 50200 192.168.1.84").is_err(), "a base needs the port after it");
            assert!(prepared("LIVE_MOVE_PREPARED 50100 50200 host").is_err());
            assert!(prepared("nothing").is_err());
        }

        #[test]
        fn k4_live_move_a_listing_reads_back_each_file_or_its_absence() {
            let out = "== README.md\nthe project\nAPPENDED README.md\nAPPENDED README.md\n== made.txt\nmade.txt\nAPPENDED made.txt\n== on-b.txt\non-b.txt\n== src/lib.rs\npub fn f() {}\n-- absent gone.txt\n";
            assert_eq!(listing(out), expected());
            let short = "== README.md\nthe project\n-- absent gone.txt\n";
            assert_ne!(listing(short), expected());
        }

        #[test]
        fn k4_live_move_a_turn_is_read_by_its_stub_done() {
            let lines: Vec<String> = ["CWD /a", "STUB_DONE", "HEARD x", "STUB_DONE", "CWD /b", "WROTE y", "STUB_DONE"].iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(turn(&lines, 0), vec!["CWD /a", "STUB_DONE"]);
            assert_eq!(turn(&lines, 2), vec!["CWD /b", "WROTE y", "STUB_DONE"]);
            assert!(turn(&lines, 3).is_empty());
        }

        #[test]
        fn k4_live_move_a_refusal_names_its_kind() {
            let said = "Couldn't move to 6770f2: Slates couldn't place its workspace's records in time and kept it where it was: move it again [kind: unplaced] — it goes on here";
            assert_eq!(refused_kind(said).as_deref(), Some("unplaced"));
            assert_eq!(refused_kind("Moved to 6770f2"), None);
            assert_eq!(refused_kind("[kind: ]"), None);
        }

        #[test]
        fn k4_live_move_a_workspace_refused_for_now_is_started_again() {
            assert!(refused_for_now("its workspace: Slates' base create: this node has no consensus group yet; create it once"));
            assert!(refused_for_now(r#""error":"its workspace: Slates' mount: refused for now (LeaseUnconfirmed { version: 7 }""#));
            assert!(!refused_for_now("its workspace: Slates' mount: the daemon's MOUNT service refused the export (mountstat3 2)"));
        }

        #[test]
        fn k4_live_move_a_move_is_asked_again_only_where_its_refusal_says_to() {
            assert!(asked_again("Couldn't move to b: Slates couldn't place its workspace's records in time [kind: unplaced] — it goes on here"));
            assert!(asked_again("Couldn't move to b: Slates refused to move its workspace: Slates' unmount: /w is busy: its holders were not read [kind: slates] — it goes on here"));
            assert!(!asked_again("Couldn't move to b: Slates refused: something else [kind: slates]"));
            assert!(!asked_again("Couldn't move to b [kind: not_plain]"));
        }

        #[test]
        fn k4_live_move_kiln_not_reaching_a_runner_is_told_from_the_scripts_failure() {
            assert!(kiln_unreached("ERROR b24c5c951260dcdfd0972f2aeaf3cf51 did not answer in 15s"));
            assert!(kiln_unreached("ERROR 2768d0d5 no longer has this run (its runner restarted)"));
            assert!(!kiln_unreached("curl: (22) The requested URL returned error: 404"));
        }

        #[test]
        fn k4_live_move_a_join_says_how_many_nodes_it_heard() {
            let line = "2026-11-20T06:23:12Z \u{1b}[32m INFO\u{1b}[0m this runner's Slates joins kiln's Slates fleet \u{1b}[3mmanifest\u{1b}[0m\u{1b}[2m=\u{1b}[0m/x/fleet.json \u{1b}[3mnodes\u{1b}[0m\u{1b}[2m=\u{1b}[0m2 \u{1b}[3mf\u{1b}[0m\u{1b}[2m=\u{1b}[0m1";
            assert_eq!(nodes_heard(line), Some(2));
            assert_eq!(nodes_heard("this runner's Slates joins kiln's Slates fleet manifest=/x nodes=3 f=1"), Some(3));
            assert_eq!(nodes_heard("enrolled with server"), None);
        }

        #[test]
        fn k4_live_move_a_mount_under_the_folder_is_found_and_no_other() {
            let table = "/dev/disk3s1 on / (apfs, local)\nlocalhost:/x on /Users/a/.cache/live/r-b/state/workspaces/kiln-ws (nfs, nodev)\nlocalhost:/y on /Users/a/.cache/liver (nfs)\n";
            assert_eq!(mounted_under(table, "/Users/a/.cache/live/"), vec!["/Users/a/.cache/live/r-b/state/workspaces/kiln-ws".to_owned()]);
            assert!(mounted_under(table, "/Users/a/.cache/live/r-c").is_empty());
        }
    }
}

use checks::{Mode, Prepared};

/// Whether the test runs here, and how; a skip says why.
fn mode() -> Option<Mode> {
    match checks::mode(std::env::var(SWITCH).ok().as_deref()) {
        Ok(m) => Some(m),
        Err(why) => {
            eprintln!("skipped: {SWITCH} ({why}): `local`, or two production runners `<B>,<C>` with {HOST}, runs a live move across machines");
            None
        }
    }
}

/// A file server of the test's own for the remote machines: this build's `kiln`, the stand-in, and the server's join
/// token, the token served at most twice (once to each remote runner) so it never rides on a command line.
struct Files {
    port: u16,
    stop: Arc<Mutex<bool>>,
}

impl Files {
    fn start(join: String) -> anyhow::Result<Files> {
        // This build's kiln gzipped once (about 260 MB, about 100 MB gzipped): a LAN link between two of the Macs ran
        // at 280 KB/s (2026-11-20), and the whole binary didn't arrive within the preparation's ten minutes.
        let gz = Command::new("/usr/bin/gzip").args(["-6", "-c"]).arg(common::BIN).output()?;
        anyhow::ensure!(gz.status.success() && !gz.stdout.is_empty(), "gzip {}: {}", common::BIN, String::from_utf8_lossy(&gz.stderr));
        let gz = gz.stdout;
        let listener = std::net::TcpListener::bind("0.0.0.0:0")?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let stop = Arc::new(Mutex::new(false));
        let stopped = Arc::clone(&stop);
        let mut joins_left = 2_u8;
        std::thread::spawn(move || {
            while !stopped.lock().map(|s| *s).unwrap_or(true) {
                let Ok((mut s, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                let _ = s.set_nonblocking(false);
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                let mut head = [0_u8; 1024];
                let n = s.read(&mut head).unwrap_or(0);
                let line = String::from_utf8_lossy(head.get(..n).unwrap_or_default()).lines().next().unwrap_or_default().to_owned();
                let path = line.split_whitespace().nth(1).unwrap_or_default().to_owned();
                let body = match path.as_str() {
                    "/kiln.gz" => Some(gz.clone()),
                    "/stub" => std::fs::read(vm_e2e::stub_path()).ok(),
                    "/join" if joins_left > 0 => {
                        joins_left -= 1;
                        Some(join.clone().into_bytes())
                    }
                    _ => None,
                };
                let _ = match body {
                    Some(b) => s.write_all(format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", b.len()).as_bytes()).and_then(|()| s.write_all(&b)),
                    None => s.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
                };
            }
        });
        Ok(Files { port, stop })
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        if let Ok(mut s) = self.stop.lock() {
            *s = true;
        }
    }
}

/// Production's `kiln` (the one on this machine's PATH), for `kiln run`: never this build's, which is the test's own.
fn production_kiln() -> Command {
    let mut c = Command::new("kiln");
    c.env_remove("KILN_CONFIG");
    c
}

/// `script` run on production runner `runner` in its home folder: its output, or what it said failing.
/// Asked again, after a pause, while kiln itself says it couldn't reach the runner (its server or the runner didn't
/// answer in time, or the runner restarted under the run: production runners flap), up to five times; every script the
/// test runs this way can run again.
fn on(runner: &str, script: &str, timeout_s: u32) -> anyhow::Result<String> {
    let mut tries = 0_u32;
    loop {
        tries = tries.saturating_add(1);
        let out = production_kiln().args(["run", "-q", "--on", runner, "--no-sync", "--timeout", &timeout_s.to_string(), "--", script]).stdin(Stdio::null()).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            return Ok(text);
        }
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        if tries < 5 && checks::kiln_unreached(&said) {
            eprintln!("WAITED: kiln didn't reach {runner} (try {tries}): {}", said.trim().chars().take(200).collect::<String>());
            std::thread::sleep(Duration::from_secs(20));
            continue;
        }
        anyhow::bail!("on {runner}: {script}\n{}: {text}\n{said}", out.status);
    }
}

/// Where a runner runs.
enum At {
    /// On this machine, a runner of the test's fleet.
    Here,
    /// On production runner `runner`, under `dir` there, its `kiln run` (which runs it) held here.
    There { runner: String, dir: String, run: Option<Child> },
}

/// One runner of the test: the name it was started as, its machine key, its Slates instance, its node's address.
struct Node {
    name: &'static str,
    machine: String,
    instance: String,
    address: String,
    at: At,
    otlp: u16,
}

struct Lab {
    fleet: common::Fleet,
    nodes: Vec<Node>,
    project: PathBuf,
    files: Option<Files>,
    agent: Option<String>,
    /// This machine's address the runners reach the server at.
    host: String,
}

/// Two consecutive UDP ports free on `ip`: a node's base and the one after it.
fn two_ports(ip: &str) -> anyhow::Result<u16> {
    for _ in 0..64 {
        let base = std::net::UdpSocket::bind((ip, 0))?.local_addr()?.port();
        let Some(next) = base.checked_add(1) else { continue };
        if std::net::UdpSocket::bind((ip, base)).is_ok() && std::net::UdpSocket::bind((ip, next)).is_ok() {
            return Ok(base);
        }
    }
    anyhow::bail!("no two consecutive UDP ports free on {ip}")
}

/// The preparation of a remote machine's folder `dir`: this build's `kiln`, the stand-in under Claude Code's name, the
/// join token, a login shell that runs what it's given on the stand-in's PATH, and its ports and address said.
fn preparation(dir: &str, files: &str, host: &str) -> String {
    format!(
        r#"set -e; d="{dir}"; mkdir -p "$d/bin" "$d/home" "$d/state" "$d/work"; cd "$d"
curl -fsS "{files}/kiln.gz" | gunzip > kiln
for f in stub join; do curl -fsS -o "$f" "{files}/$f"; done
chmod 755 kiln stub; chmod 600 join; ln -sf "$d/stub" "$d/bin/opencode"; "$d/kiln" --version > /dev/null
printf '#!/bin/sh\nPATH=%s/bin:/usr/bin:/bin:/usr/sbin:/sbin\nexport PATH\nwhile [ $# -gt 1 ]; do shift; done\nexec /bin/sh -c "$1"\n' "$d" > login-shell; chmod 755 login-shell
/usr/bin/python3 -c '
import socket, sys
def two():
    for _ in range(64):
        a = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); a.bind(("0.0.0.0", 0)); p = a.getsockname()[1]
        b = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            b.bind(("0.0.0.0", p + 1)); a.close(); b.close(); return p
        except OSError:
            a.close(); b.close()
t = socket.socket(); t.bind(("127.0.0.1", 0)); tcp = t.getsockname()[1]; t.close()
u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); u.connect((sys.argv[1], 9)); ip = u.getsockname()[0]
print("LIVE_MOVE_PREPARED", two(), tcp, ip)
' "{host}""#
    )
}

impl Lab {
    fn start(mode: &Mode) -> anyhow::Result<Lab> {
        let run = format!("{}{}", std::process::id(), Instant::now().elapsed().subsec_nanos() % 100_000);
        let host = match mode {
            Mode::Local => "127.0.0.1".to_owned(),
            Mode::Remote { .. } => std::env::var(HOST).map_err(|_| anyhow::anyhow!("{HOST} names no address of this machine's the others reach"))?,
        };
        let bind = if matches!(mode, Mode::Local) { "127.0.0.1" } else { "0.0.0.0" };
        let mut root = tempfile::tempdir()?;
        if std::env::var_os(KEEP).is_some_and(|v| v == "1") {
            root.disable_cleanup(true);
        }
        eprintln!("LAB: {}", root.path().display());
        let (http, quic) = (common::free_port()?, common::free_udp_port()?);
        let fleet = common::Fleet { root, http, admin: String::new(), server_env: Vec::new(), server: None, runners: Default::default(), otlp: Default::default() };
        let data = fleet.root.path().join("server");
        let project = fleet.root.path().join("project");
        make_project(&project)?;
        let mut lab = Lab { fleet, nodes: Vec::new(), project, files: None, agent: None, host: host.clone() };
        let instance = |name: &str| format!("kilnlive{run}{}", name.replace('-', ""));
        let [a, name_b, name_c] = NAMES;
        lab.nodes.push(Node { name: a, machine: common::machine(a), instance: instance(a), address: format!("{host}:{}", two_ports(&host)?), at: At::Here, otlp: common::free_port()? });
        let remotes: Vec<(&'static str, Option<String>)> = match mode {
            Mode::Local => vec![(name_b, None), (name_c, None)],
            Mode::Remote { b, c } => vec![(name_b, Some(b.clone())), (name_c, Some(c.clone()))],
        };
        // The server first (its join token is what the remote machines fetch), its fleet listing filled in below.
        let mut prepared: Vec<(&'static str, Option<(String, String, Prepared)>)> = Vec::new();
        let write_config = |lab: &Lab, listed: &str| -> anyhow::Result<()> {
            let cfg = format!(
                "[server]\nhttp_addr = \"{bind}:{http}\"\nquic_addr = \"{bind}:{quic}\"\npublic_url = \"http://{host}:{http}\"\nquic_advertise = \"{host}:{quic}\"\ndata_dir = {data}\n\n[gitops]\nauto_pipelines = false\n\n[local_agent]\nenabled = false\n\n[discovery]\nenabled = false\n\n[kubernetes]\nenabled = false\n\n[cd]\nenabled = false\n\n[telemetry]\nsample_seconds = 3600\n\n[observe]\nenabled = true\nhost_seconds = 3600\n\n[slates_fleet]\nf = 1\n[slates_fleet.nodes]\n{listed}",
                data = common::toml_path(&data)
            );
            std::fs::write(lab.fleet.root.path().join("kiln.toml"), cfg)?;
            Ok(())
        };
        if matches!(mode, Mode::Remote { .. }) {
            // A server listing no fleet yet, for its join token; started again below with the listing.
            write_config(&lab, "")?;
            lab.fleet.start_server()?;
            let join = std::fs::read_to_string(data.join("join.token"))?.trim().to_owned();
            lab.fleet.kill_server();
            let files = Files::start(join)?;
            let url = format!("http://{host}:{}", files.port);
            lab.files = Some(files);
            for (name, runner) in &remotes {
                let runner = runner.clone().ok_or_else(|| anyhow::anyhow!("no runner for {name}"))?;
                let dir = format!("$HOME/.cache/kiln-live-move/{run}-{name}");
                let out = match on(&runner, &format!("{}; echo \"LIVE_MOVE_DIR $d\"", preparation(&dir, &url, &host)), PREPARE_S) {
                    Ok(out) => out,
                    Err(e) => {
                        // Nothing runs there yet: its folder goes.
                        let _ = on(&runner, &format!("rm -rf \"{dir}\""), 60);
                        return Err(e);
                    }
                };
                let p = checks::prepared(&out).map_err(|e| anyhow::anyhow!("{runner}: {e}"))?;
                let dir = out.lines().find_map(|l| l.strip_prefix("LIVE_MOVE_DIR ")).map(str::to_owned).unwrap_or(dir);
                eprintln!("PREPARED: {name} on {runner} at {} (node {}:{})", dir, p.ip, p.base);
                prepared.push((name, Some((runner, dir, p))));
            }
        } else {
            for (name, _) in &remotes {
                prepared.push((name, None));
            }
        }
        for (name, there) in prepared {
            let node = match there {
                None => Node { name, machine: common::machine(name), instance: instance(name), address: format!("{host}:{}", two_ports(&host)?), at: At::Here, otlp: common::free_port()? },
                Some((runner, dir, p)) => Node { name, machine: common::machine(name), instance: instance(name), address: format!("{}:{}", p.ip, p.base), at: At::There { runner, dir, run: None }, otlp: p.otlp },
            };
            lab.nodes.push(node);
        }
        let listed: String = lab.nodes.iter().map(|n| format!("\"{}\" = \"{}\"\n", n.machine, n.address)).collect();
        write_config(&lab, &listed)?;
        lab.fleet.start_server()?;
        lab.fleet.admin = std::fs::read_to_string(data.join("admin.token"))?.trim().to_owned();
        // One at a time, each once the ones before it are connected and joined: the server answers a joining node with
        // the nodes of connected runners only, so runners started together each hear only themselves and form fleets
        // of one that never merge (seen 2026-11-19: `nodes=1` in each one's manifest, members 1 for ten minutes).
        for i in 0..lab.nodes.len() {
            lab.start_runner(i)?;
            let n = lab.node(i)?;
            let log = lab.fleet.root.path().join(format!("{}.log", n.name));
            common::agents::connected(lab.fleet.http, &lab.fleet.admin, n.name, &log, WITHIN).map_err(|e| anyhow::anyhow!("{e:#}\n{}", lab.remote_logs()))?;
            lab.until_joined(i)?;
            lab.on_host(i)?;
        }
        eprintln!("PROVEN: every runner connected ({})", lab.nodes.iter().map(|n| n.name).collect::<Vec<_>>().join(", "));
        lab.until_formed()?;
        Ok(lab)
    }

    fn start_runner(&mut self, i: usize) -> anyhow::Result<()> {
        let http = self.fleet.http;
        let host = self.host.clone();
        let root = self.fleet.root.path().to_owned();
        let Some(n) = self.nodes.get_mut(i) else { anyhow::bail!("no runner {i}") };
        match &mut n.at {
            At::Here => {
                let dir = root.join(format!("{}-stub", n.name));
                let mut env = vm_e2e::stub_host_env(&dir)?;
                env.extend([
                    ("KILN_MACHINE_ID", n.name.to_owned()),
                    ("KILN_SLATES_INSTANCE", n.instance.clone()),
                    ("KILN_SLATES_DIRECT", "1".to_owned()),
                    ("KILN_SLATES", String::new()),
                ]);
                // As kiln's e2e fixture starts its runner (`vm_e2e::runner`): a terminal's runner, whose harness side
                // gives agents workspaces and starts a listed runner's Slates as the fleet's node (`--no-terminal`
                // turns all of it off).
                let join = std::fs::read_to_string(root.join("server").join("join.token"))?.trim().to_owned();
                let home = root.join(n.name).join("home");
                std::fs::create_dir_all(&home)?;
                let log_path = root.join(format!("{}.log", n.name));
                let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
                let child = common::kiln()
                    .args(["agent", "--server", &format!("http://127.0.0.1:{http}"), "--token", &join, "--name", n.name])
                    .arg("--work-dir")
                    .arg(root.join(n.name).join("work"))
                    .arg("--state-dir")
                    .arg(root.join(n.name).join("state"))
                    .env("HOME", &home)
                    .env_remove("GIT_CONFIG_GLOBAL")
                    .env("NO_COLOR", "1")
                    .env(common::WARM_FLOOR.0, common::WARM_FLOOR.1)
                    .env("KILN_OTLP_ADDR", format!("127.0.0.1:{}", n.otlp))
                    .env("KILN_OTLP_GRPC_ADDR", "off")
                    .env("KILN_OBSERVE_HOST_SECONDS", "3600")
                    .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone()?))
                    .stderr(Stdio::from(log))
                    .spawn()?;
                self.fleet.runners.insert(n.name.to_owned(), Some(common::Proc { child, log: log_path }));
                Ok(())
            }
            At::There { runner, dir, run } => {
                // `env -i`: a run's own KILN_* would point this runner at production.
                let script = format!(
                    r#"d="{dir}"; echo $$ > "$d/runner.pid"; exec env -i HOME="$d/home" PATH="$d/bin:/usr/bin:/bin:/usr/sbin:/sbin" SHELL="$d/login-shell" LANG=C.UTF-8 NO_COLOR=1 KILN_LOG=info KILN_MACHINE_ID={name} KILN_SLATES_INSTANCE={instance} KILN_SLATES_DIRECT=1 KILN_SLATES= KILN_OTLP_ADDR=127.0.0.1:{otlp} KILN_OTLP_GRPC_ADDR=off KILN_OBSERVE_HOST_SECONDS=3600 KILN_WARM_FLOOR=0 "$d/kiln" agent --server http://{host}:{http} --token "$(cat "$d/join")" --name {name} --state-dir "$d/state" --work-dir "$d/work" >> "$d/runner.log" 2>&1"#,
                    name = n.name,
                    instance = n.instance,
                    otlp = n.otlp,
                );
                let log = std::fs::File::create(root.join(format!("{}.log", n.name)))?;
                let child = production_kiln()
                    .args(["run", "-q", "--on", runner.as_str(), "--no-sync", "--detach", "--timeout", "7200", "--", &script])
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone()?))
                    .stderr(Stdio::from(log))
                    .spawn()?;
                *run = Some(child);
                Ok(())
            }
        }
    }

    /// Runner `i`.
    fn node(&self, i: usize) -> anyhow::Result<&Node> {
        self.nodes.get(i).ok_or_else(|| anyhow::anyhow!("no runner {i}"))
    }

    /// `script` run by `/bin/sh` on runner `i`'s machine: its output.
    fn run_on(&self, i: usize, script: &str) -> anyhow::Result<String> {
        match &self.node(i)?.at {
            At::Here => Ok(String::from_utf8_lossy(&Command::new("/bin/sh").args(["-c", script]).output()?.stdout).into_owned()),
            At::There { runner, .. } => on(runner, script, 60),
        }
    }

    /// Runner `n`'s log, read where it runs.
    fn log(&self, n: &Node) -> String {
        match &n.at {
            At::Here => std::fs::read_to_string(self.fleet.root.path().join(format!("{}.log", n.name))).unwrap_or_default(),
            At::There { runner, dir, .. } => on(runner, &format!("cat \"{dir}/runner.log\""), 60).unwrap_or_default(),
        }
    }

    /// Waits until runner `i`'s Slates joined kiln's Slates fleet having heard every runner started before it.
    fn until_joined(&self, i: usize) -> anyhow::Result<()> {
        let n = self.node(i)?;
        let began = Instant::now();
        loop {
            let log = self.log(n);
            let joined = log.lines().filter(|l| l.contains("joins kiln's Slates fleet")).filter_map(|l| checks::nodes_heard(l)).last();
            if let Some(heard) = joined {
                anyhow::ensure!(heard > i, "{} joined having heard {heard} node(s), not the {} started before it and its own", n.name, i);
                eprintln!("PROVEN: {} joined kiln's Slates fleet hearing {heard} node(s) ({:?})", n.name, began.elapsed());
                return Ok(());
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "{}'s Slates never joined kiln's Slates fleet:\n{}", n.name, log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"));
            std::thread::sleep(LOOK);
        }
    }

    /// Runner `i` set as kiln's e2e fixture sets its runner (`vm_e2e::up_own`, `slates_e2e`'s fixture): its agents on
    /// its host (engine `host`), and its headroom off, so a machine others keep busy doesn't hold its agents' turns.
    fn on_host(&self, i: usize) -> anyhow::Result<()> {
        let n = self.node(i)?;
        common::agents::headroom_off(self.fleet.http, &self.fleet.admin, n.name, WITHIN)?;
        let path = format!("/api/runners/{}/engine", n.machine);
        let began = Instant::now();
        loop {
            let (code, body) = common::http(self.fleet.http, "PUT", &path, &self.fleet.admin, Some(&json!({"engine": "host"})))?;
            let now = self.get(&path).unwrap_or(Value::Null);
            if code == 200 && now.pointer("/isolation/engine").and_then(Value::as_str) == Some("host") {
                return Ok(());
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "{}'s engine never became host: {code} {body}; {now}", n.name);
            std::thread::sleep(LOOK);
        }
    }

    /// The first runner's node counts three members.
    fn until_formed(&self) -> anyhow::Result<()> {
        let a = self.node(0)?;
        let began = Instant::now();
        loop {
            let members = slates_api::Instance::attach(&a.instance).and_then(|mut i| i.daemon_status()).map(|r| r.fleet.members.len());
            if let Ok(m) = members
                && m >= self.nodes.len()
            {
                eprintln!("PROVEN: formed: {}'s node counts {m} members ({:?})", a.name, began.elapsed());
                return Ok(());
            }
            anyhow::ensure!(began.elapsed() < WITHIN, "the fleet never formed: {members:?}\n{}\n{}", self.fleet.logs(), self.remote_logs());
            std::thread::sleep(LOOK);
        }
    }

    fn remote_logs(&self) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            if let At::There { runner, dir, .. } = &n.at {
                let tail = on(runner, &format!("tail -40 \"{dir}/runner.log\""), 60).unwrap_or_else(|e| format!("{e:#}"));
                out.push_str(&format!("--- {} on {runner}\n{tail}\n", n.name));
            }
        }
        out
    }

    fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.fleet.call("GET", path, None)
    }

    fn op(&self, name: &str, input: &Value) -> anyhow::Result<(u16, Value)> {
        let (code, body) = self.fleet.request("POST", &format!("/api/ops/{name}"), Some(input))?;
        Ok((code, serde_json::from_slice(&body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))))
    }

    fn session(&self, id: &str) -> anyhow::Result<Value> {
        self.get(&format!("/api/harness/sessions/{id}"))
    }

    fn lines(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let v = self.get(&format!("/api/harness/sessions/{id}/events?limit=1000"))?;
        let events = match v {
            Value::Array(a) => a,
            other => other.get("events").and_then(Value::as_array).cloned().unwrap_or_default(),
        };
        Ok(vm_e2e::said(&events))
    }

    /// kiln's notices of the session's that refuse a move by kind (`Couldn't move to ... [kind: <kind>]`), in order.
    fn refusals(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let v = self.get(&format!("/api/harness/sessions/{id}/events?limit=1000"))?;
        let events = match v {
            Value::Array(a) => a,
            other => other.get("events").and_then(Value::as_array).cloned().unwrap_or_default(),
        };
        Ok(events
            .iter()
            .filter(|e| e.get("kind").and_then(Value::as_str) == Some("notice"))
            .filter_map(|e| e.get("text").and_then(Value::as_str))
            .filter(|t| checks::refused_kind(t).is_some())
            .map(str::to_owned)
            .collect())
    }

    /// Waits until the stand-in said [`vm_e2e::STUB_DONE`] `turns` times: every line it said.
    fn until_turns(&self, id: &str, turns: usize, what: &str) -> anyhow::Result<Vec<String>> {
        let began = Instant::now();
        loop {
            let lines = self.lines(id)?;
            if lines.iter().filter(|l| l.contains(vm_e2e::STUB_DONE)).count() >= turns {
                return Ok(lines);
            }
            let session = self.session(id).unwrap_or(Value::Null);
            let status = session.get("status").and_then(Value::as_str).unwrap_or_default();
            anyhow::ensure!(!matches!(status, "exited" | "ended" | "failed"), "{what}: the session {status} first: {session}\n{lines:?}\n{}\n{}", self.fleet.logs(), self.remote_logs());
            anyhow::ensure!(began.elapsed() < WITHIN, "{what}: never ended: {lines:?}\n{}\n{}", self.fleet.logs(), self.remote_logs());
            std::thread::sleep(LOOK);
        }
    }

    /// The node the session runs on now, by kiln's word (`target`: `agent:<machine>`).
    fn target(&self, id: &str) -> anyhow::Result<String> {
        let s = self.session(id)?;
        Ok(s.get("target").and_then(Value::as_str).unwrap_or_default().trim_start_matches("agent:").to_owned())
    }

    /// The mount table of node `i`'s machine.
    fn mount_table(&self, i: usize) -> anyhow::Result<String> {
        match &self.node(i)?.at {
            At::Here => Ok(String::from_utf8_lossy(&Command::new("/sbin/mount").output()?.stdout).into_owned()),
            At::There { runner, .. } => on(runner, "/sbin/mount", 60),
        }
    }

    /// The folder node `i` keeps its state under, as its machine's mount table spells it.
    fn folder(&self, n: &Node) -> String {
        match &n.at {
            At::Here => std::fs::canonicalize(self.fleet.root.path()).unwrap_or_else(|_| self.fleet.root.path().to_owned()).display().to_string(),
            At::There { dir, .. } => dir.clone(),
        }
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if let Some(id) = self.agent.take() {
            let _ = self.op("agents.stop", &json!({"agent": id}));
            std::thread::sleep(Duration::from_secs(5));
        }
        for i in 0..self.nodes.len() {
            let Some(n) = self.nodes.get(i) else { continue };
            let (folder, here) = (self.folder(n), matches!(n.at, At::Here));
            let mut left = self.mount_table(i).map(|t| checks::mounted_under(&t, &folder)).unwrap_or_else(|e| vec![format!("(no mount table: {e:#})")]);
            if here && !left.is_empty() {
                for m in &left {
                    let _ = Command::new("/sbin/umount").arg(m).output();
                }
                left = self.mount_table(i).map(|t| checks::mounted_under(&t, &folder)).unwrap_or_else(|e| vec![format!("(no mount table: {e:#})")]);
            }
            let Some(n) = self.nodes.get_mut(i) else { continue };
            if !left.is_empty() {
                eprintln!("LEFT RUNNING: {} (instance {}): mounts still under {folder}: {left:?}", n.name, n.instance);
                continue;
            }
            // End its runner, then every process naming its instance, exactly (never by a pattern).
            let end = format!(
                r#"ps -A -ww -o pid= -o args= | awk -v i="{inst}" '{{ for (k = 2; k < NF; k++) if ($k == "--instance" && $(k+1) == i) print $1 }}' | while read p; do kill -TERM "$p"; done"#,
                inst = n.instance
            );
            match &mut n.at {
                At::Here => {
                    if let Some(Some(mut p)) = self.fleet.runners.insert(n.name.to_owned(), None) {
                        p.ask_to_stop();
                        std::thread::sleep(Duration::from_secs(2));
                        p.kill();
                    }
                    let _ = Command::new("/bin/sh").args(["-c", &end]).output();
                }
                At::There { runner, dir, run } => {
                    let script = format!(r#"d="{dir}"; [ -f "$d/runner.pid" ] && kill -TERM "$(cat "$d/runner.pid")"; sleep 3; {end}; sleep 3; rm -rf "$d""#);
                    if let Err(e) = on(runner, &script, 120) {
                        eprintln!("LEFT RUNNING: {} on {runner}: {e:#}", n.name);
                    }
                    if let Some(mut c) = run.take() {
                        let _ = c.kill();
                        let _ = c.wait();
                    }
                }
            }
        }
    }
}

/// A git project of a few committed files at `dir`: the folder the agent's workspace is captured from.
fn make_project(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("README.md"), "the project\n")?;
    std::fs::write(dir.join("gone.txt"), "to be removed\n")?;
    std::fs::write(dir.join("src").join("lib.rs"), "pub fn f() {}\n")?;
    for args in [vec!["init", "-q", "-b", "main"], vec!["add", "."], vec!["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "-qm", "start"]] {
        let out = Command::new("git").args(&args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// The listing of the moved volume's files under `mount` (`checks::listing`'s shape).
fn listing_script(mount: &str) -> String {
    format!(r#"cd "{mount}" && for f in README.md made.txt on-b.txt src/lib.rs gone.txt; do if [ -e "$f" ]; then echo "== $f"; cat "$f"; else echo "-- absent $f"; fi; done"#)
}

/// K4, K5: an agent with a live Slates volume, moved by kiln from one runner to another, carries on there with its
/// changes intact, and the old runner lets the volume go (the module's 1 to 6).
#[test]
fn k4_k5_an_agent_moved_with_its_live_volume_carries_on_and_the_old_runner_lets_it_go() -> anyhow::Result<()> {
    let Some(mode) = mode() else { return Ok(()) };
    let mut lab = Lab::start(&mode)?;
    let (name_a, a, instance_a) = {
        let n = lab.node(0)?;
        (n.name, n.machine.clone(), n.instance.clone())
    };
    let (name_b, b, b_here) = {
        let n = lab.node(1)?;
        (n.name, n.machine.clone(), matches!(n.at, At::Here))
    };
    let project = std::fs::canonicalize(&lab.project)?.display().to_string();

    // 2. Worked on the first runner.
    let spec = json!({"kind": HARNESS, "cwd": project, "prompt": FIRST, "workspace": {"kind": "volume"}});
    // A fleet node takes a workspace once it has imported its groups (Slates' `ConsensusNotInitialized`: "wait for it to
    // import the groups"), which can follow the fleet's forming by a while: its start asked again until then.
    let began = Instant::now();
    let mut starts = 0_u32;
    let (id, lines) = loop {
        starts += 1;
        let (code, body) = common::http_within(lab.fleet.http, "POST", "/api/harness/sessions", &lab.fleet.admin, Some(&json!({"target": format!("agent:{a}"), "spec": spec})), Duration::from_secs(120))?;
        anyhow::ensure!(code == 200, "the agent's start: {code} {body}\n{}", lab.fleet.logs());
        let id = serde_json::from_str::<Value>(&body)?.pointer("/0/id").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no session named: {body}"))?;
        lab.agent = Some(id.clone());
        match lab.until_turns(&id, 1, "its first turn") {
            Ok(lines) => break (id, lines),
            Err(e) if checks::refused_for_now(&format!("{e:#}")) && began.elapsed() < WITHIN => {
                let why = format!("{e:#}");
                let why = why.lines().next().unwrap_or_default().chars().take(300).collect::<String>();
                eprintln!("WAITED: start {starts} on {name_a} refused for now ({:?}): {why}", began.elapsed());
                std::thread::sleep(Duration::from_secs(5));
            }
            Err(e) => return Err(e),
        }
    };
    eprintln!("PROVEN: the agent started on {} at start {starts} ({:?} after the fleet formed)", name_a, began.elapsed());
    for want in ["WROTE made.txt", "APPENDED README.md", "REMOVED gone.txt"] {
        anyhow::ensure!(lines.iter().any(|l| l.contains(want)), "its first turn didn't say {want}: {lines:?}");
    }
    let info = lab.session(&id)?;
    let made = info.pointer("/workspace/made").cloned().unwrap_or(Value::Null);
    let volume = made.get("volume").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mount_a = made.get("mount").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!volume.is_empty() && !mount_a.is_empty(), "kiln names no volume and mount of the session's: {info}");
    let cwd = vm_e2e::cwd_said(&lines).unwrap_or_default();
    let real_mount_a = std::fs::canonicalize(&mount_a).map(|p| p.display().to_string()).unwrap_or_else(|_| mount_a.clone());
    anyhow::ensure!(cwd.starts_with(&real_mount_a) || cwd.starts_with(&mount_a), "its first turn worked in {cwd}, outside its volume's mount {mount_a}");
    eprintln!("PROVEN: worked: three edits in volume {volume} at {mount_a} on {}", name_a);

    // 3. Moved. A move Slates refuses `unplaced` (its records not yet placed on f + 1 other members within the fleet's
    // failover delay; the volume thawed where it was) says to ask again: asked again, up to MOVES times; any other kind
    // fails the test, naming it.
    let began = Instant::now();
    let mut asked = 0_usize;
    let mut last_ask: Option<Instant> = None;
    loop {
        if lab.target(&id)? == b {
            break;
        }
        let refusals = lab.refusals(&id)?;
        if let Some(last) = refusals.last() {
            anyhow::ensure!(checks::asked_again(last), "the move was refused by kind: {last}\n{}\n{}", lab.fleet.logs(), lab.remote_logs());
        }
        // kiln says a refusal once however often it's repeated: asked again each ASK_AGAIN while it hasn't moved.
        if last_ask.is_none_or(|t| t.elapsed() >= ASK_AGAIN) {
            anyhow::ensure!(asked < MOVES, "the move was asked {asked} times and never made, refused: {refusals:?}\n{}\n{}", lab.fleet.logs(), lab.remote_logs());
            if asked > 0 {
                eprintln!("WAITED: move {asked} not made (refused: {:?}); asking again ({:?})", refusals.last().and_then(|r| checks::refused_kind(r)), began.elapsed());
            }
            let (code, said) = lab.op("agents.move", &json!({"agent": id, "to": b}))?;
            anyhow::ensure!((200..300).contains(&code), "kiln refused the move: {code} {said}");
            asked += 1;
            last_ask = Some(Instant::now());
        }
        std::thread::sleep(LOOK);
    }
    eprintln!("PROVEN: moved: kiln says the agent works on {} at move {asked} ({:?})", name_b, began.elapsed());

    // 4. Carried on. kiln goes on with the moved agent on the second runner with a hand-off turn that carries its
    // conversation in words, which the stand-in hears and acts on again: its removal finds gone.txt gone (the first
    // runner's removal moved with the volume) and its append adds a second line. Then a turn of its own, asked there.
    let lines = lab.until_turns(&id, 2, "its hand-off turn after the move")?;
    let handoff = checks::turn(&lines, 1);
    for want in ["kiln_handoff", "REMOVE_FAIL", "APPENDED README.md"] {
        anyhow::ensure!(handoff.iter().any(|l| l.contains(want)), "its hand-off turn didn't say {want}: {handoff:?}");
    }
    let (code, said) = lab.op("agents.send", &json!({"agent": id, "text": SECOND}))?;
    anyhow::ensure!((200..300).contains(&code), "kiln refused the second turn's message: {code} {said}");
    let lines = lab.until_turns(&id, 3, "its own turn on the second runner")?;
    let second = checks::turn(&lines, 2);
    for want in ["WROTE on-b.txt", "APPENDED made.txt"] {
        anyhow::ensure!(second.iter().any(|l| l.contains(want)), "its turn on the second runner didn't say {want}: {second:?}");
    }
    let info = lab.session(&id)?;
    let mount_b = info.pointer("/workspace/made/mount").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!mount_b.is_empty(), "kiln names no mount of the moved volume: {info}");
    anyhow::ensure!(info.pointer("/workspace/made/volume").and_then(Value::as_str) == Some(volume.as_str()), "the moved session's volume isn't the one it moved with: {info}");
    let cwd_b = second.iter().rev().find_map(|l| l.find("CWD ").map(|i| l.get(i + 4..).unwrap_or_default().trim().to_owned())).unwrap_or_default();
    anyhow::ensure!(cwd_b.contains(mount_b.trim_start_matches("/private")), "its second turn worked in {cwd_b}, outside its volume's mount {mount_b} on {}", name_b);
    let listed = lab.run_on(1, &listing_script(&mount_b))?;
    let got = checks::listing(&listed);
    anyhow::ensure!(got == checks::expected(), "the moved volume on {} holds {got:?}, not {:?}", name_b, checks::expected());
    eprintln!("PROVEN: carried on: its second turn on {} appended to the file its first wrote; the mount {mount_b} holds both turns' work byte for byte", name_b);

    // 5. The old runner let go.
    let table = lab.mount_table(0)?;
    let at_a = checks::mounted_under(&table, &real_mount_a);
    anyhow::ensure!(at_a.is_empty(), "the old runner still mounts the volume: {at_a:?}");
    let vid = slates_api::VolumeId { bytes: u128::from_str_radix(volume.trim(), 16)?.to_be_bytes() };
    let mut old = slates_api::Instance::attach(&instance_a).map_err(|e| anyhow::anyhow!("the old runner's node: {e:?}"))?;
    match old.attach_volume(vid, slates_api::Intent::Write) {
        Ok(a) => anyhow::bail!("the old runner's node took a write attachment of the moved volume: {a:?}"),
        Err(e) => {
            anyhow::ensure!(!matches!(e, slates_api::Error::Unavailable { .. }), "the old runner's node is gone, not refusing: {e:?}");
            eprintln!("PROVEN: the old runner refuses a write attachment of the moved volume by kind: {e:?}");
        }
    }
    match old.read(vid, "/made.txt", slates_api::ReadAt::Head) {
        Ok(bytes) => anyhow::ensure!(bytes == b"made.txt\nAPPENDED made.txt\n", "a read through the old runner's node is stale: {:?}", String::from_utf8_lossy(&bytes)),
        Err(e) => eprintln!("PROVEN: a read through the old runner's node is refused by kind: {e:?}"),
    }
    eprintln!("PROVEN: the old runner let go: no mount of the volume on {}, and its node serves nothing stale", name_a);

    // 6. The end.
    let (code, said) = lab.op("agents.stop", &json!({"agent": id}))?;
    anyhow::ensure!((200..300).contains(&code), "kiln refused the stop: {code} {said}");
    lab.agent = None;
    let mount_b = if b_here { std::fs::canonicalize(&mount_b).map(|p| p.display().to_string()).unwrap_or(mount_b) } else { mount_b };
    let began = Instant::now();
    loop {
        let left = checks::mounted_under(&lab.mount_table(1)?, &mount_b);
        if left.is_empty() {
            break;
        }
        anyhow::ensure!(began.elapsed() < WITHIN, "the session ended, and its mount stays on {}: {left:?}", name_b);
        std::thread::sleep(LOOK);
    }
    eprintln!("PROVEN: the end: ended for good, its mount on {} went", name_b);
    Ok(())
}
