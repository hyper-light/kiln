//! `kiln gocacheprog` (docs/ci.md W24-R08): the go command's `GOCACHEPROG` helper, Go 1.24's
//! protocol (`cmd/go/internal/cacheprog`) on stdin and stdout, over the runner's cache service.
//!
//! The go command writes one JSON request per line (a `put`'s body follows on a line of its own,
//! a JSON string of base64) and reads JSON responses, which may come in any order. The helper
//! announces `get`, `put` and `close` first. An action's output is kept on disk where the go
//! command reads it (`DiskPath`, under `KILN_GOCACHEPROG_DIR`) and on the service as the entry
//! `go/<action id>`: a hit this machine's disk lacks is fetched, checked against its digest (an
//! output's id is its SHA-256) and kept; a put goes to the service after the go command has its
//! answer. A service that can't be reached makes misses, never a failed build.
//!
//! ```text
//! GOCACHEPROG="kiln gocacheprog" KILN_CACHE_URL=http://127.0.0.1:41234 KILN_CACHE_TOKEN=… go build ./...
//! ```

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use base64::Engine as _;
use serde_json::{Value, json};

use super::client::{Conn, Endpoint};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Requests answered at once.
const WORKERS: usize = 8;
/// The service's namespace for Go's actions.
const NS: &str = "go";

#[derive(clap::Args, Clone, Debug, Default)]
pub struct Args {
    /// The runner's cache service: `http://127.0.0.1:<port>` or `unix:/path`.
    #[arg(long, env = "KILN_CACHE_URL")]
    pub url: Option<String>,
    /// A token of the service's for this repository and trust domain.
    #[arg(long, env = "KILN_CACHE_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// Where outputs are kept for the go command to read (the user's cache folder's
    /// `kiln/gocacheprog` by default).
    #[arg(long, env = "KILN_GOCACHEPROG_DIR")]
    pub dir: Option<PathBuf>,
}

/// What answers the go command's requests.
#[derive(Debug, Clone)]
pub struct Helper {
    dir: PathBuf,
    remote: Option<(Endpoint, String)>,
    /// One warning about the service is enough.
    warned: Arc<AtomicBool>,
}

/// `kiln gocacheprog`.
pub fn main(a: Args) -> anyhow::Result<()> {
    let dir = match a.dir {
        Some(d) if d.is_absolute() => d,
        Some(d) => std::env::current_dir()?.join(d),
        None => crate::os::home_dir().join(".cache").join("kiln").join("gocacheprog"),
    };
    let remote = match (a.url, a.token) {
        (Some(u), Some(t)) => Some((Endpoint::parse(&u)?, t)),
        (Some(_), None) => {
            eprintln!("kiln gocacheprog: KILN_CACHE_URL without KILN_CACHE_TOKEN: keeping outputs on this machine only");
            None
        }
        _ => None,
    };
    let helper = Helper::new(dir, remote)?;
    serve(std::io::BufReader::new(std::io::stdin()), std::io::stdout(), helper)
}

impl Helper {
    pub fn new(dir: PathBuf, remote: Option<(Endpoint, String)>) -> anyhow::Result<Helper> {
        std::fs::create_dir_all(dir.join("o"))?;
        std::fs::create_dir_all(dir.join("a"))?;
        Ok(Helper { dir, remote, warned: Arc::new(AtomicBool::new(false)) })
    }

    fn output_path(&self, output: &str) -> PathBuf {
        self.dir.join("o").join(output)
    }

    fn warn(&self, what: &str) {
        if !self.warned.swap(true, Ordering::Relaxed) {
            eprintln!("kiln gocacheprog: the cache service: {what} (this build keeps going on this machine's cache)");
        }
    }

    /// An action's output this machine has: its id, size and when it was put.
    fn local(&self, action: &str) -> Option<(String, u64, String)> {
        let v: Value = serde_json::from_slice(&std::fs::read(self.dir.join("a").join(action)).ok()?).ok()?;
        let output = v.get("output")?.as_str()?.to_owned();
        let size = v.get("size")?.as_u64()?;
        let time = v.get("time").and_then(Value::as_str).unwrap_or_default().to_owned();
        let on_disk = std::fs::metadata(self.output_path(&output)).ok()?;
        (on_disk.len() == size).then_some((output, size, time))
    }

    /// Keeps an output and its action's record on disk: where the output is.
    fn keep(&self, action: &str, output: &str, body: &[u8], time: &str) -> anyhow::Result<PathBuf> {
        let at = self.output_path(output);
        if std::fs::metadata(&at).map(|m| m.len()).ok() != Some(body.len() as u64) {
            write_atomic(&at, body)?;
        }
        write_atomic(&self.dir.join("a").join(action), serde_json::to_vec(&json!({ "output": output, "size": body.len(), "time": time }))?.as_slice())?;
        Ok(at)
    }

    fn hit(&self, id: i64, output: &str, size: u64, time: &str) -> Value {
        let mut v = json!({ "ID": id, "OutputID": B64.encode(hex::decode(output).unwrap_or_default()), "Size": size, "DiskPath": self.output_path(output).to_string_lossy() });
        if let (false, Some(m)) = (time.is_empty(), v.as_object_mut()) {
            m.insert("Time".into(), json!(time));
        }
        v
    }

    /// A `get`: this machine's disk, else the service's entry (kept here then), else a miss.
    fn get(&self, conn: &mut Option<Conn>, id: i64, action: &str) -> Value {
        if let Some((output, size, time)) = self.local(action) {
            return self.hit(id, &output, size, &time);
        }
        let (Some(c), Some((_, token))) = (conn.as_mut(), self.remote.as_ref()) else { return json!({ "ID": id, "Miss": true }) };
        let auth = format!("Bearer {token}");
        match c.request("GET", &format!("/_kiln/cas/v1/entries/{NS}/{action}"), &[("authorization", auth.as_str())], b"") {
            Ok(r) if r.status == 200 => {
                let output = crate::util::sha256_hex(&r.body);
                if r.header("x-kiln-digest").is_some_and(|d| d != output) {
                    self.warn("an output wasn't what the service said it was");
                    return json!({ "ID": id, "Miss": true });
                }
                let time = crate::util::now_rfc3339();
                match self.keep(action, &output, &r.body, &time) {
                    Ok(_) => self.hit(id, &output, r.body.len() as u64, &time),
                    Err(e) => json!({ "ID": id, "Err": format!("keeping an output on disk: {e:#}") }),
                }
            }
            Ok(r) if r.status == 404 => json!({ "ID": id, "Miss": true }),
            Ok(r) => {
                self.warn(&format!("HTTP {} {}", r.status, String::from_utf8_lossy(&r.body).trim()));
                json!({ "ID": id, "Miss": true })
            }
            Err(e) => {
                self.warn(&format!("{e:#}"));
                json!({ "ID": id, "Miss": true })
            }
        }
    }

    /// A `put`: kept on disk and answered, then sent to the service.
    fn put(&self, conn: &mut Option<Conn>, id: i64, action: &str, output: Option<&str>, body: &[u8], answer: &dyn Fn(&Value)) {
        let digest = crate::util::sha256_hex(body);
        if let Some(o) = output.filter(|o| *o != digest) {
            answer(&json!({ "ID": id, "Err": format!("the body hashes to {digest}, not its OutputID {o}") }));
            return;
        }
        match self.keep(action, &digest, body, &crate::util::now_rfc3339()) {
            Ok(at) => answer(&json!({ "ID": id, "DiskPath": at.to_string_lossy() })),
            Err(e) => {
                answer(&json!({ "ID": id, "Err": format!("keeping an output on disk: {e:#}") }));
                return;
            }
        }
        let (Some(c), Some((_, token))) = (conn.as_mut(), self.remote.as_ref()) else { return };
        let auth = format!("Bearer {token}");
        match c.request("PUT", &format!("/_kiln/cas/v1/entries/{NS}/{action}"), &[("authorization", auth.as_str()), ("content-type", "application/octet-stream")], body) {
            Ok(r) if (200..300).contains(&r.status) => {}
            Ok(r) => self.warn(&format!("HTTP {} {}", r.status, String::from_utf8_lossy(&r.body).trim())),
            Err(e) => self.warn(&format!("{e:#}")),
        }
    }
}

fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", crate::util::new_id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// One request's work.
enum Job {
    Get { id: i64, action: String },
    Put { id: i64, action: String, output: Option<String>, body: Vec<u8> },
}

/// The next line that isn't blank (the go command follows each request with an empty line);
/// `None` at the end.
fn next_line<R: BufRead>(input: &mut R) -> anyhow::Result<Option<String>> {
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if !line.is_empty() {
            return Ok(Some(line.to_owned()));
        }
    }
}

/// A `[]byte` field as the go command sends it (base64), in hex.
fn bytes_field(v: &Value, name: &str) -> Option<String> {
    v.get(name).and_then(Value::as_str).and_then(|s| B64.decode(s).ok()).map(hex::encode)
}

/// Answers the go command's requests on `input` with responses on `output` until it says `close`
/// (or its input ends).
pub fn serve<R: BufRead, W: Write + Send + 'static>(mut input: R, output: W, helper: Helper) -> anyhow::Result<()> {
    let out = Arc::new(Mutex::new(output));
    let say = {
        let out = Arc::clone(&out);
        move |v: &Value| {
            let mut line = serde_json::to_vec(v).unwrap_or_default();
            line.push(b'\n');
            let mut w = out.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = w.write_all(&line);
            let _ = w.flush();
        }
    };
    say(&json!({ "ID": 0, "KnownCommands": ["get", "put", "close"] }));
    let (tx, rx) = mpsc::channel::<Job>();
    let rx = Arc::new(Mutex::new(rx));
    let workers: Vec<std::thread::JoinHandle<()>> = (0..WORKERS)
        .map(|_| {
            let (rx, helper, say) = (Arc::clone(&rx), helper.clone(), say.clone());
            std::thread::spawn(move || {
                let mut conn = helper.remote.as_ref().map(|(e, _)| Conn::new(e.clone()));
                loop {
                    let job = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    match job {
                        Ok(Job::Get { id, action }) => say(&helper.get(&mut conn, id, &action)),
                        Ok(Job::Put { id, action, output, body }) => helper.put(&mut conn, id, &action, output.as_deref(), &body, &say),
                        Err(_) => return,
                    }
                }
            })
        })
        .collect();
    let finish = |tx: mpsc::Sender<Job>, workers: Vec<std::thread::JoinHandle<()>>| {
        drop(tx);
        for w in workers {
            let _ = w.join();
        }
    };
    while let Some(line) = next_line(&mut input)? {
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => anyhow::bail!("a request that isn't JSON: {e}"),
        };
        let id = req.get("ID").and_then(Value::as_i64).unwrap_or(0);
        let action = bytes_field(&req, "ActionID").unwrap_or_default();
        match req.get("Command").and_then(Value::as_str).unwrap_or("") {
            "get" => {
                let _ = tx.send(Job::Get { id, action });
            }
            "put" => {
                let size = req.get("BodySize").and_then(Value::as_i64).unwrap_or(0);
                let body = if size > 0 {
                    let text = next_line(&mut input)?.ok_or_else(|| anyhow::anyhow!("a put's body never came"))?;
                    let b64: String = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("a put's body isn't a JSON string: {e}"))?;
                    B64.decode(b64.as_bytes()).map_err(|e| anyhow::anyhow!("a put's body isn't base64: {e}"))?
                } else {
                    Vec::new()
                };
                // Go 1.23 and earlier said `ObjectID`.
                let output = bytes_field(&req, "OutputID").or_else(|| bytes_field(&req, "ObjectID"));
                let _ = tx.send(Job::Put { id, action, output, body });
            }
            "close" => {
                finish(tx, workers);
                say(&json!({ "ID": id }));
                return Ok(());
            }
            other => say(&json!({ "ID": id, "Err": format!("kiln gocacheprog doesn't know the command {other:?}") })),
        }
    }
    finish(tx, workers);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::service::tests::{access, on_thread, serve as service};
    use super::*;

    /// What a test reads back of the helper's output.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Requests as cmd/go writes them: each JSON object, a newline, then an empty line; a put's
    /// body follows as a JSON string of base64.
    fn request(id: i64, command: &str, action: &[u8], body: Option<&[u8]>) -> String {
        let mut v = json!({ "ID": id, "Command": command });
        if let Some(m) = v.as_object_mut() {
            if !action.is_empty() {
                m.insert("ActionID".into(), json!(B64.encode(action)));
            }
            if let Some(b) = body {
                let out = hex::decode(crate::util::sha256_hex(b)).unwrap_or_default();
                m.insert("OutputID".into(), json!(B64.encode(&out)));
                m.insert("ObjectID".into(), json!(B64.encode(&out)));
                if !b.is_empty() {
                    m.insert("BodySize".into(), json!(b.len()));
                }
            }
        }
        let mut s = format!("{v}\n\n");
        if let Some(b) = body.filter(|b| !b.is_empty()) {
            s.push_str(&format!("\"{}\"\n", B64.encode(b)));
        }
        s
    }

    fn responses(out: &Shared) -> anyhow::Result<Vec<Value>> {
        let text = String::from_utf8(out.0.lock().unwrap_or_else(PoisonError::into_inner).clone())?;
        Ok(text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<Result<_, _>>()?)
    }

    fn by_id(all: &[Value], id: i64) -> Value {
        all.iter().find(|v| v.get("ID").and_then(Value::as_i64) == Some(id)).cloned().unwrap_or(Value::Null)
    }

    /// W24-AC08 (Go, recorded): cmd/go's requests answered as Go 1.24 reads them: the commands
    /// announced, a miss, a put's DiskPath, a hit from disk; then on another machine's disk (a
    /// fresh folder), the hit fetched from the service, its OutputID the output's digest.
    #[test]
    fn w24_ac08_gocacheprog_speaks_go_1_24s_protocol() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        crate::rt::block_on(async move {
            let (svc, at) = service(&root.join("runner")).await?;
            let token = svc.mint(&access("acme/app", "trusted", true, true))?;
            let endpoint = Endpoint::Tcp { addr: at.to_string(), prefix: String::new() };
            let action = [7u8; 32];
            let body = b"package object bytes".to_vec();
            let (r1, ep, tk, b1) = (root.clone(), endpoint.clone(), token.clone(), body.clone());
            let first = on_thread(move || -> anyhow::Result<Vec<Value>> {
                let input = [request(1, "get", &action, None), request(2, "put", &action, Some(&b1)), request(3, "put", &[8u8; 32], Some(b"")), request(4, "put", &[9u8; 32], Some(b"x")), request(5, "close", &[], None)].concat();
                // A put whose body isn't its OutputID is refused.
                let input = input.replace(&format!("\"{}\"", B64.encode(b"x")), &format!("\"{}\"", B64.encode(b"y")));
                let out = Shared::default();
                super::serve(std::io::Cursor::new(input), out.clone(), Helper::new(r1.join("disk1"), Some((ep, tk)))?)?;
                responses(&out)
            })
            .await??;
            anyhow::ensure!(first.first() == Some(&json!({ "ID": 0, "KnownCommands": ["get", "put", "close"] })), "{first:?}");
            anyhow::ensure!(by_id(&first, 1).get("Miss") == Some(&json!(true)), "{first:?}");
            let put = by_id(&first, 2);
            let disk = put.get("DiskPath").and_then(Value::as_str).unwrap_or_default().to_owned();
            anyhow::ensure!(std::fs::read(&disk)? == body, "{put}");
            anyhow::ensure!(by_id(&first, 3).get("DiskPath").is_some(), "an empty output is an output");
            anyhow::ensure!(by_id(&first, 4).get("Err").is_some(), "{first:?}");
            anyhow::ensure!(first.last() == Some(&json!({ "ID": 5 })), "close is answered last: {first:?}");
            svc.settled().await;
            // A fresh disk: the hit comes from the service.
            let (r2, ep2, tk2) = (root.clone(), endpoint.clone(), token.clone());
            let second = on_thread(move || -> anyhow::Result<Vec<Value>> {
                let out = Shared::default();
                let input = [request(1, "get", &action, None), request(2, "get", &[1u8; 32], None), request(3, "close", &[], None)].concat();
                super::serve(std::io::Cursor::new(input), out.clone(), Helper::new(r2.join("disk2"), Some((ep2, tk2)))?)?;
                responses(&out)
            })
            .await??;
            let hit = by_id(&second, 1);
            let want = B64.encode(hex::decode(crate::util::sha256_hex(&body))?);
            anyhow::ensure!(hit.get("OutputID") == Some(&json!(want)) && hit.get("Size") == Some(&json!(body.len())) && hit.get("Time").is_some(), "{hit}");
            let disk2 = hit.get("DiskPath").and_then(Value::as_str).unwrap_or_default().to_owned();
            anyhow::ensure!(disk2.contains("disk2") && std::fs::read(&disk2)? == body, "{hit}");
            anyhow::ensure!(by_id(&second, 2).get("Miss") == Some(&json!(true)));
            // Another repository's token: a miss.
            let other = svc.mint(&access("acme/other", "trusted", true, true))?;
            let r3 = root.clone();
            let third = on_thread(move || -> anyhow::Result<Vec<Value>> {
                let out = Shared::default();
                super::serve(std::io::Cursor::new([request(1, "get", &action, None), request(2, "close", &[], None)].concat()), out.clone(), Helper::new(r3.join("disk3"), Some((endpoint, other)))?)?;
                responses(&out)
            })
            .await??;
            anyhow::ensure!(by_id(&third, 1).get("Miss") == Some(&json!(true)), "{third:?}");
            Ok(())
        })
    }
}
