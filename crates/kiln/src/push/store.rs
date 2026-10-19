//! What the server keeps for notifications (migration `20261019-0100_n1_push.sql`): each device a
//! person turned them on for, the agents they muted, and what was last said of each agent. Every
//! function here is one statement or two on a connection: call them inside `Store::call`.

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a device can hear, named as the events are (rules and webhooks get the same ones): its
/// id, its name in Settings, and what it means.
pub const KINDS: &[(&str, &str, &str)] = &[
    ("agent.needs_you", "Needs you", "an approval, a question or a plan waits on you"),
    ("agent.finished", "Finished", "a turn ended"),
    ("agent.stopped", "Stopped", "it crashed or couldn't start"),
    ("agent.paused", "Paused", "it reached a usage limit"),
    ("machine.battery_low", "Low battery", "a machine your agents run on is low"),
];

/// The longest endpoint kiln keeps (push services' are a few hundred characters).
const MAX_ENDPOINT: usize = 2048;
/// The longest device name.
const MAX_NAME: usize = 80;
/// The most devices one person has: each note is a request to each of them.
pub const MAX_DEVICES: usize = 20;

/// One device a person turned notifications on for.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Device {
    pub id: String,
    /// Whose it is: `owner` or `user:<id>`.
    #[serde(skip)]
    pub principal: String,
    /// The browser's push endpoint (its own page tells itself apart by it).
    pub endpoint: String,
    /// The device's P-256 public key and auth secret, base64url: never sent back.
    #[serde(skip)]
    pub p256dh: String,
    #[serde(skip)]
    pub auth: String,
    /// What the person calls it ("Safari on iPhone").
    pub device: String,
    /// What it hears ([`KINDS`]).
    pub hears: Vec<String>,
    pub created_at: i64,
    /// When the last delivery went through (Unix seconds).
    pub last_ok_at: Option<i64>,
    /// Why the last one didn't, and when (cleared by the next that does).
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

/// A subscription as a browser gives it (`PushSubscription.toJSON()`), with a name and what it
/// hears.
#[derive(Debug, Clone, Deserialize)]
pub struct NewDevice {
    pub subscription: BrowserSubscription,
    #[serde(default)]
    pub device: String,
    /// Absent: everything (or, with `replaces`, what that one heard).
    #[serde(default)]
    pub hears: Option<Vec<String>>,
    /// The endpoint this one takes over from (the service worker's, when the push service
    /// changed the subscription): that device's name and kinds go on, and it's forgotten.
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserSubscription {
    pub endpoint: String,
    pub keys: BrowserKeys,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BrowserKeys {
    pub p256dh: String,
    pub auth: String,
}

/// Whether a push may go to `endpoint`: an `https://` URL; plain `http://` only to a host
/// `allow` names (a push service of your own, or a test's). Where it resolves is checked when it
/// is sent.
pub fn check_endpoint(endpoint: &str, allow: &[String]) -> Result<(), String> {
    if endpoint.len() > MAX_ENDPOINT {
        return Err(format!("a push endpoint is at most {MAX_ENDPOINT} characters"));
    }
    let uri: http::Uri = endpoint.parse().map_err(|_| "the push endpoint isn't a URL".to_owned())?;
    let host = uri.host().ok_or_else(|| "the push endpoint has no host".to_owned())?;
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        return Err("a push endpoint carries no user name or password".into());
    }
    match uri.scheme_str() {
        Some("https") => Ok(()),
        Some("http") => {
            let port = uri.port_u16().unwrap_or(80);
            let ip: Option<std::net::IpAddr> = host.trim_start_matches('[').trim_end_matches(']').parse().ok();
            let named = allow.iter().any(|a| match (crate::discovery::parse_cidr(a), ip) {
                (Some((net, len)), Some(ip)) => crate::discovery::in_net(ip, net, len),
                _ => kiln_ci::trust::host_matches(a, host, port),
            });
            if named { Ok(()) } else { Err("a push endpoint is https:// (plain http:// only to a host [push] allow_private names)".into()) }
        }
        _ => Err("a push endpoint is an https:// URL".into()),
    }
}

/// A browser's subscription, checked: its endpoint, its keys (a P-256 point and a 16-byte secret
/// a message can be encrypted for), a name, and kinds kiln knows.
pub fn check_new(n: &NewDevice, allow: &[String]) -> Result<(String, Vec<String>), String> {
    check_endpoint(n.subscription.endpoint.trim(), allow)?;
    let key = super::vapid::unb64(&n.subscription.keys.p256dh).ok_or_else(|| "keys.p256dh isn't base64url".to_owned())?;
    let auth = super::vapid::unb64(&n.subscription.keys.auth).ok_or_else(|| "keys.auth isn't base64url".to_owned())?;
    super::ece::encrypt(b"{}", &key, &auth).map_err(|e| format!("the subscription's keys can't be used: {e}"))?;
    let hears = match &n.hears {
        None => KINDS.iter().map(|(k, _, _)| (*k).to_owned()).collect(),
        Some(list) => {
            if let Some(bad) = list.iter().find(|k| !KINDS.iter().any(|(id, _, _)| id == k)) {
                return Err(format!("no kind of notification `{bad}` (kinds: {})", KINDS.iter().map(|(k, _, _)| *k).collect::<Vec<_>>().join(", ")));
            }
            KINDS.iter().map(|(k, _, _)| (*k).to_owned()).filter(|k| list.contains(k)).collect()
        }
    };
    let name: String = n.device.trim().chars().take(MAX_NAME).collect();
    Ok((if name.is_empty() { "A device".to_owned() } else { name }, hears))
}

const COLUMNS: &str = "id, principal, endpoint, p256dh, auth, device, hears, created_at, last_ok_at, last_error, last_error_at";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    let hears: String = r.get(6)?;
    Ok(Device {
        id: r.get(0)?,
        principal: r.get(1)?,
        endpoint: r.get(2)?,
        p256dh: r.get(3)?,
        auth: r.get(4)?,
        device: r.get(5)?,
        hears: serde_json::from_str(&hears).unwrap_or_default(),
        created_at: r.get(7)?,
        last_ok_at: r.get(8)?,
        last_error: r.get(9)?,
        last_error_at: r.get(10)?,
    })
}

/// Keeps a device for `principal`: a new one, or the one with that endpoint again (its keys,
/// name and kinds as given now, its id and history kept; whoever subscribes it last owns it).
pub fn upsert(c: &Connection, principal: &str, n: &NewDevice, name: &str, hears: &[String], now: i64) -> anyhow::Result<Device> {
    let endpoint = n.subscription.endpoint.trim();
    let hears = serde_json::to_string(hears)?;
    c.execute(
        "INSERT INTO push_subscriptions (id, principal, endpoint, p256dh, auth, device, hears, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT (endpoint) DO UPDATE SET principal = ?2, p256dh = ?4, auth = ?5, device = ?6, hears = ?7, updated_at = ?8",
        params![crate::util::new_id(), principal, endpoint, n.subscription.keys.p256dh.trim(), n.subscription.keys.auth.trim(), name, hears, now],
    )?;
    Ok(c.query_row(&format!("SELECT {COLUMNS} FROM push_subscriptions WHERE endpoint = ?1"), [endpoint], row)?)
}

/// Whether `principal` may add a device with `endpoint` (one they have already is always fine).
pub fn room_for(c: &Connection, principal: &str, endpoint: &str) -> anyhow::Result<bool> {
    let have: i64 = c.query_row("SELECT COUNT(*) FROM push_subscriptions WHERE principal = ?1 AND endpoint != ?2", [principal, endpoint.trim()], |r| r.get(0))?;
    Ok(usize::try_from(have).unwrap_or(usize::MAX) < MAX_DEVICES)
}

/// `principal`'s devices, oldest first.
pub fn list(c: &Connection, principal: &str) -> anyhow::Result<Vec<Device>> {
    let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM push_subscriptions WHERE principal = ?1 ORDER BY created_at, id"))?;
    let rows = st.query_map([principal], row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// `principal`'s devices that hear `kind`.
pub fn hearing(c: &Connection, principal: &str, kind: &str) -> anyhow::Result<Vec<Device>> {
    Ok(list(c, principal)?.into_iter().filter(|d| d.hears.iter().any(|h| h == kind)).collect())
}

/// One of `principal`'s devices (`None`: not theirs, or gone).
pub fn get(c: &Connection, principal: &str, id: &str) -> anyhow::Result<Option<Device>> {
    Ok(c.query_row(&format!("SELECT {COLUMNS} FROM push_subscriptions WHERE id = ?1 AND principal = ?2"), [id, principal], row).optional()?)
}

/// `principal`'s device with this endpoint.
pub fn by_endpoint(c: &Connection, principal: &str, endpoint: &str) -> anyhow::Result<Option<Device>> {
    Ok(c.query_row(&format!("SELECT {COLUMNS} FROM push_subscriptions WHERE endpoint = ?1 AND principal = ?2"), [endpoint.trim(), principal], row).optional()?)
}

/// Removes one of `principal`'s devices; whether there was one.
pub fn remove(c: &Connection, principal: &str, id: &str) -> anyhow::Result<bool> {
    Ok(c.execute("DELETE FROM push_subscriptions WHERE id = ?1 AND principal = ?2", [id, principal])? > 0)
}

/// A device its push service says is gone (404, 410).
pub fn drop_endpoint(c: &Connection, endpoint: &str) -> anyhow::Result<bool> {
    Ok(c.execute("DELETE FROM push_subscriptions WHERE endpoint = ?1", [endpoint])? > 0)
}

/// A delivery went through.
pub fn delivered(c: &Connection, id: &str, now: i64) -> anyhow::Result<()> {
    c.execute("UPDATE push_subscriptions SET last_ok_at = ?2, last_error = NULL, last_error_at = NULL WHERE id = ?1", params![id, now])?;
    Ok(())
}

/// A delivery didn't, and why.
pub fn failed(c: &Connection, id: &str, error: &str, now: i64) -> anyhow::Result<()> {
    c.execute("UPDATE push_subscriptions SET last_error = ?2, last_error_at = ?3 WHERE id = ?1", params![id, error, now])?;
    Ok(())
}

pub fn mute(c: &Connection, principal: &str, agent: &str, now: i64) -> anyhow::Result<()> {
    c.execute("INSERT INTO push_mutes (principal, agent, at) VALUES (?1, ?2, ?3) ON CONFLICT (principal, agent) DO NOTHING", params![principal, agent, now])?;
    Ok(())
}

pub fn unmute(c: &Connection, principal: &str, agent: &str) -> anyhow::Result<()> {
    c.execute("DELETE FROM push_mutes WHERE principal = ?1 AND agent = ?2", [principal, agent])?;
    Ok(())
}

/// The agents `principal` muted.
pub fn muted(c: &Connection, principal: &str) -> anyhow::Result<Vec<String>> {
    let mut st = c.prepare("SELECT agent FROM push_mutes WHERE principal = ?1 ORDER BY at")?;
    let rows = st.query_map([principal], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn is_muted(c: &Connection, principal: &str, agent: &str) -> anyhow::Result<bool> {
    Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM push_mutes WHERE principal = ?1 AND agent = ?2)", [principal, agent], |r| r.get(0))?)
}

/// What was last said of each agent, by kind (`(agent, kind) → key`).
pub fn said(c: &Connection) -> anyhow::Result<Vec<(String, String, String)>> {
    let mut st = c.prepare("SELECT agent, kind, key FROM push_said")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn say(c: &Connection, agent: &str, kind: &str, key: &str, now: i64) -> anyhow::Result<()> {
    c.execute("INSERT INTO push_said (agent, kind, key, at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (agent, kind) DO UPDATE SET key = ?3, at = ?4", params![agent, kind, key, now])?;
    Ok(())
}

/// An agent that's gone: nothing more is said of it, and nobody's mute of it is kept.
pub fn forget(c: &Connection, agent: &str) -> anyhow::Result<()> {
    c.execute("DELETE FROM push_said WHERE agent = ?1", [agent])?;
    c.execute("DELETE FROM push_mutes WHERE agent = ?1", [agent])?;
    Ok(())
}

/// What GET /api/push lists as kinds.
pub fn kinds_json() -> Value {
    Value::Array(KINDS.iter().map(|(id, label, means)| serde_json::json!({ "id": id, "label": label, "means": means })).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser(endpoint: &str) -> anyhow::Result<NewDevice> {
        let device = aws_lc_rs::agreement::PrivateKey::generate(&aws_lc_rs::agreement::ECDH_P256).map_err(|e| anyhow::anyhow!("{e}"))?;
        let public = device.compute_public_key().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(NewDevice {
            subscription: BrowserSubscription { endpoint: endpoint.into(), keys: BrowserKeys { p256dh: super::super::vapid::b64(public.as_ref()), auth: super::super::vapid::b64([3u8; 16]) } },
            device: "Safari on iPhone".into(),
            hears: None,
            replaces: None,
        })
    }

    #[test]
    fn endpoints_are_https_or_named() {
        assert!(check_endpoint("https://web.push.apple.com/QGz3", &[]).is_ok());
        assert!(check_endpoint("http://127.0.0.1:9001/push/1", &[]).is_err());
        assert!(check_endpoint("http://127.0.0.1:9001/push/1", &["127.0.0.1".into()]).is_ok());
        assert!(check_endpoint("http://push.lan:9001/x", &["push.lan".into()]).is_ok());
        assert!(check_endpoint("http://10.1.2.3/x", &["10.0.0.0/8".into()]).is_ok());
        assert!(check_endpoint("https://user:pw@web.push.apple.com/x", &[]).is_err());
        assert!(check_endpoint("ftp://x/y", &[]).is_err());
        assert!(check_endpoint(&format!("https://x/{}", "a".repeat(MAX_ENDPOINT)), &[]).is_err());
    }

    #[test]
    fn a_new_device_is_checked() -> anyhow::Result<()> {
        let mut n = browser("https://fcm.googleapis.com/fcm/send/a")?;
        let (name, hears) = check_new(&n, &[]).map_err(anyhow::Error::msg)?;
        assert_eq!(name, "Safari on iPhone");
        assert_eq!(hears.len(), KINDS.len(), "everything unless said");
        n.hears = Some(vec!["agent.finished".into(), "agent.needs_you".into()]);
        assert_eq!(check_new(&n, &[]).map_err(anyhow::Error::msg)?.1, vec!["agent.needs_you".to_owned(), "agent.finished".to_owned()], "in kiln's order");
        n.hears = Some(vec!["agent.exploded".into()]);
        assert!(check_new(&n, &[]).is_err());
        n.hears = None;
        n.subscription.keys.auth = "AAAA".into();
        assert!(check_new(&n, &[]).is_err(), "an auth secret that isn't 16 bytes");
        let mut bad = browser("https://fcm.googleapis.com/fcm/send/a")?;
        bad.subscription.keys.p256dh = super::super::vapid::b64([4u8; 65]);
        assert!(check_new(&bad, &[]).is_err(), "not a point on the curve");
        Ok(())
    }

    #[test]
    fn devices_mutes_and_what_was_said() -> anyhow::Result<()> {
        let store = crate::store::Store::in_memory()?;
        crate::rt::block_on(async move {
            store
                .call(|c| {
            let n = browser("https://web.push.apple.com/one")?;
            let a = upsert(c, "owner", &n, "Safari on iPhone", &["agent.needs_you".to_owned()], 100)?;
            // The same endpoint again: the same device, its kinds as given now.
            let b = upsert(c, "owner", &n, "Safari on iPhone", &["agent.needs_you".to_owned(), "agent.finished".to_owned()], 200)?;
            assert_eq!(a.id, b.id);
            assert_eq!(b.hears.len(), 2);
            let other = upsert(c, "user:u1", &browser("https://fcm.googleapis.com/two")?, "Chrome on Linux", &["agent.finished".to_owned()], 300)?;
            assert_eq!(list(c, "owner")?.len(), 1);
            assert_eq!(hearing(c, "owner", "agent.finished")?.len(), 1);
            assert!(hearing(c, "user:u1", "agent.needs_you")?.is_empty());
            // Nobody removes another's device.
            assert!(!remove(c, "owner", &other.id)?);
            assert!(get(c, "owner", &other.id)?.is_none());
            // How deliveries went.
            failed(c, &a.id, "HTTP 403: BadJwtToken", 400)?;
            assert_eq!(get(c, "owner", &a.id)?.and_then(|d| d.last_error), Some("HTTP 403: BadJwtToken".to_owned()));
            delivered(c, &a.id, 500)?;
            let d = get(c, "owner", &a.id)?.ok_or_else(|| anyhow::anyhow!("gone"))?;
            assert_eq!((d.last_ok_at, d.last_error), (Some(500), None));
            // Twenty devices each, at most; one already there may always come again.
            for n in 0..MAX_DEVICES {
                upsert(c, "user:u2", &browser(&format!("https://fcm.googleapis.com/many/{n}"))?, "Chrome", &["agent.finished".to_owned()], 1)?;
            }
            assert!(!room_for(c, "user:u2", "https://fcm.googleapis.com/many/new")?);
            assert!(room_for(c, "user:u2", "https://fcm.googleapis.com/many/3")?);
            assert!(room_for(c, "owner", "https://fcm.googleapis.com/many/new")?);
            assert!(drop_endpoint(c, "https://web.push.apple.com/one")?);
            assert!(list(c, "owner")?.is_empty());
            // Mutes are each person's.
            mute(c, "owner", "ag1", 1)?;
            mute(c, "owner", "ag1", 2)?;
            assert!(is_muted(c, "owner", "ag1")? && !is_muted(c, "user:u1", "ag1")?);
            assert_eq!(muted(c, "owner")?, vec!["ag1".to_owned()]);
            unmute(c, "owner", "ag1")?;
            assert!(muted(c, "owner")?.is_empty());
            // What was said, by agent and kind; forgotten with the agent.
            say(c, "ag1", "agent.needs_you", "a@1", 1)?;
            say(c, "ag1", "agent.needs_you", "a@1,b@2", 2)?;
            say(c, "ag1", "agent.paused", "77", 3)?;
            assert_eq!(said(c)?.len(), 2);
            forget(c, "ag1")?;
            assert!(said(c)?.is_empty());
            Ok(())
                })
                .await
        })
    }
}
