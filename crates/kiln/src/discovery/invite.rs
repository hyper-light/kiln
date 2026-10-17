//! Invitations: an admin picks a machine a server found (`POST /api/discovery/invite`), and that
//! machine, waiting without anyone at its keyboard, joins that server through the usual join
//! request, which the admin still approves after comparing the verification code. This is what a
//! machine that hears several servers and has no terminal to ask does, instead of giving up.
//!
//! * **The server** keeps invitations in memory for 30 minutes, by instance id, and answers
//!   `GET /api/join/invited?instance=<id>` (open: the machine has no credentials yet) with
//!   `{invited, ca}`; each source address may ask about five times in a burst, then once a second.
//! * **The machine** (`kiln agent --discover` or `kiln join` without `--server`, not enrolled)
//!   announces itself and asks every server it hears every ten seconds. The first that invited it
//!   is joined; a server whose `ca` is not the one it announces is not believed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Deserialize;

use crate::rt::Shutdown;

use super::{Instance, Table};

pub const INVITE_TTL: Duration = Duration::from_secs(1800);
const MAX_INVITES: usize = 256;
/// The open route's rate limit per source address: a burst, then one a second.
const BURST: f64 = 5.0;
const PER_SECOND: f64 = 1.0;
const MAX_SOURCES: usize = 1024;
/// How often a waiting machine asks each server.
const ASK_EVERY: Duration = Duration::from_secs(10);
/// A server that does not know invitations (an older kiln) is asked again this much later.
const UNSUPPORTED_BACKOFF: Duration = Duration::from_secs(300);

/// An instance id as announced: 1–64 ASCII letters and digits.
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The server's invitations and the open route's rate limit.
#[derive(Default)]
pub struct Invites {
    inner: Mutex<HashMap<String, Instant>>,
    limits: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl Invites {
    /// Invite instance `id` for [`INVITE_TTL`]. False for an id no instance could have.
    pub fn invite(&self, id: &str, now: Instant) -> bool {
        if !valid_id(id) {
            return false;
        }
        let id = id.to_ascii_lowercase();
        let mut g = self.inner.lock();
        g.retain(|_, until| *until > now);
        if g.len() >= MAX_INVITES
            && !g.contains_key(&id)
            && let Some(oldest) = g.iter().min_by_key(|(_, t)| **t).map(|(k, _)| k.clone())
        {
            g.remove(&oldest);
        }
        g.insert(id, now + INVITE_TTL);
        true
    }

    /// Withdraw an invitation; false when there was none.
    pub fn revoke(&self, id: &str) -> bool {
        self.inner.lock().remove(&id.to_ascii_lowercase()).is_some()
    }

    pub fn invited(&self, id: &str, now: Instant) -> bool {
        self.inner.lock().get(&id.to_ascii_lowercase()).is_some_and(|until| *until > now)
    }

    pub fn purge(&self, now: Instant) {
        self.inner.lock().retain(|_, until| *until > now);
        self.limits.lock().retain(|_, (_, at)| now.saturating_duration_since(*at) < Duration::from_secs(60));
    }

    /// The open route's rate limit: `Err(wait)` when `from` asked too often.
    pub fn allow(&self, from: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut g = self.limits.lock();
        if g.len() >= MAX_SOURCES && !g.contains_key(&from) {
            g.retain(|_, (_, at)| now.saturating_duration_since(*at) < Duration::from_secs(10));
            if g.len() >= MAX_SOURCES {
                return Err(Duration::from_secs(10));
            }
        }
        let (tokens, at) = g.entry(from).or_insert((BURST, now));
        let refill = now.saturating_duration_since(*at).as_secs_f64() * PER_SECOND;
        *tokens = (*tokens + refill).min(BURST);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64(((1.0 - *tokens) / PER_SECOND).max(0.1)))
        }
    }
}

/// The pending join request a discovered machine made, if any: an agent instance with the
/// request's host name, at the request's address (or the only one with that host name). Older
/// machines send no instance id with their requests, so this goes by what both sides know.
pub fn request_for(i: &Instance, all: &[Instance], requests: &[crate::join::Listed]) -> Option<String> {
    if i.role != "agent" {
        return None;
    }
    let same_host = |r: &crate::join::Listed, x: &Instance| x.role == "agent" && x.name.eq_ignore_ascii_case(&r.hostname);
    requests.iter().filter(|r| r.status == crate::join::Status::Pending && same_host(r, i)).find_map(|r| {
        let at_address = |x: &Instance| x.addresses.iter().any(|a| a.to_string() == r.address);
        let candidates: Vec<&Instance> = all.iter().filter(|x| same_host(r, x)).collect();
        let mine = if at_address(i) { true } else { candidates.len() == 1 && !candidates.iter().any(|x| at_address(x) && x.id != i.id) };
        mine.then(|| r.id.clone())
    })
}

// ------------------------------------------------------------------------------ the machine's side

/// `GET /api/join/invited`'s answer.
#[derive(Deserialize)]
struct Answer {
    invited: bool,
    #[serde(default)]
    ca: Option<String>,
}

/// Ask `server` whether it invited `me`: `Ok(Some(ca))` when it did, `Ok(None)` when not, `Err`
/// when it does not know invitations (an older kiln: 401/404).
async fn ask(client: &crate::http::client::Client, url: &str, me: &str) -> Result<Option<Option<String>>, Duration> {
    let resp = match client.get(&format!("{url}/api/join/invited?instance={me}")).timeout(Duration::from_secs(4)).send().await {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    match resp.status() {
        200 => match resp.json::<Answer>(16 << 10).await {
            Ok(a) if a.invited => Ok(Some(a.ca)),
            _ => Ok(None),
        },
        429 => {
            let secs = resp.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(10).clamp(1, 600);
            Err(Duration::from_secs(secs))
        }
        401 | 404 => Err(UNSUPPORTED_BACKOFF),
        _ => Ok(None),
    }
}

fn describe(i: &Instance) -> String {
    format!("{} ({})", i.name, i.url().unwrap_or_default())
}

/// Wait until a server `table` lists invites this machine (instance `me`): its URL. With
/// `single`, a lone server that stays the only one for ten seconds is joined without an
/// invitation (as before invitations existed), unless more than one was ever heard.
pub async fn wait_for_invitation(table: &Table, me: &str, single: bool, shutdown: &Shutdown, say: &(dyn Fn(&str) + Send + Sync)) -> anyhow::Result<String> {
    let client = crate::http::client::Client::public()?;
    let short = me.get(..8).unwrap_or(me).to_owned();
    say(&format!(
        "Waiting for a kiln server to invite this machine (it announces itself as \"… ({short})\"): in the server's UI, \
         invite it from the machines found on the network, then approve its join request."
    ));
    let mut next_ask: HashMap<String, Instant> = HashMap::new();
    let mut announced: Vec<String> = Vec::new();
    let mut most = 0usize;
    let mut alone_since: Option<(String, Instant)> = None;
    loop {
        let now = Instant::now();
        let servers = crate::join::joinable(table.snapshot());
        most = most.max(servers.len());
        for s in &servers {
            if !announced.contains(&s.id) {
                announced.push(s.id.clone());
                say(&format!("Heard {}; asking it for an invitation every {} s.", describe(s), ASK_EVERY.as_secs()));
            }
        }
        for s in &servers {
            let Some(url) = s.url() else { continue };
            if next_ask.get(&s.id).is_some_and(|t| *t > now) {
                continue;
            }
            match ask(&client, &url, me).await {
                Ok(Some(ca)) => {
                    // The answer must come from the server that was heard (same CA as announced).
                    if let (Some(theirs), Some(heard)) = (ca.as_deref(), s.ca.as_deref())
                        && !theirs.eq_ignore_ascii_case(heard)
                    {
                        say(&format!("{} says it invited this machine, but its CA is not the one it announces; ignored.", describe(s)));
                        next_ask.insert(s.id.clone(), now + UNSUPPORTED_BACKOFF);
                        continue;
                    }
                    say(&format!("Invited by {}.", describe(s)));
                    return Ok(url);
                }
                Ok(None) => {
                    next_ask.insert(s.id.clone(), now + ASK_EVERY);
                }
                Err(wait) => {
                    next_ask.insert(s.id.clone(), now + wait);
                }
            }
        }
        if single && most <= 1 {
            match (servers.as_slice(), &alone_since) {
                ([only], Some((id, since))) if *id == only.id && since.elapsed() >= Duration::from_secs(10) => {
                    if let Some(url) = only.url() {
                        say(&format!("Found only {}; asking to join it.", describe(only)));
                        return Ok(url);
                    }
                }
                ([only], Some((id, _))) if *id == only.id => {}
                ([only], _) => alone_since = Some((only.id.clone(), now)),
                _ => alone_since = None,
            }
        }
        if !shutdown.sleep(Duration::from_secs(2)).await {
            anyhow::bail!("interrupted while waiting for an invitation");
        }
    }
}

/// A machine announcing itself while it waits to be invited; `end` says goodbye.
pub struct Waiting {
    announcer: super::AgentAnnouncer,
    stop: Shutdown,
}

impl Waiting {
    pub async fn end(self) {
        self.stop.cancel();
        self.announcer.finish().await;
    }
}

/// `kiln join` without `--server`: the server to join, found on the network. One server, or a
/// terminal to choose with, is as before; several and no terminal wait for one of them to invite
/// this machine, announcing it meanwhile (the returned [`Waiting`] keeps announcing until the join
/// is done). `None`: an enrolled machine, which `join::cli` asks about first.
pub async fn server_for_join(a: &crate::join::JoinArgs, settings: &crate::config::Discovery, shutdown: &Shutdown) -> anyhow::Result<Option<(String, Option<Waiting>)>> {
    let state_dir = crate::config::default_data_dir().join("agent");
    if crate::join::enrolled(&state_dir) && !a.yes {
        return Ok(None);
    }
    let interactive = crate::join::interactive();
    crate::join::say_stderr("Looking for kiln servers on this network…");
    let found = crate::join::find_servers(Duration::from_secs(a.wait.clamp(1, 60)), &settings.interfaces).await?;
    if found.len() <= 1 || interactive {
        return Ok(Some((crate::join::choose(found, a.yes, interactive, shutdown).await?, None)));
    }
    crate::join::say_stderr(&format!(
        "Found {} kiln servers and no terminal to ask which one to join:\n  {}",
        found.len(),
        found.iter().map(describe).collect::<Vec<_>>().join("\n  ")
    ));
    let stop = Shutdown::new();
    {
        // The announcer stops with the process too.
        let (outer, inner) = (shutdown.clone(), stop.clone());
        crate::rt::spawn(async move {
            futures::future::select(Box::pin(outer.cancelled()), Box::pin(inner.cancelled())).await;
            inner.cancel();
        });
    }
    let announcer = super::AgentAnnouncer::start(&state_dir, None, settings.clone(), stop.clone());
    let say = |m: &str| crate::join::say_stderr(m);
    let url = wait_for_invitation(&announcer.table, &announcer.id(), false, shutdown, &say).await?;
    Ok(Some((url, Some(Waiting { announcer, stop }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitations_live_thirty_minutes() {
        let inv = Invites::default();
        let now = Instant::now();
        assert!(!inv.invite("not/an id", now));
        assert!(inv.invite("ABCdef0123", now));
        assert!(inv.invited("abcdef0123", now));
        assert!(inv.invited("abcdef0123", now + INVITE_TTL - Duration::from_secs(1)));
        assert!(!inv.invited("abcdef0123", now + INVITE_TTL));
        assert!(inv.revoke("abcdef0123"));
        assert!(!inv.revoke("abcdef0123"));
        for n in 0..MAX_INVITES + 3 {
            inv.invite(&format!("id{n}"), now + Duration::from_millis(u64::try_from(n).unwrap_or(0)));
        }
        assert_eq!(inv.inner.lock().len(), MAX_INVITES);
        assert!(!inv.invited("id0", now));
    }

    #[test]
    fn the_open_route_is_rate_limited() {
        let inv = Invites::default();
        let now = Instant::now();
        let ip: IpAddr = [192, 168, 1, 9].into();
        for _ in 0..5 {
            assert!(inv.allow(ip, now).is_ok());
        }
        assert!(inv.allow(ip, now).is_err_and(|wait| wait <= Duration::from_secs(1)));
        // Another address has its own budget; a second later there is room again.
        assert!(inv.allow([192, 168, 1, 10].into(), now).is_ok());
        assert!(inv.allow(ip, now + Duration::from_secs(1)).is_ok());
    }

    fn agent(id: &str, host: &str, ip: [u8; 4]) -> Instance {
        let pairs = vec![("id".to_owned(), id.to_owned()), ("name".to_owned(), host.to_owned()), ("role".to_owned(), "agent".to_owned())];
        super::super::instance_from(&format!("kiln agent on {host} ({id})"), 0, &pairs, vec![IpAddr::from(ip)], super::super::Via::Mdns, Instant::now(), Duration::from_secs(120))
            .unwrap_or_else(|| agent("x", "x", [0, 0, 0, 0]))
    }

    fn request(id: &str, host: &str, address: &str, status: crate::join::Status) -> crate::join::Listed {
        crate::join::Listed {
            id: id.into(),
            name: host.into(),
            hostname: host.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            labels: vec![],
            version: "0.1.0".into(),
            address: address.into(),
            status,
            code: None,
            created_at: 0,
            expires_at: 0,
            agent: None,
        }
    }

    #[test]
    fn join_requests_are_matched_to_instances() {
        let a = agent("aaaa1111", "bench", [192, 168, 1, 20]);
        let b = agent("bbbb2222", "Bench", [192, 168, 1, 21]);
        let c = agent("cccc3333", "laptop", [192, 168, 1, 30]);
        let all = vec![a.clone(), b.clone(), c.clone()];
        let reqs = vec![request("r1", "bench", "192.168.1.21", crate::join::Status::Pending), request("r2", "laptop", "10.8.0.4", crate::join::Status::Pending)];
        // Two machines called bench: the address decides.
        assert_eq!(request_for(&b, &all, &reqs).as_deref(), Some("r1"));
        assert_eq!(request_for(&a, &all, &reqs), None);
        // The only laptop, though the request came through another address (a VPN).
        assert_eq!(request_for(&c, &all, &reqs).as_deref(), Some("r2"));
        // Decided requests and servers are not marked.
        let decided = vec![request("r3", "laptop", "192.168.1.30", crate::join::Status::Approved)];
        assert_eq!(request_for(&c, &all, &decided), None);
    }
}
