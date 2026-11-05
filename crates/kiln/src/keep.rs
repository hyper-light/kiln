//! Small files kept whole, written off the shards (goal 3, plan 01a1a59c2a8e step 5): what a registry
//! keeps across restarts (agents' names, their epochs, which ones kiln doesn't keep going) used to be
//! written where it changed, on a shard, which waited on the disk each time and held up every
//! request on it. [`later`] hands a file's newest bytes to one writer task, which writes each on the
//! blocking pool, whole or not at all (to `<file>.part`, then renamed over it), in the order kept.
//! Bytes that come for a file while its last write is on its way are written as one: the newest.
//!
//! What waits is bounded by the files kept (goal 11, plan 01a1a87b2e56 step 3: on `/metrics` as
//! `kiln_memory_bytes{owner="keep_waiting"}`): each file's newest bytes alone wait, in a place of its
//! own, the bytes before them let go as they come; the writer's queue holds a file's path, once,
//! while its bytes wait. So at most one copy of each kept file waits, and one is written.
//!
//! What a crash costs: the newest value of a file not yet written, a few milliseconds' worth, as
//! with `holders.json` since perf/api-pass (`crate::harness::registry`). A file's writes keep its
//! order, so it never goes back to an older value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwapOption;

/// The newest bytes of one file not yet written: taken by the writer as it writes them.
type Newest = Arc<ArcSwapOption<Vec<u8>>>;

/// The writer's ends: each kept file's place for its newest bytes, and the paths whose bytes wait, in
/// the order they came.
struct Keeper {
    newest: crate::rcu::Rcu<HashMap<PathBuf, Newest>>,
    tx: async_channel::Sender<PathBuf>,
}

static KEEPER: OnceLock<Keeper> = OnceLock::new();

/// Keeps `bytes` as the file at `path`, off the calling thread: written soon, in the order kept.
pub fn later(path: PathBuf, bytes: Vec<u8>) {
    let keeper = KEEPER.get_or_init(|| {
        let (tx, rx) = async_channel::unbounded();
        let newest = crate::rcu::Rcu::default();
        crate::rt::spawn(write_each(rx));
        Keeper { newest, tx }
    });
    let place = match keeper.newest.load().get(&path) {
        Some(p) => Arc::clone(p),
        None => keeper.newest.update(|m| Arc::clone(m.entry(path.clone()).or_default())),
    };
    crate::memory::owners::KEEP_WAITING.add(bytes.len());
    match place.swap(Some(Arc::new(bytes))) {
        // Its bytes before these hadn't been written: these take their place, its path already waits.
        Some(before) => crate::memory::owners::KEEP_WAITING.sub(before.len()),
        None => {
            if keeper.tx.try_send(path.clone()).is_err() {
                if let Some(lost) = place.swap(None) {
                    crate::memory::owners::KEEP_WAITING.sub(lost.len());
                }
                tracing::warn!(path = %path.display(), "the kept files' writer is gone: a file wasn't kept");
            }
        }
    }
}

/// Writes each file's newest bytes as its path comes, in the order the paths came.
async fn write_each(rx: async_channel::Receiver<PathBuf>) {
    while let Ok(path) = rx.recv().await {
        let Some(keeper) = KEEPER.get() else { return };
        let Some(bytes) = keeper.newest.load().get(&path).and_then(|p| p.swap(None)) else { continue };
        let shown = path.display().to_string();
        let len = bytes.len();
        match crate::rt::blocking(move || write(&path, &bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) | Err(e) => tracing::warn!(path = %shown, error = %format!("{e:#}"), "couldn't keep a file"),
        }
        crate::memory::owners::KEEP_WAITING.sub(len);
    }
}

/// `bytes` as the file at `path`, whole or not at all.
fn write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::write(&part, bytes)?;
    std::fs::rename(&part, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 11, plan 01a1a87b2e56 step 3: a file kept again and again before it's written waits as its
    /// newest bytes alone, which are what's written; what waits is let go as it's written.
    #[test]
    fn g11_s3_a_file_waits_as_its_newest_bytes_alone() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let (a, b) = (dir.path().join("a.json"), dir.path().join("b.json"));
            for n in 0..200u32 {
                later(a.clone(), format!("{{\"n\": {n}}}").into_bytes());
            }
            later(b.clone(), b"{}".to_vec());
            anyhow::ensure!(KEEPER.get().is_some_and(|k| k.newest.load().contains_key(&a) && k.newest.load().contains_key(&b)), "each file has its place");
            for _ in 0..500 {
                if std::fs::read(&a).ok().as_deref() == Some(b"{\"n\": 199}".as_slice()) && std::fs::read(&b).is_ok() {
                    return Ok(());
                }
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
            }
            anyhow::bail!("the newest bytes weren't written: {:?}", std::fs::read_to_string(&a))
        })
    }
}
