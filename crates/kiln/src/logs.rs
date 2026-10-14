//! Job logs: one append-only file per step at `<data>/logs/<id[..2]>/<job>/<step>.log`, plus a
//! broadcast channel per live job so any number of UI viewers can tail without touching disk.
//! A per-job byte cap protects the disk from runaway output.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum LogEvent {
    Data { idx: u32, data: Bytes },
    Step { idx: u32, status: &'static str },
    End,
}

struct Live {
    tx: broadcast::Sender<LogEvent>,
    files: HashMap<u32, std::fs::File>,
    bytes: u64,
    truncated: bool,
}

#[derive(Clone)]
pub struct Logs {
    dir: PathBuf,
    max_bytes: u64,
    live: Arc<Mutex<HashMap<String, Live>>>,
}

fn safe(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

impl Logs {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes, live: Arc::new(Mutex::new(HashMap::new())) }
    }

    fn job_dir(&self, job: &str) -> PathBuf {
        let j = safe(job);
        let shard = j.get(j.len().saturating_sub(2)..).unwrap_or("00").to_owned();
        self.dir.join(shard).join(j)
    }

    fn step_path(&self, job: &str, idx: u32) -> PathBuf {
        self.job_dir(job).join(format!("{idx}.log"))
    }

    fn with_live<R>(&self, job: &str, f: impl FnOnce(&mut Live) -> R) -> R {
        let mut g = self.live.lock();
        let live = g.entry(job.to_owned()).or_insert_with(|| Live {
            tx: broadcast::channel(1024).0,
            files: HashMap::new(),
            bytes: 0,
            truncated: false,
        });
        f(live)
    }

    /// Append output for a step. Never fails the caller: disk errors are logged and the data still
    /// reaches live viewers.
    pub fn append(&self, job: &str, idx: u32, data: &[u8]) {
        crate::metrics::global().log_bytes.add(data.len() as u64);
        let path = self.step_path(job, idx);
        let max = self.max_bytes;
        let notice: &[u8] = b"\r\n\x1b[33m[kiln] log size limit reached; further output discarded\x1b[0m\r\n";
        self.with_live(job, |live| {
            if live.truncated {
                return;
            }
            let (chunk, truncate_now) = if live.bytes + data.len() as u64 > max {
                (notice, true)
            } else {
                (data, false)
            };
            live.bytes += chunk.len() as u64;
            live.truncated = truncate_now;
            let file = match live.files.entry(idx) {
                std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
                std::collections::hash_map::Entry::Vacant(v) => {
                    let opened = path
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| {
                            std::fs::OpenOptions::new().create(true).append(true).open(&path)
                        });
                    match opened {
                        Ok(f) => Some(v.insert(f)),
                        Err(e) => {
                            tracing::warn!(error = %e, path = %path.display(), "cannot open log");
                            None
                        }
                    }
                }
            };
            if let Some(f) = file
                && let Err(e) = f.write_all(chunk) {
                    tracing::warn!(error = %e, "log write failed");
                }
            let _ = live.tx.send(LogEvent::Data { idx, data: Bytes::copy_from_slice(chunk) });
        });
    }

    pub fn step(&self, job: &str, idx: u32, status: &'static str) {
        self.with_live(job, |live| {
            if status != "running" {
                live.files.remove(&idx);
            }
            let _ = live.tx.send(LogEvent::Step { idx, status });
        });
    }

    /// The job is over: close files, tell viewers, drop the channel.
    pub fn finish(&self, job: &str) {
        if let Some(live) = self.live.lock().remove(job) {
            let _ = live.tx.send(LogEvent::End);
        }
    }

    /// Subscribe to a job's live output. Subscribe *before* reading the backlog so nothing falls
    /// in between (viewers may see a few bytes twice at the seam, never miss any).
    pub fn subscribe(&self, job: &str) -> broadcast::Receiver<LogEvent> {
        self.with_live(job, |live| live.tx.subscribe())
    }

    pub fn is_live(&self, job: &str) -> bool {
        self.live.lock().contains_key(job)
    }

    pub async fn read(&self, job: &str, idx: u32) -> Vec<u8> {
        tokio::fs::read(self.step_path(job, idx)).await.unwrap_or_default()
    }

    /// Size of the backlog already on disk (for the UI's "download" affordance).
    pub async fn size(&self, job: &str, idx: u32) -> u64 {
        tokio::fs::metadata(self.step_path(job, idx)).await.map(|m| m.len()).unwrap_or(0)
    }

    pub async fn remove(&self, jobs: Vec<String>) {
        for j in jobs {
            let d = self.job_dir(&j);
            if let Err(e) = tokio::fs::remove_dir_all(&d).await
                && e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, dir = %d.display(), "log prune failed");
                }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_tail_and_cap() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let logs = Logs::new(dir.path().to_path_buf(), 16);
        let mut rx = logs.subscribe("job1");
        logs.append("job1", 0, b"hello ");
        logs.append("job1", 0, b"world");
        logs.append("job1", 0, b"this exceeds the cap");
        logs.append("job1", 0, b"dropped");
        logs.finish("job1");
        let on_disk = logs.read("job1", 0).await;
        assert!(on_disk.starts_with(b"hello world"));
        assert!(String::from_utf8_lossy(&on_disk).contains("limit reached"));
        let mut n = 0;
        while let Ok(ev) = rx.recv().await {
            n += 1;
            if matches!(ev, LogEvent::End) {
                break;
            }
        }
        assert_eq!(n, 4);
        Ok(())
    }
}
