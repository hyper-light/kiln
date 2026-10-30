//! A runner's headroom settings, as the admin set them (`PUT /api/runners/:name/headroom`): one
//! shape for the API's body, the server's stored copy and the runner's own file, so a runner
//! restarted without its server starts from what it was last told.
//!
//! Only what a person decides is here (docs/headroom.md, "Where every number comes from"): how
//! many false alarms a day they'll accept, how slow each target may get, how much of a served
//! target's time its probes may take, and what headroom may touch. Everything else is measured.

use serde::{Deserialize, Serialize};

/// Kubernetes' own default probe timeout (`timeoutSeconds`, 1 s): the tolerance of a target
/// that declares none. The platform's default, not a tuning choice.
pub const PLATFORM_TOLERANCE_MS: u32 = 1000;
/// Kubernetes' own default probe period (`periodSeconds`, 10 s) over its timeout: the share of a
/// target's time the platform's own probes may take, 1 in 10.
pub const PLATFORM_PROBE_SHARE: f64 = 0.1;

/// What headroom keeps healthy, and what it may touch to keep it so.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Off: work starts and runs as it would without headroom (targets are still checked and shown).
    pub enabled: bool,
    /// What must stay healthy beyond what kiln adds itself (`automatic`).
    pub targets: Vec<Target>,
    /// kiln adds its own targets: a small sync in the runner's data folder, and the health of the
    /// server it reports to.
    pub automatic: bool,
    /// How many times a day headroom may hold or slow work when nothing would have gone wrong.
    pub false_alarms_per_day: f64,
    /// How much of a served target's time its probes may take (0 to 1).
    pub probe_load_share: f64,
    /// Work the user's service manager runs outside kiln's groups (a build an agent started with
    /// `systemd-run --user`) is paced too.
    pub escapes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, targets: Vec::new(), automatic: true, false_alarms_per_day: 1.0, probe_load_share: PLATFORM_PROBE_SHARE, escapes: true }
    }
}

/// Something that must stay healthy, and how slow it may get before it's harmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Shown in the UI and the logs.
    #[serde(default)]
    pub name: String,
    pub probe: Probe,
    /// Slower than this harms it (none: the platform's default, 1 s).
    #[serde(default, alias = "goal_ms")]
    pub tolerance_ms: Option<u32>,
}

impl Target {
    pub fn tolerance_ms(&self) -> u32 {
        self.tolerance_ms.unwrap_or(PLATFORM_TOLERANCE_MS)
    }
}

/// How a target is checked. A target that refuses or fails isn't counted as slow: it may be down
/// for its own reasons (a deploy), and holding work back wouldn't bring it up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Probe {
    /// A small write made durable in this folder: what a database commit or a container's
    /// teardown waits for on that file system.
    Fsync { path: String },
    /// `GET` this `http://` URL.
    Http { url: String },
    /// Open a TCP connection to `host:port`.
    Tcp { addr: String },
}

impl Probe {
    /// A target that serves others: its probes are held to the load share.
    pub fn served(&self) -> bool {
        !matches!(self, Probe::Fsync { .. })
    }
}

impl Settings {
    /// These settings, or what's wrong with them.
    pub fn validated(self) -> Result<Self, String> {
        if !self.false_alarms_per_day.is_finite() || self.false_alarms_per_day <= 0.0 {
            return Err("false_alarms_per_day must be more than 0".into());
        }
        if !self.probe_load_share.is_finite() || self.probe_load_share <= 0.0 || self.probe_load_share > 1.0 {
            return Err("probe_load_share must be more than 0 and at most 1".into());
        }
        for t in &self.targets {
            if t.tolerance_ms == Some(0) {
                return Err(format!("target {:?}: tolerance_ms must be more than 0", t.name));
            }
            match &t.probe {
                Probe::Fsync { path } if path.trim().is_empty() => return Err(format!("target {:?}: an fsync needs a folder", t.name)),
                Probe::Http { url } if !url.starts_with("http://") => return Err(format!("target {:?}: only http:// URLs are probed", t.name)),
                Probe::Tcp { addr } if !addr.contains(':') => return Err(format!("target {:?}: a TCP target is host:port", t.name)),
                _ => {}
            }
        }
        Ok(self)
    }

    /// The core's policy from these settings.
    pub fn policy(&self) -> kiln_ci::headroom::Policy {
        kiln_ci::headroom::Policy { false_alarms_per_day: self.false_alarms_per_day, probe_load_share: self.probe_load_share }
    }

    /// Names these settings, so the server can tell whether a runner applied them.
    pub fn fingerprint(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        crate::util::sha256_hex(json.as_bytes()).chars().take(16).collect()
    }
}

/// What the server sends a runner (`ServerMsg::Headroom`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Push {
    pub settings: Settings,
    /// Another process of the machine paces (its newest): this one only holds work back.
    pub standby: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_a_v1_file_still_reads() -> anyhow::Result<()> {
        Settings::default().validated().map_err(anyhow::Error::msg)?;
        // A v1 file: its window, pacing and goal fields are ignored or carried over.
        let s: Settings = serde_json::from_str(r#"{"enabled":false,"window":{"max":4},"pacing":{"enabled":true},"targets":[{"name":"db","probe":{"kind":"fsync","path":"/x"},"goal_ms":500}]}"#)?;
        anyhow::ensure!(!s.enabled && s.false_alarms_per_day == 1.0);
        anyhow::ensure!(s.targets.first().map(Target::tolerance_ms) == Some(500));
        Ok(())
    }

    #[test]
    fn bad_settings_say_what_is_wrong() {
        let s = Settings { false_alarms_per_day: 0.0, ..Settings::default() };
        assert!(s.validated().err().unwrap_or_default().contains("false_alarms_per_day"));
        let s = Settings { probe_load_share: 2.0, ..Settings::default() };
        assert!(s.validated().is_err());
        let s = Settings { targets: vec![Target { name: "web".into(), probe: Probe::Http { url: "https://x".into() }, tolerance_ms: None }], ..Settings::default() };
        assert!(s.validated().err().unwrap_or_default().contains("http://"));
    }

    #[test]
    fn a_target_without_a_tolerance_takes_the_platforms() -> anyhow::Result<()> {
        let t: Target = serde_json::from_str(r#"{"name":"api","probe":{"kind":"http","url":"http://x/healthz"}}"#)?;
        anyhow::ensure!(t.tolerance_ms() == 1000 && t.probe.served());
        Ok(())
    }
}
