//! A runner's side of installed skills: what the server says ([`crate::proto::ServerMsg::Skills`],
//! [`crate::proto::ServerMsg::SkillFile`]), kept and laid out by [`super::place`], and what it says
//! back ([`crate::proto::AgentMsg::Skills`], [`crate::proto::AgentMsg::SkillFile`]).

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::place::{self, FileRef, Keeper, Manifest, Place, State};
use crate::exec::Uplink;
use crate::harness::host::Host;
use crate::proto::AgentMsg;

/// This runner's sessions, and its link to the server.
struct Runner {
    up: Uplink,
    host: Option<Arc<Host>>,
}

impl Keeper for Runner {
    fn owner(&self) -> &'static str {
        "runner"
    }

    fn live(&self) -> BTreeSet<String> {
        self.host.as_ref().map(|h| h.list().into_iter().map(|i| i.id).collect()).unwrap_or_default()
    }

    fn reload(&self, sessions: &[String]) {
        if let Some(h) = &self.host {
            for s in sessions {
                super::reload(h, s);
            }
        }
    }

    fn report(&self, states: Vec<State>) -> futures::future::BoxFuture<'static, ()> {
        let up = self.up.clone();
        Box::pin(async move { up.msg(AgentMsg::Skills { json: json!({"states": states}).to_string() }).await })
    }
}

/// [`crate::proto::ServerMsg::Skills`]: what this machine lays out (asking for the files it lacks,
/// then settling it), or a folder to read.
pub fn told(json: &str, up: Uplink, host: Option<Arc<Host>>) {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        tracing::debug!("a skills message this runner couldn't read");
        return;
    };
    if let Some(r) = v.get("read") {
        let req = r.get("req").and_then(Value::as_u64).unwrap_or(0);
        let path = r.get("path").and_then(Value::as_str).unwrap_or("").to_owned();
        crate::rt::spawn(folder(req, path, up));
        return;
    }
    let Some(m) = v.get("manifest").and_then(|m| serde_json::from_value::<Manifest>(m.clone()).ok()) else { return };
    place::remember("runner", &m);
    crate::rt::spawn(async move {
        let want = {
            let m = m.clone();
            crate::rt::blocking(move || Place::new(place::root(), "runner").want(&m)).await.unwrap_or_default()
        };
        if !want.is_empty() {
            up.msg(AgentMsg::Skills { json: json!({"want": want}).to_string() }).await;
        }
        place::settle(Arc::new(Runner { up, host })).await;
    });
}

/// [`crate::proto::ServerMsg::SkillFile`]: a piece of a file it asked for; once the manifest's
/// last file is here, it's settled again.
pub async fn file(sha256: String, offset: u64, data: Vec<u8>, done: bool, up: Uplink, host: Option<Arc<Host>>) {
    let got = crate::rt::blocking(move || {
        let place = Place::new(place::root(), "runner");
        let kept = place.receive(&sha256, offset, &data, done);
        let complete = done && kept.is_ok() && place::last("runner").is_some_and(|m| place.want(&m).is_empty());
        (kept, complete)
    }).await;
    match got {
        Ok((Err(e), _)) => tracing::warn!(error = %e, "a skill's file came wrong: it's asked for again with the next manifest"),
        Ok((Ok(()), true)) => {
            crate::rt::spawn(place::settle(Arc::new(Runner { up, host })));
        }
        _ => {}
    }
}

/// A folder on this machine, read for skills: its files said, then each one's bytes in pieces.
async fn folder(req: u64, path: String, up: Uplink) {
    let p = super::expand(&path);
    let read = crate::rt::blocking(move || super::source::walk(&p)).await;
    let files = match read {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => return up.msg(AgentMsg::Skills { json: json!({"read": req, "error": e.to_string()}).to_string() }).await,
        Err(e) => return up.msg(AgentMsg::Skills { json: json!({"read": req, "error": format!("{e:#}")}).to_string() }).await,
    };
    let refs: Vec<FileRef> = files.iter().map(|f| FileRef { path: f.path.clone(), sha256: super::hex(&Sha256::digest(&f.bytes)), size: f.bytes.len() as u64, exec: f.exec }).collect();
    up.msg(AgentMsg::Skills { json: json!({"read": req, "files": refs}).to_string() }).await;
    let mut sent = BTreeSet::new();
    for (f, r) in files.iter().zip(&refs) {
        if !sent.insert(r.sha256.clone()) {
            continue;
        }
        let mut offset = 0usize;
        loop {
            let end = (offset + super::PIECE).min(f.bytes.len());
            up.msg(AgentMsg::SkillFile { req, sha256: r.sha256.clone(), offset: offset as u64, data: f.bytes.get(offset..end).unwrap_or_default().to_vec(), done: end >= f.bytes.len() }).await;
            if end >= f.bytes.len() {
                break;
            }
            offset = end;
        }
    }
}
