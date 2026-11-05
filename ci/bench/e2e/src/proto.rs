//! The reach socket's protocol, as `crates/kiln/src/reach/local.rs` and `reach/wire.rs` define it
//! (copied at lb/bench 3280a3fb; the protocol only grows, so variants keep their indexes): each
//! message a big-endian u32 length and its postcard body. Only what the generator says (`Req`'s
//! first three variants) and all it can hear (`Ev`) are here. A variant kiln appends after these
//! reads as an error the generator counts, never as a wrong answer.

use serde::{Deserialize, Serialize};

// Every variant and field is here for its position (postcard encodes by index), read or not.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
pub enum Req {
    Status,
    Runners { want: Option<String> },
    Run(Box<RunReq>),
}

/// `reach::local::RunReq`, field for field (postcard encodes fields in order, by position).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunReq {
    pub target: String,
    pub command: String,
    pub cwd: Option<String>,
    pub remote_cwd: Option<String>,
    pub sync: bool,
    pub env: Vec<(String, String)>,
    pub stdin: bool,
    pub timeout_ms: Option<u64>,
    pub shell: Option<String>,
    pub detach: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub enum Ev {
    Status(String),
    Runners(String),
    Picked { runner: String, why: String },
    Syncing { files: u64, bytes: u64 },
    Synced { root: String, sent: u32, removed: u32, bytes: u64, ms: u64 },
    Started { runner: String, pid: u32, cwd: String },
    Output { err: bool, data: Vec<u8> },
    Exit { code: Option<i32>, error: Option<String>, lost: u64 },
    Error(String),
    Forgot(bool),
    Answer(String),
    Note(String),
}

/// The reach socket's limit on one message (`reach::wire::MAX_MESSAGE`'s order: a frame longer
/// than this is a broken stream, not a message).
const MAX: usize = 64 << 20;

pub fn frame<T: Serialize>(m: &T) -> Result<Vec<u8>, String> {
    let body = postcard::to_allocvec(m).map_err(|e| format!("encode: {e}"))?;
    let len = u32::try_from(body.len()).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

#[derive(Default)]
pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn take(&mut self) -> Result<Option<Ev>, String> {
        let Some(head) = self.buf.first_chunk::<4>() else { return Ok(None) };
        let len = usize::try_from(u32::from_be_bytes(*head)).unwrap_or(usize::MAX);
        if len > MAX {
            return Err(format!("a frame of {len} bytes"));
        }
        let Some(body) = self.buf.get(4..4 + len) else { return Ok(None) };
        let ev = postcard::from_bytes::<Ev>(body).map_err(|e| format!("decode: {e} (a variant newer than this copy?)"))?;
        self.buf.drain(..4 + len);
        Ok(Some(ev))
    }
}
