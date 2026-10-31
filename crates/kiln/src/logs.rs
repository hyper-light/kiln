//! Job logs bound to the canonical owner publish an Amphora accepted job-view prefix
//! over immutable configured Blobs bytes. This is observed data, not producer closure.
//! Files at `<data>/logs/<id[..2]>/<job>/<step>.log`, timestamps and live broadcasts are
//! compatibility projections after the operation's own durable receipt; projection errors
//! can leave them behind accepted authority. Unbound callers retain file-only compatibility.
//! A per-job byte cap bounds accepted bytes. `<step>.times` records projected byte offsets
//! and arrival times for Ship's Output (plan 01a1856fe81375b1bd1d1d86f270a45b, step9).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use crate::sql::OptionalExtension as _;

pub(crate) mod accepted;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum LogEvent {
    Data { idx: u32, data: Bytes },
    Step { idx: u32, status: String },
    End,
    /// Same-topic resnapshot hint after an own canonical append/close receipt. It carries
    /// no bytes or completion authority; strict readers consult the immutable prefix.
    Accepted,
}

struct Live {
    tx: crate::bus::Broadcast<LogEvent>,
    files: HashMap<u32, std::fs::File>,
    /// Each step's `<step>.times`, open while it's written.
    times: HashMap<u32, std::fs::File>,
    bytes: u64,
    truncated: bool,
}

#[derive(Clone)]
pub struct Logs {
    dir: PathBuf,
    max_bytes: u64,
    live: Arc<Mutex<HashMap<String, Live>>>,
    // U05: the SAME retained I/O survives net_loop supervision. Futures capture Projection,
    // never Logs/Hub, so this owner has no self-retaining Arc cycle.
    io: Arc<Mutex<accepted::Owner>>,
}

#[derive(Clone)]
pub(super) struct Projection {
    dir:PathBuf,max_bytes:u64,live:Arc<Mutex<HashMap<String,Live>>>,
}

impl Projection {
    fn logs(&self)->Logs {
        Logs{dir:self.dir.clone(),max_bytes:self.max_bytes,live:Arc::clone(&self.live),io:Arc::new(Mutex::new(accepted::Owner::default()))}
    }
    fn max_bytes(&self)->u64 {self.max_bytes}
    fn append(&self,job:&str,step:u32,bytes:&[u8]) {self.logs().project(job,step,bytes)}
    fn finish(&self,job:&str) {self.logs().finish_projection(job)}
    fn accepted(&self,job:&str) {
        self.logs().with_live(job,|live|{let _=live.tx.send(LogEvent::Accepted);});
    }
    async fn delete_capture(&self,job:&str)->anyhow::Result<String> {
        let path=self.logs().job_dir(job);
        crate::rt::blocking(move ||Ok(serde_json::to_string(&directory_identity(&path)?)?)).await?
    }
    async fn delete_captured(&self,job:&str,expected:String)->anyhow::Result<()> {
        let path=self.logs().job_dir(job);
        crate::rt::blocking(move ||{
            let actual=directory_identity(&path)?;
            if !actual.present {return Ok(())}
            anyhow::ensure!(serde_json::to_string(&actual)?==expected,"deferred log directory identity changed; original intent retained");
            // Same legacy physical operation, one owned job directory outside SQL. This
            // makes no kernel-latency guarantee and follows no directory symlink.
            std::fs::remove_dir_all(&path)?;Ok(())
        }).await?
    }
    async fn copy_capture(&self,job:&str,step:u32)->anyhow::Result<(Option<accepted::CapturedFile>,String)> {
        let path=self.logs().step_path(job,step);let directory=self.logs().job_dir(job);
        crate::rt::blocking(move ||{
            let before=directory_identity(&directory)?;
            let capture=match open_capture(&path) {
                Ok(file)=>{let metadata=file.metadata()?;let (device,inode)=capture_identity(&metadata,&file)?;Some(accepted::CapturedFile{step,bytes:metadata.len(),device,inode,modified_ns:metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos()})},
                Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error|error.kind()==std::io::ErrorKind::NotFound)=>None,
                Err(error)=>return Err(error),
            };
            anyhow::ensure!(directory_identity(&directory)?==before,"legacy source directory changed while capturing metadata");
            Ok((capture,serde_json::to_string(&before)?))
        }).await?
    }
    async fn start_capture(&self,job:&str)->anyhow::Result<Capture> {
        let path=self.logs().job_dir(job);
        crate::rt::blocking(move ||{
            let identity=directory_identity(&path)?;
            let rows=if identity.present {Some(std::fs::read_dir(&path)?)}else{None};
            anyhow::ensure!(directory_identity(&path)?==identity,"historical directory changed while opening its finite capture");
            Ok(Capture{path,identity,rows})
        }).await?
    }
    async fn capture_page(&self,mut capture:Capture)->anyhow::Result<(Capture,Vec<accepted::PreparedFile>,bool)> {
        crate::rt::blocking(move ||{
            anyhow::ensure!(directory_identity(&capture.path)?==capture.identity,"historical directory changed during retained capture");
            let mut files=Vec::new();let mut done=false;
            for _ in 0..accepted::PAGE {
                let row=match capture.rows.as_mut().and_then(Iterator::next) {Some(row)=>row?,None=>{done=true;break}};
                let name=row.file_name();let name=name.to_str().ok_or_else(||anyhow::anyhow!("historical log name is not UTF-8"))?;
                let (number,log)=if let Some(number)=name.strip_suffix(".log") {(number,true)}else if let Some(number)=name.strip_suffix(".times") {(number,false)}else{anyhow::bail!("unexpected historical log entry")};
                let step=number.parse::<u32>()?;anyhow::ensure!(number==step.to_string(),"ambiguous historical log name");
                if !log {continue}
                let file=open_capture(&row.path())?;let metadata=file.metadata()?;let (device,inode)=capture_identity(&metadata,&file)?;
                let file=accepted::CapturedFile{step,bytes:metadata.len(),device,inode,modified_ns:metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos()};
                let capture=serde_json::to_string(&file)?;anyhow::ensure!(capture.len()<=1024,"historical file metadata exceeds its per-row bound");
                files.push(accepted::PreparedFile{step,bytes:file.bytes,capture});
            }
            anyhow::ensure!(directory_identity(&capture.path)?==capture.identity,"historical directory changed during its captured page");
            Ok((capture,files,done))
        }).await?
    }
    async fn verify_directory(&self,job:&str,expected:String)->anyhow::Result<()> {
        let path=self.logs().job_dir(job);
        crate::rt::blocking(move ||{
            anyhow::ensure!(serde_json::to_string(&directory_identity(&path)?)?==expected,"historical directory changed after its original capture");Ok(())
        }).await?
    }
    async fn captured_chunk(&self,job:&str,captured:&accepted::CapturedFile,offset:u64)->anyhow::Result<Vec<u8>> {
        let path=self.logs().step_path(job,captured.step);let captured=captured.clone();
        crate::rt::blocking(move ||{
            use std::io::{Read as _,Seek as _};
            anyhow::ensure!(offset<=captured.bytes,"historical offset exceeds its captured prefix");
            let mut file=open_capture(&path)?;let metadata=file.metadata()?;
            anyhow::ensure!(capture_identity(&metadata,&file)?==(captured.device,captured.inode) && metadata.len()==captured.bytes && metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos()==captured.modified_ns,"historical log identity changed during retained capture");
            let length=(captured.bytes-offset).min(u64::try_from(accepted::CHUNK)?);
            file.seek(std::io::SeekFrom::Start(offset))?;
            let mut bytes=vec![0;usize::try_from(length)?];file.read_exact(&mut bytes)?;
            let after=file.metadata()?;
            anyhow::ensure!(capture_identity(&after,&file)?==(captured.device,captured.inode) && after.len()==captured.bytes && after.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos()==captured.modified_ns,"historical log changed while reading its captured chunk");
            Ok(bytes)
        }).await?
    }
}

struct Capture {path:PathBuf,identity:CapturedDir,rows:Option<std::fs::ReadDir>}
#[derive(Clone,Debug,serde::Serialize,PartialEq,Eq)]
struct CapturedDir {present:bool,device:u64,inode:u64,modified_ns:u128}
fn directory_identity(path:&std::path::Path)->anyhow::Result<CapturedDir> {
    let metadata=match std::fs::symlink_metadata(path) {
        Ok(metadata)=>metadata,
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(CapturedDir{present:false,device:0,inode:0,modified_ns:0}),
        Err(error)=>return Err(error.into()),
    };
    anyhow::ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(),"historical log directory is not a regular owned directory");
    let mut options=std::fs::OpenOptions::new();options.read(true);
    #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt as _;options.custom_flags(nix::libc::O_DIRECTORY|nix::libc::O_NOFOLLOW);}
    #[cfg(windows)] {use std::os::windows::fs::OpenOptionsExt as _;options.custom_flags(0x02000000|0x00200000);}
    let file=options.open(path)?;let metadata=file.metadata()?;
    anyhow::ensure!(metadata.is_dir() && !reparse(&metadata),"historical directory handle is not a plain directory");
    let (device,inode)=capture_identity(&metadata,&file)?;
    Ok(CapturedDir{present:true,device,inode,modified_ns:metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos()})
}

fn open_capture(path:&std::path::Path)->anyhow::Result<std::fs::File> {
    anyhow::ensure!(!std::fs::symlink_metadata(path)?.file_type().is_symlink(),"historical log is a symlink");
    let mut options=std::fs::OpenOptions::new();options.read(true);
    #[cfg(unix)] {use std::os::unix::fs::OpenOptionsExt as _;options.custom_flags(nix::libc::O_NOFOLLOW);}
    #[cfg(windows)] {use std::os::windows::fs::OpenOptionsExt as _;options.custom_flags(0x00200000);}
    let file=options.open(path)?;let metadata=file.metadata()?;
    anyhow::ensure!(metadata.is_file() && !reparse(&metadata),"historical capture requires a plain regular file");Ok(file)
}

#[cfg(unix)]
fn capture_identity(metadata:&std::fs::Metadata,_file:&std::fs::File)->anyhow::Result<(u64,u64)> {
    use std::os::unix::fs::MetadataExt as _;Ok((metadata.dev(),metadata.ino()))
}
#[cfg(windows)]
fn capture_identity(_metadata:&std::fs::Metadata,file:&std::fs::File)->anyhow::Result<(u64,u64)> {
    // Existing dependency/API already used by Observe; no timestamp/path identity fallback.
    let info=winapi_util::file::information(file)?;Ok((info.volume_serial_number(),info.file_index()))
}
#[cfg(not(any(unix,windows)))]
fn capture_identity(_metadata:&std::fs::Metadata,_file:&std::fs::File)->anyhow::Result<(u64,u64)> {
    anyhow::bail!("historical baseline capture is unavailable on this platform; original files stay explicitly unqualified")
}

#[cfg(windows)]
fn reparse(metadata:&std::fs::Metadata)->bool {
    use std::os::windows::fs::MetadataExt as _;metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn reparse(metadata:&std::fs::Metadata)->bool {metadata.file_type().is_symlink()}

fn safe(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

impl Logs {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self { dir, max_bytes, live: Arc::new(Mutex::new(HashMap::new())),io:Arc::new(Mutex::new(accepted::Owner::default())) }
    }

    pub(crate) fn bind_accepted(&self,store:crate::store::Store,blobs:Arc<dyn kiln_seams::blobs::Blobs>,host:String) {
        self.io.lock().bind(accepted::Context{store,blobs,host,files:Projection{dir:self.dir.clone(),max_bytes:self.max_bytes,live:Arc::clone(&self.live)}});
    }

    fn job_dir(&self, job: &str) -> PathBuf {
        let j = safe(job);
        let shard = j.get(j.len().saturating_sub(2)..).unwrap_or("00").to_owned();
        self.dir.join(shard).join(j)
    }

    fn step_path(&self, job: &str, idx: u32) -> PathBuf {
        self.job_dir(job).join(format!("{idx}.log"))
    }

    fn times_path(&self, job: &str, idx: u32) -> PathBuf {
        self.job_dir(job).join(format!("{idx}.times"))
    }

    fn with_live<R>(&self, job: &str, f: impl FnOnce(&mut Live) -> R) -> R {
        let mut g = self.live.lock();
        let live = g.entry(job.to_owned()).or_insert_with(|| {
            Live { tx: crate::bus::Broadcast::new(1024), files: HashMap::new(), times: HashMap::new(), bytes: 0, truncated: false }
        });
        f(live)
    }

    /// Unbound file/live compatibility only. A bound owner refuses this void API before
    /// mutation; production writers use its bounded original operation and durable receipt.
    pub fn append(&self,job:&str,idx:u32,data:&[u8]) {
        if self.io.lock().bound() {
            tracing::warn!(job,idx,"direct mutable-file append refused on bound accepted Logs owner");return;
        }
        self.project(job,idx,data)
    }
    #[cfg(test)]
    pub(crate) fn historical_fixture_append(&self,job:&str,idx:u32,data:&[u8]) {self.project(job,idx,data)}

    fn project(&self, job: &str, idx: u32, data: &[u8]) {
        crate::metrics::global().log_bytes.add(data.len() as u64);
        let path = self.step_path(job, idx);
        let times_path = self.times_path(job, idx);
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
            // Where this piece starts in the step's log (whatever else wrote to it, a retry's
            // mark), noted with when it came once it's written.
            let at = file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len());
            let wrote = match file {
                Some(f) => match f.write_all(chunk) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(error = %e, "log write failed");
                        false
                    }
                },
                None => false,
            };
            // A piece that holds no bytes holds no line: nothing to note.
            if let (true, Some(at), false) = (wrote, at, chunk.is_empty()) {
                let times = match live.times.entry(idx) {
                    std::collections::hash_map::Entry::Occupied(o) => Some(o.into_mut()),
                    std::collections::hash_map::Entry::Vacant(v) => match std::fs::OpenOptions::new().create(true).append(true).open(&times_path) {
                        Ok(f) => Some(v.insert(f)),
                        Err(e) => {
                            tracing::warn!(error = %e, path = %times_path.display(), "cannot open log times");
                            None
                        }
                    },
                };
                if let Some(t) = times
                    && let Err(e) = writeln!(t, "{at} {}", crate::util::now_ms())
                {
                    tracing::warn!(error = %e, "log times write failed");
                }
            }
            for part in chunk.chunks(64 * 1024) {
                let _ = live.tx.send(LogEvent::Data { idx, data: Bytes::copy_from_slice(part) });
            }
        });
    }

    /// A compatibility status hint; this does not close the live channel or qualify a log
    /// frontier. Earlier admitted bytes may still project after a terminal step hint.
    pub fn step(&self, job: &str, idx: u32, status: &'static str) {
        self.with_live(job, |live| {
            if status != "running" {
                live.files.remove(&idx);
                live.times.remove(&idx);
            }
            let _ = live.tx.send(LogEvent::Step { idx, status: status.into() });
        });
    }

    /// Defer the compatibility End while this job has already-owned/queued input; the
    /// last completed projection/refusal closes files and the original channel. This
    /// synchronous control call neither waits for storage nor proves producer closure.
    pub fn finish(&self, job: &str) {
        // Control/physical release never awaits retained storage. Only already-admitted
        // inputs delay this compatibility End; later unsequenced data is not closure.
        if self.io.lock().defer_finish(job) {return}
        self.finish_projection(job)
    }
    fn finish_projection(&self,job:&str) {
        if let Some(live) = self.live.lock().remove(job) {
            let _ = live.tx.send(LogEvent::End);
        }
    }

    /// Subscribe to a job's live output. Subscribe *before* reading the backlog so nothing falls
    /// in between (viewers may see a few bytes twice at the seam, never miss any).
    pub fn subscribe(&self, job: &str) -> crate::bus::Receiver<LogEvent> {
        self.with_live(job, |live| live.tx.subscribe())
    }
    /// Observe an existing publisher without creating a compatibility Live/topic. Strict
    /// readers resnapshot canonical state after subscribing and refuse an absent publisher
    /// while incomplete; a completed immutable read needs no new live object.
    pub(crate) fn subscribe_existing(&self,job:&str)->Option<crate::bus::Receiver<LogEvent>> {
        self.live.lock().get(job).map(|live|live.tx.subscribe())
    }

    pub fn is_live(&self, job: &str) -> bool {
        self.live.lock().contains_key(job)
    }

    pub async fn read(&self, job: &str, idx: u32) -> Vec<u8> {
        let p = self.step_path(job, idx);
        crate::rt::blocking(move || std::fs::read(p).unwrap_or_default()).await.unwrap_or_default()
    }

    /// The derived index reads a captured finite regular file, keeping only its normalized
    /// prefix. Errors remain backlog; they never become an empty successful log. The source
    /// byte count is the captured file size, even when the prefix needs only its beginning.
    pub(crate) async fn index_text(&self, job: &str, idx: u32, limit: usize, source_limit: u64) -> anyhow::Result<Option<(String, u64)>> {
        let path = self.step_path(job, idx);
        let declared=self.max_bytes.saturating_add(1024);
        crate::rt::blocking(move || -> anyhow::Result<Option<(String, u64)>> {
            use std::io::Read as _;
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some((String::new(), 0))),
                Err(e) => return Err(e.into()),
            };
            let metadata = file.metadata()?;
            anyhow::ensure!(metadata.is_file(), "log indexing requires a regular file");
            let bytes = metadata.len();
            anyhow::ensure!(bytes<=declared,"log file exceeds its declared job-log byte bound; left unindexed");
            if bytes>source_limit{return Ok(None)}
            Ok(Some((crate::ci::logsearch::plain_reader(file.take(bytes), limit)?, bytes)))
        }).await?
    }

    /// When each piece of a step's log arrived, oldest first: its byte offset in the log and unix
    /// ms. A log written before kiln noted them has none.
    pub async fn times(&self, job: &str, idx: u32) -> Vec<(u64, i64)> {
        let p = self.times_path(job, idx);
        crate::rt::blocking(move || std::fs::read_to_string(p).map(|s| times_of(&s)).unwrap_or_default()).await.unwrap_or_default()
    }

    /// The last `max` bytes of a step's log on disk (all of it when it's shorter).
    pub async fn tail(&self, job: &str, idx: u32, max: u64) -> Vec<u8> {
        let p = self.step_path(job, idx);
        crate::rt::blocking(move || tail_of(&p, max).unwrap_or_default()).await.unwrap_or_default()
    }

    /// A retry boundary is another server-observed append, never a producer close.
    /// An unbound legacy Logs value remains explicitly file-only compatibility.
    pub async fn mark(&self,job:&str,idx:u32,line:&str) {
        let text=format!("\r\n\x1b[33m[kiln] {line}\x1b[0m\r\n");
        if self.io.lock().bound() {
            let result=async {
                let ctx=self.accepted_context()?;let id=job.to_owned();
                let captured=ctx.store.call(move |c|{
                    if let Some(prefix)=accepted::prefix(c,&id)? {
                        return Ok(Some(c.query_row("SELECT step_bytes FROM ci_log_accepted_chunks WHERE view=?1 AND step=?2 AND revision<=?3 ORDER BY revision DESC LIMIT 1",crate::sql::params![prefix.view,idx,prefix.revision],|r|r.get::<_,u64>(0)).optional()?.unwrap_or(0)));
                    }Ok(None)
                }).await?;
                let nonempty=if let Some(bytes)=captured {bytes>0}else{ctx.files.copy_capture(job,idx).await?.0.is_some_and(|file|file.bytes>0)};
                if nonempty {let _=self.append_server(job.to_owned(),idx,text.into_bytes()).await?;}Ok::<_,anyhow::Error>(())
            }.await;
            if let Err(error)=result {tracing::warn!(job,error=%format!("{error:#}"),"retry mark remains unaccepted; compatibility source unchanged");}return;
        }
        let path=self.step_path(job,idx);
        let result=crate::rt::blocking(move ||->std::io::Result<()>{if std::fs::metadata(&path).map_or(0,|m|m.len())==0{return Ok(())}std::fs::OpenOptions::new().append(true).open(path)?.write_all(text.as_bytes())}).await;
        if let Ok(Err(error))|Err(error)=result.map_err(|e|std::io::Error::other(format!("{e:#}"))){tracing::warn!(job,error=%error,"unqualified retry mark failed");}
    }

    /// Size of the backlog already on disk (for the UI's "download" affordance).
    pub async fn size(&self, job: &str, idx: u32) -> u64 {
        let p = self.step_path(job, idx);
        crate::rt::blocking(move || std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).await.unwrap_or(0)
    }

    pub async fn remove(&self, jobs: Vec<String>) {
        if self.io.lock().bound() {
            let result=async {
                let ctx=self.accepted_context()?;
                for page in jobs.chunks(accepted::PAGE) {
                    let page=page.to_vec();
                    ctx.store.call(move |c|{let tx=crate::store::tx(c)?;for job in page{tx.execute("INSERT INTO ci_log_deferred_delete(job) VALUES(?1) ON CONFLICT(job) DO NOTHING",[job])?;}tx.commit()?;Ok(())}).await?;
                }
                crate::ci::logsearch::kick();Ok::<_,anyhow::Error>(())
            }.await;
            if let Err(error)=result{tracing::warn!(error=%format!("{error:#}"),"deferred log deletion intent remains unknown; no physical deletion");}return;
        }
        let dirs: Vec<PathBuf> = jobs.iter().map(|j| self.job_dir(j)).collect();
        let r = crate::rt::blocking(move || {
            for d in dirs {
                if let Err(e) = std::fs::remove_dir_all(&d)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(error = %e, dir = %d.display(), "log prune failed");
                }
            }
        })
        .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "log prune task failed");
        }
    }
}

/// `<step>.times` read: a line each, its offset and its ms; a line torn by a crash is left out.
fn times_of(s: &str) -> Vec<(u64, i64)> {
    s.lines()
        .filter_map(|l| {
            let (at, ms) = l.split_once(' ')?;
            Some((at.parse().ok()?, ms.parse().ok()?))
        })
        .collect()
}

/// [`Logs::tail`]'s read, from where the last `max` bytes start.
fn tail_of(p: &std::path::Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(p)?;
    let len = f.metadata()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut out = Vec::new();
    f.take(max).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_tail_and_cap() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
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
        // Its end, for what a failed step lacked (toolfix): the last bytes, or all of a short one.
        assert_eq!(Some(logs.tail("job1", 0, 5).await.as_slice()), on_disk.get(on_disk.len().saturating_sub(5)..));
        assert_eq!(logs.tail("job1", 0, 1 << 20).await, on_disk);
        assert!(logs.tail("job2", 0, 5).await.is_empty());
        let mut n = 0;
        while let Ok(ev) = rx.recv().await {
            n += 1;
            if matches!(ev, LogEvent::End) {
                break;
            }
        }
        assert_eq!(n, 4);
        Ok(())
    })
    }

    /// Plan 01a1856fe81375b1bd1d1d86f270a45b, step 9: each piece of a step's log is noted with
    /// where it starts and when it came, as written, in order; a retry's mark between pieces
    /// moves the next one's offset, and a torn line in the file is left out.
    #[test]
    fn each_piece_of_a_log_is_noted_with_where_it_starts_and_when() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let logs = Logs::new(dir.path().to_path_buf(), 1 << 20);
            let before = crate::util::now_ms();
            logs.append("job1", 0, b"$ cargo clippy\n");
            logs.append("job1", 0, b"    Checking kiln\n");
            logs.mark("job1", 0, "attempt 2").await;
            logs.append("job1", 0, b"done\n");
            logs.append("job1", 1, b"step two\n");
            logs.finish("job1");
            let log = logs.read("job1", 0).await;
            let times = logs.times("job1", 0).await;
            let offsets: Vec<u64> = times.iter().map(|(at, _)| *at).collect();
            let done = log.windows(5).position(|w| w == b"done\n").and_then(|i| u64::try_from(i).ok());
            assert_eq!(offsets, vec![0, 15, done.unwrap_or(u64::MAX)]);
            assert!(times.iter().all(|(_, ms)| *ms >= before) && times.windows(2).all(|w| w.first().map(|a| a.1) <= w.get(1).map(|b| b.1)), "{times:?}");
            assert_eq!(logs.times("job1", 1).await.iter().map(|(at, _)| *at).collect::<Vec<_>>(), vec![0]);
            assert!(logs.times("job2", 0).await.is_empty());
            assert_eq!(times_of("0 1700000000000\n12 17000000\nnot a line\n40"), vec![(0, 1_700_000_000_000), (12, 17_000_000)]);
            Ok(())
        })
    }
}
