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
//!
//! A file kept [`synced`] is written as `crate::sync::group::save_atomic` writes one: synced to the disk
//! before it takes the old one's place, and its folder after, so a power cut leaves the old file or the
//! new one whole (goal 9, plan 01a1a87b2cec step 3: which runners mirror the conversation store was
//! written so on the hub's loop, as a runner's Build came, holding its shard 5 to 34 ms in the lab's
//! g9p6).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwapOption;

/// One kept file's place: its newest bytes not yet written, taken by the writer as it writes them, and
/// how it's written, as its first keeping said ([`later`] or [`synced`]: a file is kept one way).
struct Place {
    newest: ArcSwapOption<Vec<u8>>,
    synced: bool,
}

/// The writer's ends: each kept file's place, and the paths whose bytes wait, in the order they came.
struct Keeper {
    places: crate::rcu::Rcu<HashMap<PathBuf, Arc<Place>>>,
    tx: async_channel::Sender<PathBuf>,
}

static KEEPER: OnceLock<Keeper> = OnceLock::new();

/// Keeps `bytes` as the file at `path`, off the calling thread: written soon, in the order kept.
pub fn later(path: PathBuf, bytes: Vec<u8>) {
    keep(path, bytes, false);
}

/// [`later`], the file synced to the disk before it takes the old one's place, and its folder after: for
/// a file whose loss across a power cut costs more than its newest few milliseconds would (a mirror of
/// the conversation store chosen again, and fed the whole store, `crate::convo::mirror`). The disk's
/// waits are the blocking pool's, never the caller's.
pub fn synced(path: PathBuf, bytes: Vec<u8>) {
    keep(path, bytes, true);
}

fn keep(path: PathBuf, bytes: Vec<u8>, synced: bool) {
    let keeper = KEEPER.get_or_init(|| {
        let (tx, rx) = async_channel::unbounded();
        let places = crate::rcu::Rcu::default();
        crate::rt::spawn(write_each(rx));
        Keeper { places, tx }
    });
    let place = match keeper.places.load().get(&path) {
        Some(p) => Arc::clone(p),
        None => keeper.places.update(|m| Arc::clone(m.entry(path.clone()).or_insert_with(|| Arc::new(Place { newest: ArcSwapOption::empty(), synced })))),
    };
    crate::memory::owners::KEEP_WAITING.add(bytes.len());
    match place.newest.swap(Some(Arc::new(bytes))) {
        // Its bytes before these hadn't been written: these take their place, its path already waits.
        Some(before) => crate::memory::owners::KEEP_WAITING.sub(before.len()),
        None => {
            if keeper.tx.try_send(path.clone()).is_err() {
                if let Some(lost) = place.newest.swap(None) {
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
        let Some((bytes, synced)) = keeper.places.load().get(&path).and_then(|p| p.newest.swap(None).map(|b| (b, p.synced))) else { continue };
        let shown = path.display().to_string();
        let len = bytes.len();
        let written = crate::rt::blocking(move || if synced { crate::sync::group::save_atomic(&path, &bytes).map_err(anyhow::Error::from) } else { write(&path, &bytes) }).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(e)) | Err(e) => tracing::warn!(path = %shown, error = %format!("{e:#}"), "couldn't keep a file"),
        }
        crate::memory::owners::KEEP_WAITING.sub(len);
    }
}

/// Tests: whether the file at `path` has a place with the writer (it was kept through it), and whether that
/// writes it synced.
#[cfg(test)]
pub(crate) fn kept_through(path: &Path) -> Option<bool> {
    KEEPER.get().and_then(|k| k.places.load().get(path).map(|p| p.synced))
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
            anyhow::ensure!(KEEPER.get().is_some_and(|k| k.places.load().contains_key(&a) && k.places.load().contains_key(&b)), "each file has its place");
            for _ in 0..500 {
                if std::fs::read(&a).ok().as_deref() == Some(b"{\"n\": 199}".as_slice()) && std::fs::read(&b).is_ok() {
                    return Ok(());
                }
                crate::rt::sleep(std::time::Duration::from_millis(10)).await;
            }
            anyhow::bail!("the newest bytes weren't written: {:?}", std::fs::read_to_string(&a))
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a file kept synced is written by the writer, as `save_atomic` writes
    /// one, its newest bytes whole, in the order kept: one kept again while its last write is on its way is
    /// written as its newest, and never goes back to an older one. Its folder is made there too, and no
    /// temporary file is left beside it.
    #[test]
    fn g9_s3_a_file_kept_synced_is_written_whole_off_the_caller() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("made").join("mirrors.json");
            synced(file.clone(), b"[1]".to_vec());
            anyhow::ensure!(kept_through(&file) == Some(true), "through the writer, synced");
            let written = |want: &'static [u8]| {
                let file = file.clone();
                async move {
                    for _ in 0..500 {
                        if std::fs::read(&file).ok().as_deref() == Some(want) {
                            return true;
                        }
                        crate::rt::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    false
                }
            };
            anyhow::ensure!(written(b"[1]").await, "the first written");
            for n in 2..=100u32 {
                synced(file.clone(), format!("[{n}]").into_bytes());
            }
            anyhow::ensure!(written(b"[100]").await, "the newest written: {:?}", std::fs::read_to_string(&file));
            // Nothing left to write over it.
            crate::rt::sleep(std::time::Duration::from_millis(50)).await;
            anyhow::ensure!(std::fs::read(&file)? == b"[100]", "it went back to an older one");
            let left: Vec<String> = std::fs::read_dir(dir.path().join("made"))?.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned())).collect();
            anyhow::ensure!(left == ["mirrors.json"], "{left:?}");
            // A file kept plainly is written plainly.
            let plain = dir.path().join("plain.json");
            later(plain.clone(), b"{}".to_vec());
            anyhow::ensure!(kept_through(&plain) == Some(false));
            Ok(())
        })
    }
}
