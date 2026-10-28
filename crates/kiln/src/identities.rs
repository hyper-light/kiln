//! Identities kiln's CA issued, and their revocations, kept (docs/ci.md W27-R01; migration
//! `w27_identities`). Each certificate is recorded as it's issued, by kind and subject (a machine's
//! name, else its kiln:// URI) with its serial and the SHA-256 of its key. A revocation is a row
//! with the time it took effect, why and by whom: the TLS directory refuses revoked keys from
//! then on (loaded at start, so a restart keeps them, and a machine enrolling again under the same
//! name gets a new key the revocation doesn't touch), and [`revocations`] feeds the chain checker
//! (`kiln_ci::identity::chain`).

use kiln_ci::identity::chain::{Actor, Holder, Identity, Revocation};
use rusqlite::{OptionalExtension as _, params};

use crate::hub::Hub;
use crate::instances::PeerId;
use crate::proto::{IdentityAsk, IdentityGiven, ServerMsg};
use crate::store::Store;

/// How long a runner process's certificate lasts; it asks again on each connection.
pub const RUNNER_HOURS: i64 = 7 * 24;
/// How long an agent session's certificate lasts, for one epoch; asked again on each connection.
pub const AGENT_HOURS: i64 = 7 * 24;
/// How long the server's and CD's signing certificates last; renewed once less than 30 days are
/// left ([`own_identities`]).
pub const SERVER_DAYS: i64 = 90;

/// How often the running server looks at its and CD's identities again ([`own_identities_loop`]):
/// well inside the 30 days a certificate is renewed before it expires.
pub const OWN_EVERY: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// The server's and CD's own signing identities (W27-R01): keys made here, in `<data>/pki`
/// (0600, never sent), apart from the server's TLS leaf; certified by kiln's CA and recorded.
/// Looked at as the server starts, every [`OWN_EVERY`] while it runs and after any revocation
/// (`POST /api/identities/revoke`): a certificate missing, expiring or not for its key is issued
/// anew, as is one revoked; a key revoked (lost) is replaced by a new one, and that certified.
pub async fn own_identities(store: &Store, pki: &crate::pki::Pki) -> anyhow::Result<()> {
    let revoked = revocations(store).await?;
    for (kind, actor) in [("server", Actor::Server), ("cd", Actor::Cd)] {
        let identity = Identity::new(pki.organization(), actor);
        let uri = identity.uri().map_err(|e| anyhow::anyhow!("{e}"))?;
        let (key_path, cert_path) = (pki.dir().join(format!("{kind}-signing.key")), pki.dir().join(format!("{kind}-signing.pem")));
        let held = std::fs::read_to_string(&cert_path).ok();
        let marks = held.as_deref().and_then(|c| crate::pki::fingerprints(c).ok());
        let key_revoked = marks.as_ref().is_some_and(|m| revoked.iter().any(|r| r.key.as_deref().is_some_and(|k| k.eq_ignore_ascii_case(&m.key))));
        let cert_revoked = marks.as_ref().is_some_and(|m| revoked.iter().any(|r| r.serial.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(&m.serial))));
        let key = match std::fs::read_to_string(&key_path) {
            Ok(k) if !key_revoked => k,
            _ => {
                let k = crate::pki::new_key_pem()?;
                crate::util::write_secret(&key_path, k.as_bytes(), 0o600)?;
                k
            }
        };
        let current = held.filter(|c| !cert_revoked && !crate::pki::needs_renewal(c) && crate::pki::cert_matches_key(c, &key));
        if current.is_some() {
            continue;
        }
        let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
        let pem = pki.issue_identity(&csr, &identity, &[], time::Duration::days(SERVER_DAYS))?;
        crate::util::write_secret(&cert_path, pem.as_bytes(), 0o644)?;
        revoke(store, Target::Certificates(uri.clone()), "superseded by a newer certificate", "kiln", crate::util::now_unix()).await?;
        record(store, kind, &uri, &pem).await?;
        tracing::info!(identity = %identity, "issued an identity");
    }
    Ok(())
}

/// Keeps the server's and CD's identities current while the server runs (W27-R01):
/// [`own_identities`] every [`OWN_EVERY`], so neither expires between restarts.
pub async fn own_identities_loop(hub: std::sync::Arc<Hub>) -> anyhow::Result<()> {
    while hub.shutdown.sleep(OWN_EVERY).await {
        if let Err(e) = own_identities(&hub.store, &hub.pki).await {
            tracing::warn!(error = %format!("{e:#}"), "the server's and CD's identities weren't renewed");
        }
    }
    Ok(())
}

/// How long a CI leg's certificate lasts: as long as its attempt may run (its timeout), and an
/// hour more, as the leg's tokens do (W10-R03), never past a week (a runner process's own
/// certificate's lifetime, whatever timeout a workflow declares). Not revoked when the leg ends:
/// its key is dropped then.
pub fn leg_lifetime(timeout_minutes: u32) -> time::Duration {
    time::Duration::minutes((i64::from(timeout_minutes) + 60).min(RUNNER_HOURS * 60))
}

/// How long a person's certificate lasts after they sign in: hours, never past a day. Their CLI
/// asks again once half of it has gone, while they stay signed in ([`renewal_due`]).
pub const PERSON_HOURS: i64 = 12;

/// A person's certificate, as kiln issued it (W27-R02).
#[derive(Clone, Debug)]
pub struct Issued {
    pub uri: String,
    /// Who it names: `sam of hyper-light`.
    pub identity: String,
    pub cert_pem: String,
    pub not_before: i64,
    pub not_after: i64,
}

/// Why kiln didn't certify a person's key (W27-R02).
#[derive(Debug, thiserror::Error)]
pub enum PersonRefused {
    #[error("a person's certificate is for a person signed in (a passkey, a password, a sign-in link or a personal access token), never an agent, a job or a peer")]
    NotAPerson,
    #[error("this person was disabled: kiln certifies them no more")]
    Disabled,
    #[error("{0}")]
    Unnamed(String),
    #[error("this key was {0}: make a new one")]
    KeyRevoked(String),
    #[error("this key is already certified as {0}, and a key certifies one identity: make a new one")]
    KeyTaken(String),
    #[error("not a certificate request: {0}")]
    BadCsr(String),
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

impl PersonRefused {
    /// The API's `code` for it: the CLI makes a new key for `key_revoked` and `key_taken`, and
    /// takes `not_a_person` and `disabled` (and a 401) as its person no longer signed in.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAPerson => "not_a_person",
            Self::Disabled => "disabled",
            Self::Unnamed(_) => "unnamed",
            Self::KeyRevoked(_) => "key_revoked",
            Self::KeyTaken(_) => "key_taken",
            Self::BadCsr(_) => "bad_csr",
            Self::Failed(_) => "failed",
        }
    }

    pub fn status(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::NotAPerson | Self::Disabled | Self::Unnamed(_) => StatusCode::FORBIDDEN,
            Self::KeyRevoked(_) | Self::KeyTaken(_) => StatusCode::CONFLICT,
            Self::BadCsr(_) => StatusCode::BAD_REQUEST,
            Self::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The name a person's identity gives them (W27-R01, R02): the owner's is `admin`; a user's is
/// their name exactly, which kiln gives no one else (names are never reused, nor told apart by
/// case). A name that can't be an identity's part as it is (`sam@acme`, `.`), or that reads as
/// the owner's (`Admin`), gets none: changed to fit, it could name someone else.
pub fn person_name(user: Option<&str>) -> Result<String, String> {
    let Some(name) = user else { return Ok("admin".to_owned()) };
    if name.eq_ignore_ascii_case("admin") || name.eq_ignore_ascii_case("owner") {
        return Err(format!("`{name}` reads as the owner's name, and kiln certifies only the owner as admin"));
    }
    match Identity::new("kiln", Actor::Person { name: name.to_owned() }).uri() {
        Ok(_) => Ok(name.to_owned()),
        Err(kiln_ci::identity::chain::Refusal::BadIdentity { why, .. }) => Err(format!("kiln can't name `{name}` in an identity as it is ({why}), and changed to fit it could name someone else")),
        Err(e) => Err(format!("kiln can't name `{name}` in an identity: {e}")),
    }
}

/// A person's identity for a key made on their own device (W27-R02): issued to a person signed
/// in (the owner, a user, a user's personal access token), never to an agent, a job or a peer.
pub async fn issue_person(hub: &Hub, caller: &crate::authz::Caller, csr: &str) -> Result<Issued, PersonRefused> {
    use crate::authz::gate::Kind;
    if caller.agent.is_some() || !matches!(caller.kind, Kind::Owner | Kind::User | Kind::Pat) {
        return Err(PersonRefused::NotAPerson);
    }
    // A user's session or token names them; the owner's (and the owner's tokens) name no user.
    certify_person(&hub.store, &hub.pki, caller.user.as_deref(), csr).await
}

/// A person's certificate for the key of `csr`, made on their device and never sent: for user
/// `user` (their id; `None`: the owner), for [`PERSON_HOURS`], recorded. It's recorded in the
/// transaction that checks they're still enabled, so none is issued once they're disabled; a key
/// that was revoked, or certifies another identity, isn't certified again.
pub async fn certify_person(store: &Store, pki: &crate::pki::Pki, user: Option<&str>, csr: &str) -> Result<Issued, PersonRefused> {
    let name = match user {
        None => None,
        Some(uid) => {
            let uid = uid.to_owned();
            let live: Option<String> = store.call(move |c| Ok(c.query_row("SELECT name FROM users WHERE id = ?1 AND disabled_at IS NULL", [uid], |r| r.get(0)).optional()?)).await?;
            Some(live.ok_or(PersonRefused::Disabled)?)
        }
    };
    let identity = Identity::new(pki.organization(), Actor::Person { name: person_name(name.as_deref()).map_err(PersonRefused::Unnamed)? });
    let uri = identity.uri().map_err(|e| PersonRefused::Unnamed(e.to_string()))?;
    let pem = pki.issue_identity(csr, &identity, &[], time::Duration::hours(PERSON_HOURS)).map_err(|e| PersonRefused::BadCsr(format!("{e:#}")))?;
    let f = crate::pki::fingerprints(&pem)?;
    let (uid, subject, cert, fp) = (user.map(str::to_owned), uri.clone(), pem.clone(), f.clone());
    let refused = store
        .call(move |c| {
            // A savepoint: the store may already run this inside its own transaction (the cluster group).
            let tx = c.savepoint()?;
            if let Some(uid) = &uid
                && !tx.query_row("SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1 AND disabled_at IS NULL)", [uid], |r| r.get::<_, bool>(0))?
            {
                return Ok(Some(PersonRefused::Disabled));
            }
            let revoked: Option<(i64, String, String)> =
                tx.query_row("SELECT at, reason, by FROM revocations WHERE key_sha256 = ?1 ORDER BY at, id LIMIT 1", [&fp.key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            if let Some((at, reason, by)) = revoked {
                return Ok(Some(PersonRefused::KeyRevoked(refusal(at, &reason, &by))));
            }
            let other: Option<String> = tx.query_row("SELECT subject FROM identities WHERE key_sha256 = ?1 AND subject <> ?2 LIMIT 1", params![fp.key, subject], |r| r.get(0)).optional()?;
            if let Some(other) = other {
                return Ok(Some(PersonRefused::KeyTaken(Identity::parse(&other).map_or(other, |i| i.to_string()))));
            }
            tx.execute(
                "INSERT INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at, cert_pem) VALUES (?1, 'person', ?2, ?3, ?4, ?5, ?6, ?7)",
                params![fp.serial, subject, fp.key, fp.not_before, fp.not_after, crate::util::now_unix(), cert],
            )?;
            tx.commit()?;
            Ok(None)
        })
        .await?;
    match refused {
        Some(r) => Err(r),
        None => Ok(Issued { identity: identity.to_string(), uri, cert_pem: pem, not_before: f.not_before, not_after: f.not_after }),
    }
}

/// Person `name` was disabled at `at`, the moment kiln recorded it: their certificates are
/// refused from then (W27-R01), whatever device holds their keys. A name kiln never certified
/// ([`person_name`]) has none.
pub async fn person_disabled(store: &Store, pki: &crate::pki::Pki, name: &str, by: &str, at: i64) -> anyhow::Result<()> {
    let Ok(name) = person_name(Some(name)) else { return Ok(()) };
    let uri = Identity::new(pki.organization(), Actor::Person { name }).uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    revoke(store, Target::Subject(uri), "person disabled", by, at).await?;
    Ok(())
}

// ------------------------------------------------------------------ a person's device (the CLI)

/// Where a person's device keeps their identity (W27-R02): `<config>/kiln` (`~/.config/kiln`).
/// `identity.key` is its key (0600, made here, never sent); `identity.pem` the certificate kiln
/// issued for it; `identity.json` (0600) the server that was and the sign-in it was asked with, so
/// the certificate is renewed while that sign-in lasts.
pub fn device_dir() -> std::path::PathBuf {
    crate::os::config_home().join("kiln")
}

/// How `kiln login` signed in to have this device's key certified, kept to renew it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignIn {
    /// A personal access token (or the owner's admin token), given to `kiln login`.
    Token(String),
    /// A session, from a sign-in link or a password: it lasts until it expires, they sign out or
    /// they're disabled.
    Session(String),
    /// The admin token this machine has (the server's token file, the cluster's credentials),
    /// found again each time.
    Found,
}

/// `identity.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Kept {
    url: String,
    sign_in: SignIn,
}

/// What a request for this device's certificate is signed in with.
enum Cred<'a> {
    Bearer(&'a str),
    Cookie(&'a str),
}

/// This device's certificate, as kiln issued it.
pub struct Certified {
    pub identity: String,
    pub not_after: i64,
    pub path: std::path::PathBuf,
}

impl std::fmt::Display for Certified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} until {} ({}; its key stays on this device, renewed while you stay signed in)", self.identity, crate::ci::cli::when(Some(self.not_after)), self.path.display())
    }
}

/// kiln's answer when it didn't certify this device's key: its status, `code` and why.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct Refused {
    pub status: u16,
    pub code: String,
    pub error: String,
}

impl Refused {
    /// Whether the sign-in no longer holds: signed out, disabled, a token revoked or expired.
    fn signed_out(&self) -> bool {
        self.status == 401 || matches!(self.code.as_str(), "not_a_person" | "disabled")
    }
}

/// Whether a token is a person's own (a personal access token) or an agent's, not the owner's
/// admin token: `kiln login` with one certifies this device instead of printing the owner's links.
pub fn personal_token(token: &str) -> bool {
    token.starts_with(crate::authz::gate::PAT_PREFIX) || token.starts_with(crate::authz::gate::AGENT_PREFIX)
}

/// Whether this device's certificate should be asked for again at `now`: it's missing, isn't for
/// its key, or half of its life has gone.
pub fn renewal_due(cert_pem: Option<&str>, key_pem: Option<&str>, now: i64) -> bool {
    let (Some(cert), Some(key)) = (cert_pem, key_pem) else { return true };
    if !crate::pki::cert_matches_key(cert, key) {
        return true;
    }
    crate::pki::fingerprints(cert).map_or(true, |f| now >= f.not_before + (f.not_after - f.not_before) / 2)
}

/// `scheme://host[:port]` of a URL, lowercase, without a default port or credentials: the server
/// it names, as git's credential helper is asked about one.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.trim().split_once("://").unwrap_or(("http", url.trim()));
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h).to_ascii_lowercase();
    let default = if scheme == "https" { ":443" } else { ":80" };
    format!("{scheme}://{}", host.strip_suffix(default).unwrap_or(&host))
}

/// This device's key (`identity.key`): made here when there's none, it's unreadable, or `fresh`.
/// One already here is kept to its owner (0600).
fn device_key(dir: &std::path::Path, fresh: bool) -> anyhow::Result<String> {
    let path = dir.join("identity.key");
    if !fresh
        && let Ok(key) = std::fs::read_to_string(&path)
        && rcgen::KeyPair::from_pem(&key).is_ok()
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        return Ok(key);
    }
    let key = crate::pki::new_key_pem()?;
    crate::util::write_secret(&path, key.as_bytes(), 0o600)?;
    Ok(key)
}

/// `POST <base>/api/me/identity` with a CSR for `key`: kiln's answer, or why it refused.
async fn ask(client: &crate::http::client::Client, base: &str, cred: &Cred<'_>, key: &str) -> anyhow::Result<Result<serde_json::Value, Refused>> {
    use serde_json::Value;
    let csr = crate::pki::make_csr_with_key("kiln-identity", key)?;
    let req = client.post(&format!("{}/api/me/identity", base.trim_end_matches('/'))).json(&serde_json::json!({ "csr": csr }));
    let req = match cred {
        Cred::Bearer(t) => req.bearer(t),
        Cred::Cookie(s) => req.header("Cookie", &format!("kiln_session={s}")),
    };
    let resp = req.send().await?;
    let status = resp.status();
    let v: Value = resp.json(1 << 20).await.unwrap_or_default();
    if (200..300).contains(&status) {
        return Ok(Ok(v));
    }
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let error = if s("error").is_empty() { format!("HTTP {status}") } else { s("error") };
    Ok(Err(Refused { status, code: s("code"), error }))
}

/// This device's key certified at `base` as whoever `cred` signs in as (W27-R02): the key is made
/// here (a new one when kiln says this one was revoked or certifies someone else) and only a CSR
/// goes. Keeps the certificate (`identity.pem`) and `keep`, to renew it.
async fn certify_device(base: &str, cred: &Cred<'_>, keep: &SignIn) -> anyhow::Result<Certified> {
    let dir = device_dir();
    let client = crate::http::client::Client::public()?;
    let mut key = device_key(&dir, false)?;
    let mut answer = ask(&client, base, cred, &key).await?;
    if let Err(r) = &answer
        && matches!(r.code.as_str(), "key_revoked" | "key_taken")
    {
        key = device_key(&dir, true)?;
        answer = ask(&client, base, cred, &key).await?;
    }
    let v = answer?;
    let field = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let pem = field("cert_pem");
    anyhow::ensure!(crate::pki::cert_matches_key(&pem, &key), "the server sent no certificate for this device's key");
    let path = dir.join("identity.pem");
    crate::util::write_secret(&path, pem.as_bytes(), 0o644)?;
    let kept = serde_json::to_vec(&Kept { url: base.trim_end_matches('/').to_owned(), sign_in: keep.clone() })?;
    crate::util::write_secret(&dir.join("identity.json"), &kept, 0o600)?;
    Ok(Certified { identity: field("identity"), not_after: v.get("not_after").and_then(serde_json::Value::as_i64).unwrap_or_default(), path })
}

/// `kiln login --certificate`, or `kiln login` with a personal access token: this device's key
/// certified as whoever `t`'s token signs in as. A token `given` (`--token`, `KILN_ADMIN_TOKEN`)
/// is kept to renew the certificate; one this machine has is found again each time.
pub async fn login_certificate(t: &crate::access::Target, given: bool) -> anyhow::Result<Certified> {
    let keep = if given { SignIn::Token(t.token.clone()) } else { SignIn::Found };
    certify_device(&t.url, &Cred::Bearer(&t.token), &keep).await
}

/// `kiln login --link <link>` or `--name <name>`: a person signs in on their own device with a
/// sign-in link or their password (W27-R02), and this device's key is certified as them; the
/// session is kept to renew the certificate while it lasts. `url`: the server (a link names its
/// own).
pub async fn login_person(url: Option<&str>, link: Option<&str>, name: Option<&str>, password_stdin: bool) -> anyhow::Result<Certified> {
    let client = crate::http::client::Client::public()?;
    let (base, resp) = match (link, name) {
        (Some(link), _) => {
            let base = url.map_or_else(|| origin(link), |u| u.trim_end_matches('/').to_owned());
            let path = link.trim().split_once("://").and_then(|(_, rest)| rest.find('/').and_then(|at| rest.get(at..))).unwrap_or_default();
            anyhow::ensure!(path.starts_with("/api/auth/otp?"), "that isn't a kiln sign-in link (…/api/auth/otp?code=…)");
            (base.clone(), client.get(&format!("{base}{path}")).send().await?)
        }
        (None, Some(name)) => {
            let base = url.map(|u| u.trim_end_matches('/').to_owned()).ok_or_else(|| anyhow::anyhow!("say which server (--url, or KILN_URL)"))?;
            let password = if password_stdin {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            } else {
                rpassword::prompt_password(format!("{name}'s password for {base}: "))?
            };
            (base.clone(), client.post(&format!("{base}/api/auth/password")).json(&serde_json::json!({ "name": name, "password": password })).send().await?)
        }
        (None, None) => anyhow::bail!("sign in with a sign-in link (--link) or your name and password (--name)"),
    };
    let session = resp
        .headers()
        .into_iter()
        .filter(|(k, _)| k == "set-cookie")
        .find_map(|(_, v)| v.split(';').next().and_then(|c| c.trim().strip_prefix("kiln_session=")).filter(|s| !s.is_empty()).map(str::to_owned));
    let Some(session) = session else {
        let v: serde_json::Value = resp.json(64 << 10).await.unwrap_or_default();
        let why = v.get("error").and_then(serde_json::Value::as_str).map(str::to_owned);
        anyhow::bail!("not signed in: {}", why.unwrap_or_else(|| "the sign-in link was used already, expired, or is another server's".to_owned()));
    };
    certify_device(&base, &Cred::Cookie(&session), &SignIn::Session(session.clone())).await
}

/// The credential helper's part (W27-R02): when this device's certificate is from the server git
/// asks about (`url`) and half its life has gone, it's asked for again with the sign-in `kiln
/// login` kept, so it's renewed while that sign-in lasts. A device `kiln login` didn't certify is
/// left alone. A sign-in that no longer holds (signed out, disabled, its token revoked) is
/// forgotten, and the certificate lapses. What to tell the person, if anything.
pub async fn renew_device(cfg: &crate::config::Config, url: &str) -> Option<String> {
    let dir = device_dir();
    let kept: Kept = serde_json::from_str(&std::fs::read_to_string(dir.join("identity.json")).ok()?).ok()?;
    if origin(&kept.url) != origin(url) {
        return None;
    }
    let (cert, key) = (std::fs::read_to_string(dir.join("identity.pem")).ok(), std::fs::read_to_string(dir.join("identity.key")).ok());
    if !renewal_due(cert.as_deref(), key.as_deref(), crate::util::now_unix()) {
        return None;
    }
    let found;
    let cred = match &kept.sign_in {
        SignIn::Token(t) => Cred::Bearer(t),
        SignIn::Session(s) => Cred::Cookie(s),
        SignIn::Found => {
            found = crate::access::resolve(cfg, Some(&kept.url), None).await.ok()?.token;
            Cred::Bearer(&found)
        }
    };
    let e = certify_device(&kept.url, &cred, &kept.sign_in).await.err()?;
    match e.downcast_ref::<Refused>() {
        Some(r) if r.signed_out() => {
            let _ = std::fs::remove_file(dir.join("identity.json"));
            Some(format!("kiln: no longer signed in to {} ({r}): this device's certificate isn't renewed; `kiln login` signs in again", kept.url))
        }
        Some(r) => Some(format!("kiln: this device's certificate wasn't renewed: {r}")),
        // The server out of reach: the next time.
        None => None,
    }
}

/// What a revocation names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// One certificate, by its serial (hex).
    Serial(String),
    /// Every certificate for a key, by its SPKI's SHA-256 (hex): a key lost.
    Key(String),
    /// Every certificate recorded for a subject: a machine's name, or a kiln:// URI.
    Subject(String),
    /// A subject's certificates, by serial only: their key goes on (a runner process given a
    /// newer certificate for the same key, or gone with its key).
    Certificates(String),
}

/// Records a certificate kiln just issued.
pub async fn record(store: &Store, kind: &str, subject: &str, cert_pem: &str) -> anyhow::Result<()> {
    let f = crate::pki::fingerprints(cert_pem)?;
    let (kind, subject, pem, now) = (kind.to_owned(), subject.to_owned(), cert_pem.to_owned(), crate::util::now_unix());
    store
        .call(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO identities (serial, kind, subject, key_sha256, not_before, not_after, issued_at, cert_pem) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![f.serial, kind, subject, f.key, f.not_before, f.not_after, now, pem],
            )?;
            Ok(())
        })
        .await
}

/// A person's name as an identity's path segment: `user:ada` and `pat:7 of user:ada` are `ada`;
/// the owner (`admin`) is `admin`; anything else is made safe the way machine names are.
pub fn person_of(caller: &str) -> String {
    let name = caller.rsplit_once("user:").map_or(caller, |(_, n)| n);
    let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() || safe == "." || safe == ".." { "admin".to_owned() } else { crate::util::truncate(safe, 128).to_owned() }
}

/// Records who enrolled `machine` (its last enrollment's).
pub async fn enrolled(store: &Store, machine: &str, by: &str) -> anyhow::Result<()> {
    let (machine, by, now) = (machine.to_owned(), person_of(by), crate::util::now_unix());
    store
        .call(move |c| {
            c.execute("INSERT INTO enrollments (machine, enrolled_by, at) VALUES (?1, ?2, ?3) ON CONFLICT (machine) DO UPDATE SET enrolled_by = excluded.enrolled_by, at = excluded.at", params![machine, by, now])?;
            Ok(())
        })
        .await
}

/// Who enrolled `machine`: `admin` for one enrolled before kiln recorded it.
pub async fn enrolled_by(store: &Store, machine: &str) -> anyhow::Result<String> {
    let machine = machine.to_owned();
    let by: Option<String> = store.call(move |c| Ok(c.query_row("SELECT enrolled_by FROM enrollments WHERE machine = ?1", [machine], |r| r.get(0)).optional()?)).await?;
    Ok(by.unwrap_or_else(|| "admin".to_owned()))
}

/// A runner process's identity: its machine and instance, who enrolled the machine, kiln's
/// organization.
pub async fn runner_identity(hub: &Hub, machine: &str, instance: &str) -> anyhow::Result<Identity> {
    let enrolled_by = enrolled_by(&hub.store, machine).await?;
    Ok(Identity::new(hub.pki.organization(), Actor::Runner { enrolled_by, machine: machine.to_owned(), instance: instance.to_owned() }))
}

/// A runner process asked for an identity ([`crate::proto::AgentMsg::Identity`]): the certificate,
/// recorded, or why not, back to it. Its earlier certificates for the same subject are revoked by
/// serial (the key goes on in the same process).
pub async fn on_ask(hub: &Hub, peer: PeerId, machine: &str, json: &str) {
    let ask: IdentityAsk = match serde_json::from_str(json) {
        Ok(a) => a,
        Err(e) => return tracing::debug!(agent = %machine, error = %e, "an unreadable identity request"),
    };
    let mut given = IdentityGiven { kind: ask.kind.clone(), of: ask.of.clone(), epoch: ask.epoch, ..IdentityGiven::default() };
    match issue(hub, peer, machine, &ask).await {
        Ok((uri, pem)) => {
            given.uri = uri;
            given.cert_pem = Some(pem);
        }
        Err(e) => {
            tracing::warn!(agent = %machine, kind = %ask.kind, error = %format!("{e:#}"), "no identity issued");
            given.error = Some(format!("{e:#}"));
        }
    }
    if let Ok(json) = serde_json::to_string(&given) {
        hub.send_peer(peer, &ServerMsg::Identity { json }).await;
    }
}

async fn issue(hub: &Hub, peer: PeerId, machine: &str, ask: &IdentityAsk) -> anyhow::Result<(String, String)> {
    let mut facts: Vec<(u64, String)> = Vec::new();
    let (identity, lifetime) = match ask.kind.as_str() {
        "runner" => {
            let instance = hub.instances.lock().get(peer).map(|i| i.label()).unwrap_or_else(|| "legacy".to_owned());
            (runner_identity(hub, machine, &instance).await?, time::Duration::hours(RUNNER_HOURS))
        }
        "agent" => {
            let session = ask.of.as_deref().ok_or_else(|| anyhow::anyhow!("an agent's identity names its session"))?;
            let epoch = ask.epoch.ok_or_else(|| anyhow::anyhow!("an agent's identity names its epoch"))?;
            // The runner process asking holds the session's key at this epoch: named in its certificate.
            let instance = hub.instances.lock().get(peer).map(|i| i.label()).unwrap_or_else(|| "legacy".to_owned());
            (agent_identity(hub, machine, &instance, session, epoch).await?, time::Duration::hours(AGENT_HOURS))
        }
        "leg" => {
            let leg = ask.of.as_deref().ok_or_else(|| anyhow::anyhow!("a leg's identity names its leg"))?;
            let instance = hub.instances.lock().get(peer).map(|i| i.label()).unwrap_or_else(|| "legacy".to_owned());
            let (identity, f, lifetime) = leg_identity(hub, machine, &instance, leg).await?;
            facts = f;
            (identity, lifetime)
        }
        other => anyhow::bail!("kiln doesn't issue {other:?} identities"),
    };
    let uri = identity.uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    let facts: Vec<(u64, &str)> = facts.iter().map(|(m, v)| (*m, v.as_str())).collect();
    let pem = hub.pki.issue_identity(&ask.csr, &identity, &facts, lifetime)?;
    let now = crate::util::now_unix();
    revoke(&hub.store, Target::Certificates(uri.clone()), "superseded by a newer certificate", "kiln", now).await?;
    if let Actor::Agent { epoch, held_by, .. } = &identity.actor
        && let Some(session) = ask.of.as_deref()
    {
        // What the session signed at an epoch it has left is refused from the hand-over on, saying
        // both epochs and where it went: "epoch 2 left: moved on to epoch 3, on runner adacachy
        // (instance 01a17502)".
        let to = held_by.as_ref().map(|h| format!(", on runner {} (instance {})", h.machine, h.instance)).unwrap_or_default();
        for (subject, left) in epochs_of(&hub.store, session).await?.into_iter().filter(|(_, e)| e < epoch) {
            revoke(&hub.store, Target::Subject(subject), &format!("epoch {left} left: moved on to epoch {epoch}{to}"), "kiln", now).await?;
        }
    }
    record(&hub.store, &ask.kind, &uri, &pem).await?;
    tracing::info!(agent = %machine, identity = %identity, "issued an identity");
    Ok((uri, pem))
}

/// An agent session's identity at `epoch`, for the runner process of `machine` holding it
/// (`instance`, named in the certificate): the session must run there, at that epoch or a later
/// one than kiln last heard (a copy left behind at an older one gets none), and kiln must know
/// whom it acts for.
async fn agent_identity(hub: &Hub, machine: &str, instance: &str, session: &str, epoch: u64) -> anyhow::Result<Identity> {
    let info = hub.harness.info(session).ok_or_else(|| anyhow::anyhow!("kiln knows no agent session {session} yet"))?;
    anyhow::ensure!(info.target == format!("agent:{machine}"), "agent session {session} runs on {}, not {machine}", info.target);
    let known = hub.harness.epoch_of(session);
    anyhow::ensure!(epoch >= known, "agent session {session} is at epoch {known}; epoch {epoch} is a copy left behind");
    let person = match crate::mcp::whose(hub, session).await {
        Some(p) if p == "owner" => "admin".to_owned(),
        Some(p) => person_of(&p),
        None => anyhow::bail!("kiln can't say whom agent session {session} acts for"),
    };
    let held_by = Some(Holder { machine: machine.to_owned(), instance: instance.to_owned() });
    Ok(Identity::new(hub.pki.organization(), Actor::Agent { person, agent: session.to_owned(), epoch, held_by }))
}

/// A CI leg's identity, for the runner process of `machine` it was given to: its run's number,
/// its attempt and key, on that runner (and through it who enrolled the machine); as facts, W10's
/// claims for it in Fulcio's extensions, as Fulcio puts a CI job's (its workflow's ref and digest,
/// its repository, commit and ref, the event, its run and attempt); and how long it lasts.
async fn leg_identity(hub: &Hub, machine: &str, instance: &str, leg: &str) -> anyhow::Result<(Identity, Vec<(u64, String)>, time::Duration)> {
    use kiln_ci::identity::chain::fulcio;
    let id = leg.to_owned();
    type Row = (i64, i64, String, Option<String>, Option<String>);
    let row: Option<Row> = hub
        .store
        .call(move |c| {
            Ok(c.query_row("SELECT r.number, j.attempt, j.key, j.agent, j.leg FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1", [&id], |x| {
                Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?))
            })
            .optional()?)
        })
        .await?;
    let (number, attempt, key, agent, spec) = row.ok_or_else(|| anyhow::anyhow!("kiln knows no leg {leg}"))?;
    anyhow::ensure!(agent.as_deref() == Some(machine), "leg {leg} was given to {}, not {machine}", agent.unwrap_or_else(|| "no runner".into()));
    let claims = crate::ci::oidc::leg_claims(hub, leg).await?.ok_or_else(|| anyhow::anyhow!("leg {leg} is of no run kiln planned"))?;
    // Its spec's timeout, as the scheduler reads it (360 minutes when it has none).
    let timeout = spec
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("timeout_minutes").and_then(serde_json::Value::as_u64))
        .map_or(360, |t| u32::try_from(t).unwrap_or(u32::MAX));
    let enrolled_by = enrolled_by(&hub.store, machine).await?;
    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let safe = crate::util::truncate(safe.trim_matches('-'), 128).to_owned();
    let identity = Identity::new(
        hub.pki.organization(),
        Actor::Leg {
            enrolled_by,
            machine: machine.to_owned(),
            instance: instance.to_owned(),
            run: u64::try_from(number).unwrap_or_default(),
            attempt: u32::try_from(attempt.max(1)).unwrap_or(1),
            leg: if safe.is_empty() { "leg".to_owned() } else { safe },
        },
    );
    Ok((identity, fulcio::from_claims(&claims, &hub.cfg.server.public_url), leg_lifetime(timeout)))
}

/// Every subject an agent session's certificates were issued for, with its epoch.
async fn epochs_of(store: &Store, session: &str) -> anyhow::Result<Vec<(String, u64)>> {
    let pattern = format!("%/agent/{session}/epoch/%");
    let subjects: Vec<String> = store
        .call(move |c| {
            let mut q = c.prepare("SELECT DISTINCT subject FROM identities WHERE kind = 'agent' AND subject LIKE ?1")?;
            let rows = q.query_map([pattern], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(rows)
        })
        .await?;
    Ok(subjects
        .into_iter()
        .filter_map(|s| match Identity::parse(&s).ok()?.actor {
            Actor::Agent { agent, epoch, .. } if agent == session => Some((s, epoch)),
            _ => None,
        })
        .collect())
}

/// Agent session `session` ended: every certificate of every epoch it had is revoked from now.
pub async fn agent_ended(store: &Store, session: &str) -> anyhow::Result<()> {
    let now = crate::util::now_unix();
    for (subject, _) in epochs_of(store, session).await? {
        revoke(store, Target::Subject(subject), "agent ended", "kiln", now).await?;
    }
    Ok(())
}

/// A runner process of `machine` went while another goes on (retired after a blue/green
/// upgrade): its certificates are revoked from now (its key went with it).
pub async fn runner_retired(hub: &Hub, machine: &str, instance: &str) -> anyhow::Result<()> {
    let uri = runner_identity(hub, machine, instance).await?.uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    revoke(&hub.store, Target::Certificates(uri), "runner process retired", "kiln", crate::util::now_unix()).await?;
    Ok(())
}

/// One certificate kiln issued, as `GET /api/identities` lists it: checked now, against every
/// revocation, by `kiln_ci::identity::chain`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Listed {
    pub serial: String,
    pub kind: String,
    pub subject: String,
    pub key: String,
    pub not_before: i64,
    pub not_after: i64,
    pub issued_at: i64,
    /// Who it verifies as (`agent matrix, epoch 3, for sam, of hyper-light`), else why it doesn't.
    pub verified: Option<String>,
    pub refused: Option<String>,
}

/// The certificates kiln issued of `kind` (all, when `None`), newest first, each checked now.
pub async fn list(hub: &Hub, kind: Option<&str>) -> anyhow::Result<Vec<Listed>> {
    let kind = kind.map(str::to_owned);
    let rows: Vec<(String, String, String, String, i64, i64, i64, String)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT serial, kind, subject, key_sha256, not_before, not_after, issued_at, cert_pem FROM identities WHERE (?1 IS NULL OR kind = ?1) ORDER BY issued_at DESC, serial")?;
            let rows = q.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let revs = revocations(&hub.store).await?;
    let now = crate::util::now_unix();
    Ok(rows
        .into_iter()
        .map(|(serial, kind, subject, key, not_before, not_after, issued_at, pem)| {
            let checked = crate::pki::der_of(&pem).map_err(|e| e.to_string()).and_then(|der| kiln_ci::identity::chain::verify(&[&der], &hub.pki.ca_der, &revs, now).map_err(|e| e.to_string()));
            let (verified, refused) = match checked {
                Ok(v) => (Some(v.to_string()), None),
                // A machine's TLS certificate names no kiln identity: say so only when it's revoked.
                Err(e) if kind == "machine" && !e.contains("revoked") => (None, None),
                Err(e) => (None, Some(e)),
            };
            Listed { serial, kind, subject, key, not_before, not_after, issued_at, verified, refused }
        })
        .collect())
}

/// Revokes what `target` names from `at` (unix seconds) on; returns the keys it revoked (for the
/// TLS directory). A subject with no recorded certificate revokes nothing.
pub async fn revoke(store: &Store, target: Target, reason: &str, by: &str, at: i64) -> anyhow::Result<Vec<String>> {
    let (reason, by) = (reason.to_owned(), by.to_owned());
    store
        .call(move |c| {
            // A savepoint: the store may already run this inside its own transaction (the cluster group).
            let tx = c.savepoint()?;
            // (serial, key, subject) of each row to write.
            let rows: Vec<(Option<String>, Option<String>, String)> = match &target {
                Target::Certificates(s) => {
                    let mut q = tx.prepare("SELECT serial FROM identities WHERE subject = ?1 AND serial NOT IN (SELECT serial FROM revocations WHERE serial IS NOT NULL)")?;
                    q.query_map([s], |r| Ok((Some(r.get::<_, String>(0)?), None, s.clone())))?.collect::<Result<Vec<_>, _>>()?
                }
                Target::Subject(s) => {
                    let mut q = tx.prepare("SELECT serial, key_sha256 FROM identities WHERE subject = ?1")?;
                    q.query_map([s], |r| Ok((Some(r.get::<_, String>(0)?), Some(r.get::<_, String>(1)?), s.clone())))?.collect::<Result<Vec<_>, _>>()?
                }
                Target::Serial(serial) => {
                    let serial = serial.to_ascii_lowercase();
                    let known: Option<(String, String)> =
                        tx.query_row("SELECT key_sha256, subject FROM identities WHERE serial = ?1", [&serial], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    let (key, subject) = known.map_or((None, serial.clone()), |(k, s)| (Some(k), s));
                    vec![(Some(serial), key, subject)]
                }
                Target::Key(key) => {
                    let key = key.to_ascii_lowercase();
                    let subject: Option<String> = tx.query_row("SELECT subject FROM identities WHERE key_sha256 = ?1 LIMIT 1", [&key], |r| r.get(0)).optional()?;
                    vec![(None, Some(key.clone()), subject.unwrap_or(key))]
                }
            };
            for (serial, key, subject) in &rows {
                tx.execute(
                    "INSERT INTO revocations (serial, key_sha256, subject, at, reason, by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![serial, key, subject, at, reason, by],
                )?;
            }
            tx.commit()?;
            let mut keys: Vec<String> = rows.into_iter().filter_map(|(_, k, _)| k).collect();
            keys.sort();
            keys.dedup();
            Ok(keys)
        })
        .await
}

/// Every revocation, for the chain checker.
pub async fn revocations(store: &Store) -> anyhow::Result<Vec<Revocation>> {
    store
        .call(|c| {
            let mut q = c.prepare("SELECT serial, key_sha256, at, reason, by FROM revocations ORDER BY at, id")?;
            let rows = q
                .query_map([], |r| Ok(Revocation { serial: r.get(0)?, key: r.get(1)?, at: r.get(2)?, reason: r.get(3)?, by: r.get(4)? }))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

/// What the TLS directory says as it refuses a revoked key: `revoked at <RFC 3339> (<reason>, by
/// <who>)`, as the chain checker says it.
pub fn refusal(at: i64, reason: &str, by: &str) -> String {
    let when = time::OffsetDateTime::from_unix_timestamp(at).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_else(|| at.to_string());
    format!("revoked at {when} ({reason}, by {by})")
}

/// Every revoked key, for the TLS directory, with its first revocation's [`refusal`].
pub async fn revoked_keys(store: &Store) -> anyhow::Result<Vec<(String, String)>> {
    let rows: Vec<(String, i64, String, String, i64)> = store
        .call(|c| {
            let mut q = c.prepare(
                "SELECT key_sha256, at, reason, by, id FROM revocations WHERE key_sha256 IS NOT NULL \
                 UNION SELECT i.key_sha256, r.at, r.reason, r.by, r.id FROM revocations r JOIN identities i ON i.serial = r.serial \
                 ORDER BY 2, 5",
            )?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut first: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (key, at, reason, by, _) in rows {
        first.entry(key).or_insert_with(|| refusal(at, &reason, &by));
    }
    Ok(first.into_iter().collect())
}

/// `kiln identities`: the certificates kiln's CA issued, their revocations, and revoking one
/// (W27-R01), over the API.
#[derive(clap::Args)]
pub struct IdentitiesArgs {
    #[arg(long, env = "KILN_URL", global = true)]
    url: Option<String>,
    /// The admin token, or a personal access token with `settings.manage` (to revoke) or
    /// `runners.read` (to list).
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, global = true, allow_hyphen_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    cmd: IdentitiesCmd,
}

#[derive(clap::Subcommand)]
enum IdentitiesCmd {
    /// The certificates issued, newest first, each checked now: who it verifies as, or why not.
    List {
        /// Only this kind: machine, runner, agent, leg, person, server, cd.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Every revocation: when, why, by whom.
    Revocations,
    /// Revoke from now on: one certificate (`--serial`), every certificate of a key (`--key`), or
    /// every certificate of a subject (a machine's name or a kiln:// URI).
    Revoke {
        #[arg(long, conflicts_with_all = ["key", "subject"], required_unless_present_any = ["key", "subject"])]
        serial: Option<String>,
        #[arg(long, conflicts_with = "subject")]
        key: Option<String>,
        subject: Option<String>,
        /// Why (`key lost`, `runner retired`).
        #[arg(long)]
        reason: String,
    },
}

/// `kiln forget <runner>`: the runner is forgotten, its certificates revoked from now on, with
/// why (`runner forgotten`) and by whom (W27-R01); enrolling again brings a new key.
#[derive(clap::Args)]
pub struct ForgetArgs {
    #[arg(long, env = "KILN_URL")]
    url: Option<String>,
    /// The admin token, or a personal access token that may manage runners.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, allow_hyphen_values = true)]
    token: Option<String>,
    /// The runner's name.
    runner: String,
}

/// `kiln forget`.
pub async fn forget(args: ForgetArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    let api = crate::ci::cli::Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    api.call_api(crate::http::client::Method::Delete, &format!("/api/agents/{}", crate::ci::cli::enc(&args.runner)), None).await?;
    println!("forgot {}; its certificates are revoked (runner forgotten)", args.runner);
    Ok(())
}

/// `kiln identities`.
pub async fn cli(args: IdentitiesArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    use crate::ci::cli::{Api, enc, when};
    use crate::http::client::Method;
    use serde_json::Value;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    match args.cmd {
        IdentitiesCmd::List { kind } => {
            let path = kind.map_or_else(|| "/api/identities".to_owned(), |k| format!("/api/identities?kind={}", enc(&k)));
            let v = api.call_api(Method::Get, &path, None).await?;
            for i in v.get("identities").and_then(Value::as_array).cloned().unwrap_or_default() {
                let state = i.get("refused").and_then(Value::as_str).map_or_else(|| s(&i, "verified"), |r| format!("refused: {r}"));
                println!("{}  {:<8} {}  until {}  {state}", s(&i, "serial"), s(&i, "kind"), s(&i, "subject"), when(i.get("not_after").and_then(Value::as_i64)));
            }
        }
        IdentitiesCmd::Revocations => {
            let v = api.call_api(Method::Get, "/api/identities/revocations", None).await?;
            for r in v.get("revocations").and_then(Value::as_array).cloned().unwrap_or_default() {
                let what = r.get("serial").and_then(Value::as_str).map_or_else(|| format!("key {}", s(&r, "key")), |x| format!("serial {x}"));
                println!("{}  {what}  {} by {}", when(r.get("at").and_then(Value::as_i64)), s(&r, "reason"), s(&r, "by"));
            }
        }
        IdentitiesCmd::Revoke { serial, key, subject, reason } => {
            let v = api.call_api(Method::Post, "/api/identities/revoke", Some(serde_json::json!({ "serial": serial, "key": key, "subject": subject, "reason": reason }))).await?;
            let keys = v.get("keys").and_then(Value::as_array).map_or(0, Vec::len);
            println!("revoked at {} ({reason}); {keys} key(s) refused from now on", when(v.get("at").and_then(Value::as_i64)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(pki: &crate::pki::Pki, name: &str) -> anyhow::Result<String> {
        let (csr, _key) = crate::pki::make_csr(name)?;
        pki.sign_csr(&csr, name, 30)
    }

    /// W27-R01: a leg's certificate lasts as long as its attempt may (its timeout) and an hour
    /// more, and no workflow's timeout makes it last past a week.
    #[test]
    fn w27_r01_a_legs_certificate_lasts_its_attempt() {
        assert_eq!(leg_lifetime(360), time::Duration::hours(7));
        assert_eq!(leg_lifetime(60), time::Duration::hours(2));
        assert_eq!(leg_lifetime(u32::MAX), time::Duration::days(7));
    }

    #[test]
    fn w27_r02_a_persons_name_in_their_identity() {
        assert_eq!(person_name(None).as_deref(), Ok("admin"), "the owner");
        assert_eq!(person_name(Some("sam")).as_deref(), Ok("sam"));
        assert_eq!(person_name(Some("sam.lee_2-x")).as_deref(), Ok("sam.lee_2-x"));
        // Exactly: `sam-` isn't `sam`.
        assert_eq!(person_name(Some("sam-")).as_deref(), Ok("sam-"));
        // None for a name that would have to change to fit (and could then be someone else's), or
        // that reads as the owner's.
        for name in ["sam@acme", "@", ".", "..", "", "sam o'neil", "Admin", "ADMIN", "owner"] {
            assert!(person_name(Some(name)).is_err(), "{name:?}");
        }
    }

    #[test]
    fn w27_r02_renewed_once_half_its_life_has_gone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let key = crate::pki::new_key_pem()?;
        let csr = crate::pki::make_csr_with_key("kiln-identity", &key)?;
        let cert = pki.issue_identity(&csr, &Identity::new("hyper-light", Actor::Person { name: "sam".into() }), &[], time::Duration::hours(PERSON_HOURS))?;
        let f = crate::pki::fingerprints(&cert)?;
        let half = f.not_before + (f.not_after - f.not_before) / 2;
        assert!(!renewal_due(Some(&cert), Some(&key), f.not_before + 60));
        assert!(!renewal_due(Some(&cert), Some(&key), half - 1));
        assert!(renewal_due(Some(&cert), Some(&key), half));
        assert!(renewal_due(Some(&cert), Some(&key), f.not_after + 1), "expired");
        assert!(renewal_due(None, Some(&key), f.not_before + 60), "none yet");
        assert!(renewal_due(Some(&cert), None, f.not_before + 60), "its key gone");
        assert!(renewal_due(Some(&cert), Some(&crate::pki::new_key_pem()?), f.not_before + 60), "not for this key");
        Ok(())
    }

    #[test]
    fn w27_r02_the_server_a_url_names() {
        assert_eq!(origin("http://127.0.0.1:7070/api/auth/otp?code=u.1.2"), "http://127.0.0.1:7070");
        assert_eq!(origin("HTTPS://Kiln.Example:443/"), "https://kiln.example");
        assert_eq!(origin("http://x:secret@kiln.home:80"), "http://kiln.home");
        assert_eq!(origin("https://kiln.home"), origin("https://kiln.home/git/acme/app.git"));
        assert_ne!(origin("http://kiln.home:7070"), origin("http://kiln.home:7071"));
    }

    /// W27-R01, R02: a person is certified for a key of their own, for hours, while they're
    /// enabled; never under another's name or for another identity's key, nor for a key revoked;
    /// disabled at T, what their keys sign from T is refused, naming T.
    #[test]
    fn w27_r02_people_certified_while_enabled() -> anyhow::Result<()> {
        use kiln_ci::identity::chain::{Refusal, verify_signed};
        use rcgen::SigningKey as _;
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            store
                .call(|c| {
                    c.execute_batch("INSERT INTO users (id, name, created_at, created_by) VALUES ('u1', 'sam', 0, 'admin'), ('u2', 'sam@acme', 0, 'admin'), ('u3', 'Admin', 0, 'admin');")?;
                    Ok(())
                })
                .await?;
            let csr = |key: &str| crate::pki::make_csr_with_key("kiln-identity", key);
            let (owners, laptop) = (crate::pki::new_key_pem()?, crate::pki::new_key_pem()?);
            let owner = certify_person(&store, &pki, None, &csr(&owners)?).await?;
            assert_eq!((owner.identity.as_str(), owner.uri.as_str()), ("admin of hyper-light", "kiln://hyper-light/person/admin"));
            let sam = certify_person(&store, &pki, Some("u1"), &csr(&laptop)?).await?;
            assert_eq!((sam.identity.as_str(), sam.uri.as_str()), ("sam of hyper-light", "kiln://hyper-light/person/sam"));
            assert!(sam.not_after - sam.not_before <= 24 * 3600, "hours");
            assert!(crate::pki::cert_matches_key(&sam.cert_pem, &laptop));
            // Asked again, renewed for the same key.
            let again = certify_person(&store, &pki, Some("u1"), &csr(&laptop)?).await?;
            assert_ne!(crate::pki::fingerprints(&again.cert_pem)?.serial, crate::pki::fingerprints(&sam.cert_pem)?.serial);
            // A name kiln can't give as it is, or the owner's.
            for uid in ["u2", "u3"] {
                assert!(matches!(certify_person(&store, &pki, Some(uid), &csr(&crate::pki::new_key_pem()?)?).await, Err(PersonRefused::Unnamed(_))), "{uid}");
            }
            // One key, one identity: the owner's key isn't sam's too.
            assert!(matches!(certify_person(&store, &pki, Some("u1"), &csr(&owners)?).await, Err(PersonRefused::KeyTaken(who)) if who == "admin of hyper-light"));
            // A key revoked (lost) isn't certified again.
            revoke(&store, Target::Key(crate::pki::fingerprints(&sam.cert_pem)?.key), "key lost", "sam", 1_790_000_000).await?;
            assert!(matches!(certify_person(&store, &pki, Some("u1"), &csr(&laptop)?).await, Err(PersonRefused::KeyRevoked(why)) if why.contains("(key lost, by sam)")));
            assert!(matches!(certify_person(&store, &pki, Some("u1"), "not a request").await, Err(PersonRefused::BadCsr(_))));

            // Disabled at T, on a new key's certificate: signed before T, sam's; at T or after,
            // refused, naming T. None is issued after.
            let desk = crate::pki::new_key_pem()?;
            let before = certify_person(&store, &pki, Some("u1"), &csr(&desk)?).await?;
            let t = crate::util::now_unix() + 60;
            store.call(move |c| Ok(c.execute("UPDATE users SET disabled_at = ?1 WHERE id = 'u1'", [t]).map(|_| ())?)).await?;
            person_disabled(&store, &pki, "sam", "admin", t).await?;
            assert!(matches!(certify_person(&store, &pki, Some("u1"), &csr(&crate::pki::new_key_pem()?)?).await, Err(PersonRefused::Disabled)));
            let (revs, der, msg) = (revocations(&store).await?, crate::pki::der_of(&before.cert_pem)?, b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n");
            let sig = rcgen::KeyPair::from_pem(&desk)?.sign(msg)?;
            assert_eq!(verify_signed(&[&der], &pki.ca_der, &revs, t - 1, msg, &sig)?.to_string(), "sam of hyper-light");
            let named = refusal(t, "person disabled", "admin");
            for at in [t, t + 3600] {
                let got = verify_signed(&[&der], &pki.ca_der, &revs, at, msg, &sig);
                assert!(matches!(&got, Err(e @ Refusal::Revoked { .. }) if e.to_string().contains(&named)), "{got:?}");
            }
            // The owner, disabled by no one, is still certified.
            assert!(verify_signed(&[&crate::pki::der_of(&owner.cert_pem)?], &pki.ca_der, &revs, t + 3600, msg, &rcgen::KeyPair::from_pem(&owners)?.sign(msg)?).is_ok());
            // A name never certified has nothing to revoke.
            person_disabled(&store, &pki, "sam@acme", "admin", t).await?;
            assert_eq!(revocations(&store).await?.len(), revs.len());
            Ok(())
        })
    }

    #[test]
    fn w27_r01_the_person_an_enrollment_names() {
        assert_eq!(person_of("user:ada"), "ada");
        assert_eq!(person_of("pat:7 of user:ada"), "ada");
        assert_eq!(person_of("admin"), "admin");
        assert_eq!(person_of("user:sam o'neil"), "sam-o-neil");
        assert_eq!(person_of("user:"), "admin");
        assert_eq!(person_of("user:.."), "admin");
    }

    #[test]
    fn w27_r01_revocations_recorded_with_when_why_and_who() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = Store::in_memory()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let (a1, a2, b) = (cert(&pki, "fedora-2")?, cert(&pki, "fedora-2")?, cert(&pki, "mac-1")?);
            for c in [&a1, &a2] {
                record(&store, "machine", "fedora-2", c).await?;
            }
            record(&store, "machine", "mac-1", &b).await?;
            let keys = revoke(&store, Target::Subject("fedora-2".into()), "runner forgotten", "ada", 1_790_000_000).await?;
            let (fa1, fa2, fb) = (crate::pki::fingerprints(&a1)?, crate::pki::fingerprints(&a2)?, crate::pki::fingerprints(&b)?);
            let mut want = vec![fa1.key.clone(), fa2.key.clone()];
            want.sort();
            assert_eq!(keys, want);
            let stored = revoked_keys(&store).await?;
            assert_eq!(stored.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), want, "mac-1's key isn't revoked");
            assert!(stored.iter().all(|(_, why)| why == "revoked at 2026-09-21T14:13:20Z (runner forgotten, by ada)"), "{stored:?}");
            let revs = revocations(&store).await?;
            assert_eq!(revs.len(), 2);
            assert!(revs.iter().all(|r| r.at == 1_790_000_000 && r.reason == "runner forgotten" && r.by == "ada"));
            // A later revocation of a revoked key doesn't change when it was revoked.
            revoke(&store, Target::Subject("fedora-2".into()), "key lost", "sam", 1_790_000_050).await?;
            assert!(revoked_keys(&store).await?.iter().all(|(_, why)| why.contains("runner forgotten")));
            // By serial: its key comes from the record.
            revoke(&store, Target::Serial(fb.serial.to_uppercase()), "key lost", "sam", 1_790_000_100).await?;
            assert!(revoked_keys(&store).await?.iter().any(|(k, why)| *k == fb.key && why.contains("key lost, by sam")));
            // A subject with nothing recorded revokes nothing.
            assert!(revoke(&store, Target::Subject("nobody".into()), "x", "ada", 0).await?.is_empty());
            Ok(())
        })
    }
}
