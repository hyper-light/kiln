//! What Datadog's Agent says of its host (`docs/observe.md` §10.9), kept by host as Datadog's host
//! list shows it: its host metadata (`/intake/`, at its start and every 30 minutes: its version,
//! the host's platform, its tags by source, its aliases, how it was installed, gohai's system
//! information), its checks (`/intake/` too), and its inventory of the host (`/api/v1/metadata`'s
//! `host_metadata`: cores, memory, kernel, OS version, cloud). An Agent 7.84's payloads, as it sent
//! them, are the tests' fixtures.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// gohai's system information kept, at the most (an Agent's is some kilobytes).
const GOHAI_BYTES: usize = 16 << 10;
/// Checks, tags a source, sources, aliases and inventory fields a host keeps, at the most.
const CHECKS: usize = 256;
const TAGS: usize = 256;
const SOURCES: usize = 16;
const ALIASES: usize = 64;
/// A value of an inventory field kept, at the most.
const FIELD_BYTES: usize = 1024;
/// Hosts kept, at the most: a fleet's (a new one past it is not kept).
pub const HOSTS: usize = 100_000;

/// The fields of the Agent's system stats Datadog's host list shows in `meta`, as it names them.
const STATS: [&str; 9] = ["cpuCores", "machine", "platform", "processor", "pythonV", "macV", "nixV", "fbsdV", "winV"];
/// The inventory's fields the host's page reads.
const INVENTORY: [&str; 17] = [
    "cpu_cores",
    "cpu_logical_processors",
    "cpu_vendor",
    "cpu_model",
    "cpu_frequency",
    "kernel_name",
    "kernel_release",
    "os",
    "os_version",
    "cpu_architecture",
    "memory_total_kb",
    "memory_swap_total_kb",
    "ip_address",
    "ipv6_address",
    "cloud_provider",
    "cloud_provider_host_id",
    "instance-type",
];

/// What the Agent says of one host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentHost {
    /// When it last said anything of it (ms).
    pub updated_ms: i64,
    /// Datadog's host list's `meta`: `agent_version`, `cpuCores`, `machine`, `platform`,
    /// `processor`, `pythonV`, `macV`, `nixV`, `fbsdV`, `winV`, `socket-hostname`, `socket-fqdn`,
    /// `install_method`, `gohai`, `agent_checks`.
    #[serde(default)]
    pub meta: Map<String, Value>,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Its host tags by where they come from: the Agent's own (`tags`, Datadog's "Datadog"), a
    /// cloud's.
    #[serde(default)]
    pub tags_by_source: BTreeMap<String, Vec<String>>,
    /// Its inventory of the host (`/api/v1/metadata`'s `host_metadata`).
    #[serde(default)]
    pub inventory: Map<String, Value>,
}

impl AgentHost {
    /// Its version, as it says it.
    pub fn version(&self) -> Option<&str> {
        self.meta.get("agent_version").and_then(Value::as_str)
    }

    /// An inventory field's text, or number as text.
    pub fn field(&self, key: &str) -> Option<String> {
        match self.inventory.get(key)? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
}

/// What a payload says of its host, to be kept: one of the Agent's three.
#[derive(Debug, Clone, PartialEq)]
pub enum Said {
    /// Its host metadata: `meta`'s fields, aliases, tags by source.
    Host { meta: Map<String, Value>, aliases: Vec<String>, tags: BTreeMap<String, Vec<String>> },
    /// Its checks, each as it lists them (`[name, id, instance, status, error, …]`).
    Checks(Vec<Value>),
    /// Its inventory of the host.
    Inventory(Map<String, Value>),
}

/// A text cut to `max` bytes at a character's end.
fn cut(s: &str, max: usize) -> &str {
    let mut c = s.len().min(max);
    while !s.is_char_boundary(c) {
        c -= 1;
    }
    s.get(..c).unwrap_or_default()
}

fn text(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Tags as the Agent lists them: strings, the empty left out, at most [`TAGS`].
fn tag_list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).take(TAGS).map(|t| cut(t, 200).to_owned()).collect()).unwrap_or_default()
}

/// A tag source as Datadog's host list names it: the Agent's own tags (`system`) are "Datadog".
fn source_name(key: &str) -> String {
    match key {
        "system" => "Datadog".to_owned(),
        k => cut(k, 64).to_owned(),
    }
}

/// What a payload posted to `/intake/` or `/api/v1/metadata` says of its host: the host, and what
/// to keep; none when it is none of the three (events, an inventory of something else).
pub fn read(v: &Value) -> Option<(String, Said)> {
    let o = v.as_object()?;
    // `/api/v1/metadata`: an inventory, of the host or of something else.
    if let Some(inv) = o.get("host_metadata").and_then(Value::as_object) {
        let host = text(o.get("hostname"))?;
        let mut kept = Map::new();
        for k in INVENTORY {
            match inv.get(k) {
                Some(Value::String(s)) if !s.is_empty() => {
                    kept.insert(k.to_owned(), Value::String(cut(s, FIELD_BYTES).to_owned()));
                }
                Some(n @ Value::Number(_)) => {
                    kept.insert(k.to_owned(), n.clone());
                }
                _ => {}
            }
        }
        return Some((host.to_owned(), Said::Inventory(kept)));
    }
    let host = text(o.get("internalHostname"))?;
    // Its checks: `[name, check, instance, status, error, …]` each.
    if let Some(checks) = o.get("agent_checks").and_then(Value::as_array) {
        return Some((host.to_owned(), Said::Checks(checks.iter().take(CHECKS).cloned().collect())));
    }
    // Its host metadata: what says the host's platform.
    let stats = o.get("systemStats").and_then(Value::as_object);
    let meta = o.get("meta").and_then(Value::as_object);
    if stats.is_none() && meta.is_none() {
        return None;
    }
    let mut m = Map::new();
    if let Some(v) = text(o.get("agentVersion")) {
        m.insert("agent_version".into(), json!(v));
    }
    for k in STATS {
        if let Some(v) = stats.and_then(|s| s.get(k)).filter(|v| !v.is_null()) {
            m.insert(k.into(), v.clone());
        }
    }
    for k in ["socket-hostname", "socket-fqdn"] {
        if let Some(v) = text(meta.and_then(|s| s.get(k))) {
            m.insert(k.into(), json!(v));
        }
    }
    if let Some(im) = o.get("install-method").and_then(Value::as_object) {
        let field = |k: &str| text(im.get(k)).map_or(Value::Null, |v| json!(v));
        m.insert("install_method".into(), json!({ "tool": field("tool"), "tool_version": field("tool_version"), "installer_version": field("installer_version") }));
    }
    if let Some(g) = text(o.get("gohai")) {
        // Kept whole or not at all: a JSON text cut is no JSON.
        if g.len() <= GOHAI_BYTES {
            m.insert("gohai".into(), json!(g));
        }
    }
    let mut aliases: Vec<String> = Vec::new();
    let mut alias = |a: &str| {
        if a != host && !aliases.iter().any(|x| x == a) && aliases.len() < ALIASES {
            aliases.push(cut(a, 255).to_owned());
        }
    };
    for a in meta.and_then(|s| s.get("host_aliases")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        alias(a.trim());
    }
    for k in ["ec2-hostname", "instance-id"] {
        if let Some(a) = text(meta.and_then(|s| s.get(k))) {
            alias(a);
        }
    }
    let mut tags = BTreeMap::new();
    for (k, v) in o.get("host-tags").and_then(Value::as_object).into_iter().flatten().take(SOURCES) {
        let list = tag_list(v);
        if !list.is_empty() {
            tags.insert(source_name(k), list);
        }
    }
    Some((host.to_owned(), Said::Host { meta: m, aliases, tags }))
}

/// Every host the Agent spoke of: kept in `<observe>/hosts.json`. Each shared as it is (an
/// inventory holds it as it was), changed as a copy.
#[derive(Debug, Default)]
pub struct Agents {
    pub hosts: BTreeMap<String, Arc<AgentHost>>,
}

/// The file's form, written of what is held, read into it.
#[derive(Serialize)]
struct SavedRef<'a> {
    hosts: BTreeMap<&'a str, &'a AgentHost>,
}

#[derive(Default, Deserialize)]
struct Saved {
    #[serde(default)]
    hosts: BTreeMap<String, AgentHost>,
}

impl Agents {
    /// What is held, as its file keeps it.
    pub fn to_json(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(&SavedRef { hosts: self.hosts.iter().map(|(k, v)| (k.as_str(), v.as_ref())).collect() })
    }

    /// What its file kept; none when it is not one.
    pub fn from_json(b: &[u8]) -> Option<Self> {
        let saved: Saved = serde_json::from_slice(b).ok()?;
        Some(Self { hosts: saved.hosts.into_iter().map(|(k, v)| (k, Arc::new(v))).collect() })
    }

    /// What `said` says of `host`, kept: whether it was (a new host past [`HOSTS`] is not).
    pub fn keep(&mut self, host: &str, said: Said, now_ms: i64) -> bool {
        if !self.hosts.contains_key(host) && self.hosts.len() >= HOSTS {
            return false;
        }
        let h = Arc::make_mut(self.hosts.entry(cut(host, 255).to_owned()).or_default());
        h.updated_ms = h.updated_ms.max(now_ms);
        match said {
            Said::Host { meta, aliases, tags } => {
                // Its checks are their own payload's: kept across host metadata.
                let checks = h.meta.remove("agent_checks");
                h.meta = meta;
                if let Some(c) = checks {
                    h.meta.insert("agent_checks".into(), c);
                }
                h.aliases = aliases;
                h.tags_by_source = tags;
            }
            Said::Checks(c) => {
                h.meta.insert("agent_checks".into(), Value::Array(c));
            }
            Said::Inventory(inv) => h.inventory = inv,
        }
        true
    }

    /// Hosts the Agent has said nothing of since `before_ms` let go.
    pub fn expire(&mut self, before_ms: i64) -> usize {
        let n = self.hosts.len();
        self.hosts.retain(|_, h| h.updated_ms >= before_ms);
        n - self.hosts.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/src/observe/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// An Agent 7.84's three payloads of its host, as it sent them, read as Datadog's host list
    /// shows them; its checks kept across its next host metadata; an event payload is none of them.
    #[test]
    fn an_agent_s_payloads_say_its_host() {
        let mut a = Agents::default();
        let (host, said) = read(&fixture("dd-agent-host-7.84.json")).unwrap();
        assert_eq!(host, "probe-host");
        let Said::Host { meta, aliases, tags } = &said else { panic!("host metadata") };
        assert_eq!(meta.get("agent_version"), Some(&json!("7.84.4")));
        assert_eq!(meta.get("platform"), Some(&json!("linux")));
        assert_eq!(meta.get("nixV"), Some(&json!(["ubuntu", "24.04", ""])));
        assert_eq!(meta.get("socket-hostname"), Some(&json!("probe-host")));
        assert_eq!(meta.get("install_method"), Some(&json!({ "tool": "docker", "tool_version": "docker", "installer_version": "docker" })));
        assert!(meta.get("gohai").and_then(Value::as_str).is_some_and(|g| serde_json::from_str::<Value>(g).is_ok()));
        assert!(aliases.is_empty(), "no alias but its own name");
        assert_eq!(tags.get("Datadog"), Some(&vec!["env:capture".to_owned(), "gpu_host:true".to_owned(), "team:observe".to_owned()]));
        assert!(a.keep(&host, said, 1_000));
        let (host, said) = read(&fixture("dd-agent-checks-7.84.json")).unwrap();
        assert!(matches!(&said, Said::Checks(c) if c.len() == 13));
        assert!(a.keep(&host, said, 2_000));
        let (host, said) = read(&fixture("dd-agent-inventory-host-7.84.json")).unwrap();
        assert!(a.keep(&host, said, 3_000));
        // Its next host metadata keeps its checks.
        let (host, said) = read(&fixture("dd-agent-host-7.84.json")).unwrap();
        assert!(a.keep(&host, said, 4_000));
        let h = a.hosts.get("probe-host").unwrap();
        assert_eq!((h.updated_ms, h.version()), (4_000, Some("7.84.4")));
        assert_eq!(h.meta.get("agent_checks").and_then(Value::as_array).map(Vec::len), Some(13));
        assert_eq!((h.field("cpu_cores"), h.field("memory_total_kb"), h.field("os_version")), (Some("16".into()), Some("130859532".into()), Some("ubuntu 24.04".into())));
        // An event payload says nothing of the host.
        let events = json!({ "apiKey": "", "events": { "System": [{ "msg_title": "Agent Startup" }] }, "internalHostname": "probe-host" });
        assert_eq!(read(&events), None);
        // Through its file and back, the same.
        let again = Agents::from_json(&a.to_json().unwrap()).unwrap();
        assert_eq!(again.hosts, a.hosts);
        assert!(Agents::from_json(b"not json").is_none());
        assert_eq!(a.expire(3_000), 0);
        assert_eq!(a.expire(5_000), 1);
    }

    /// What a hostile payload says is bounded: tags, aliases, checks and gohai cut to their bounds,
    /// a host past the fleet's bound not kept.
    #[test]
    fn what_is_kept_is_bounded() {
        let many: Vec<String> = (0..1_000).map(|i| format!("t:{i}")).collect();
        let v = json!({
            "internalHostname": "h", "systemStats": { "cpuCores": 2 }, "meta": { "host_aliases": many },
            "host-tags": { "system": many, "aws": ["region:x"] }, "gohai": "x".repeat(GOHAI_BYTES + 1)
        });
        let (_, said) = read(&v).unwrap();
        let Said::Host { meta, aliases, tags } = said else { panic!("host metadata") };
        assert_eq!((aliases.len(), tags.get("Datadog").map(Vec::len), meta.contains_key("gohai")), (ALIASES, Some(TAGS), false));
        let checks = json!({ "internalHostname": "h", "agent_checks": (0..1_000).map(|i| json!([format!("c{i}")])).collect::<Vec<_>>() });
        assert!(matches!(read(&checks), Some((_, Said::Checks(c))) if c.len() == CHECKS));
        let mut a = Agents::default();
        for i in 0..HOSTS {
            a.hosts.insert(format!("h{i}"), Arc::new(AgentHost::default()));
        }
        assert!(!a.keep("one-too-many", Said::Checks(Vec::new()), 1));
        assert!(a.keep("h7", Said::Checks(Vec::new()), 1), "one kept already is kept still");
    }
}
