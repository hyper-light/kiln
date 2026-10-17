//! The server's side of runners' front doors: their settings (in the `kv` table, one key per
//! runner), what each runner last reported, the certificates the kiln CA issues for them, and the
//! API (`/api/runners/:name/front-door`, `/ca.pem`, `/ca.mobileconfig`).
//!
//! * **Pushing.** A runner that announces `CAP_FRONT_DOOR` gets its settings when it connects
//!   and whenever they change. While a machine runs two runner processes (an upgrade beside the
//!   old one), the newest runs the front door and the older ones are told to stand by.
//! * **Issuing.** A status with a CSR asks for a certificate: signed by the kiln CA when the
//!   runner's settings use kiln's certificate and HTTPS is on, for the names those settings serve
//!   and the addresses the runner reports, at most once a minute per runner.
//! * **Uploads.** A certificate and key uploaded for a runner are checked (they parse, the key
//!   is the certificate's, it serves TLS, it has not expired) and sent to the runner, which keeps
//!   them; the server keeps neither, logs neither, and never sends the key back.
//! * **Renewing now.** Asked to, the server tells the runner to renew its self-signed
//!   certificate (a new key, a CSR for it at once) and lets that CSR past the once-a-minute limit.
//! * **Downloads.** A runner's status carries the chain it serves, certificates only; the server
//!   hands it out (`GET …/front-door/cert`) and leaves it out of the view.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use http::{StatusCode, header};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;

use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;
use crate::instances::PeerId;
use crate::net::Kind;
use crate::proto::{CAP_FRONT_DOOR, LANE_CONTROL, ServerMsg};

use super::{CertMode, CustomCert, Push, Settings, Status};

const KEY_PREFIX: &str = "front_door/";
/// At most one certificate a minute for a runner.
const ISSUE_EVERY: Duration = Duration::from_secs(60);
/// An uploaded chain and key, as JSON.
const UPLOAD_LIMIT: usize = 256 * 1024;
/// A runner process that has announced no build this long after connecting never will: it is
/// from before builds were announced, so from before front doors.
const BUILD_GRACE: i64 = 15;

pub type Answer = Result<Response, (StatusCode, String)>;

fn fail(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

#[derive(Clone, Debug, Default)]
struct Entry {
    settings: Settings,
    status: Option<Status>,
    status_at: i64,
    issued_at: Option<Instant>,
}

impl Entry {
    /// The runner's MagicDNS name, while its front door serves HTTPS on the tailnet.
    fn tailnet_name(&self) -> Option<&str> {
        let s = &self.settings;
        let name = self.status.as_ref()?.tailscale.as_ref()?.name.as_str();
        (s.tailscale.enabled && s.https.enabled && !name.is_empty()).then_some(name)
    }
}

/// Every runner's front door, as the server knows it.
#[derive(Default)]
pub struct FrontDoors {
    inner: Mutex<HashMap<String, Entry>>,
}

impl FrontDoors {
    /// Read the stored settings (at server start).
    pub async fn load(&self, store: &crate::store::Store) -> anyhow::Result<()> {
        let rows = store.kv_prefix(KEY_PREFIX.to_owned()).await?;
        let mut g = self.inner.lock();
        for (k, v) in rows {
            let Some(runner) = k.strip_prefix(KEY_PREFIX) else { continue };
            match serde_json::from_str::<Settings>(&v).map_err(|e| e.to_string()).and_then(|s| s.validated()) {
                Ok(settings) => {
                    g.entry(runner.to_owned()).or_default().settings = settings;
                }
                Err(e) => tracing::warn!(runner, error = %e, "front door: stored settings unreadable; that runner's front door is off"),
            }
        }
        Ok(())
    }

    pub fn settings(&self, runner: &str) -> Settings {
        self.inner.lock().get(runner).map(|e| e.settings.clone()).unwrap_or_default()
    }

    fn status(&self, runner: &str) -> (Option<Status>, i64) {
        self.inner.lock().get(runner).map(|e| (e.status.clone(), e.status_at)).unwrap_or_default()
    }

    /// `https://…` for every name a runner's front door serves.
    pub fn origins(&self) -> Vec<String> {
        let mut runners: Vec<String> = self.inner.lock().keys().cloned().collect();
        runners.sort();
        let mut out: Vec<String> = Vec::new();
        for r in runners {
            for o in self.origins_of(&r) {
                if !out.contains(&o) {
                    out.push(o);
                }
            }
        }
        out
    }

    /// `https://…` for every name one runner's front door serves (its MagicDNS name too, on the
    /// tailnet).
    pub fn origins_of(&self, runner: &str) -> Vec<String> {
        let g = self.inner.lock();
        let Some(e) = g.get(runner) else { return Vec::new() };
        let port = if e.settings.https.port == super::HTTPS_PORT { String::new() } else { format!(":{}", e.settings.https.port) };
        let mut out = e.settings.origins();
        if let Some(o) = e.tailnet_name().map(|n| format!("https://{n}{port}"))
            && !out.contains(&o)
        {
            out.push(o);
        }
        out
    }

    /// Whether `host` (no port) is a name some runner's front door serves over HTTPS: a request
    /// for it came through a front door, so the browser is on HTTPS.
    pub fn serves(&self, host: &str) -> bool {
        self.inner.lock().values().any(|e| e.settings.serves(host) || e.tailnet_name().is_some_and(|n| n.eq_ignore_ascii_case(host)))
    }

    /// Every runner's settings and last status, in name order (for the tailnet's DNS).
    pub fn snapshot(&self) -> Vec<(String, Settings, Option<Status>)> {
        let g = self.inner.lock();
        let mut v: Vec<(String, Settings, Option<Status>)> = g.iter().map(|(k, e)| (k.clone(), e.settings.clone(), e.status.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// The runners with front-door settings, in name order.
    fn runners(&self) -> Vec<String> {
        let mut v: Vec<String> = self.inner.lock().iter().filter(|(_, e)| e.settings != Settings::default() || e.status.is_some()).map(|(k, _)| k.clone()).collect();
        v.sort();
        v
    }

    /// Forget a runner (it was forgotten itself).
    pub async fn forget(&self, store: &crate::store::Store, runner: &str) {
        self.inner.lock().remove(runner);
        if let Err(e) = store.kv_delete(format!("{KEY_PREFIX}{runner}")).await {
            tracing::warn!(runner, error = %format!("{e:#}"), "front door: settings not deleted");
        }
    }
}

// ------------------------------------------------------------------------------ to runners

/// The process of `machine` that runs its front door (its newest, when that one can), and its
/// other processes that can run one (they stand by).
fn peers(hub: &Hub, machine: &str) -> (Option<PeerId>, Vec<PeerId>) {
    let inst = hub.instances.lock();
    let newest = inst.newest(machine);
    let capable = |p: PeerId| inst.get(p).is_some_and(|i| i.caps & CAP_FRONT_DOOR != 0);
    let others = inst.of(machine).into_iter().filter(|p| Some(*p) != newest && capable(*p)).collect();
    (newest.filter(|p| capable(*p)), others)
}

async fn send(hub: &Hub, peer: PeerId, push: &Push) -> bool {
    let Ok(json) = serde_json::to_string(push) else { return false };
    match crate::proto::encode(&ServerMsg::FrontDoor { json }) {
        Ok(b) => hub.net.send(peer, LANE_CONTROL, Kind::Control, b).await,
        Err(_) => false,
    }
}

/// Send `machine` its front door (to the process that runs it; the others stand by), with a
/// certificate or an uploaded one when given. Whether the process running it got it.
pub async fn push(hub: &Hub, machine: &str, issued: Option<String>, custom: Option<CustomCert>) -> bool {
    let (runs, others) = peers(hub, machine);
    for p in others {
        send(hub, p, &Push { standby: true, ..Push::default() }).await;
    }
    let Some(peer) = runs else { return false };
    let settings = hub.front_doors.settings(machine);
    send(hub, peer, &Push { settings, standby: false, issued, custom, renew: false }).await
}

/// A runner process said what it can do (`AgentMsg::Build`): one that runs front doors gets
/// its machine's (and its other processes stand by). Whether it can is known from now on, so
/// the UI asks again.
pub async fn on_build(hub: &Hub, machine: &str, caps: u32) {
    if caps & CAP_FRONT_DOOR != 0 {
        push(hub, machine, None, None).await;
    }
    hub.emit(&json!({ "type": "front_door", "runner": machine }));
}

/// What the process that runs a machine's front door can do, once that is known: unknown from
/// its connecting until its `AgentMsg::Build`, which follows Hello within moments; nothing, for a
/// process too old to announce a build (from before instances, or silent past [`BUILD_GRACE`]).
fn caps_known(i: Option<&crate::instances::Instance>, now: i64) -> Option<u32> {
    let i = i?;
    (i.build.is_some() || i.id.is_none() || now - i.connected_at > BUILD_GRACE).then_some(i.caps)
}

/// A runner process of `machine` went away: another one may run its front door now.
pub async fn on_instance_lost(hub: &Hub, machine: &str) {
    push(hub, machine, None, None).await;
}

/// A runner reported its front door (`AgentMsg::FrontDoorStatus`).
pub async fn on_status(hub: &Hub, peer: PeerId, machine: &str, json: &str) {
    let status: Status = match serde_json::from_str(json) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(machine, error = %e, "front door: unreadable status");
            return;
        }
    };
    // Only the process that runs it speaks for the machine.
    if status.standby || peers(hub, machine).0 != Some(peer) {
        return;
    }
    // Where the tailnet's DNS should go, as this runner says: a change is synced to the tailnet.
    let tailnet_of = |s: &Status| (s.settings.clone(), s.tailscale.as_ref().map(|t| t.nameserver.clone()));
    let (changed, tailnet_changed) = {
        let mut g = hub.front_doors.inner.lock();
        let e = g.entry(machine.to_owned()).or_default();
        let changed = e.status.as_ref() != Some(&status);
        let tailnet_changed = e.status.as_ref().map(tailnet_of) != Some(tailnet_of(&status));
        e.status = Some(status.clone());
        e.status_at = crate::util::now_unix();
        (changed, tailnet_changed)
    };
    if changed {
        hub.emit(&json!({ "type": "front_door", "runner": machine }));
    }
    if tailnet_changed {
        hub.tailnet.wake.notify();
    }
    if let Some(csr) = &status.csr {
        issue(hub, peer, machine, csr, &status).await;
    }
}

/// The names a certificate for `status` may carry: those `settings` serve, and the runner's own
/// addresses (not loopback, wildcard or multicast ones).
fn names_to_sign(settings: &Settings, status: &Status) -> Vec<String> {
    let served = settings.cert_names();
    let mut names = Vec::new();
    for n in &status.want {
        let ok = if let Ok(ip) = n.parse::<IpAddr>() {
            status.addresses.contains(n) && !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
        } else {
            // The runner's MagicDNS name too, while its front door is on the tailnet.
            served.contains(n) || (settings.tailscale.enabled && status.tailscale.as_ref().is_some_and(|t| !t.name.is_empty() && t.name == *n))
        };
        if ok && !names.contains(n) {
            names.push(n.clone());
        }
    }
    names
}

async fn issue(hub: &Hub, peer: PeerId, machine: &str, csr: &str, status: &Status) {
    let settings = hub.front_doors.settings(machine);
    // Asked for settings that are not (or no longer) the runner's: it asks again with the new ones.
    if !(settings.https.enabled && settings.cert.mode == CertMode::Kiln) || status.settings != settings.fingerprint() {
        return;
    }
    {
        let mut g = hub.front_doors.inner.lock();
        let e = g.entry(machine.to_owned()).or_default();
        if e.issued_at.is_some_and(|t| t.elapsed() < ISSUE_EVERY) {
            return;
        }
        e.issued_at = Some(Instant::now());
    }
    let names = names_to_sign(&settings, status);
    if names.is_empty() {
        return;
    }
    let pem = match hub.pki.sign_front_door(csr, &names, crate::pki::FRONT_DOOR_DAYS) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(machine, error = %format!("{e:#}"), "front door: certificate not issued");
            return;
        }
    };
    let until = crate::pki::cert_info(&pem).map(|i| i.not_after).unwrap_or_default();
    tracing::info!(machine, names = ?names, "front door: issued a certificate from the kiln CA");
    hub.store.audit(machine.to_owned(), "front_door.certificate".into(), format!("kiln CA certificate for {} until {}", names.join(", "), date(until))).await;
    let settings = hub.front_doors.settings(machine);
    send(hub, peer, &Push { settings, standby: false, issued: Some(pem), custom: None, renew: false }).await;
}

fn date(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| t.format(&time::macros::format_description!("[year]-[month]-[day]")).ok())
        .unwrap_or_else(|| unix.to_string())
}

// ------------------------------------------------------------------------------ the API

/// A runner the server knows (enrolled, or connected now).
async fn known(hub: &Hub, name: &str) -> bool {
    hub.agents.lock().contains_key(name) || hub.store.agents().await.is_ok_and(|rows| rows.iter().any(|a| a.name == name))
}

/// `GET /api/runners/:name/front-door` (and what the others answer with).
pub async fn view(hub: &Hub, name: &str) -> serde_json::Value {
    let settings = hub.front_doors.settings(name);
    let (status, status_at) = hub.front_doors.status(name);
    let connected = hub.agents.lock().get(name).is_some_and(|a| a.connected);
    // Unknown (null: "waiting for the runner") until the runner has said what it can do.
    let caps = {
        let inst = hub.instances.lock();
        caps_known(inst.newest(name).and_then(|p| inst.get(p)), crate::util::now_unix())
    };
    let supported = if connected { caps.map(|c| c & CAP_FRONT_DOOR != 0) } else { None };
    let applied = status.as_ref().map(|s| s.settings == settings.fingerprint());
    let status = status.map(|mut s| {
        s.csr = None;
        s.cert.pem = None;
        s
    });
    let ca = crate::pki::cert_info_der(&hub.pki.ca_der).ok();
    json!({
        "runner": name,
        "connected": connected,
        "supported": supported,
        "settings": settings,
        "applied": applied,
        "status": status,
        "status_at": (status_at > 0).then_some(status_at),
        "origins": hub.front_doors.origins_of(name),
        "ca": ca.map(|c| json!({ "subject": c.subject, "sha256": c.sha256, "not_after": c.not_after })),
    })
}

pub async fn get(hub: &Hub, name: &str) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    Ok(srv::json(StatusCode::OK, &view(hub, name).await))
}

/// `GET /api/front-doors`: every runner with front-door settings.
pub async fn list(hub: &Hub) -> Answer {
    let mut out = Vec::new();
    for r in hub.front_doors.runners() {
        out.push(view(hub, &r).await);
    }
    Ok(srv::json(StatusCode::OK, &out))
}

/// `PUT /api/runners/:name/front-door`: new settings, checked, stored, and sent to the runner.
pub async fn put(hub: &Hub, name: &str, req: Request) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let asked: Settings = srv::body_json(req.into_body(), 64 * 1024).await.map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let settings = asked.validated().map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
    let json = serde_json::to_string(&settings).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    hub.store.kv_set(format!("{KEY_PREFIX}{name}"), json).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.front_doors.inner.lock().entry(name.to_owned()).or_default().settings = settings.clone();
    hub.store.audit("admin".into(), "front_door.update".into(), format!("{name}: {}", settings.summary())).await;
    push(hub, name, None, None).await;
    hub.tailnet.wake.notify();
    hub.emit(&json!({ "type": "front_door", "runner": name }));
    Ok(srv::json(StatusCode::OK, &view(hub, name).await))
}

#[derive(Deserialize)]
struct Upload {
    cert_pem: String,
    key_pem: String,
}

/// `POST /api/runners/:name/front-door/cert`: a certificate chain and its key for the runner's
/// front door. Checked here, handed to the runner (which must be connected: only it keeps them),
/// never stored here, never logged, never sent back.
pub async fn upload(hub: &Hub, name: &str, req: Request) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let up: Upload = srv::body_json(req.into_body(), UPLOAD_LIMIT)
        .await
        .map_err(|_| fail(StatusCode::BAD_REQUEST, "expected {\"cert_pem\": \"…\", \"key_pem\": \"…\"}"))?;
    crate::http::tls::ServerTls::from_pem(&up.cert_pem, &up.key_pem).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let info = crate::pki::cert_info(&up.cert_pem).map_err(|e| fail(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if info.not_after <= crate::util::now_unix() {
        return Err(fail(StatusCode::BAD_REQUEST, format!("that certificate expired on {}", date(info.not_after))));
    }
    if !info.server_auth {
        return Err(fail(StatusCode::BAD_REQUEST, "that certificate is not for TLS servers (its extended key usage lacks serverAuth)"));
    }
    if peers(hub, name).0.is_none() {
        return Err(fail(
            StatusCode::CONFLICT,
            format!("{name} is not connected (or too old for a front door): the key is only ever kept on the runner, so it has to be online to take it"),
        ));
    }
    let mut settings = hub.front_doors.settings(name);
    settings.cert.mode = CertMode::Custom;
    let json = serde_json::to_string(&settings).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    hub.store.kv_set(format!("{KEY_PREFIX}{name}"), json).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    hub.front_doors.inner.lock().entry(name.to_owned()).or_default().settings = settings;
    let sent = push(hub, name, None, Some(CustomCert { cert_pem: up.cert_pem, key_pem: up.key_pem })).await;
    if !sent {
        return Err(fail(StatusCode::BAD_GATEWAY, format!("{name} could not be reached; try again")));
    }
    let short: String = info.sha256.chars().take(16).collect();
    hub.store
        .audit("admin".into(), "front_door.cert".into(), format!("{name}: uploaded a certificate for {} (sha256 {short}…, until {})", info.names.join(", "), date(info.not_after)))
        .await;
    hub.emit(&json!({ "type": "front_door", "runner": name }));
    Ok(srv::json(StatusCode::OK, &json!({ "cert": info, "front_door": view(hub, name).await })))
}

/// Why a runner's front door cannot renew its certificate now, if it cannot.
fn renewable(settings: &Settings, name: &str) -> Result<(), String> {
    if !settings.https.enabled {
        return Err(format!("HTTPS is off on {name}: turn it on and save first"));
    }
    if settings.cert.mode != CertMode::Kiln {
        return Err(format!("{name} serves your own certificate: renewing is for the self-signed one (upload a new certificate instead)"));
    }
    Ok(())
}

/// `POST /api/runners/:name/front-door/cert/renew`: a new key and self-signed certificate now.
/// The runner makes the key and asks with a CSR at once; the CA signs it as soon as it arrives.
pub async fn renew(hub: &Hub, name: &str) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    renewable(&hub.front_doors.settings(name), name).map_err(|e| fail(StatusCode::CONFLICT, e))?;
    let Some(peer) = peers(hub, name).0 else {
        return Err(fail(StatusCode::CONFLICT, format!("{name} is not connected (or too old for a front door): its key is made there, so it has to be online")));
    };
    // The new key's CSR is signed as soon as it arrives, even within a minute of the last one.
    if let Some(e) = hub.front_doors.inner.lock().get_mut(name) {
        e.issued_at = None;
    }
    let settings = hub.front_doors.settings(name);
    if !send(hub, peer, &Push { settings, renew: true, ..Push::default() }).await {
        return Err(fail(StatusCode::BAD_GATEWAY, format!("{name} could not be reached; try again")));
    }
    hub.store.audit("admin".into(), "front_door.renew".into(), format!("{name}: asked for a new key and self-signed certificate")).await;
    hub.emit(&json!({ "type": "front_door", "runner": name }));
    Ok(srv::json(StatusCode::OK, &view(hub, name).await))
}

/// The file name a downloaded certificate gets: its first name (a wildcard's domain), else the
/// runner's, `.pem`.
fn pem_filename(info: Option<&super::CertInfo>, runner: &str) -> String {
    let first = info.and_then(|i| i.names.iter().find(|n| n.parse::<IpAddr>().is_err()).cloned()).unwrap_or_default();
    let name: String = first.trim_start_matches("*.").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
    let name = name.trim_matches('.');
    if name.is_empty() { format!("{}.pem", crate::pki::sanitize_name(runner)) } else { format!("{name}.pem") }
}

/// `GET /api/runners/:name/front-door/cert`: the certificate chain the runner's HTTPS serves, as
/// it last reported it (PEM, certificates only: never a key).
pub async fn download(hub: &Hub, name: &str) -> Answer {
    if !known(hub, name).await {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runner named {name}")));
    }
    let (status, _) = hub.front_doors.status(name);
    let cert = status.map(|s| s.cert).unwrap_or_default();
    let Some(pem) = cert.pem.and_then(|p| crate::pki::public_chain_pem(&p).ok()) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("{name} serves no certificate (yet)")));
    };
    let r = srv::response(StatusCode::OK, "application/x-pem-file", srv::full(pem.into_bytes()));
    let r = srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"{}\"", pem_filename(cert.info.as_ref(), name)));
    Ok(srv::with_header(r, header::CACHE_CONTROL, "no-cache"))
}

/// `GET /ca.pem`, `/ca.crt` (DER) and `/ca.mobileconfig`: kiln's CA certificate for devices,
/// open (it is public).
pub fn ca(hub: &Arc<Hub>, which: &str) -> Response {
    let (mime, file, body): (&str, &str, Vec<u8>) = match which {
        "crt" => ("application/x-x509-ca-cert", "kiln-ca.crt", hub.pki.ca_der.clone()),
        "mobileconfig" => ("application/x-apple-aspen-config", "kiln-ca.mobileconfig", super::mobileconfig::profile(&hub.pki.ca_der).into_bytes()),
        _ => ("application/x-pem-file", "kiln-ca.pem", hub.pki.ca_pem.clone().into_bytes()),
    };
    let r = srv::response(StatusCode::OK, mime, srv::full(body));
    let r = srv::with_header(r, header::CONTENT_DISPOSITION, &format!("attachment; filename=\"{file}\""));
    srv::with_header(r, header::CACHE_CONTROL, "no-cache")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontdoor::{Https, Mdns};

    #[test]
    fn certificates_carry_only_the_names_settings_serve_and_the_runners_addresses() {
        let settings = Settings {
            domain: "kiln.home".into(),
            aliases: vec!["ci.home".into()],
            https: Https { enabled: true, ..Https::default() },
            mdns: Mdns { enabled: true, name: "kiln".into() },
            ..Settings::default()
        };
        let status = Status {
            want: ["kiln.home", "*.kiln.home", "ci.home", "kiln.local", "evil.example", "kiln-server", "192.168.1.77", "10.0.0.1", "127.0.0.1", "kiln.home"]
                .map(str::to_owned)
                .to_vec(),
            addresses: vec!["192.168.1.77".into(), "127.0.0.1".into()],
            ..Status::default()
        };
        assert_eq!(names_to_sign(&settings, &status), ["kiln.home", "*.kiln.home", "ci.home", "kiln.local", "192.168.1.77"]);
        assert_eq!(date(1_825_000_000), "2027-10-31");
    }

    /// A runner that has just connected has not said what it can do yet: unknown, not "too old",
    /// until its Build arrives; one that never says (too old to) is known to have nothing.
    #[test]
    fn whether_a_runner_runs_front_doors_is_unknown_until_it_says() {
        let hello = crate::proto::Hello {
            protocol: crate::proto::VERSION,
            name: "rocket".into(),
            labels: vec![],
            slots: 1,
            hostname: "rocket".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            version: "0.1.0".into(),
            ephemeral: false,
            dedicated_job: None,
            running: vec![],
            terminal: true,
        };
        let supported = |m: &crate::instances::Instances, peer: PeerId, now: i64| caps_known(m.get(peer), now).map(|c| c & CAP_FRONT_DOOR != 0);
        let mut m = crate::instances::Instances::default();
        m.announce(7, crate::proto::InstanceHello { id: "new".into(), created_at: 100, pid: 1 });
        m.join(7, "rocket", hello.clone(), 1_000);
        assert_eq!(supported(&m, 7, 1_000), None, "just connected: not known yet");
        assert_eq!(supported(&m, 7, 1_000 + BUILD_GRACE), None, "still within the grace");
        m.set_build(7, "build-1".into(), CAP_FRONT_DOOR | crate::proto::CAP_UPGRADE);
        assert_eq!(supported(&m, 7, 1_001), Some(true));
        m.set_build(7, "build-0".into(), crate::proto::CAP_UPGRADE);
        assert_eq!(supported(&m, 7, 1_001), Some(false), "it said so: too old for a front door");
        // A process that never announces a build is too old once the grace is over.
        m.announce(8, crate::proto::InstanceHello { id: "silent".into(), created_at: 200, pid: 2 });
        m.join(8, "quiet", hello.clone(), 1_000);
        assert_eq!(supported(&m, 8, 1_000 + BUILD_GRACE + 1), Some(false));
        // One from before instances says nothing, ever: known at once.
        m.join(9, "legacy", hello, 1_000);
        assert_eq!(supported(&m, 9, 1_000), Some(false));
        assert_eq!(caps_known(None, 1_000), None, "no process: not known");
    }

    #[test]
    fn only_a_self_signed_certificate_with_https_on_renews() {
        let on = Settings { domain: "kiln.dev".into(), https: Https { enabled: true, ..Https::default() }, ..Settings::default() };
        assert_eq!(renewable(&on, "rocket"), Ok(()));
        let off = Settings { https: Https { enabled: false, ..Https::default() }, ..on.clone() };
        assert!(renewable(&off, "rocket").err().is_some_and(|e| e.contains("HTTPS is off")));
        let own = Settings { cert: crate::frontdoor::Cert { mode: CertMode::Custom }, ..on };
        assert!(renewable(&own, "rocket").err().is_some_and(|e| e.contains("your own")));
    }

    #[test]
    fn downloads_are_named_after_the_certificate() {
        let info = |names: &[&str]| super::super::CertInfo { names: names.iter().map(|n| (*n).to_owned()).collect(), ..super::super::CertInfo::default() };
        assert_eq!(pem_filename(Some(&info(&["kiln.dev", "*.kiln.dev"])), "rocket"), "kiln.dev.pem");
        assert_eq!(pem_filename(Some(&info(&["*.kiln.dev"])), "rocket"), "kiln.dev.pem");
        assert_eq!(pem_filename(Some(&info(&["192.168.1.77", "ci.home"])), "rocket"), "ci.home.pem");
        assert_eq!(pem_filename(Some(&info(&["192.168.1.77"])), "Rocket Box"), "rocket-box.pem");
        assert_eq!(pem_filename(Some(&info(&["\"evil\";.x"])), "rocket"), "evil.x.pem", "nothing that breaks the header");
        assert_eq!(pem_filename(None, "rocket"), "rocket.pem");
    }

    #[test]
    fn origins_come_from_every_front_door_with_https() {
        let doors = FrontDoors::default();
        {
            let mut g = doors.inner.lock();
            g.entry("b".into()).or_default().settings = Settings { domain: "kiln.home".into(), https: Https { enabled: true, ..Https::default() }, ..Settings::default() };
            g.entry("a".into()).or_default().settings = Settings { domain: "ci.lan".into(), https: Https { enabled: false, ..Https::default() }, ..Settings::default() };
        }
        assert_eq!(doors.origins(), ["https://kiln.home"]);
        assert!(doors.serves("kiln.home") && doors.serves("x.kiln.home"));
        assert!(!doors.serves("ci.lan"), "HTTPS is off there");
        assert_eq!(doors.runners(), ["a", "b"]);
    }
}
