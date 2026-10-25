//! Peers (docs/ci.md W24-R27): an object this runner lacks, fetched from a runner on its LAN that
//! holds it before the server is asked, each copy checked by its digest.
//!
//! - [`Peers`]: who holds what (the directory says which runner holds which object, for which
//!   repository) and an object's bytes from one of them. Its transport is reach's end-to-end
//!   links (`crate::reach`) once they carry objects; until then nothing is set, and
//!   `tests::Lan` stands in for it.
//! - [`fetch`]: the holders asked in turn, nearest first; each copy is hashed before it's kept,
//!   and one that isn't what its name says is refused (never kept, never served) and the next
//!   holder asked.
//!
//! What a peer is asked is scoped as the service's own existence checks are: by repository, so a
//! repository never learns through a peer what another stored (`super::service`).

use std::path::Path;
use std::sync::Arc;

use futures::future::BoxFuture;

use super::store::{Store, is_digest};

/// Runners on this one's LAN, and the objects they hold.
pub trait Peers: Send + Sync + 'static {
    /// The runners that hold `digest` for `repo`, nearest first.
    fn holders<'a>(&'a self, repo: &'a str, digest: &'a str) -> BoxFuture<'a, Vec<String>>;

    /// `digest`'s bytes for `repo` from `peer`, written to `into`: whether it sent them.
    fn get<'a>(&'a self, peer: &'a str, repo: &'a str, digest: &'a str, into: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>>;
}

/// What a fetch did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The peer whose copy is kept now, if one's was.
    pub from: Option<String>,
    /// The peers whose copies weren't what their name says.
    pub refused: Vec<String>,
}

/// `digest` for `repo` from the first of its holders whose copy is what its name says, kept in
/// `store`.
pub async fn fetch(store: &Arc<Store>, peers: &dyn Peers, repo: &str, digest: &str) -> anyhow::Result<Fetched> {
    let mut done = Fetched::default();
    if !is_digest(digest) {
        return Ok(done);
    }
    for peer in peers.holders(repo, digest).await {
        let tmp = store.temp();
        let got = peers.get(&peer, repo, digest, &tmp).await;
        if !matches!(got, Ok(true)) {
            let _ = std::fs::remove_file(&tmp);
            if let Err(e) = got {
                tracing::debug!(target: "kiln.work", %peer, object = %digest, error = %format!("{e:#}"), "a peer couldn't send an object");
            }
            continue;
        }
        // Checked before it's kept: a wrong copy never enters the store, under any name.
        let (s, t, want) = (Arc::clone(store), tmp.clone(), digest.to_owned());
        let kept = crate::rt::blocking(move || -> anyhow::Result<bool> {
            if crate::sync::blobs::hash_file(&t)? != want {
                return Ok(false);
            }
            s.put_file(&t)?;
            Ok(true)
        })
        .await;
        let _ = std::fs::remove_file(&tmp);
        match kept {
            Ok(Ok(true)) => {
                done.from = Some(peer);
                return Ok(done);
            }
            Ok(Ok(false)) => {
                tracing::warn!(target: "kiln.work", %peer, object = %digest, "a peer's copy of an object wasn't what its name says: refused");
                done.refused.push(peer);
            }
            Ok(Err(e)) | Err(e) => return Err(e),
        }
    }
    Ok(done)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::runner::work::service::tests::{access, call, conn, on_thread, serve};
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// A LAN of peers in memory: each holds objects by repository, and says what it sent.
    #[derive(Default)]
    pub(crate) struct Lan {
        /// Peer → (repository, digest) → the bytes it sends (not always what the name says).
        pub(crate) held: Mutex<Vec<(String, HashMap<(String, String), Vec<u8>>)>>,
        pub(crate) asked: Mutex<Vec<String>>,
    }

    impl Lan {
        pub(crate) fn hold(&self, peer: &str, repo: &str, digest: &str, bytes: &[u8]) {
            let mut held = self.held.lock();
            if let Some((_, m)) = held.iter_mut().find(|(p, _)| p == peer) {
                m.insert((repo.to_owned(), digest.to_owned()), bytes.to_vec());
            } else {
                held.push((peer.to_owned(), HashMap::from([((repo.to_owned(), digest.to_owned()), bytes.to_vec())])));
            }
        }
    }

    impl Peers for Lan {
        fn holders<'a>(&'a self, repo: &'a str, digest: &'a str) -> BoxFuture<'a, Vec<String>> {
            Box::pin(async move {
                let key = (repo.to_owned(), digest.to_owned());
                self.held.lock().iter().filter(|(_, m)| m.contains_key(&key)).map(|(p, _)| p.clone()).collect()
            })
        }

        fn get<'a>(&'a self, peer: &'a str, repo: &'a str, digest: &'a str, into: &'a Path) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                self.asked.lock().push(peer.to_owned());
                let key = (repo.to_owned(), digest.to_owned());
                let found = self.held.lock().iter().find(|(p, _)| p == peer).and_then(|(_, m)| m.get(&key).cloned());
                match found {
                    Some(bytes) => {
                        std::fs::write(into, bytes)?;
                        Ok(true)
                    }
                    None => Ok(false),
                }
            })
        }
    }

    /// W24-R27: an object this runner lacks comes from a LAN peer, checked by its digest: a
    /// corrupted copy is refused (and never kept), the next holder's taken; it's this runner's
    /// then, and the peers aren't asked again.
    #[test]
    fn w24_r27_an_object_comes_from_a_peer_checked_by_its_digest() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let lan = Arc::new(Lan::default());
            let blob = b"an rlib from the next desk".to_vec();
            let d = crate::util::sha256_hex(&blob);
            lan.hold("poisoned", "acme/app", &d, b"not what its name says");
            lan.hold("beta", "acme/app", &d, &blob);
            svc.set_peers(lan.clone());
            let t = svc.mint(&access("acme/app", "untrusted", true, false))?;
            let (t2, d2) = (t.clone(), d.clone());
            let r = on_thread(move || call(&mut conn(at), "GET", &format!("/_kiln/cas/v1/objects/{d2}"), Some(&t2), b"")).await??;
            anyhow::ensure!(r.status == 200 && r.body == blob, "{r:?}");
            anyhow::ensure!(*lan.asked.lock() == ["poisoned", "beta"], "{:?}", lan.asked.lock());
            anyhow::ensure!(svc.store().has(&d) && !svc.store().has(&crate::util::sha256_hex(b"not what its name says")), "the wrong copy was never kept");
            // Here now: the peers aren't asked again.
            let (t3, d3) = (t.clone(), d.clone());
            let r = on_thread(move || call(&mut conn(at), "GET", &format!("/bazel/cas/{d3}"), Some(&t3), b"")).await??;
            anyhow::ensure!(r.status == 200 && lan.asked.lock().len() == 2, "{r:?}");
            Ok(())
        })
    }

    /// W24-R27: every holder's copy corrupted is a miss; and a peer is asked only for what the
    /// asker's repository holds there, so no repository learns through a peer what another stored.
    #[test]
    fn w24_r27_corrupt_copies_and_other_repositories_objects_are_misses() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = serve(&root).await?;
            let lan = Arc::new(Lan::default());
            let d = crate::util::sha256_hex(b"the real bytes");
            lan.hold("poisoned", "acme/app", &d, b"poison");
            let other = crate::util::sha256_hex(b"acme/other's object");
            lan.hold("beta", "acme/other", &other, b"acme/other's object");
            svc.set_peers(lan.clone());
            let t = svc.mint(&access("acme/app", "trusted", true, true))?;
            let paths = [format!("/_kiln/cas/v1/objects/{d}"), format!("/_kiln/cas/v1/objects/{other}")];
            let statuses = on_thread(move || -> anyhow::Result<Vec<u16>> {
                let mut c = conn(at);
                paths.iter().map(|p| call(&mut c, "GET", p, Some(&t), b"").map(|r| r.status)).collect()
            })
            .await??;
            anyhow::ensure!(statuses == [404, 404], "{statuses:?}");
            anyhow::ensure!(*lan.asked.lock() == ["poisoned"], "beta isn't asked for acme/app: {:?}", lan.asked.lock());
            anyhow::ensure!(!svc.store().has(&d) && !svc.store().has(&other));
            let f = fetch(svc.store(), lan.as_ref(), "acme/app", &d).await?;
            anyhow::ensure!(f == Fetched { from: None, refused: vec!["poisoned".to_owned()] }, "{f:?}");
            Ok(())
        })
    }
}
