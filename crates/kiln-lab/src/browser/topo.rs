//! The lab's namespaces, made without root (plan 01a1a832e9cc step 1; research f15). `kiln-lab
//! browser` runs itself again inside a new user namespace (its uid mapped to root there, and only
//! there), with network, mount and PID namespaces of its own: the network namespace it gets is the
//! link's, and when it ends, the PID namespace takes every process it started with it, so nothing
//! outlives a lab. Inside, it holds the server's and each client's network namespace with a
//! sleeping process of its own (its PID recorded), joins them with veth pairs and shapes them with
//! the commands `link` renders.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::link::{self, Cmd, Condition, Ns};

/// Set inside the lab's namespaces, so the lab knows it is in them.
pub const INSIDE: &str = "KILN_LAB_INSIDE";
/// The uid the lab was started by, outside its namespaces.
pub const OUTER_UID: &str = "KILN_LAB_OUTER_UID";

/// The tc the lab uses: `KILN_LAB_TC` if set (a machine whose tc isn't installed can point at one
/// unpacked under ~/.cache, its distribution tables in `TC_LIB_DIR`), else `tc` on the PATH.
pub fn tc() -> String {
    std::env::var("KILN_LAB_TC").unwrap_or_else(|_| "tc".to_owned())
}

/// Whether `prog` runs (`prog args` exits 0).
pub fn runs(prog: &str, args: &[&str]) -> bool {
    Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The uid of this process (its `/proc/self/status`).
pub fn uid() -> Option<u32> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()
}

/// Runs this lab command again inside its namespaces, and waits for it: what it came to.
pub fn reenter(args: &[String]) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let status = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--mount", "--pid", "--fork", "--mount-proc", "--kill-child", "--"])
        .arg(&exe)
        .args(args)
        .env(INSIDE, "1")
        .env(OUTER_UID, uid().map(|u| u.to_string()).unwrap_or_default())
        .status()
        .map_err(|e| anyhow::anyhow!("unshare didn't start ({e}): the lab needs util-linux's unshare"))?;
    anyhow::ensure!(status.success(), "the lab in its namespaces ended {status}");
    Ok(())
}

pub fn inside() -> bool {
    std::env::var(INSIDE).as_deref() == Ok("1")
}

/// A network namespace held by a sleeping process.
struct Holder {
    child: Child,
}

impl Holder {
    fn new() -> anyhow::Result<Self> {
        let child = Command::new("unshare").args(["--net", "--", "sleep", "infinity"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        let pid = child.id();
        let mine = std::fs::read_link("/proc/self/ns/net")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        // Until it has its own namespace, joining it would join ours.
        loop {
            let theirs = std::fs::read_link(format!("/proc/{pid}/ns/net")).ok();
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            if theirs.is_some_and(|t| t != mine) && comm.trim() == "sleep" {
                return Ok(Self { child });
            }
            anyhow::ensure!(Instant::now() < deadline, "a namespace's holder never came up (unshare --net)");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A client link's interface counters, on the client's own end (`eth0` in its namespace): what it
/// received after the downlink's losses and what it sent before the uplink's, frames with their
/// Ethernet headers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Counters {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
}

impl Counters {
    pub fn since(self, before: Counters) -> Counters {
        Counters {
            rx_bytes: self.rx_bytes.saturating_sub(before.rx_bytes),
            rx_packets: self.rx_packets.saturating_sub(before.rx_packets),
            tx_bytes: self.tx_bytes.saturating_sub(before.tx_bytes),
            tx_packets: self.tx_packets.saturating_sub(before.tx_packets),
        }
    }
}

/// `dev`'s counters in a `/proc/<pid>/net/dev`.
pub fn parse_dev(text: &str, dev: &str) -> Option<Counters> {
    let line = text.lines().find(|l| l.trim_start().starts_with(&format!("{dev}:")))?;
    let (_, rest) = line.split_once(':')?;
    let v: Vec<u64> = rest.split_whitespace().filter_map(|w| w.parse().ok()).collect();
    Some(Counters { rx_bytes: *v.first()?, rx_packets: *v.get(1)?, tx_bytes: *v.get(8)?, tx_packets: *v.get(9)? })
}

/// The lab's network: the link (this namespace), the server's and the clients'.
pub struct Lab {
    server: Holder,
    clients: Vec<Holder>,
    pub ethtool: bool,
    /// Every command it ran, in order (a round's record says how its network was made).
    pub log: Vec<String>,
}

impl Lab {
    /// The network for `k` clients.
    pub fn up(k: usize) -> anyhow::Result<Lab> {
        anyhow::ensure!(inside(), "the lab's network is made inside its namespaces");
        let server = Holder::new()?;
        let clients = (0..k).map(|_| Holder::new()).collect::<anyhow::Result<Vec<_>>>()?;
        let ethtool = runs("ethtool", &["--version"]);
        let mut lab = Lab { server, clients, ethtool, log: Vec::new() };
        let pids: Vec<u32> = lab.clients.iter().map(Holder::pid).collect();
        for c in link::topology(lab.server.pid(), &pids, ethtool) {
            lab.exec(&c)?;
        }
        Ok(lab)
    }

    fn pid(&self, ns: Ns) -> Option<u32> {
        match ns {
            Ns::Link => None,
            Ns::Server => Some(self.server.pid()),
            Ns::Client(i) => self.clients.get(i).map(Holder::pid),
        }
    }

    /// `argv` as a command in namespace `ns`.
    pub fn command(&self, ns: Ns, argv: &[String]) -> anyhow::Result<Command> {
        let (prog, args) = argv.split_first().ok_or_else(|| anyhow::anyhow!("an empty command"))?;
        let prog = if prog == "tc" { tc() } else { prog.clone() };
        let mut c = match self.pid(ns) {
            None if ns == Ns::Link => Command::new(prog),
            None => anyhow::bail!("no namespace {ns:?}"),
            Some(pid) => {
                let mut c = Command::new("nsenter");
                c.args(["--target", &pid.to_string(), "--net", "--"]).arg(prog);
                c
            }
        };
        c.args(args);
        Ok(c)
    }

    /// Runs one of `link`'s commands, failing with what it said unless it may fail.
    pub fn exec(&mut self, c: &Cmd) -> anyhow::Result<()> {
        let out: Output = self.command(c.ns, &c.argv)?.stdin(Stdio::null()).output()?;
        self.log.push(format!("[{:?}] {}{}", c.ns, c.argv.join(" "), if out.status.success() { "" } else { "  (failed)" }));
        anyhow::ensure!(c.may_fail || out.status.success(), "[{:?}] `{}` failed: {}", c.ns, c.argv.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        Ok(())
    }

    /// Puts condition `c` on client `i`'s link.
    pub fn shape(&mut self, i: usize, c: &Condition) -> anyhow::Result<()> {
        for cmd in link::shape(i, c) {
            self.exec(&cmd)?;
        }
        Ok(())
    }

    /// Drops UDP on client `i`'s link from now on (a profile's `DroppedAfter`), unlogged.
    pub fn block_udp(&self, i: usize) -> anyhow::Result<()> {
        for c in link::udp_rules(i, true) {
            let out = self.command(c.ns, &c.argv)?.stdin(Stdio::null()).output()?;
            anyhow::ensure!(c.may_fail || out.status.success(), "`{}` failed: {}", c.argv.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    /// Client `i`'s link counters, now.
    pub fn counters(&self, i: usize) -> anyhow::Result<Counters> {
        let pid = self.pid(Ns::Client(i)).ok_or_else(|| anyhow::anyhow!("no client {i}"))?;
        let text = std::fs::read_to_string(format!("/proc/{pid}/net/dev"))?;
        parse_dev(&text, "eth0").ok_or_else(|| anyhow::anyhow!("client {i} has no eth0"))
    }

    /// What `tc -s qdisc` says of client `i`'s two directions (sent, dropped), as text.
    pub fn qdisc_stats(&self, i: usize) -> String {
        let mut out = String::new();
        for dev in [format!("c{i}"), format!("ifb{i}")] {
            if let Ok(o) = Command::new(tc()).args(["-s", "qdisc", "show", "dev", &dev]).output() {
                out.push_str(&String::from_utf8_lossy(&o.stdout));
            }
        }
        out
    }
}

/// cgroup v2 scopes of the lab's own, under the cgroup it was started in: each server's CPU is
/// read from its scope's `cpu.stat`, which counts every process in it whether or not the cpu
/// controller is on (f37).
pub struct Scopes {
    pub root: PathBuf,
}

impl Scopes {
    /// The lab's cgroup, if it may make scopes under it.
    pub fn mine() -> anyhow::Result<Scopes> {
        let text = std::fs::read_to_string("/proc/self/cgroup")?;
        let rel = text.lines().find_map(|l| l.strip_prefix("0::")).ok_or_else(|| anyhow::anyhow!("no cgroup v2 (/proc/self/cgroup has no 0:: line)"))?;
        let root = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
        let probe = root.join("kiln-lab-probe");
        std::fs::create_dir_all(&probe).map_err(|e| anyhow::anyhow!("can't make a cgroup under {} ({e})", root.display()))?;
        let _ = std::fs::remove_dir(&probe);
        Ok(Scopes { root })
    }

    /// Scope `name` (made if new): its directory.
    pub fn scope(&self, name: &str) -> anyhow::Result<PathBuf> {
        let d = self.root.join(name);
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }

    /// `cmd` started in scope `dir`: a shell moves itself in first, then becomes the command, so
    /// everything it starts is counted there from its first instruction.
    pub fn wrap(dir: &Path, argv: &[String]) -> Vec<String> {
        let mut out = vec!["sh".to_owned(), "-c".to_owned(), "echo $$ > \"$0\" && shift && exec \"$@\"".to_owned(), dir.join("cgroup.procs").to_string_lossy().into_owned(), "-".to_owned()];
        out.extend(argv.iter().cloned());
        out
    }

    /// A scope's CPU so far, in microseconds (`usage_usec`).
    pub fn cpu_us(dir: &Path) -> anyhow::Result<u64> {
        let text = std::fs::read_to_string(dir.join("cpu.stat"))?;
        parse_cpu_stat(&text).get("usage_usec").copied().ok_or_else(|| anyhow::anyhow!("no usage_usec in {}", dir.display()))
    }
}

pub fn parse_cpu_stat(text: &str) -> BTreeMap<String, u64> {
    text.lines().filter_map(|l| l.split_once(' ')).filter_map(|(k, v)| Some((k.to_owned(), v.trim().parse().ok()?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_s4_counters_come_from_net_dev() {
        let text = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n  eth0:  123456     100    0    0    0     0          0         0    7890      55    0    0    0     0       0          0\n";
        assert_eq!(parse_dev(text, "eth0"), Some(Counters { rx_bytes: 123_456, rx_packets: 100, tx_bytes: 7890, tx_packets: 55 }));
        assert_eq!(parse_dev(text, "ctl"), None);
        let c = Counters { rx_bytes: 10, rx_packets: 1, tx_bytes: 5, tx_packets: 1 };
        assert_eq!(c.since(Counters::default()), c);
    }

    #[test]
    fn g1_s4_cpu_stat_parses() {
        let m = parse_cpu_stat("usage_usec 1234\nuser_usec 1000\nsystem_usec 234\nnr_periods 0\n");
        assert_eq!(m.get("usage_usec"), Some(&1234));
        assert_eq!(m.get("system_usec"), Some(&234));
    }

    #[test]
    fn g1_s4_a_scope_wraps_its_command() {
        let w = Scopes::wrap(Path::new("/sys/fs/cgroup/x/srv"), &["kiln".to_owned(), "server".to_owned()]);
        assert_eq!(w.get(3).map(String::as_str), Some("/sys/fs/cgroup/x/srv/cgroup.procs"));
        assert_eq!(w.get(5..).map(<[String]>::to_vec), Some(vec!["kiln".to_owned(), "server".to_owned()]));
    }
}
