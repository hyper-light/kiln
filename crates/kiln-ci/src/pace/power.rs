//! What a runner on battery does with work placed on it (W05, W13-R01; docs/needs.md "Pace"):
//! `[runner] on_battery`, a person's setting.
//!
//! - **`drain`, the default**: on battery it takes nothing new, says why (its runs wait with
//!   `draining`, and its log and page say it's on battery), and what runs there runs on to its end;
//!   back on its charger it takes work again. This is the default that software doing work on a
//!   machine for others ships with: BOINC suspends computing on battery ("Suspend when computer is
//!   on battery", on unless the person turns it off), Folding@home pauses (`pause-on-battery`,
//!   true by default), and Windows' Task Scheduler starts a task only on AC power and stops it when
//!   the machine goes to battery unless the task says otherwise (`DisallowStartIfOnBatteries` and
//!   `StopIfGoingOnBatteries`, both true by default). On battery a run finishes later than the
//!   machine's speed says (macOS and Windows lower its performance on battery), it's lost if the
//!   person closes the lid, and the charge it spends is the person's: they unplugged it to carry it.
//! - **`take`**: as on its charger.
//!
//! It refrains, never acts: a runner on battery drains, which only ever leaves work to others.

use serde::{Deserialize, Serialize};

/// `[runner] on_battery`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnBattery {
    #[default]
    Drain,
    Take,
}

impl OnBattery {
    /// The setting as written (`drain`, `take`); none for anything else.
    pub fn parse(s: &str) -> Option<OnBattery> {
        match s.trim().to_ascii_lowercase().as_str() {
            "drain" => Some(OnBattery::Drain),
            "take" => Some(OnBattery::Take),
            _ => None,
        }
    }

    /// Whether a runner on battery (`on_battery`; none: no battery, or unread) drains by it.
    pub fn drains(self, on_battery: Option<bool>) -> bool {
        self == OnBattery::Drain && on_battery == Some(true)
    }

    /// Why it drains, for a person.
    pub fn why(self) -> &'static str {
        match self {
            OnBattery::Drain => "it's on battery, and `[runner] on_battery = \"drain\"` (the default) takes nothing new until it's on its charger",
            OnBattery::Take => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W05: on battery, the default drains; `take` never does; off battery, or with no battery
    /// read, nothing drains.
    #[test]
    fn w05_on_battery_default_drains() {
        assert_eq!(OnBattery::default(), OnBattery::Drain);
        assert!(OnBattery::Drain.drains(Some(true)));
        assert!(!OnBattery::Drain.drains(Some(false)));
        assert!(!OnBattery::Drain.drains(None));
        assert!(!OnBattery::Take.drains(Some(true)));
        assert_eq!(OnBattery::parse(" Take "), Some(OnBattery::Take));
        assert_eq!(OnBattery::parse("budget"), None);
    }
}
