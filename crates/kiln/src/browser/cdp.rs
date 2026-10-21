//! The Chrome DevTools Protocol: commands answered by their id, events handed to whoever listens
//! for a page's session (sessions flattened onto the one connection). Over the browser's own pipe
//! where there is one (Unix: it ends when the pipe closes, so with kiln, and nothing else on the
//! machine can reach it), else over its DevTools socket on loopback.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_tungstenite::tungstenite::Message;
use futures::StreamExt as _;
use parking_lot::Mutex;
use serde_json::{Value, json};

/// A command the browser doesn't answer in this long is given up on.
const ANSWER: Duration = Duration::from_secs(60);

/// Something the browser said of its own accord: a page loaded, a console line, a frame.
#[derive(Debug, Clone)]
pub struct Event {
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

type Answer = Result<Value, String>;

pub struct Cdp {
    out: async_channel::Sender<String>,
    pending: Mutex<HashMap<u64, futures::channel::oneshot::Sender<Answer>>>,
    listeners: Mutex<Vec<(Option<String>, async_channel::Sender<Event>)>>,
    next: AtomicU64,
    gone: AtomicBool,
}

impl Cdp {
    fn new(out: async_channel::Sender<String>) -> Arc<Self> {
        Arc::new(Self { out, pending: Mutex::default(), listeners: Mutex::default(), next: AtomicU64::new(1), gone: AtomicBool::new(false) })
    }

    /// Over the browser's pipe (`--remote-debugging-pipe`): `to` is what it reads (its fd 3), `from`
    /// what it writes (its fd 4), each message ending in a NUL. Read and written on threads of
    /// their own, as the pipe blocks.
    #[cfg(unix)]
    pub fn over_pipe(to: std::fs::File, from: std::fs::File) -> anyhow::Result<Arc<Self>> {
        use std::io::{BufRead as _, Write as _};
        let (out, to_send) = async_channel::unbounded::<String>();
        let cdp = Self::new(out);
        std::thread::Builder::new().name("kiln-browser-out".into()).spawn(move || {
            let mut w = std::io::BufWriter::new(to);
            while let Ok(m) = to_send.recv_blocking() {
                if w.write_all(m.as_bytes()).and_then(|()| w.write_all(&[0])).and_then(|()| w.flush()).is_err() {
                    break;
                }
            }
        })?;
        let reader = Arc::downgrade(&cdp);
        std::thread::Builder::new().name("kiln-browser-in".into()).spawn(move || {
            let mut r = std::io::BufReader::with_capacity(1 << 20, from);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(0, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.last() == Some(&0) {
                            buf.pop();
                        }
                        let Some(cdp) = reader.upgrade() else { return };
                        cdp.heard(&String::from_utf8_lossy(&buf));
                    }
                }
            }
            if let Some(cdp) = reader.upgrade() {
                cdp.lost();
            }
        })?;
        Ok(cdp)
    }

    /// Over a browser's DevTools socket (`ws://127.0.0.1:<port>/devtools/browser/<id>`).
    pub async fn connect(url: &str) -> anyhow::Result<Arc<Self>> {
        let client = crate::http::client::Client::public()?;
        // A screenshot of a tall page comes as one message.
        let ws = client.websocket(url, 256 << 20).await?;
        let (mut sink, mut stream) = ws.split();
        let (out, to_send) = async_channel::unbounded::<String>();
        let cdp = Self::new(out);
        crate::rt::spawn(async move {
            while let Ok(m) = to_send.recv().await {
                if sink.send(Message::text(m)).await.is_err() {
                    break;
                }
            }
        });
        let reader = Arc::downgrade(&cdp);
        crate::rt::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Some(cdp) = reader.upgrade() else { return };
                match msg {
                    Message::Text(t) => cdp.heard(t.as_str()),
                    Message::Binary(b) => cdp.heard(&String::from_utf8_lossy(&b)),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            if let Some(cdp) = reader.upgrade() {
                cdp.lost();
            }
        });
        Ok(cdp)
    }

    fn heard(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            if let Some(tx) = self.pending.lock().remove(&id) {
                let answer = match v.get("error") {
                    Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("the browser refused it").to_owned()),
                    None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = tx.send(answer);
            }
            return;
        }
        let Some(method) = v.get("method").and_then(Value::as_str) else { return };
        let event = Event { session: v.get("sessionId").and_then(Value::as_str).map(str::to_owned), method: method.to_owned(), params: v.get("params").cloned().unwrap_or(Value::Null) };
        // To each listener for its session (or for all): one that's full misses it, one gone goes.
        // The browser's own events are its session `""`'s.
        self.listeners.lock().retain(|(session, tx)| {
            if session.is_none() || session.as_deref() == Some(event.session.as_deref().unwrap_or("")) {
                let _ = tx.try_send(event.clone());
            }
            !tx.is_closed()
        });
    }

    fn lost(&self) {
        self.gone.store(true, Ordering::Relaxed);
        for (_, tx) in self.pending.lock().drain() {
            let _ = tx.send(Err("the browser went away".to_owned()));
        }
        self.listeners.lock().clear();
    }

    /// Whether the browser's socket is still there.
    pub fn alive(&self) -> bool {
        !self.gone.load(Ordering::Relaxed)
    }

    /// A command, to the browser (`session` none) or to a page's session; its result.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> anyhow::Result<Value> {
        anyhow::ensure!(self.alive(), "the browser went away");
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let msg = match session {
            Some(s) => json!({"id": id, "method": method, "params": params, "sessionId": s}),
            None => json!({"id": id, "method": method, "params": params}),
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        self.pending.lock().insert(id, tx);
        if self.out.send(msg.to_string()).await.is_err() {
            self.pending.lock().remove(&id);
            anyhow::bail!("the browser went away");
        }
        match crate::rt::timeout(ANSWER, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => anyhow::bail!("{method}: {e}"),
            Ok(Err(_)) => anyhow::bail!("the browser went away"),
            Err(_) => {
                self.pending.lock().remove(&id);
                anyhow::bail!("{method}: the browser didn't answer in time")
            }
        }
    }

    /// What the browser says from now on, for a page's session (or everything, with none).
    pub fn listen(&self, session: Option<&str>) -> async_channel::Receiver<Event> {
        let (tx, rx) = async_channel::bounded(4096);
        self.listeners.lock().push((session.map(str::to_owned), tx));
        rx
    }

    /// What the browser itself says from now on (a target made, changed or gone), none of its
    /// pages' sessions.
    pub fn listen_browser(&self) -> async_channel::Receiver<Event> {
        self.listen(Some(""))
    }
}
