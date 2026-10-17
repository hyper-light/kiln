//! How each discovery method is doing, and what to do about what is wrong: the `methods` and
//! `problems` of `GET /api/discovery`. The methods record their state in [`Health`] as they run;
//! [`super::Discovery::methods`] and [`super::Discovery::problems`] read it on request. States are
//! honest: a method that cannot work here says why, one that has nothing to do is `off`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::{Discovery, Via};

/// After this long without a single multicast DNS packet from another host while browsing, the
/// network is suspect (a firewall, or Wi-Fi client isolation).
pub const SILENT_AFTER: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MdnsState {
    #[default]
    Starting,
    Running,
    /// UDP 5353 could not be bound (or shared).
    PortBusy(String),
    /// The engine stopped with an error (it is restarted).
    Failed(String),
    /// Not used, and why.
    Off(String),
}

#[derive(Clone, Debug, Default)]
pub struct MdnsHealth {
    pub state: MdnsState,
    /// The interfaces it joined.
    pub interfaces: Vec<String>,
    pub since: Option<Instant>,
    /// When a packet from another host last arrived.
    pub last_foreign: Option<Instant>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NativeState {
    /// This OS has no service discovery kiln uses (Linux).
    #[default]
    Unsupported,
    /// Turned off (`[discovery] native = false`).
    Off,
    Starting,
    Running,
    /// Not there (old Windows, mDNSResponder not running); kiln's own multicast DNS runs instead.
    Unavailable(String),
}

#[derive(Clone, Debug, Default)]
pub struct NativeHealth {
    pub state: NativeState,
    /// The name the registration got.
    pub registered: Option<String>,
    /// The last error of a call that is retried (registration, browse).
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DnsSdHealth {
    /// Whether a browse has run yet.
    pub ran: bool,
    pub domains: Vec<String>,
    pub servers: Vec<String>,
    /// Why the last browse found nothing it could use (every query failed).
    pub error: Option<String>,
    pub at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TailscaleState {
    #[default]
    Off,
    Checking,
    Unavailable(String),
    Active { peers: usize },
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct SeedsHealth {
    pub count: usize,
    /// Seeds whose names do not resolve.
    pub unresolved: Vec<String>,
}

/// `dns_sd.publish` of `GET /api/discovery`.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct PublishStatus {
    pub configured: bool,
    pub zone: Option<String>,
    /// `null` until the first update was answered.
    pub ok: Option<bool>,
    pub error: Option<String>,
    /// When the last update was answered (or failed).
    pub at: Option<i64>,
}

/// What every method last reported.
#[derive(Clone, Debug)]
pub struct Health {
    pub mdns: MdnsHealth,
    pub native: NativeHealth,
    pub dns_sd: DnsSdHealth,
    pub tailscale: TailscaleState,
    pub seeds: SeedsHealth,
    pub publish: PublishStatus,
    /// When an instance was last found each way (unix seconds).
    pub heard: HashMap<Via, i64>,
    /// `[discovery] answer_from` entries that mean nothing.
    pub bad_answer_from: Vec<String>,
}

impl Default for Health {
    fn default() -> Self {
        Self::new(&crate::config::Discovery::default())
    }
}

impl Health {
    pub fn new(settings: &crate::config::Discovery) -> Self {
        let native = if !super::native::SUPPORTED {
            NativeState::Unsupported
        } else if settings.native {
            NativeState::Starting
        } else {
            NativeState::Off
        };
        Self {
            mdns: MdnsHealth::default(),
            native: NativeHealth { state: native, registered: None, error: None },
            dns_sd: DnsSdHealth::default(),
            tailscale: if settings.tailscale { TailscaleState::Checking } else { TailscaleState::Off },
            seeds: SeedsHealth { count: settings.seeds.len(), unresolved: Vec::new() },
            publish: PublishStatus {
                configured: settings.publish.is_some(),
                zone: settings.publish.as_ref().map(|p| p.zone.trim_end_matches('.').to_owned()),
                ..PublishStatus::default()
            },
            heard: HashMap::new(),
            bad_answer_from: super::AnswerFrom::parse(&settings.answer_from).1,
        }
    }

    /// An instance was just found `via`.
    pub fn heard(&mut self, via: Via) {
        self.heard.insert(via, crate::util::now_unix());
    }
}

/// `methods[]` of `GET /api/discovery`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Method {
    pub id: &'static str,
    pub label: String,
    pub state: State,
    pub detail: String,
    /// Instances currently known through it.
    pub heard: usize,
    pub last_heard: Option<i64>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Active,
    Off,
    Unavailable,
    Error,
}

/// `problems[]` of `GET /api/discovery`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Problem {
    pub id: &'static str,
    pub text: String,
    pub fixes: Vec<Fix>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Fix {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

fn fix(os: Option<&'static str>, text: Option<&str>, command: Option<&str>) -> Fix {
    Fix { os, text: text.map(str::to_owned), command: command.map(str::to_owned) }
}

/// This machine's OS as fixes name it.
fn this_os() -> Option<&'static str> {
    match std::env::consts::OS {
        "linux" => Some("linux"),
        "macos" => Some("macos"),
        "windows" => Some("windows"),
        _ => None,
    }
}

/// The macOS firewall command that lets kiln receive multicast DNS.
fn macos_firewall_command() -> String {
    let exe = if cfg!(target_os = "macos") {
        std::env::current_exe().map(|p| format!("\"{}\"", p.display())).unwrap_or_else(|_| "\"$(command -v kiln)\"".to_owned())
    } else {
        "\"$(command -v kiln)\"".to_owned()
    };
    format!("sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add {exe} && sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp {exe}")
}

const WINDOWS_FIREWALL: &str = "New-NetFirewallRule -DisplayName kiln-mdns -Direction Inbound -Protocol UDP -LocalPort 5353 -Action Allow";

/// Opening UDP 5353 on every OS: the silent machine may be any of them.
fn firewall_fixes(subnet: &str) -> Vec<Fix> {
    vec![
        fix(Some("linux"), Some("ufw"), Some(&format!("sudo ufw allow from {subnet} to any port 5353 proto udp"))),
        fix(Some("linux"), Some("firewalld"), Some("sudo firewall-cmd --permanent --add-service=mdns && sudo firewall-cmd --reload")),
        fix(Some("macos"), Some("Let kiln through the Application Firewall"), Some(&macos_firewall_command())),
        fix(Some("windows"), Some("Allow multicast DNS in (PowerShell as administrator)"), Some(WINDOWS_FIREWALL)),
    ]
}

/// "a, b, c" from at most `n` items, then "+k".
fn list(items: &[String], n: usize) -> String {
    let shown: Vec<&str> = items.iter().take(n).map(String::as_str).collect();
    match items.len().saturating_sub(n) {
        0 => shown.join(", "),
        more => format!("{}, +{more}", shown.join(", ")),
    }
}

impl Discovery {
    /// How each way of finding instances is doing (`methods[]`).
    pub fn methods(&self) -> Vec<Method> {
        let h = self.health.lock().clone();
        let table = self.table.snapshot();
        let count = |via: &[Via]| table.iter().filter(|i| i.via.iter().any(|v| via.contains(v))).count();
        let last = |via: &[Via]| via.iter().filter_map(|v| h.heard.get(v).copied()).max();
        let mut out = Vec::new();
        if !self.announcing() && !self.browsing() {
            let why = if self.settings.enabled { "announcing and browsing are turned off" } else { "discovery is turned off ([discovery] enabled = false)" };
            for (id, label) in [("mdns", "Multicast DNS".to_owned()), ("dns-sd", "DNS-SD".to_owned()), ("scan", "Network scan".to_owned()), ("tailscale", "Tailscale".to_owned()), ("seeds", "Seed addresses".to_owned())] {
                out.push(Method { id, label, state: State::Off, detail: why.to_owned(), heard: 0, last_heard: None });
            }
            return out;
        }

        // Multicast DNS.
        let (state, detail) = match &h.mdns.state {
            MdnsState::Off(why) => (State::Off, why.clone()),
            MdnsState::Starting => (State::Active, "starting".to_owned()),
            MdnsState::Running if h.mdns.interfaces.is_empty() => (State::Unavailable, "no multicast-capable network interface".to_owned()),
            MdnsState::Running => (State::Active, format!("UDP 5353 on {}", list(&h.mdns.interfaces, 4))),
            MdnsState::PortBusy(e) => (State::Unavailable, format!("cannot use UDP 5353: {e}")),
            MdnsState::Failed(e) => (State::Error, e.clone()),
        };
        out.push(Method { id: "mdns", label: "Multicast DNS".to_owned(), state, detail, heard: count(&[Via::Mdns]), last_heard: last(&[Via::Mdns]) });

        // The system's service discovery.
        if h.native.state != NativeState::Unsupported {
            let (state, detail) = match &h.native.state {
                NativeState::Off => (State::Off, "turned off ([discovery] native = false)".to_owned()),
                NativeState::Starting => (State::Active, "starting".to_owned()),
                NativeState::Running => match (&h.native.error, &h.native.registered) {
                    (Some(e), _) => (State::Error, e.clone()),
                    (None, Some(name)) => (State::Active, format!("registered as \"{name}\"")),
                    (None, None) => (State::Active, "browsing".to_owned()),
                },
                NativeState::Unavailable(e) => (State::Unavailable, e.clone()),
                NativeState::Unsupported => (State::Unavailable, String::new()),
            };
            out.push(Method { id: "native", label: super::native::LABEL.to_owned(), state, detail, heard: count(&[Via::Native]), last_heard: last(&[Via::Native]) });
        }

        // Unicast DNS-SD.
        let label = if h.dns_sd.domains.is_empty() { "DNS-SD".to_owned() } else { format!("DNS-SD ({})", list(&h.dns_sd.domains, 2)) };
        let (state, detail) = if !self.browsing() {
            (State::Off, "browsing is turned off".to_owned())
        } else if !h.dns_sd.ran {
            (State::Active, "starting".to_owned())
        } else if h.dns_sd.domains.is_empty() {
            (State::Off, "no domain to browse: set [discovery] domains, or a DNS search domain on this machine".to_owned())
        } else if h.dns_sd.servers.is_empty() {
            (State::Unavailable, "no name server known ([discovery] dns_servers)".to_owned())
        } else if let Some(e) = &h.dns_sd.error {
            (State::Error, e.clone())
        } else {
            (State::Active, format!("_kiln._tcp in {} via {}", list(&h.dns_sd.domains, 3), list(&h.dns_sd.servers, 2)))
        };
        out.push(Method { id: "dns-sd", label, state, detail, heard: count(&[Via::DnsSd]), last_heard: last(&[Via::DnsSd]) });

        // Scans and probes.
        let scan = self.probes.scan();
        let targets = self.probes.targets();
        let (state, detail) = match &scan {
            Some(s) if s.running => (State::Active, format!("scanning {}: {} of {} addresses", list(&s.cidrs, 2), s.probed, s.total)),
            _ if targets > 0 => (State::Active, format!("re-probing {targets} address{} found by scans and probes every minute", if targets == 1 { "" } else { "es" })),
            Some(s) => (State::Off, format!("last scan: {} found in {}", s.found, list(&s.cidrs, 2))),
            None => (State::Off, "on demand".to_owned()),
        };
        out.push(Method {
            id: "scan",
            label: "Network scan".to_owned(),
            state,
            detail,
            heard: count(&[Via::Scan, Via::Probe]),
            last_heard: last(&[Via::Scan, Via::Probe]),
        });

        // Tailscale.
        let (state, detail) = match &h.tailscale {
            TailscaleState::Off => (State::Off, "turned off ([discovery] tailscale = false)".to_owned()),
            TailscaleState::Checking => (State::Active, "checking".to_owned()),
            TailscaleState::Unavailable(why) => (State::Unavailable, why.clone()),
            TailscaleState::Active { peers } => (State::Active, format!("{peers} peer{} online, probed every 5 minutes", if *peers == 1 { "" } else { "s" })),
            TailscaleState::Error(e) => (State::Error, e.clone()),
        };
        out.push(Method { id: "tailscale", label: "Tailscale".to_owned(), state, detail, heard: count(&[Via::Tailscale]), last_heard: last(&[Via::Tailscale]) });

        // Seeds.
        let (state, detail) = if h.seeds.count == 0 {
            (State::Off, "none configured ([discovery] seeds)".to_owned())
        } else if h.seeds.unresolved.len() >= h.seeds.count {
            (State::Error, format!("cannot resolve {}", list(&h.seeds.unresolved, 3)))
        } else if !h.seeds.unresolved.is_empty() {
            (State::Active, format!("{} addresses, probed every minute; cannot resolve {}", h.seeds.count, list(&h.seeds.unresolved, 3)))
        } else {
            (State::Active, format!("{} address{}, probed every minute", h.seeds.count, if h.seeds.count == 1 { "" } else { "es" }))
        };
        out.push(Method { id: "seeds", label: "Seed addresses".to_owned(), state, detail, heard: count(&[Via::Seed]), last_heard: last(&[Via::Seed]) });
        out
    }

    /// What is wrong with discovery here, and how to fix it (`problems[]`).
    pub fn problems(&self) -> Vec<Problem> {
        if !self.announcing() && !self.browsing() {
            return Vec::new();
        }
        let h = self.health.lock().clone();
        let now = Instant::now();
        let os = this_os();
        let mut out = Vec::new();
        let subnet = super::subnets(&self.settings.interfaces).into_iter().next().unwrap_or_else(|| "192.168.1.0/24".to_owned());

        if let MdnsState::PortBusy(e) = &h.mdns.state {
            let mut fixes = vec![fix(None, Some("Another program holds UDP port 5353 without sharing it (avahi and mDNSResponder share it). Stop it, or rely on the other ways of finding machines (scan, seeds, DNS-SD)."), None)];
            fixes.extend(
                [
                    ("linux", "sudo ss -ulpn 'sport = :5353'"),
                    ("macos", "sudo lsof -nP -iUDP:5353"),
                    ("windows", "Get-NetUDPEndpoint -LocalPort 5353 | Select-Object LocalAddress,OwningProcess"),
                ]
                .into_iter()
                .filter(|(o, _)| os == Some(*o))
                .map(|(o, c)| fix(Some(o), Some("See which program holds the port"), Some(c))),
            );
            out.push(Problem {
                id: "mdns-port",
                text: format!("Multicast DNS cannot use UDP port 5353 here ({e}), so this machine neither announces itself nor hears the others."),
                fixes,
            });
        }

        if matches!(h.mdns.state, MdnsState::Running) && h.mdns.interfaces.is_empty() {
            let text = if self.settings.interfaces.is_empty() {
                "No network interface here can carry multicast DNS: none is up, multicast-capable and not loopback, a VPN or a container bridge.".to_owned()
            } else {
                format!("None of the interfaces in [discovery] interfaces ({}) is up with an address.", self.settings.interfaces.join(", "))
            };
            let mut fixes = vec![fix(None, Some("Name the interface to use in [discovery] interfaces, e.g. interfaces = [\"eth0\"]."), None)];
            fixes.extend(
                [("linux", "ip -brief address"), ("macos", "ifconfig -l"), ("windows", "Get-NetAdapter | Where-Object Status -eq Up")]
                    .into_iter()
                    .filter(|(o, _)| os == Some(*o))
                    .map(|(o, c)| fix(Some(o), Some("List the interfaces"), Some(c))),
            );
            out.push(Problem { id: "no-multicast-iface", text, fixes });
        }

        if matches!(h.mdns.state, MdnsState::Running) && !h.mdns.interfaces.is_empty() && self.browsing() {
            let quiet_since = [h.mdns.since, h.mdns.last_foreign].into_iter().flatten().max();
            if let Some(t) = quiet_since
                && now.saturating_duration_since(t) >= SILENT_AFTER
            {
                let minutes = now.saturating_duration_since(t).as_secs() / 60;
                let mut fixes = firewall_fixes(&subnet);
                fixes.push(fix(
                    None,
                    Some("If the Wi-Fi isolates its clients (AP or client isolation), turn that off in the router, or find machines without multicast: scan the network, add [discovery] seeds, or publish to DNS ([discovery.publish])."),
                    None,
                ));
                out.push(Problem {
                    id: "silent-network",
                    text: format!(
                        "Nothing has come in over multicast DNS for {minutes} minutes, not even from other devices. A firewall may drop UDP 5353 (here or on the other machines), or the network may isolate its clients."
                    ),
                    fixes,
                });
            }
        }

        if let NativeState::Unavailable(e) = &h.native.state {
            let mut fixes = Vec::new();
            match os {
                Some("macos") => fixes.push(fix(Some("macos"), Some("Restart mDNSResponder"), Some("sudo launchctl kickstart -k system/com.apple.mDNSResponder"))),
                Some("windows") => {
                    fixes.push(fix(Some("windows"), Some("Windows 10 1809 or later has DNS-SD built in. Meanwhile kiln's own multicast DNS needs UDP 5353 allowed in (PowerShell as administrator):"), Some(WINDOWS_FIREWALL)));
                }
                _ => {}
            }
            out.push(Problem {
                id: "native-unavailable",
                text: format!("{} is not available ({e}); kiln uses its own multicast DNS instead, which needs UDP 5353 open in the firewall.", super::native::LABEL),
                fixes,
            });
        }
        if h.native.state == NativeState::Running
            && let Some(e) = &h.native.error
        {
            out.push(Problem {
                id: "native-error",
                text: format!("{} reported an error: {e}. kiln retries every few seconds.", super::native::LABEL),
                fixes: Vec::new(),
            });
        }

        if h.publish.configured && h.publish.ok == Some(false) {
            let error = h.publish.error.clone().unwrap_or_default();
            let mut fixes = vec![
                fix(None, Some("The name server must accept updates to the zone signed with this key, e.g. in BIND: update-policy { grant <key name> zonesub ANY; };"), None),
                fix(None, Some("[discovery.publish] key_secret_file holds the key's base64 secret (alone, or as tsig-keygen writes it); key_name and algorithm must match the server's."), None),
            ];
            if error.contains("BADTIME") {
                fixes.insert(0, fix(None, Some("The clocks of this machine and the name server differ by more than five minutes: sync them (NTP)."), None));
            }
            out.push(Problem {
                id: "dns-sd-publish",
                text: format!("Publishing this instance to DNS ({}) failed: {error}", h.publish.zone.clone().unwrap_or_default()),
                fixes,
            });
        }

        if h.dns_sd.ran && !h.dns_sd.domains.is_empty() && h.dns_sd.servers.is_empty() {
            out.push(Problem {
                id: "dns-sd-servers",
                text: "Unicast DNS-SD has domains to browse but no name server to ask.".to_owned(),
                fixes: vec![fix(None, Some("Set [discovery] dns_servers, e.g. dns_servers = [\"192.168.1.1\"]."), None)],
            });
        }

        if !h.bad_answer_from.is_empty() {
            out.push(Problem {
                id: "answer-from",
                text: format!("Ignored [discovery] answer_from entries: {}.", h.bad_answer_from.join(", ")),
                fixes: vec![fix(None, Some("Use \"private\", \"any\", or addresses and CIDRs such as \"203.0.113.0/24\"."), None)],
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_short() {
        let v: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(list(&v, 2), "a, b, +2");
        assert_eq!(list(&v, 4), "a, b, c, d");
        assert_eq!(list(&[], 2), "");
    }

    #[test]
    fn methods_and_problems_say_what_is_going_on() {
        let d = Discovery::for_browsing(crate::config::Discovery::default());
        // Starting: nothing to complain about yet.
        assert!(d.problems().is_empty());
        let ids: Vec<&str> = d.methods().iter().map(|m| m.id).collect();
        assert!(ids.starts_with(&["mdns"]) && ids.ends_with(&["dns-sd", "scan", "tailscale", "seeds"]));
        {
            let mut h = d.health.lock();
            h.mdns.state = MdnsState::PortBusy("bind UDP 0.0.0.0:5353: address in use".into());
            h.publish = PublishStatus { configured: true, zone: Some("example.com".into()), ok: Some(false), error: Some("BADTIME".into()), at: Some(1) };
        }
        let problems = d.problems();
        let ids: Vec<&str> = problems.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["mdns-port", "dns-sd-publish"]);
        assert!(problems.iter().all(|p| !p.fixes.is_empty()));
        let mdns = d.methods().into_iter().find(|m| m.id == "mdns");
        assert_eq!(mdns.map(|m| m.state), Some(State::Unavailable));
        // A network that stays silent for ten minutes is suspect; any packet clears it.
        {
            let mut h = d.health.lock();
            h.mdns = MdnsHealth {
                state: MdnsState::Running,
                interfaces: vec!["eth0".into()],
                since: Instant::now().checked_sub(SILENT_AFTER + Duration::from_secs(60)),
                last_foreign: None,
            };
            h.publish.ok = Some(true);
        }
        let silent = d.problems().into_iter().find(|p| p.id == "silent-network");
        let fixes = silent.map(|p| p.fixes).unwrap_or_default();
        assert!(fixes.iter().any(|f| f.os == Some("windows") && f.command.as_deref() == Some(WINDOWS_FIREWALL)));
        assert!(fixes.iter().any(|f| f.command.as_deref().is_some_and(|c| c.contains("socketfilterfw --add"))));
        assert!(fixes.iter().any(|f| f.command.as_deref().is_some_and(|c| c.starts_with("sudo ufw allow from "))));
        d.health.lock().mdns.last_foreign = Some(Instant::now());
        assert!(d.problems().is_empty());
        // Serialised as the contract has it: optional fix fields are left out.
        let json = serde_json::to_value(fix(None, Some("x"), None)).unwrap_or_default();
        assert_eq!(json, serde_json::json!({ "text": "x" }));
    }
}
