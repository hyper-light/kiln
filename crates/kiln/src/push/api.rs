//! `/api/push` (docs/notifications.md): whether push is available and with which key, each
//! person's devices, a test, and the agents they muted. A device is the person's who turned it on;
//! nobody sees or removes another's.
//!
//! | Route | |
//! |---|---|
//! | `GET /api/push` | `{enabled, available, why, public_key, kinds, muted, reached}` |
//! | `GET /api/push/subscriptions` | `{devices}`: yours |
//! | `POST /api/push/subscriptions` | `{subscription, device, hears?}`: a browser's subscription (again: the same device) |
//! | `DELETE /api/push/subscriptions/:id` | forgets one of yours |
//! | `POST /api/push/test` | `{id?}`: a test to your devices (or one), each tried once: `{sent, results}` |
//! | `PUT`/`DELETE /api/push/mutes/:agent` | mutes, or unmutes, an agent for you |

use std::sync::Arc;

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::{Service, send, store};
use crate::authz::Caller;
use crate::authz::gate::Kind;
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

type Fail = (StatusCode, String);

/// Whose devices a caller's are: `owner` or `user:<id>`. An agent or a job has none.
pub fn principal(caller: &Caller) -> Result<String, Fail> {
    match (caller.kind, &caller.user) {
        (Kind::Agent | Kind::Job, _) => Err((StatusCode::FORBIDDEN, "notifications are a person's: sign in as yourself to choose your devices".into())),
        (Kind::Owner, _) => Ok("owner".into()),
        (_, Some(u)) => Ok(format!("user:{u}")),
        (_, None) if caller.is_owner() => Ok("owner".into()),
        _ => Err((StatusCode::FORBIDDEN, "this token acts for nobody who has devices".into())),
    }
}

fn store_err(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// Why devices can't be given notifications here, when they can't.
async fn unavailable(hub: &Arc<Hub>) -> Option<String> {
    if !hub.cfg.push.enabled {
        return Some("Push is off on this server ([push] enabled = false in kiln.toml).".into());
    }
    Service::keys(hub).await.err().map(|e| format!("This server's push key can't be used: {e}"))
}

pub async fn route(hub: &Arc<Hub>, caller: &Caller, method: &Method, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let who = principal(caller)?;
    match (method, rest) {
        (&Method::GET, []) => {
            let why = unavailable(hub).await;
            let key = if why.is_none() { Service::keys(hub).await.ok().map(|k| k.public.clone()) } else { None };
            let p = who.clone();
            let muted = hub.store.call(move |c| store::muted(c, &p)).await.map_err(store_err)?;
            let reached = if hub.cfg.push.enabled { Some(Service::reached(hub)) } else { None };
            Ok(srv::json(StatusCode::OK, &json!({ "enabled": hub.cfg.push.enabled, "available": why.is_none(), "why": why, "public_key": key, "kinds": store::kinds_json(), "muted": muted, "reached": reached })))
        }
        (&Method::GET, ["subscriptions"]) => {
            let devices = hub.store.call(move |c| store::list(c, &who)).await.map_err(store_err)?;
            Ok(srv::json(StatusCode::OK, &json!({ "devices": devices })))
        }
        (&Method::POST, ["subscriptions"]) => {
            if let Some(why) = unavailable(hub).await {
                return Err((StatusCode::CONFLICT, why));
            }
            let mut n: store::NewDevice = srv::body_json(req.into_body(), 16 << 10).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
            // The push service changed a device's subscription (the service worker says so): its
            // name and kinds go on.
            let (p, old) = (who.clone(), n.replaces.clone());
            let before = match old {
                Some(e) => hub.store.call(move |c| store::by_endpoint(c, &p, &e)).await.map_err(store_err)?,
                None => None,
            };
            if let Some(b) = &before {
                if n.device.trim().is_empty() {
                    n.device.clone_from(&b.device);
                }
                if n.hears.is_none() {
                    n.hears = Some(b.hears.clone());
                }
            }
            let (name, hears) = store::check_new(&n, &hub.cfg.push.allow_private).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            let (p, label) = (who.clone(), name.clone());
            let d = hub
                .store
                .call(move |c| {
                    if !store::room_for(c, &p, &n.subscription.endpoint)? && before.is_none() {
                        return Ok(None);
                    }
                    let d = store::upsert(c, &p, &n, &name, &hears, crate::util::now_unix())?;
                    if let Some(b) = before.filter(|b| b.id != d.id) {
                        store::remove(c, &p, &b.id)?;
                    }
                    Ok(Some(d))
                })
                .await
                .map_err(store_err)?
                .ok_or_else(|| (StatusCode::CONFLICT, format!("you have {} devices already, the most there may be: remove one first", store::MAX_DEVICES)))?;
            hub.store.audit(caller.name.clone(), "push.device".into(), format!("{label} hears {}", d.hears.join(", "))).await;
            hub.emit(&json!({ "type": "push" }));
            Ok(srv::json(StatusCode::CREATED, &d))
        }
        (&Method::DELETE, ["subscriptions", id]) => {
            let id = (*id).to_owned();
            let gone = hub.store.call(move |c| store::remove(c, &who, &id)).await.map_err(store_err)?;
            if !gone {
                return Err((StatusCode::NOT_FOUND, "no such device of yours".into()));
            }
            hub.store.audit(caller.name.clone(), "push.device.remove".into(), String::new()).await;
            hub.emit(&json!({ "type": "push" }));
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        (&Method::POST, ["test"]) => {
            if let Some(why) = unavailable(hub).await {
                return Err((StatusCode::CONFLICT, why));
            }
            let body: Value = srv::body_json(req.into_body(), 4 << 10).await.unwrap_or(Value::Null);
            let one = body.get("id").and_then(Value::as_str).map(str::to_owned);
            let devices = hub.store.call(move |c| store::list(c, &who)).await.map_err(store_err)?;
            let devices: Vec<store::Device> = devices.into_iter().filter(|d| one.as_ref().is_none_or(|id| &d.id == id)).collect();
            if devices.is_empty() {
                return Err((StatusCode::NOT_FOUND, "no device of yours gets notifications yet: turn them on in Settings first".into()));
            }
            let payload = serde_json::to_vec(&json!({ "title": "kiln", "body": "Notifications reach this device.", "tag": "kiln-test", "url": "/settings", "kind": "test", "at": crate::util::now_ms() })).unwrap_or_default();
            // Each tried once, at once: Settings says how it went.
            let tries = devices.iter().map(|d| super::deliver(hub, d, &payload, "normal", None, 300, &[0]));
            let outcomes = futures::future::join_all(tries).await;
            let results: Vec<Value> = devices
                .iter()
                .zip(outcomes)
                .map(|(d, o)| match o {
                    send::Outcome::Delivered => json!({ "id": d.id, "device": d.device, "ok": true }),
                    send::Outcome::Gone(s) => json!({ "id": d.id, "device": d.device, "ok": false, "error": format!("its push service no longer knows it (HTTP {s}): it's removed; turn notifications on there again") }),
                    send::Outcome::Failed(why) => json!({ "id": d.id, "device": d.device, "ok": false, "error": why }),
                })
                .collect();
            let sent = results.iter().filter(|r| r.get("ok") == Some(&Value::Bool(true))).count();
            Ok(srv::json(StatusCode::OK, &json!({ "sent": sent, "results": results })))
        }
        (&Method::PUT | &Method::DELETE, ["mutes", agent]) => {
            let (agent, on) = ((*agent).to_owned(), method == Method::PUT);
            let a = agent.clone();
            hub.store.call(move |c| if on { store::mute(c, &who, &a, crate::util::now_unix()) } else { store::unmute(c, &who, &a) }).await.map_err(store_err)?;
            hub.store.audit(caller.name.clone(), if on { "push.mute".into() } else { "push.unmute".into() }, agent).await;
            hub.emit(&json!({ "type": "push" }));
            Ok(srv::status(StatusCode::NO_CONTENT))
        }
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_are_a_persons() {
        assert_eq!(principal(&Caller::owner()).ok().as_deref(), Some("owner"));
        let mut user = Caller::with(Kind::User, "user:ada", Some(crate::authz::Rights::default()), None);
        user.user = Some("u1".into());
        assert_eq!(principal(&user).ok().as_deref(), Some("user:u1"));
        let mut agent = Caller::with(Kind::Agent, "agent:x for user:ada", Some(crate::authz::Rights::default()), None);
        agent.user = Some("u1".into());
        assert_eq!(principal(&agent).map_err(|e| e.0), Err(StatusCode::FORBIDDEN), "an agent can't take its person's notifications");
        assert_eq!(principal(&Caller::with(Kind::Job, "job:1", Some(crate::authz::Rights::default()), None)).map_err(|e| e.0), Err(StatusCode::FORBIDDEN));
    }
}
