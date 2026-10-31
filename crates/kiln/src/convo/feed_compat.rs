//! U15: bounded readers for retired conversation control frames. The postcard envelope stays
//! append-only; decoding this JSON never starts a feed, mirror, recovery or write controller.
use serde::{Deserialize,Serialize};
use super::telemetry::Report;

pub const LANE: u32=crate::proto::LANE_CONVO;
const CONTROL_MAX:usize=64<<10;
const STORE_MAX:usize=256;

/// A message of the feed (JSON; a batch's bytes travel beside it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// Runner to server, when the link comes up (and when the server says nothing for too long):
    /// its store. `restore`: the store is its copy of the server's (a mirror's), sent back for
    /// the server to restore its own from ([`Msg::Restore`]).
    Hello {
        store: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        restore: bool,
    },
    /// Server to runner: it has `store`'s feed through `rev`; send from there. `zip`: it takes
    /// batches compressed; `kept`: it takes record lines as they are kept, compressed
    /// ([`RecordRow::kept`](super::rows::RecordRow::kept)). A taker from before says neither,
    /// and gets them as they are. `about`: the batch it answers (by its `from`), one that didn't
    /// follow or didn't come whole (None: a hello's answer, or a taker from before).
    From {
        store: String,
        rev: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        zip: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        kept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        about: Option<u64>,
    },
    /// Runner to server: part `part` (from 0) of `parts` of a batch that follows `from` and reads
    /// through `upto` (its bytes: JSON [`super::store::Changes`], brotli-compressed when `zip`). `told`: where
    /// the server last told it to go on from; a runner that says so leaves out an answer about
    /// what it has sent since (a runner from before doesn't say, and goes on from every answer).
    Batch {
        store: String,
        from: u64,
        upto: u64,
        part: u32,
        parts: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        zip: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        told: Option<u64>,
    },
    /// Server to runner: it has `store`'s feed through `rev`.
    Ack { store: String, rev: u64 },
    /// Server to a mirror: send back your copy of my store, which is `store` now (the one you
    /// copied was lost).
    Restore { store: String },
    /// Mirror to server: its copy `store` has been sent whole, through `rev`.
    Restored { store: String, rev: u64 },
    /// Runner to server, every few seconds while linked: what its store `store` (its own, or its
    /// copy of the server's) is doing, for the server's telemetry. A server from before takes it
    /// for nothing.
    Stats { store: String, report: Box<Report> },
}

impl Msg {
    /// The store it is about.
    pub fn store(&self) -> &str {
        match self {
            Msg::Hello { store, .. } | Msg::From { store, .. } | Msg::Batch { store, .. } | Msg::Ack { store, .. } | Msg::Restore { store } | Msg::Restored { store, .. } | Msg::Stats { store, .. } => store,
        }
    }
}

#[derive(Debug,thiserror::Error)]
pub enum DecodeError {
    #[error("legacy conversation control exceeds {CONTROL_MAX} bytes")]
    TooLarge,
    #[error("invalid legacy conversation control: {0}")]
    Invalid(String),
}

pub fn decode(json:&str)->Result<Msg,DecodeError> {
    if json.len()>CONTROL_MAX{return Err(DecodeError::TooLarge);}
    let message:Msg=serde_json::from_str(json).map_err(|e|DecodeError::Invalid(e.to_string()))?;
    if message.store().len()>STORE_MAX{return Err(DecodeError::Invalid("store identity exceeds 256 bytes".into()));}
    match &message {
        Msg::Batch {from,upto,part,parts,..} if *parts==0 || *parts>128 || part>=parts || upto<from=>return Err(DecodeError::Invalid("invalid batch coordinates".into())),
        Msg::Stats {report,..} if report.store.waits.len()>super::telemetry::WAIT_BUCKETS=>return Err(DecodeError::Invalid("too many wait buckets".into())),
        _=>{}
    }
    Ok(message)
}
