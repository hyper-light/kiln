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

pub fn session_of(_pid: u32) -> Option<String> {
    None
}

pub fn how() -> String {
    "none (processes are found by their mark)".to_owned()
}
