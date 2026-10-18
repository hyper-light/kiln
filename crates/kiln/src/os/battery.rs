//! A laptop's battery, as its runner reports it ([`crate::proto::AgentMsg::Power`]): how full,
//! whether it runs on its charger, and how long it lasts. A machine on battery sleeps when it runs
//! out (macOS hibernates at about 1%), and its runner with it, so kiln says so first.
//!
//! - macOS: `pmset -g batt`.
//! - Linux: `/sys/class/power_supply` (a peripheral's battery, a mouse's, doesn't count).
//! - Elsewhere: none reported.

use serde::{Deserialize, Serialize};

/// A machine's battery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    /// How full, 0–100.
    pub percent: u8,
    /// It runs on its charger (`false`: on battery).
    pub ac: bool,
    /// It's charging.
    pub charging: bool,
    /// Minutes left on battery, when the machine says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
}

impl Battery {
    /// Low: on battery at `LOW` or less. Its machine sleeps soon.
    pub fn low(&self) -> bool {
        !self.ac && self.percent <= LOW
    }
}

/// At or below this, on battery, a machine is low.
pub const LOW: u8 = 20;

/// This machine's battery: `None` without one (a desktop, a server), or where it can't be read.
/// Blocking (macOS runs `pmset`).
pub fn read() -> Option<Battery> {
    imp::read()
}

/// `pmset -g batt`'s answer.
pub fn from_pmset(text: &str) -> Option<Battery> {
    let ac = text.lines().next().is_some_and(|l| l.contains("'AC Power'"));
    let line = text.lines().find(|l| l.contains("InternalBattery"))?;
    let after = line.split_once('\t').map_or(line, |(_, a)| a);
    let mut fields = after.split(';').map(str::trim);
    let percent: u8 = fields.next()?.strip_suffix('%')?.trim().parse().ok()?;
    let state = fields.next().unwrap_or_default().to_ascii_lowercase();
    let charging = state == "charging" || state == "finishing charge";
    let rest = fields.next().unwrap_or_default();
    let minutes = rest.split_whitespace().next().and_then(|t| t.split_once(':')).and_then(|(h, m)| Some(h.parse::<u32>().ok()? * 60 + m.parse::<u32>().ok()?)).filter(|m| *m > 0);
    // Charging, its time is until full: minutes left are on battery only.
    let ac = ac || charging;
    Some(Battery { percent: percent.min(100), ac, charging, minutes: if ac { None } else { minutes } })
}

/// One power supply of `/sys/class/power_supply`: its files, by name (`type`, `capacity`, ...).
pub type Supply = std::collections::BTreeMap<String, String>;

/// What `/sys/class/power_supply` says: the machine's own battery (a peripheral's, `scope=Device`,
/// doesn't count), and its charger.
pub fn from_sysfs(supplies: &[Supply]) -> Option<Battery> {
    let get = |s: &Supply, k: &str| s.get(k).map(|v| v.trim().to_owned());
    let num = |s: &Supply, k: &str| get(s, k).and_then(|v| v.parse::<u64>().ok());
    let bat = supplies.iter().find(|s| get(s, "type").as_deref() == Some("Battery") && get(s, "scope").as_deref() != Some("Device") && get(s, "present").as_deref() != Some("0"))?;
    let percent = u8::try_from(num(bat, "capacity")?.min(100)).ok()?;
    let status = get(bat, "status").unwrap_or_default();
    let charging = status == "Charging";
    let mains = supplies.iter().filter(|s| matches!(get(s, "type").as_deref(), Some("Mains" | "USB" | "USB_C" | "USB_PD")));
    let online = mains.clone().any(|s| get(s, "online").as_deref() == Some("1"));
    let ac = online || charging || (mains.count() == 0 && status != "Discharging");
    // Minutes left: what's in it over what it draws (energy in µWh and power in µW, or charge in
    // µAh and current in µA).
    let minutes = if ac {
        None
    } else {
        let (now, rate) = match (num(bat, "energy_now"), num(bat, "power_now"), num(bat, "charge_now"), num(bat, "current_now")) {
            (Some(e), Some(p), _, _) => (e, p),
            (_, _, Some(c), Some(i)) => (c, i),
            _ => (0, 0),
        };
        (rate > 0).then(|| u32::try_from(now * 60 / rate).unwrap_or(u32::MAX)).filter(|m| *m > 0)
    };
    Some(Battery { percent, ac, charging, minutes })
}

#[cfg(target_os = "macos")]
mod imp {
    pub fn read() -> Option<super::Battery> {
        let out = std::process::Command::new("/usr/bin/pmset").args(["-g", "batt"]).stdin(std::process::Stdio::null()).output().ok()?;
        out.status.success().then(|| super::from_pmset(&String::from_utf8_lossy(&out.stdout))).flatten()
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn read() -> Option<super::Battery> {
        let mut supplies = Vec::new();
        for e in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
            let mut s = super::Supply::new();
            for k in ["type", "scope", "present", "status", "capacity", "online", "energy_now", "power_now", "charge_now", "current_now"] {
                if let Ok(v) = std::fs::read_to_string(e.path().join(k)) {
                    s.insert(k.to_owned(), v);
                }
            }
            supplies.push(s);
        }
        super::from_sysfs(&supplies)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn read() -> Option<super::Battery> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_macs_battery_from_pmset() {
        let on_battery = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t34%; discharging; 2:15 remaining present: true\n";
        assert_eq!(from_pmset(on_battery), Some(Battery { percent: 34, ac: false, charging: false, minutes: Some(135) }));
        let low = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t1%; discharging; (no estimate) present: true\n";
        let b = from_pmset(low);
        assert_eq!(b, Some(Battery { percent: 1, ac: false, charging: false, minutes: None }));
        assert!(b.is_some_and(|b| b.low()));
        let charging = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t1%; charging; (no estimate) present: true\n";
        assert_eq!(from_pmset(charging), Some(Battery { percent: 1, ac: true, charging: true, minutes: None }));
        assert!(from_pmset(charging).is_some_and(|b| !b.low()), "on its charger it isn't low, however empty");
        let full = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t100%; charged; 0:00 remaining present: true\n";
        assert_eq!(from_pmset(full), Some(Battery { percent: 100, ac: true, charging: false, minutes: None }));
        let held = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t80%; AC attached; not charging present: true\n";
        assert_eq!(from_pmset(held), Some(Battery { percent: 80, ac: true, charging: false, minutes: None }));
        // As a Mac said it, 2026-10-18, charging after it had run out.
        let real = "Now drawing from 'AC Power'\n -InternalBattery-0 (id=23330915)\t22%; charging; 2:10 remaining present: true\n";
        assert_eq!(from_pmset(real), Some(Battery { percent: 22, ac: true, charging: true, minutes: None }));
        // A Mac without a battery.
        assert_eq!(from_pmset("Now drawing from 'AC Power'\n"), None);
    }

    #[test]
    fn a_linux_laptops_battery_from_sysfs() {
        let supply = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), format!("{v}\n"))).collect::<Supply>();
        let bat = supply(&[("type", "Battery"), ("status", "Discharging"), ("capacity", "15"), ("energy_now", "6000000"), ("power_now", "12000000"), ("present", "1")]);
        let ac_off = supply(&[("type", "Mains"), ("online", "0")]);
        let mouse = supply(&[("type", "Battery"), ("scope", "Device"), ("capacity", "90"), ("status", "Discharging")]);
        assert_eq!(from_sysfs(&[mouse.clone(), ac_off.clone(), bat.clone()]), Some(Battery { percent: 15, ac: false, charging: false, minutes: Some(30) }));
        let ac_on = supply(&[("type", "Mains"), ("online", "1")]);
        let charging = supply(&[("type", "Battery"), ("status", "Charging"), ("capacity", "40")]);
        assert_eq!(from_sysfs(&[charging, ac_on]), Some(Battery { percent: 40, ac: true, charging: true, minutes: None }));
        // A desktop (only a peripheral's battery), and a server (nothing).
        assert_eq!(from_sysfs(&[mouse]), None);
        assert_eq!(from_sysfs(&[]), None);
    }
}
