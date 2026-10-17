//! Tailscale, as kiln uses it.
//!
//! * **This machine** ([`Local`]): its tailnet addresses, MagicDNS name and tailnet, whether it
//!   takes the tailnet's DNS settings, and the split DNS routes the tailnet gives it, from the
//!   `tailscale` command (`status --json`, `dns status --json`; neither needs root).
//! * **The tailnet** ([`api`]): the Tailscale API, with which the server points the tailnet's
//!   split DNS for a front door's names at the runner that serves them (`crate::frontdoor::tailnet`).

pub mod api;
pub mod split;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a `tailscale` command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Most routes and health lines kept from what Tailscale says.
const MAX_ROUTES: usize = 256;
const MAX_HEALTH: usize = 16;

/// The `tailscale` command, where it usually is.
pub fn command() -> Option<PathBuf> {
    if let Some(p) = crate::os::which("tailscale") {
        return Some(p);
    }
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale"]
    } else if cfg!(windows) {
        &[r"C:\Program Files\Tailscale\tailscale.exe"]
    } else {
        &["/usr/bin/tailscale", "/usr/sbin/tailscale"]
    };
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// A resolver the tailnet sends a domain's queries to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Resolver {
    /// An address (`100.101.102.103`, `[fd7a::1]:53`) or a DNS-over-HTTPS URL.
    pub addr: String,
    /// Still used while a device uses an exit node (otherwise the exit node answers instead).
    pub exit_node: bool,
}

impl Resolver {
    /// The resolver's IP address, when it is one (with or without a port).
    pub fn ip(&self) -> Option<IpAddr> {
        let a = self.addr.trim();
        a.parse::<IpAddr>().ok().or_else(|| a.parse::<std::net::SocketAddr>().ok().map(|s| s.ip()))
    }
}

/// What this machine's Tailscale says about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Local {
    /// `Running`, `Stopped`, `NeedsLogin`, `NeedsMachineAuth`, `Starting`, `NoState`.
    pub backend: String,
    pub version: String,
    /// This machine's tailnet addresses, IPv4 first.
    pub ips: Vec<IpAddr>,
    /// Its MagicDNS name, without the final dot (`box.tail1234.ts.net`).
    pub name: String,
    /// The tailnet, as Tailscale names it.
    pub tailnet: String,
    pub magic_dns: bool,
    /// The addresses are on a kernel interface (`tailscale0`, `utun…`). Not with userspace
    /// networking, where no program of the machine can listen on them.
    pub tun: bool,
    /// This machine takes the tailnet's DNS settings (`--accept-dns`); `None`: unknown.
    pub accept_dns: Option<bool>,
    /// The tailnet's split DNS as this machine got it: domain (lowercase, no final dot) →
    /// resolvers. `None`: this Tailscale cannot say (older than `tailscale dns status --json`).
    pub routes: Option<BTreeMap<String, Vec<Resolver>>>,
    /// Tailscale's own warnings.
    pub health: Vec<String>,
}

impl Local {
    pub fn running(&self) -> bool {
        self.backend == "Running"
    }

    /// The address the tailnet reaches this machine at first: its IPv4 one, else its first.
    pub fn primary(&self) -> Option<IpAddr> {
        self.ips.iter().find(|a| a.is_ipv4()).or_else(|| self.ips.first()).copied()
    }

    /// The route that decides where the tailnet sends `name`'s queries: that of the longest
    /// domain `name` is or is under. `None` when no route covers it, or the routes are unknown.
    pub fn route_for(&self, name: &str) -> Option<(&str, &[Resolver])> {
        let name = normalize(name);
        let routes = self.routes.as_ref()?;
        routes
            .iter()
            .filter(|(d, _)| name == **d || name.ends_with(&format!(".{d}")))
            .max_by_key(|(d, _)| d.len())
            .map(|(d, r)| (d.as_str(), r.as_slice()))
    }
}

/// A domain as Tailscale's routes are keyed here: lowercase, no final dot.
pub fn normalize(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn str_of<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or_default()
}

/// What `tailscale status --json` says about this machine.
pub fn parse_status(v: &serde_json::Value) -> Local {
    let me = v.get("Self").unwrap_or(&serde_json::Value::Null);
    let mut ips: Vec<IpAddr> = Vec::new();
    let listed = me.get("TailscaleIPs").or_else(|| v.get("TailscaleIPs")).and_then(serde_json::Value::as_array);
    for ip in listed.into_iter().flatten().filter_map(|x| x.as_str()?.parse::<IpAddr>().ok()) {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    ips.sort_by_key(|a| !a.is_ipv4());
    let tailnet = v.get("CurrentTailnet").unwrap_or(&serde_json::Value::Null);
    Local {
        backend: str_of(v, "BackendState").to_owned(),
        version: str_of(v, "Version").split('-').next().unwrap_or_default().to_owned(),
        ips,
        name: normalize(str_of(me, "DNSName")),
        tailnet: str_of(tailnet, "Name").to_owned(),
        magic_dns: tailnet.get("MagicDNSEnabled").and_then(serde_json::Value::as_bool).unwrap_or(false),
        // Older versions say nothing: they had no userspace mode on the desktop platforms.
        tun: v.get("TUN").and_then(serde_json::Value::as_bool).unwrap_or(true),
        accept_dns: None,
        routes: None,
        health: v
            .get("Health")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).take(MAX_HEALTH).collect())
            .unwrap_or_default(),
    }
}

/// Add what `tailscale dns status --json` says: whether this machine takes the tailnet's DNS,
/// and the split DNS routes it was given.
pub fn parse_dns_status(v: &serde_json::Value, local: &mut Local) {
    local.accept_dns = v.get("TailscaleDNS").and_then(serde_json::Value::as_bool);
    let mut routes = BTreeMap::new();
    if let Some(m) = v.get("SplitDNSRoutes").and_then(serde_json::Value::as_object) {
        for (domain, resolvers) in m.iter().take(MAX_ROUTES) {
            let list = resolvers
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| {
                            let addr = r.get("Addr")?.as_str()?.to_owned();
                            Some(Resolver { addr, exit_node: r.get("UseWithExitNode").and_then(serde_json::Value::as_bool).unwrap_or(false) })
                        })
                        .collect()
                })
                .unwrap_or_default();
            routes.insert(normalize(domain), list);
        }
    }
    local.routes = Some(routes);
    if local.name.is_empty()
        && let Some(n) = v.get("CurrentTailnet").and_then(|t| t.get("SelfDNSName")).and_then(serde_json::Value::as_str)
    {
        local.name = normalize(n);
    }
}

/// Run `tailscale <args>`: its standard output, or why not.
async fn run(exe: &std::path::Path, args: &'static [&'static str]) -> Result<Vec<u8>, String> {
    let exe = exe.to_path_buf();
    let job = crate::rt::blocking(move || std::process::Command::new(exe).args(args).stdin(std::process::Stdio::null()).output());
    let output = crate::rt::timeout(COMMAND_TIMEOUT, job)
        .await
        .map_err(|_| format!("`tailscale {}` did not answer", args.join(" ")))?
        .map_err(|e| format!("{e:#}"))?
        .map_err(|e| format!("cannot run tailscale: {e}"))?;
    if !output.status.success() {
        let why = String::from_utf8_lossy(&output.stderr).lines().find(|l| !l.trim().is_empty()).unwrap_or("it failed").trim().to_owned();
        return Err(format!("`tailscale {}`: {why}", args.join(" ")));
    }
    Ok(output.stdout)
}

/// Ask this machine's Tailscale about itself. An error means there is nothing to ask (not
/// installed, the daemon not running); a Tailscale that is logged out or stopped is an answer
/// ([`Local::running`] says).
pub async fn local() -> Result<Local, String> {
    let exe = command().ok_or_else(|| "Tailscale is not installed on this machine".to_owned())?;
    let out = run(&exe, &["status", "--json"]).await?;
    let v: serde_json::Value = serde_json::from_slice(&out).map_err(|e| format!("`tailscale status --json`: {e}"))?;
    let mut local = parse_status(&v);
    if local.running() {
        // Since Tailscale 1.80. An older one simply cannot say what the tailnet's DNS is.
        if let Ok(out) = run(&exe, &["dns", "status", "--json"]).await
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out)
        {
            parse_dns_status(&v, &mut local);
        }
    }
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tailscale status --json` of a Linux machine (1.104), cut to what kiln reads.
    const STATUS: &str = r#"{
      "Version": "1.104.1-t7f4efe814-g0123",
      "TUN": true,
      "BackendState": "Running",
      "TailscaleIPs": ["100.107.111.49", "fd7a:115c:a1e0::4638:6f32"],
      "Self": {
        "HostName": "box",
        "DNSName": "box.tail4b5a4f.ts.net.",
        "TailscaleIPs": ["fd7a:115c:a1e0::4638:6f32", "100.107.111.49"],
        "OS": "linux",
        "Online": true
      },
      "Health": ["systemd-resolved and NetworkManager are wired together incorrectly; MagicDNS will probably not work."],
      "MagicDNSSuffix": "tail4b5a4f.ts.net",
      "CurrentTailnet": { "Name": "someone@example.com", "MagicDNSSuffix": "tail4b5a4f.ts.net", "MagicDNSEnabled": true },
      "Peer": {}
    }"#;

    /// `tailscale dns status --json` (1.104), with a route kiln set.
    const DNS: &str = r#"{
      "TailscaleDNS": true,
      "CurrentTailnet": { "MagicDNSEnabled": true, "MagicDNSSuffix": "tail4b5a4f.ts.net", "SelfDNSName": "box.tail4b5a4f.ts.net." },
      "SplitDNSRoutes": {
        "ts.net.": [ { "Addr": "199.247.155.53" }, { "Addr": "2620:111:8007::53" } ],
        "Kiln.Dev.": [ { "Addr": "100.107.111.49", "UseWithExitNode": true } ]
      },
      "SearchDomains": ["tail4b5a4f.ts.net"]
    }"#;

    #[test]
    fn status_and_dns_status_are_read() -> anyhow::Result<()> {
        let mut l = parse_status(&serde_json::from_str(STATUS)?);
        assert!(l.running() && l.tun && l.magic_dns);
        assert_eq!(l.version, "1.104.1");
        assert_eq!(l.name, "box.tail4b5a4f.ts.net");
        assert_eq!(l.tailnet, "someone@example.com");
        assert_eq!(l.ips.first().map(IpAddr::to_string).as_deref(), Some("100.107.111.49"), "IPv4 first");
        assert_eq!(l.ips.len(), 2);
        assert_eq!(l.primary().map(|a| a.to_string()).as_deref(), Some("100.107.111.49"));
        assert_eq!(l.health.len(), 1);
        assert_eq!((l.accept_dns, l.routes.as_ref()), (None, None), "unknown until dns status is read");
        assert!(l.route_for("kiln.dev").is_none());

        parse_dns_status(&serde_json::from_str(DNS)?, &mut l);
        assert_eq!(l.accept_dns, Some(true));
        let (domain, to) = l.route_for("KILN.dev.").ok_or_else(|| anyhow::anyhow!("a route for kiln.dev"))?;
        assert_eq!(domain, "kiln.dev");
        assert_eq!(to, [Resolver { addr: "100.107.111.49".into(), exit_node: true }]);
        // Names under a routed domain go the same way; the longest domain wins.
        assert_eq!(l.route_for("ci.kiln.dev").map(|(d, _)| d), Some("kiln.dev"));
        assert_eq!(l.route_for("box.tail4b5a4f.ts.net").map(|(d, _)| d), Some("ts.net"));
        assert!(l.route_for("dev").is_none() && l.route_for("notkiln.dev").is_none());
        assert_eq!(to.first().and_then(Resolver::ip).map(|a| a.to_string()).as_deref(), Some("100.107.111.49"));
        assert_eq!(Resolver { addr: "[fd7a::1]:53".into(), exit_node: false }.ip().map(|a| a.to_string()).as_deref(), Some("fd7a::1"));
        assert_eq!(Resolver { addr: "https://dns.example/q".into(), exit_node: false }.ip(), None);
        Ok(())
    }

    #[test]
    fn a_stopped_or_odd_tailscale_is_still_an_answer() -> anyhow::Result<()> {
        let l = parse_status(&serde_json::from_str(r#"{"BackendState":"NeedsLogin","Self":{"DNSName":""}}"#)?);
        assert!(!l.running());
        assert!(l.ips.is_empty() && l.primary().is_none());
        assert!(l.tun, "an older Tailscale that says nothing about TUN had one");
        let l = parse_status(&serde_json::from_str(r#"{"BackendState":"Running","TUN":false,"Self":{"TailscaleIPs":["100.64.0.9","bogus"]}}"#)?);
        assert!(!l.tun);
        assert_eq!(l.ips.len(), 1);
        // Junk in the DNS status is skipped, not an error.
        let mut l = Local::default();
        parse_dns_status(&serde_json::from_str(r#"{"SplitDNSRoutes":{"a.b.":"x","c.d":[{"Addr":7},{"Addr":"1.2.3.4"}]}}"#)?, &mut l);
        let routes = l.routes.clone().unwrap_or_default();
        assert_eq!(routes.get("a.b").map(Vec::len), Some(0));
        assert_eq!(routes.get("c.d").map(Vec::len), Some(1));
        Ok(())
    }
}
