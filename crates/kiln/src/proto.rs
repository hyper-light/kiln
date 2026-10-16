//! Agent ⇄ server protocol, carried over hyper-transport lanes (ordered, mutually authenticated
//! QUIC streams). Messages are postcard-encoded: compact, schema'd by Rust types, and decoding a
//! malformed frame is an error, never a panic.
//!
//! Lanes:
//! * [`LANE_CONTROL`] — control messages in both directions (class `Control`, highest priority).
//! * [`LANE_TERM`]    — interactive PTY traffic (class `Control`: keystrokes must not queue behind
//!   bulk logs).
//! * [`LANE_LOGS`]    — job output (class `Bulk`).

use serde::{Deserialize, Serialize};

use crate::metrics::HostSample;
use crate::pipeline::PlannedJob;
use crate::store::Status;

pub const LANE_CONTROL: u32 = 0;
pub const LANE_TERM: u32 = 1;
pub const LANE_LOGS: u32 = 2;

/// Protocol version; bumped on incompatible changes. Agents with another version are refused.
pub const VERSION: u32 = 1;

/// Largest single log chunk an agent sends (fits comfortably inside one lane frame).
pub const LOG_CHUNK: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentMsg {
    Hello(Hello),
    Heartbeat { free_slots: u32, running: Vec<String> },
    Sample(HostSample),
    JobStarted { job: String },
    StepStarted { job: String, idx: u32 },
    StepFinished { job: String, idx: u32, status: Status, exit_code: Option<i32> },
    JobFinished(JobResult),
    JobSample { job: String, rss: u64, cpu_seconds: f64 },
    /// The agent could not take the job (e.g. no slot after a race); server requeues it.
    Reject { job: String, reason: String },
    TermOutput { session: String, data: Vec<u8> },
    TermExit { session: String, code: Option<i32>, error: Option<String> },
    /// The terminals still running on this machine (sent on every (re)connect, so a restarted
    /// server learns about sessions that outlived it).
    TermList { sessions: Vec<TermInfo> },
    /// Bytes that bring a fresh terminal to the session's current state (answer to TermAttach).
    TermReplay { session: String, data: Vec<u8> },
}

/// A terminal session kept by an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TermInfo {
    pub session: String,
    pub cols: u16,
    pub rows: u16,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol: u32,
    pub name: String,
    pub labels: Vec<String>,
    pub slots: u32,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub ephemeral: bool,
    /// Pod agents are launched for exactly one job.
    pub dedicated_job: Option<String>,
    /// Jobs the agent is still executing (after a reconnect).
    pub running: Vec<String>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobResult {
    pub job: String,
    pub status: Status,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub peak_rss: Option<u64>,
    pub cpu_seconds: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ServerMsg {
    Welcome { server_version: String, sample_seconds: u32 },
    Refused { reason: String },
    Assign(Box<Assignment>),
    Cancel { job: String, reason: String },
    /// Stop accepting jobs and exit once idle (ephemeral agents, upgrades).
    Drain,
    TermOpen { session: String, cols: u16, rows: u16, command: Option<String> },
    TermInput { session: String, data: Vec<u8> },
    TermResize { session: String, cols: u16, rows: u16 },
    TermClose { session: String },
    /// Send the session's replay (TermReplay); live output keeps flowing as TermOutput.
    TermAttach { session: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub job_id: String,
    pub run_id: String,
    pub repo: String,
    pub clone_url: String,
    pub sha: String,
    pub branch: String,
    pub event: String,
    pub run_number: i64,
    pub attempt: u32,
    pub spec: PlannedJob,
    /// Already resolved secrets; masked from logs by the agent.
    pub secrets: Vec<(String, String)>,
    /// Bearer token scoped to this job for the source tarball and artifact upload endpoints.
    pub job_token: String,
    /// Base URL of the server HTTP API.
    pub server_url: String,
}

/// One chunk of output for a job's step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogChunk {
    pub job: String,
    pub idx: u32,
    pub data: Vec<u8>,
}

pub fn encode<T: Serialize>(v: &T) -> anyhow::Result<Vec<u8>> {
    postcard::to_allocvec(v).map_err(|e| anyhow::anyhow!("encode: {e}"))
}

pub fn decode<'a, T: Deserialize<'a>>(b: &'a [u8]) -> anyhow::Result<T> {
    postcard::from_bytes(b).map_err(|e| anyhow::anyhow!("decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() -> anyhow::Result<()> {
        let m = AgentMsg::StepFinished {
            job: "j".into(),
            idx: 3,
            status: Status::Failed,
            exit_code: Some(2),
        };
        let b = encode(&m)?;
        assert_eq!(decode::<AgentMsg>(&b)?, m);
        assert!(decode::<AgentMsg>(&[0xff, 0xff, 0xff]).is_err());
        Ok(())
    }
}
