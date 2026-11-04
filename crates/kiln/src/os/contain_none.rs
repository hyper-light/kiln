//! Where kiln has no way to hold an agent's processes together: agents have no group, and their
//! processes are found by their mark (`crate::harness::survivors`).

use std::process::Command;

/// An agent session's group of processes: never here.
#[derive(Debug, Clone)]
pub struct Group {
    _none: (),
}

impl Group {
    pub fn of(_session: &str) -> Option<Group> {
        None
    }

    pub fn of_person(_name: &str) -> Option<Group> {
        None
    }

    pub fn existing(_session: &str) -> Option<Group> {
        None
    }

    pub fn enter(&self, _cmd: &mut Command) {}

    pub fn started(&self, _pid: u32) -> bool {
        false
    }

    pub fn pids(&self) -> Vec<u32> {
        Vec::new()
    }

    pub fn kill(&self) {}

    pub fn remove(&self) {}
}

pub fn hold_agents() {}

/// The sessions with processes held now: none here.
pub fn sessions() -> Vec<String> {
    Vec::new()
}

pub fn session_of(_pid: u32) -> Option<String> {
    None
}

pub fn how() -> String {
    "none (processes are found by their mark)".to_owned()
}

/// How legs and agents run here (docs/priority.md): as anything else does.
pub fn priority() -> String {
    "normal: kiln has no way to lower its own work's priority on this system".to_owned()
}
