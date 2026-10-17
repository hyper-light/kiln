//! The system's service discovery as a discovery method (`via: native`): Bonjour on macOS
//! ([`super::bonjour`]), DNS-SD on Windows 10 1809+ ([`super::windns`]). The system owns UDP 5353
//! and its firewall exemptions; kiln registers its service with it (same service type, instance
//! name and TXT keys as its own responder, so Linux machines running kiln's multicast DNS and
//! these see each other), browses for the others and resolves each one it hears.
//!
//! The platform modules call back on OS threads; their events reach this task through a channel.
//! Resolved instances are listed for three minutes and resolved again every minute while the
//! system still lists them; a removal takes them off at once.

use std::sync::Arc;

use crate::rt::Shutdown;

use super::Discovery;

#[cfg(target_os = "macos")]
use super::bonjour as sys;
#[cfg(windows)]
use super::windns as sys;

/// Whether this OS has a service discovery kiln uses.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", windows));

#[cfg(any(target_os = "macos", windows))]
pub const LABEL: &str = sys::LABEL;
#[cfg(not(any(target_os = "macos", windows)))]
pub const LABEL: &str = "System service discovery";

#[cfg(not(any(target_os = "macos", windows)))]
pub async fn run(_: &Arc<Discovery>, _: &Shutdown, _: &Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<()> {
    anyhow::bail!("this system has no service discovery kiln uses")
}

/// Register and browse through the system until `shutdown`. An error means it could not start
/// (the caller falls back to kiln's own multicast DNS); later failures are retried.
#[cfg(any(target_os = "macos", windows))]
pub async fn run(d: &Arc<Discovery>, shutdown: &Shutdown, on_change: &Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<()> {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::time::{Duration, Instant};

    use futures::FutureExt as _;

    use super::status::{MdnsState, NativeState};
    use super::{SERVICE_TYPE, TXT_KEYS, Via};

    /// How long a resolved instance stays listed without being resolved again.
    const HOLD: Duration = Duration::from_secs(180);
    const RESOLVE_EVERY: Duration = Duration::from_secs(60);

    let (tx, rx) = async_channel::bounded::<sys::Event>(1024);
    let sink: sys::Sink = Box::new(move |ev| {
        // A full channel drops the event: browsing reports again and resolutions are repeated.
        let _ = tx.try_send(ev);
    });
    d.health.lock().native.state = NativeState::Starting;
    let handle = sys::Handle::start(SERVICE_TYPE, &TXT_KEYS, d.browsing(), sink).map_err(anyhow::Error::msg)?;
    match crate::rt::timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Ok(sys::Event::Started)) => {}
        Ok(Ok(sys::Event::Failed { code, .. })) => anyhow::bail!("{LABEL} refused to browse (error {code})"),
        Ok(Ok(_)) => {}
        Ok(Err(_)) => anyhow::bail!("{LABEL} stopped at once"),
        Err(_) => anyhow::bail!("{LABEL} did not answer"),
    }
    {
        let mut h = d.health.lock();
        h.native.state = NativeState::Running;
        h.mdns.state = MdnsState::Off(format!("{LABEL} is used instead"));
    }
    tracing::info!(service = SERVICE_TYPE, announce = d.announcing(), browse = d.browsing(), "discovery: {LABEL} running");
    let register = || {
        if d.announcing() {
            handle.register(service_for(d));
        }
    };
    register();
    // Labels the system lists, with when each was last resolved.
    let mut listed: HashMap<String, Option<Instant>> = HashMap::new();
    enum Ev {
        Stop,
        Changed,
        Event(sys::Event),
        Closed,
        Tick,
    }
    loop {
        let ev = {
            let stop = shutdown.cancelled().fuse();
            let changed = d.changed.notified().fuse();
            let event = rx.recv().fuse();
            let tick = crate::rt::sleep(Duration::from_secs(15)).fuse();
            futures::pin_mut!(stop, changed, event, tick);
            futures::select_biased! {
                () = stop => Ev::Stop,
                () = changed => Ev::Changed,
                e = event => e.map_or(Ev::Closed, Ev::Event),
                () = tick => Ev::Tick,
            }
        };
        let now = Instant::now();
        match ev {
            Ev::Stop => break,
            Ev::Closed => anyhow::bail!("{LABEL} stopped"),
            Ev::Changed => register(),
            Ev::Tick => {}
            Ev::Event(sys::Event::Started) => {}
            Ev::Event(sys::Event::Registered { name }) => {
                let mut h = d.health.lock();
                h.native.registered = Some(name);
                h.native.error = None;
            }
            Ev::Event(sys::Event::Found { label }) => {
                if label != d.advert.lock().instance_label() && listed.len() < 256 {
                    listed.entry(label.clone()).or_insert(None);
                    handle.resolve(&label);
                }
            }
            Ev::Event(sys::Event::Lost { label }) => {
                listed.remove(&label);
                if d.table.forget(&label, &[Via::Native]) {
                    on_change();
                }
            }
            Ev::Event(sys::Event::Resolved { label, host, port, txt, addrs }) => {
                if let Some(at) = listed.get_mut(&label) {
                    *at = Some(now);
                }
                // An IPv6 link-local address carries no usable scope here.
                let addrs: Vec<IpAddr> = addrs.into_iter().filter(|a| !matches!(a, IpAddr::V6(v6) if v6.is_unicast_link_local())).collect();
                match super::instance_from(&label, port, &txt, addrs, Via::Native, now, HOLD) {
                    Some(i) => {
                        d.found(vec![i], Via::Native, &**on_change);
                    }
                    None => tracing::debug!(%label, %host, "a _kiln._tcp service without an id"),
                }
            }
            Ev::Event(sys::Event::Failed { what, code }) => {
                if what == "resolve" {
                    tracing::debug!(code, "{LABEL}: a resolution failed");
                } else {
                    tracing::warn!(what, code, "{LABEL} reported an error; retrying");
                    d.health.lock().native.error = Some(format!("{what} failed (error {code})"));
                }
            }
        }
        // Resolve again what has not been resolved for a while (keeps the entries fresh).
        let due: Vec<String> = listed
            .iter()
            .filter(|(_, at)| at.is_none_or(|t| now.saturating_duration_since(t) >= RESOLVE_EVERY))
            .map(|(l, _)| l.clone())
            .take(16)
            .collect();
        for label in due {
            if let Some(at) = listed.get_mut(&label) {
                // Not again until the next round (a resolution that fails is retried then).
                *at = Some(now);
            }
            handle.resolve(&label);
        }
    }
    // Dropping the handle withdraws the registration.
    drop(handle);
    Ok(())
}

/// What to register: macOS publishes the machine's own host name and addresses itself.
#[cfg(target_os = "macos")]
fn service_for(d: &Discovery) -> sys::Service {
    let a = d.advert.lock().clone();
    sys::Service { label: a.instance_label(), port: a.port, txt: a.txt_pairs() }
}

/// What to register: Windows needs the host name and addresses the SRV record points at.
#[cfg(windows)]
fn service_for(d: &Discovery) -> sys::Service {
    use std::net::IpAddr;
    let a = d.advert.lock().clone();
    let mut addrs: Vec<IpAddr> = Vec::new();
    for i in super::interfaces(&d.settings.interfaces) {
        addrs.extend(i.v4.iter().map(|(ip, _)| IpAddr::V4(*ip)));
        if d.ipv6 {
            addrs.extend(i.v6.iter().map(|ip| IpAddr::V6(*ip)));
        }
    }
    if let Some(route) = super::local_address_for(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1))) {
        addrs.retain(|x| *x != route);
        addrs.insert(0, route);
    }
    sys::Service { label: a.instance_label(), port: a.port, txt: a.txt_pairs(), host: format!("{}.local", a.host_label()), addrs }
}
