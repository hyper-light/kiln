//! The server's side of headroom: each runner's settings, kept (`kv` key `headroom/<runner>`)
//! and sent to it when it connects and when they change (`ServerMsg::Headroom`, to runner
//! processes announcing `CAP_HEADROOM`: the machine's newest paces, its others stand by), and
//! what each reports (`AgentMsg::HeadroomStatus`), for the API and the UI.

use std::collections::HashMap;

use http::StatusCode;
use parking_lot::Mutex;
use serde_json::json;

use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::instances::PeerId;
use crate::proto::{CAP_HEADROOM, ServerMsg};

use super::settings::{Push, Settings};

const KEY_PREFIX: &str = "headroom/";

pub type Answer = Result<Response, (StatusCode, String)>;

fn fail(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

#[derive(Clone, Debug, Default)]
struct Entry {
    /// Set by an admin (else the defaults).
    settings: Option<Settings>,
    status: Option<serde_json::Value>,
    status_at: i64,
}

/// Every runner's headroom, as the server knows it.
#[derive(Default)]
pub struct Headrooms {
    inner: Mutex<HashMap<String, Entry>>,
}

impl Headrooms {
    /// Read the stored settings (at server start).
    pub async fn load(&self, store: &crate::store::Store) -> anyhow::Result<()> {
        let rows = store.kv_prefix(KEY_PREFIX.to_owned()).await?;
        let mut g = self.inner.lock();
        for (k, v) in rows {
            let Some(runner) = k.strip_prefix(KEY_PREFIX) else { continue };
            match serde_json::from_str::<Settings>(&v).map_err(|e| e.to_string()).and_then(Settings::validated) {
                Ok(s) => g.entry(runner.to_owned()).or_default().settings = Some(s),
                Err(e) => tracing::warn!(runner, error = %e, "headroom: stored settings unreadable; that runner keeps its own"),
            }
        }
        Ok(())
    }

    pub fn settings(&self, runner: &str) -> Settings {
        self.inner.lock().get(runner).and_then(|e| e.settings.clone()).unwrap_or_default()
    }

    fn runners(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.lock().keys().cloned().collect();
        v.sort();
        v
    }

    /// Forget a runner (it was forgotten itself).
    pub async fn forget(&self, store: &crate::store::Store, runner: &str) {
        self.inner.lock().remove(runner);
        if let Err(e) = store.kv_delete(format!("{KEY_PREFIX}{runner}")).await {
            tracing::warn!(runner, error = %format!("{e:#}"), "headroom: settings not deleted");
        }
    }
}

/// The machine's processes that run headroom: its newest (it paces), and the rest (they stand by).
fn peers(hub: &Hub, machine: &str) -> (Option<PeerId>, Vec<PeerId>) {
    let inst = hub.instances.lock();
    let newest = inst.newest(machine);
    let capable = |p: PeerId| inst.get(p).is_some_and(|i| i.wide & CAP_HEADROOM != 0);
    let others = inst.of(machine).into_iter().filter(|p| Some(*p) != newest && capable(*p)).collect();
    (newest.filter(|p| capable(*p)), others)
}

async fn send(hub: &Hub, machine: &str, peer: PeerId, push: &Push) {
    let Ok(json) = serde_json::to_string(push) else { return };
    let pending = hub.command_peers(machine, vec![peer], &ServerMsg::Headroom { json }).await;
    pending.log_when_answered(crate::hub::LOG_WITHIN);
}

/// Send `machine` its settings: its newest process paces, the others stand by.
pub async fn push(hub: &Hub, machine: &str) {
    let (runs, others) = peers(hub, machine);
    let settings = hub.headrooms.settings(machine);
    for p in others {
        send(hub, machine, p, &Push { settings: settings.clone(), standby: true }).await;
    }
    if let Some(p) = runs {
        send(hub, machine, p, &Push { settings, standby: false }).await;
    }
}

/// A runner process said what it can do past `Build` (`AgentMsg::Caps`): one that runs headroom
/// gets its machine's settings.
pub async fn on_caps(hub: &Hub, machine: &str, caps: u64) {
    if caps & CAP_HEADROOM != 0 {
        push(hub, machine).await;
        hub.emit(&crate::wire::events::ServerEvent::Headroom { runner: machine.to_string() });
    }
}

/// What a runner process reports (`AgentMsg::HeadroomStatus`): kept when it's the machine's newest.
pub fn on_status(hub: &Hub, peer: PeerId, machine: &str, json: &str) {
    let Ok(status) = serde_json::from_str::<serde_json::Value>(json) else { return };
    if peers(hub, machine).0 != Some(peer) {
        return;
    }
    let changed = {
        let mut g = hub.headrooms.inner.lock();
        let e = g.entry(machine.to_owned()).or_default();
        let changed = e.status.as_ref() != Some(&status);
        e.status = Some(status);
        e.status_at = crate::util::now_unix();
        changed
    };
    if changed {
        hub.emit(&crate::wire::events::ServerEvent::Headroom { runner: machine.to_string() });
    }
}

// ------------------------------------------------------------------------------ the API

async fn known(hub: &Hub, name: &str) -> bool {
    hub.agents.lock().contains_key(name) || hub.store.agents().await.is_ok_and(|rows| rows.iter().any(|a| a.name == name))
}

/// `GET /api/runners/:name/headroom` (and what the others answer with).
pub fn view(hub: &Hub, name: &str) -> serde_json::Value {
    let (set, status, status_at) = hub.headrooms.inner.lock().get(name).map(|e| (e.settings.is_some(), e.status.clone(), e.status_at)).unwrap_or_default();
    let settings = hub.headrooms.settings(name);
    let connected = hub.agents.lock().get(name).is_some_and(|a| a.connected);
    let supported = connected.then(|| peers(hub, name).0.is_some());
    let applied = status.as_ref().and_then(|s| s.get("settings")).and_then(|f| f.as_str()).map(|f| f == settings.fingerprint());
    json!({
        "runner": name,
        "connected": connected,
        "supported": supported,
        "settings": settings,
        "customized": set,
        "applied": applied,
        "status": status,
        "status_at": (status_at > 0).then_some(status_at),
    })
}

pub async fn get(hub: &Hub, name: &str) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    Ok(srv::json(StatusCode::OK, &view(hub, name)))
}

/// `GET /api/headroom`: every runner that runs headroom or has settings for it.
pub async fn list(hub: &Hub) -> Answer {
    let mut names = hub.headrooms.runners();
    for (name, a) in hub.agents.lock().iter() {
        if a.connected && !names.contains(name) {
            names.push(name.clone());
        }
    }
    names.sort();
    let out: Vec<serde_json::Value> = names.iter().map(|n| view(hub, n)).filter(|v| v["supported"] == json!(true) || v["customized"] == json!(true) || !v["status"].is_null()).collect();
    Ok(srv::json(StatusCode::OK, &out))
}

/// `PUT /api/runners/:name/headroom`: new settings, checked, stored, and sent to the runner.
pub async fn put(hub: &Hub, name: &str, req: Request) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let asked: Settings = srv::body_json(req.into_body(), 64 * 1024).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let settings = asked.validated().map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let json = serde_json::to_string(&settings).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    hub.store.kv_set(format!("{KEY_PREFIX}{name}"), json).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.headrooms.inner.lock().entry(name.to_owned()).or_default().settings = Some(settings.clone());
    hub.store.audit("admin".into(), "headroom.update".into(), format!("{name}: enabled={} fingerprint={}", settings.enabled, settings.fingerprint())).await;
    push(hub, name).await;
    hub.emit(&crate::wire::events::ServerEvent::Headroom { runner: name.to_string() });
    Ok(srv::json(StatusCode::OK, &view(hub, name)))
}
