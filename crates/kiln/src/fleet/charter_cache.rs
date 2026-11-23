//! CP-T3.2. A session-name charter is immutable and signed by the fleet CA. Reach
//! transfers its sorted reservations by cursor. Only a verified, complete set can vote.
use std::collections::BTreeSet;
use std::io::{BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use super::charter::{self, Certificate};

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Cursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    digest: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    after: String,
    #[serde(default)]
    complete: bool,
}

impl Cursor {
    pub(crate) fn downloading(&self) -> bool { self.digest.is_some() && !self.complete }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Page {
    digest: [u8; 32],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    certificate: Option<Certificate>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    after: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    names: Vec<String>,
}

pub(crate) async fn page(hub: &Arc<crate::hub::Hub>, runners: &[crate::reach::directory::RunnerInfo], cursor: Cursor, room: usize, history: bool) -> anyhow::Result<Option<Page>> {
    let cert = charter::issue(hub, runners).await?;
    let digest = cert.digest(&hub.pki.ca_der)?;
    let known = cursor.digest == Some(digest);
    if known && cursor.complete { return Ok(None); }
    let sealed = !cert.voters.is_empty();
    let after = if known { cursor.after } else { String::new() };
    let mut page = Page { digest, certificate: (!known).then_some(cert), after, names: Vec::new() };
    let metadata = crate::memory::queue::json_bytes(&page);
    anyhow::ensure!(metadata <= room, "name charter exceeds response capacity");
    if sealed && history {
        let available = room.saturating_sub(metadata).saturating_sub(r#","names":"#.len());
        page.names = charter::page(&hub.store, &page.after, available).await?;
    }
    Ok(Some(page))
}

pub(crate) async fn add_page(hub: &Arc<crate::hub::Hub>, runners: &[crate::reach::directory::RunnerInfo], answer: &mut serde_json::Value, cursor: Option<Cursor>, history: bool) {
    let Some(cursor) = cursor else { return; };
    let used = crate::memory::queue::json_bytes(answer).saturating_add(r#","session_names":"#.len());
    let room = crate::reach::wire::MAX_MESSAGE.saturating_sub(used);
    match page(hub,runners,cursor,room,history).await {
        Ok(Some(page)) => if let Some(o) = answer.as_object_mut() && let Ok(raw) = serde_json::to_value(page) {
            o.insert("session_names".into(), raw);
        },
        Ok(None) => {}
        Err(e) => tracing::warn!(%e, "fleet session name charter unavailable"),
    }
}

struct Sealed {
    cert: Certificate,
    digest: [u8; 32],
    names: Arc<BTreeSet<String>>,
    complete: bool,
}

impl Sealed {
    fn validate(&self, words: &[String]) -> anyhow::Result<bool> {
        let last = self.names.last().map_or("", String::as_str);
        anyhow::ensure!(words.first().is_none_or(|s| s.as_str() > last)
            && words.iter().all(|s| !s.is_empty())
            && words.windows(2).all(|w| w.first() < w.last()), "unordered or invalid charter reservation");
        let total = u64::try_from(self.names.len().checked_add(words.len()).context("reservation count overflow")?)?;
        anyhow::ensure!(total <= self.cert.reserved_count, "too many charter reservations");
        if total == self.cert.reserved_count {
            let digest = charter::digest_names(self.names.iter().chain(words).map(String::as_str));
            anyhow::ensure!(digest == self.cert.reserved_digest, "signed session reservation digest differs");
            return Ok(true);
        }
        Ok(false)
    }

    fn advance(&mut self, words: Vec<String>, complete: bool) -> anyhow::Result<()> {
        Arc::get_mut(&mut self.names).context("active name set is immutable")?.extend(words);
        self.complete = complete;
        Ok(())
    }

    fn directory(&self, dir: &Path) -> PathBuf { dir.join(hex::encode(self.digest)) }
    fn next(&self, dir: &Path) -> PathBuf {
        let dir = self.directory(dir);
        self.names.last().map_or_else(|| dir.join("first"), |s| dir.join(crate::util::sha256_hex(s.as_bytes())))
    }

    fn reset_pages(&mut self, dir: &Path) -> anyhow::Result<()> {
        let folder = self.directory(dir);
        if folder.try_exists()? {
            let rejected = tempfile::tempdir_in(dir)?;
            std::fs::rename(&folder, rejected.path().join("pages"))?;
            crate::blobs::chunks::sync_dir(dir)?;
        }
        self.names = Arc::default();
        self.complete = self.validate(&[])?;
        Ok(())
    }

    fn load(cert: Certificate, digest: [u8;32], dir: &Path) -> anyhow::Result<Self> {
        let mut sealed = Self { cert, digest, names: Arc::default(), complete: false };
        sealed.complete = sealed.validate(&[])?;
        while !sealed.complete {
            let path = sealed.next(dir);
            let page = read_optional::<Vec<String>>(&path).and_then(|words| {
                let Some(words) = words else { return Ok(None); };
                anyhow::ensure!(!words.is_empty(), "empty cached charter history page");
                let complete = sealed.validate(&words)?;
                Ok(Some((words,complete)))
            });
            match page {
                Ok(Some((words,complete))) => sealed.advance(words,complete)?,
                Ok(None) => break,
                Err(e) if e.is::<std::io::Error>() || e.downcast_ref::<serde_json::Error>().is_some_and(|j| j.is_io()) => return Err(e),
                Err(e) => {
                    tracing::warn!(error=%e, "redownloading invalid cached session name history");
                    sealed.reset_pages(dir)?;
                    break;
                }
            }
        }
        Ok(sealed)
    }
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::File::open(path) {
        Ok(f) => {
            anyhow::ensure!(f.metadata()?.len() <= crate::reach::wire::MAX_MESSAGE as u64, "oversized cached charter page");
            Ok(Some(serde_json::from_reader(BufReader::new(f))?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// Blue/green processes may write the same signed generation concurrently. Never overwrite
// an already published page or certificate, even if the other process used a different page size.
fn persist_once<T: Serialize>(path: &Path, data: &T) -> anyhow::Result<bool> {
    let parent = path.parent().context("charter cache directory missing")?;
    let mut new_parents = Vec::new();
    let mut current = parent;
    while !current.try_exists()? {
        current = current.parent().context("no existing parent for charter cache")?;
        new_parents.push(current);
    }
    std::fs::create_dir_all(parent)?;
    for dir in new_parents.into_iter().rev() { crate::blobs::chunks::sync_dir(dir)?; }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut out = std::io::BufWriter::new(temp.as_file_mut());
        serde_json::to_writer(&mut out, data)?;
        out.flush()?;
    }
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(path) {
        Ok(_) => { crate::blobs::chunks::sync_dir(parent)?; Ok(true) }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.error.into()),
    }
}

// The independently fsynced digest allows a damaged header to be repaired with the
// same verified certificate. Even two overlapping processes cannot substitute another digest.
fn cache_certificate(path: &Path, cert: &Certificate, ca: &[u8]) -> anyhow::Result<[u8;32]> {
    let digest = cert.verify(ca)?;
    let marker = path.with_extension("digest");
    if let Ok(Some(old)) = read_optional::<Certificate>(path) && let Ok(old_digest) = old.verify(ca) {
        anyhow::ensure!(old_digest == digest, "conflicting published charter certificate");
    }
    persist_once(&marker, &digest)?;
    anyhow::ensure!(read_optional::<[u8;32]>(&marker)? == Some(digest), "different charter digest already pinned");
    if !persist_once(path, cert)? {
        let good = read_optional::<Certificate>(path).and_then(|c| c.context("missing charter certificate")?.verify(ca));
        if let Ok(prior) = good {
            anyhow::ensure!(prior == digest, "conflicting charter certificate");
        } else {
            crate::util::write_secret(path,&serde_json::to_vec(cert)?,0o600)?;
            crate::blobs::chunks::sync_dir(path.parent().context("missing charter directory")?)?;
        }
    }
    Ok(digest)
}

fn read_certificate(path: &Path, ca: &[u8]) -> anyhow::Result<Option<(Certificate,[u8;32])>> {
    let marker = read_optional::<[u8;32]>(&path.with_extension("digest"))?;
    let Some(cert) = read_optional::<Certificate>(path)? else {
        anyhow::ensure!(marker.is_none(), "pinned certificate needs to be fetched again");
        return Ok(None);
    };
    let digest = cert.verify(ca)?;
    let pin = match marker {
        Some(pin) => pin,
        None if persist_once(&path.with_extension("digest"),&digest)? => digest,
        None => read_optional::<[u8;32]>(&path.with_extension("digest"))?.context("charter digest marker disappeared")?,
    };
    anyhow::ensure!(pin == digest, "charter certificate differs from its pin");
    Ok(Some((cert,digest)))
}

struct State {
    registrar: Option<(String, [u8;32])>,
    sealed: Option<Sealed>,
    sealed_seen: bool,
}

pub(crate) struct Cache {
    dir: PathBuf,
    ca: Vec<u8>,
    state: parking_lot::Mutex<State>,
}

impl Cache {
    fn dir(reach: &Path, ca: &[u8]) -> PathBuf {
        reach.join(format!("session-names-{}", crate::util::sha256_hex(ca)))
    }

    pub(crate) fn save_registrar(state: &Path, ca: &[u8], cert: &Certificate) -> anyhow::Result<()> {
        cert.verify(ca)?;
        anyhow::ensure!(cert.voters.is_empty(), "enrollment must identify the initial registrar");
        let dir = Self::dir(&state.join("reach"), ca);
        let path = dir.join("registrar.json");
        cache_certificate(&path,cert,ca)?;
        Ok(())
    }

    pub(crate) fn open(reach: &Path, ca: &[u8]) -> anyhow::Result<Self> {
        let dir = Self::dir(reach, ca);
        let registrar = match read_certificate(&dir.join("registrar.json"),ca) {
            Ok(cert) => cert.and_then(|(c,d)| c.voters.is_empty().then_some((c.registrar,d))),
            Err(e) => { tracing::warn!(error=%e, "cached session name registrar requires revalidation"); None }
        };
        let path = dir.join("sealed.json");
        let sealed_seen = path.try_exists().unwrap_or(true) || path.with_extension("digest").try_exists().unwrap_or(true);
        let sealed = match read_certificate(&path,ca).and_then(|cert| cert.map(|(c,digest)| {
            anyhow::ensure!(!c.voters.is_empty() && registrar.as_ref().is_none_or(|(name,_)| *name == c.registrar), "charter registrar differs from enrollment");
            Sealed::load(c,digest,&dir)
        }).transpose()) {
            Ok(cert) => cert,
            Err(e) => { tracing::warn!(error=%e, "cached session name charter requires revalidation"); None }
        };
        Ok(Self { dir, ca: ca.to_owned(), state: parking_lot::Mutex::new(State { registrar, sealed, sealed_seen }) })
    }

    pub(crate) fn cursor(&self) -> Cursor {
        let state = self.state.lock();
        if let Some(sealed) = &state.sealed {
            Cursor { digest:Some(sealed.digest), after:if sealed.complete { String::new() } else { sealed.names.last().cloned().unwrap_or_default() }, complete:sealed.complete }
        } else if state.sealed_seen { Cursor::default() } else {
            Cursor { digest:state.registrar.as_ref().map(|(_,d)| *d),complete:state.registrar.is_some(),..Default::default() }
        }
    }

    pub(crate) fn needs_install(&self, manager: &crate::convo::raft::Manager) -> bool {
        self.state.lock().sealed.as_ref().is_some_and(|s| s.complete && manager.name_charter_digest() != Some(s.digest))
    }

    pub(crate) fn restore(&self, mgr: &crate::convo::raft::Manager) -> anyhow::Result<()> {
        let state = self.state.lock();
        let registrar=state.sealed.as_ref().map(|s|s.cert.registrar.as_str()).or_else(||state.registrar.as_ref().map(|(n,_)|n.as_str()));
        if let Some(registrar)=registrar {
            let shared=crate::liveness::Shared::get(crate::convo::raft::node_id(mgr.node_name()));
            anyhow::ensure!(shared.registrar.get_or_init(||registrar.to_owned())==registrar,"inconsistent liveness registrar identity");
            if let Some(learner)=crate::store::learner::global().filter(|l|l.machine()==mgr.node_name()) { learner.bind_liveness(registrar)?; }
        }
        if let Some(sealed) = &state.sealed {
            if mgr.name_charter_digest() == Some(sealed.digest) { return Ok(()); }
            mgr.set_name_registrar(&sealed.cert.registrar)?;
            mgr.require_name_charter(sealed.digest)?;
            if sealed.complete {
                mgr.install_name_charter(sealed.digest, sealed.cert.voters.clone(), Arc::clone(&sealed.names))?;
            }
        } else if !state.sealed_seen && let Some((name,_)) = &state.registrar { mgr.set_name_registrar(name)?; }
        Ok(())
    }

    pub(crate) fn accept(&self, page: Page, mgr: Option<&crate::convo::raft::Manager>, history: bool) -> anyhow::Result<bool> {
        let mut state = self.state.lock();
        if let Some(cert) = page.certificate {
            let digest = cert.verify(&self.ca)?;
            anyhow::ensure!(digest == page.digest && state.registrar.as_ref().is_none_or(|(r,_)| r == &cert.registrar), "invalid charter identity or digest");
            if cert.voters.is_empty() {
                anyhow::ensure!(!state.sealed_seen, "an older registrar cannot replace the sealed name charter");
                cache_certificate(&self.dir.join("registrar.json"),&cert,&self.ca)?;
                state.registrar = Some((cert.registrar,digest));
            } else {
                anyhow::ensure!(state.sealed.as_ref().is_none_or(|s| s.digest == digest), "immutable name charter changed");
                state.sealed_seen = true;
                if state.sealed.is_none() { cache_certificate(&self.dir.join("sealed.json"),&cert,&self.ca)?; }
                if let Some(mgr) = mgr { mgr.set_name_registrar(&cert.registrar)?; mgr.require_name_charter(digest)?; }
                if state.sealed.is_none() { state.sealed = Some(Sealed::load(cert,digest,&self.dir)?); }
            }
        }
        let mut more = false;
        if let Some(sealed) = state.sealed.as_mut() {
            anyhow::ensure!(sealed.digest == page.digest, "unexpected charter page digest");
            more = !sealed.complete && !history;
            if !sealed.complete && !page.names.is_empty() && sealed.names.last().map_or("",String::as_str) == page.after {
                let path = sealed.next(&self.dir);
                // Check the received page before publication. The complete set is verified against
                // the signature before it is offered to a manager.
                let complete = match sealed.validate(&page.names) {
                    Ok(complete) => complete,
                    Err(e) => {
                        // A final hash mismatch cannot distinguish a bad last page from a
                        // corrupt earlier prefix. Keep the signed fence and fetch a fresh view.
                        sealed.reset_pages(&self.dir)?;
                        return Err(e);
                    }
                };
                let (words, complete) = if persist_once(&path,&page.names)? {
                    (page.names, complete)
                } else {
                    let words: Vec<String> = read_optional(&path)?.context("missing published charter page")?;
                    let complete = sealed.validate(&words)?;
                    (words,complete)
                };
                sealed.advance(words,complete)?;
                more = !sealed.complete;
            }
        }
        drop(state);
        if let Some(mgr) = mgr { self.restore(mgr)?; }
        Ok(more)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp_t3_2_signed_name_history_resumes_after_restart_before_enabling_votes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("ca"))?;
        let other = crate::pki::Pki::open_scratch(&dir.path().join("other-ca"))?;
        let state = dir.path().join("node");
        let reach = state.join("reach");
        let registrar = charter::signed_fixture(&pki, "bootstrap", Vec::new(), &[])?;
        Cache::save_registrar(&state, &pki.ca_der, &registrar)?;
        let cert = charter::signed_fixture(&pki, "bootstrap", vec!["a".into(), "b".into(), "c".into()], &["alpha", "legacy/folder", "zulu"])?;
        let digest = cert.verify(&pki.ca_der)?;
        anyhow::ensure!(cert.verify(&other.ca_der).is_err());
        let header = || Page { certificate: Some(cert.clone()), digest, after: String::new(), names: Vec::new() };
        let page = |after: &str, names: &[&str]| Page {
            certificate: None, digest, after: after.to_owned(), names: names.iter().copied().map(str::to_owned).collect(),
        };
        let cache = Cache::open(&reach, &pki.ca_der)?;
        let (_db, store) = crate::convo::store::scratch()?;
        let mgr = crate::convo::raft::Manager::open("a", &state.join("wal"), Some(store))?;
        anyhow::ensure!(cache.accept(header(), Some(&mgr), false)?);
        anyhow::ensure!(!cache.accept(page("not-the-cursor", &["zulu"]), Some(&mgr), true)?);
        anyhow::ensure!(cache.accept(page("", &["alpha","legacy/folder"]), Some(&mgr), true)?);
        anyhow::ensure!(mgr.name_charter_digest().is_none());
        drop(cache);
        let cache = Cache::open(&reach, &pki.ca_der)?;
        cache.restore(&mgr)?;
        let cursor = cache.cursor();
        anyhow::ensure!(cursor.downloading() && cursor.after == "legacy/folder");
        anyhow::ensure!(cache.accept(page(&cursor.after,&["wrong"]), Some(&mgr), true).is_err());
        anyhow::ensure!(mgr.name_charter_digest().is_none() && cache.cursor().after.is_empty());
        anyhow::ensure!(!cache.accept(page("", &["alpha","legacy/folder","zulu"]), Some(&mgr), true)?);
        anyhow::ensure!(cache.cursor().complete && mgr.name_charter_digest() == Some(digest));
        let old = Page { digest: registrar.verify(&pki.ca_der)?, certificate: Some(registrar), after:String::new(), names:Vec::new() };
        anyhow::ensure!(cache.accept(old,Some(&mgr),false).is_err());
        let restored = Cache::open(&reach, &pki.ca_der)?;
        anyhow::ensure!(restored.cursor().digest == Some(digest) && restored.cursor().complete);
        std::fs::write(restored.dir.join("sealed.json"), b"incomplete local write")?;
        let damaged = Cache::open(&reach,&pki.ca_der)?;
        anyhow::ensure!(damaged.cursor().digest.is_none() && !damaged.cursor().complete);
        anyhow::ensure!(!damaged.accept(header(),Some(&mgr),false)? && damaged.cursor().complete);
        mgr.shutdown();
        Ok(())
    }
}
