//! A runner's headroom settings, as the admin set them (`PUT /api/runners/:name/headroom`): one
//! shape for the API's body, the server's stored copy and the runner's own file, so a runner
//! restarted without its server starts from what it was last told.

use serde::{Deserialize, Serialize};

/// What headroom keeps healthy, how much heavy work it lets run, and how it slows the rest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Off: work starts and runs as it did before headroom (probes still report).
    pub enabled: bool,
    /// What must stay healthy beyond what kiln adds itself (`automatic`).
    pub targets: Vec<Target>,
    /// kiln adds its own targets: an fsync on the runner's data folder's file system, and the
    /// health of the server it reports to.
    pub automatic: bool,
    pub window: Window,
    pub pacing: Pacing,
    /// When a group of processes counts as heavy work.
    pub heavy: Heavy,
    /// How decisions are kept from flapping on noise.
    pub damping: Damping,
    /// How long a turn let in holds its place before its work shows (it may take a while to start).
    pub admit_hold_secs: u32,
    /// Work the user's service manager runs outside kiln's groups (a build started with
    /// `systemd-run --user`) is paced too. Off: only kiln's own groups are.
    pub escapes: bool,
    /// Most agent sessions on this machine (none: the runner's built-in limit).
    pub max_sessions: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            targets: Vec::new(),
            automatic: true,
            window: Window::default(),
            pacing: Pacing::default(),
            heavy: Heavy::default(),
            damping: Damping::default(),
            admit_hold_secs: 30,
            escapes: true,
            max_sessions: None,
        }
    }
}

/// Something that must stay healthy, and how fast it must answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// Shown in the UI and the logs.
    #[serde(default)]
    pub name: String,
    pub probe: Probe,
    /// Slower than this (or no answer by then) and the machine is too busy.
    pub goal_ms: u32,
}

/// How a target is checked. A target that refuses or fails isn't counted as slow: it may be
/// down for its own reasons (a deploy), and holding work back wouldn't bring it up.
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

/// How much heavy work may run at once, found the way TCP finds a path's capacity: while every
/// target meets its goal it grows, and a miss halves it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Window {
    /// Never less than this many groups at full speed.
    pub min: u32,
    pub max: u32,
    /// Where it starts (a restart starts small: what was running resumes a few at a time).
    pub start: u32,
    /// It grows by one per this many seconds while every target is healthy.
    pub grow_secs: u32,
    /// At most one halving per this long (one slow spell isn't counted many times).
    pub cooldown_ms: u32,
}

impl Default for Window {
    fn default() -> Self {
        Self { min: 1, max: 16, start: 2, grow_secs: 10, cooldown_ms: 2000 }
    }
}

/// How work past the window is slowed: run part of every period, stopped for the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pacing {
    /// Off: work past the window waits to start, but what runs is never slowed.
    pub enabled: bool,
    pub period_ms: u32,
    /// The least share of a period paced work runs, in percent.
    pub min_duty_pct: u8,
    /// Stopped outright (0%) after targets miss for this long with the window at its least.
    pub freeze_after_secs: u32,
    /// Back to full speed after targets have been healthy this long.
    pub release_after_secs: u32,
}

impl Default for Pacing {
    fn default() -> Self {
        Self { enabled: true, period_ms: 1000, min_duty_pct: 10, freeze_after_secs: 20, release_after_secs: 30 }
    }
}

/// When a group counts as heavy work (either is enough).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Heavy {
    /// Percent of one CPU.
    pub cpu_pct: u32,
    /// Megabytes written a second.
    pub write_mb_s: u32,
}

impl Default for Heavy {
    fn default() -> Self {
        Self { cpu_pct: 50, write_mb_s: 5 }
    }
}

/// What keeps decisions from flapping: a target is missing only when it misses repeatedly (or is
/// stuck), work is heavy until it's been well under the thresholds a while, paced work is released
/// on mostly-healthy rather than unbroken health, and the window must settle before more work is
/// let in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Damping {
    /// A target counts as missing when this many of its last `confirm_of` probes missed...
    pub confirm_misses: u8,
    pub confirm_of: u8,
    /// ...or at once when one probe has been in flight this long, in percent of its goal.
    pub stuck_pct: u32,
    /// Heavy work stops counting as heavy only after this long under `heavy_exit_pct` of the
    /// thresholds (its load smoothed over a few seconds).
    pub heavy_exit_secs: u32,
    pub heavy_exit_pct: u8,
    /// Paced work runs at full speed again once this share of the last `release_after_secs` was
    /// healthy (and targets are healthy now).
    pub release_healthy_pct: u8,
    /// More work is let in only once the window has stayed at a higher whole number this long (a
    /// lower one applies at once).
    pub settle_secs: u32,
    /// With no one waiting, new work is held only once use reaches the window plus this many;
    /// work already held goes in only below the window, oldest first.
    pub admit_slack: u32,
}

impl Default for Damping {
    fn default() -> Self {
        Self { confirm_misses: 2, confirm_of: 4, stuck_pct: 200, heavy_exit_secs: 5, heavy_exit_pct: 50, release_healthy_pct: 90, settle_secs: 5, admit_slack: 1 }
    }
}

impl Settings {
    /// These settings, or what's wrong with them.
    pub fn validated(self) -> Result<Self, String> {
        let w = &self.window;
        if w.min == 0 || w.min > w.max || w.max > 1024 {
            return Err(format!("window: min ({}) must be at least 1 and at most max ({}), max at most 1024", w.min, w.max));
        }
        if w.start < w.min || w.start > w.max {
            return Err(format!("window: start ({}) must be between min ({}) and max ({})", w.start, w.min, w.max));
        }
        if w.grow_secs == 0 || w.grow_secs > 3600 {
            return Err("window: grow_secs must be 1 to 3600".into());
        }
        if !(100..=60_000).contains(&w.cooldown_ms) {
            return Err("window: cooldown_ms must be 100 to 60000".into());
        }
        let p = &self.pacing;
        if !(100..=10_000).contains(&p.period_ms) {
            return Err("pacing: period_ms must be 100 to 10000".into());
        }
        if p.min_duty_pct == 0 || p.min_duty_pct > 100 {
            return Err("pacing: min_duty_pct must be 1 to 100".into());
        }
        if p.freeze_after_secs == 0 || p.release_after_secs == 0 {
            return Err("pacing: freeze_after_secs and release_after_secs must be at least 1".into());
        }
        let d = &self.damping;
        if d.confirm_of == 0 || d.confirm_of > 32 || d.confirm_misses == 0 || d.confirm_misses > d.confirm_of {
            return Err(format!("damping: confirm_misses ({}) must be 1 to confirm_of ({}), confirm_of at most 32", d.confirm_misses, d.confirm_of));
        }
        if !(100..=10_000).contains(&d.stuck_pct) {
            return Err("damping: stuck_pct must be 100 to 10000".into());
        }
        if d.heavy_exit_pct == 0 || d.heavy_exit_pct > 100 || d.heavy_exit_secs > 600 {
            return Err("damping: heavy_exit_pct must be 1 to 100, heavy_exit_secs at most 600".into());
        }
        if d.release_healthy_pct == 0 || d.release_healthy_pct > 100 || d.settle_secs > 600 || d.admit_slack > 64 {
            return Err("damping: release_healthy_pct must be 1 to 100, settle_secs at most 600, admit_slack at most 64".into());
        }
        if self.admit_hold_secs == 0 || self.admit_hold_secs > 600 {
            return Err("admit_hold_secs must be 1 to 600".into());
        }
        if self.max_sessions == Some(0) {
            return Err("max_sessions must be at least 1 (leave it empty for the runner's own limit)".into());
        }
        if self.targets.len() > 32 {
            return Err("at most 32 targets".into());
        }
        for t in &self.targets {
            if !(1..=600_000).contains(&t.goal_ms) {
                return Err(format!("target {:?}: goal_ms must be 1 to 600000", t.name));
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
    fn defaults_are_valid_and_old_files_read() -> anyhow::Result<()> {
        Settings::default().validated().map_err(anyhow::Error::msg)?;
        // A file from a build that knew fewer fields reads, the rest defaulted.
        let s: Settings = serde_json::from_str(r#"{"enabled":false,"window":{"max":4}}"#)?;
        anyhow::ensure!(!s.enabled && s.window.max == 4 && s.window.min == 1 && s.pacing.enabled);
        Ok(())
    }

    #[test]
    fn bad_settings_say_what_is_wrong() {
        let s = Settings { window: Window { start: 99, ..Window::default() }, ..Settings::default() };
        assert!(s.validated().err().unwrap_or_default().contains("start"));
        let s = Settings { targets: vec![Target { name: "web".into(), probe: Probe::Http { url: "https://x".into() }, goal_ms: 500 }], ..Settings::default() };
        assert!(s.validated().err().unwrap_or_default().contains("http://"));
        let s = Settings { max_sessions: Some(0), ..Settings::default() };
        assert!(s.validated().is_err());
    }

    #[test]
    fn targets_take_their_kind_by_name() -> anyhow::Result<()> {
        let t: Target = serde_json::from_str(r#"{"name":"db","probe":{"kind":"fsync","path":"/var/lib/db"},"goal_ms":500}"#)?;
        anyhow::ensure!(t.probe == Probe::Fsync { path: "/var/lib/db".into() });
        Ok(())
    }
}
