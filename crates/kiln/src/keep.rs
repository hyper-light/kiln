//! Small files kept whole, written off the shards (goal 3, plan 01a1a59c2a8e step 5): what a registry
//! keeps across restarts (agents' names, their epochs, which ones kiln doesn't keep going) used to be
//! written where it changed, on a shard, which waited on the disk each time and held up every
//! request on it. [`later`] hands a file's newest bytes to one writer task, which writes each on the
//! blocking pool, whole or not at all (to `<file>.part`, then renamed over it), in the order kept.
//! Bytes that come for a file while its last write is on its way are written as one: the newest.
//!
//! What a crash costs: the newest value of a file not yet written, a few milliseconds' worth, as
//! with `holders.json` since perf/api-pass (`crate::harness::registry`). A file's writes keep its
//! order, so it never goes back to an older value.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static WRITER: OnceLock<async_channel::Sender<(PathBuf, Vec<u8>)>> = OnceLock::new();

/// Keeps `bytes` as the file at `path`, off the calling thread: written soon, in the order kept.
pub fn later(path: PathBuf, bytes: Vec<u8>) {
    let writer = WRITER.get_or_init(|| {
        let (tx, rx) = async_channel::unbounded();
        crate::rt::spawn(write_each(rx));
        tx
    });
    if let Err(e) = writer.try_send((path, bytes)) {
        let (path, _) = e.into_inner();
        tracing::warn!(path = %path.display(), "the kept files' writer is gone: a file wasn't kept");
    }
}

/// Writes each file's newest bytes as they come, the files in the order they first came in each burst.
async fn write_each(rx: async_channel::Receiver<(PathBuf, Vec<u8>)>) {
    while let Ok(first) = rx.recv().await {
        let mut order: Vec<PathBuf> = Vec::new();
        let mut newest: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        for (path, bytes) in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            if newest.insert(path.clone(), bytes).is_none() {
                order.push(path);
            }
        }
        for path in order {
            let Some(bytes) = newest.remove(&path) else { continue };
            let shown = path.display().to_string();
            match crate::rt::blocking(move || write(&path, &bytes)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) | Err(e) => tracing::warn!(path = %shown, error = %format!("{e:#}"), "couldn't keep a file"),
            }
        }
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
