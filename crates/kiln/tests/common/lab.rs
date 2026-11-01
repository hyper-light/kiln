//! The labs' viewers (`tests/resume_lab.rs`, `tests/wire_lab.rs`): a socket to an agent session that
//! keeps what it's shown as the UI's session store does, connects again after its socket ends, and
//! sends commands under ids, each sent again on its next socket until it's answered.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::agents;

/// What a viewer's socket brought: a text frame, or its end.
pub enum Got {
    Text(Vec<u8>),
    Ended(String),
}

/// A viewer of one session, keeping what it shows as the UI's session store does.
pub struct Viewer {
    pub name: String,
    pub session: String,
    pub rx: Option<mpsc::Receiver<Got>>,
    pub socket: Option<std::net::TcpStream>,
    pub born: u64,
    pub rev: u64,
    pub events: BTreeMap<String, Value>,
    /// After how many frames it stops reading, and for how long: long enough to fall behind the
    /// copy's broadcast, which it's then caught up from in-band (a `resumed` on its socket).
    pub stall: Option<(usize, Duration)>,
    /// Catch-ups it got without reconnecting.
    pub caught_up: usize,
    /// Frames since it last connected.
    pub frames: usize,
    /// Bytes its socket brought since it last connected: all of them, and those besides the
    /// events and the newest state.
    pub bytes: usize,
    pub beyond: usize,
    /// How many times it connected, whether it last got the session whole, why each of its
    /// connections ended, and how its tries to connect again failed (how many, the last's why).
    pub connects: usize,
    pub whole: bool,
    pub ends: Vec<String>,
    pub failed: (usize, String),
    /// The kinds of the last frames it got, for what a connection's end says.
    pub last: std::collections::VecDeque<String>,
    /// The commands it sent under ids (`{"t": "cmd"}`), by id: what it sent, on how many sockets,
    /// and the answers (`ack`, `nack`) it got. One not answered goes again on its next socket, as
    /// the UI's session store sends it.
    pub asked: BTreeMap<String, Asked>,
}

/// A command a viewer sent under an id: what it sent, how many times, the answers it got, whether
/// it's a message (taken only once its inbox says so), and when it last went.
#[derive(Debug, Clone)]
pub struct Asked {
    pub frame: String,
    pub sockets: usize,
    pub answers: Vec<Value>,
    pub message: bool,
    pub sent_at: Instant,
}

/// How long a command answered for now waits before it goes again, a lab's shorter than the UI's
/// `AGAIN_AFTER_MS`.
pub const AGAIN_AFTER: Duration = Duration::from_secs(2);

impl Asked {
    /// The answer that settled it: a final one, not one for now (parked, pending, or a nack to
    /// send it `again`).
    pub fn answer(&self) -> Option<&Value> {
        self.answers.iter().find(|a| !for_now(a))
    }

    /// It was answered for now last: it goes again in a while.
    fn waits(&self) -> bool {
        self.answer().is_none() && self.answers.last().is_some_and(for_now)
    }
}

/// An answer for now: parked, a message its inbox hasn't answered yet (`pending`), or a nack the
/// server says may go again.
fn for_now(a: &Value) -> bool {
    match a.get("t").and_then(Value::as_str) {
        Some("ack") => matches!(a.get("outcome").and_then(Value::as_str), Some("parked" | "pending")),
        Some("nack") => a.get("again").and_then(Value::as_bool) == Some(true),
        _ => false,
    }
}

impl Viewer {
    pub fn new(name: impl Into<String>, session: &str, stall: Option<(usize, Duration)>) -> Self {
        Self { name: name.into(), session: session.to_owned(), rx: None, socket: None, born: 0, rev: 0, events: BTreeMap::new(), stall, caught_up: 0, frames: 0, bytes: 0, beyond: 0, connects: 0, whole: false, ends: Vec::new(), failed: (0, String::new()), last: std::collections::VecDeque::new(), asked: BTreeMap::new() }
    }

    /// Connects, saying what it has (`?after=`) once it has had some of the copy.
    pub fn connect(&mut self, port: u16, admin: &str) -> anyhow::Result<()> {
        let after = if self.born != 0 { format!("?after={}.{}", self.born, self.rev) } else { String::new() };
        let mut s = agents::socket(port, admin, &format!("/api/harness/sessions/{}/ws{after}", self.session))?;
        if self.stall.is_some() {
            // A small window, so falling behind reaches the server soon.
            let _ = nix::sys::socket::setsockopt(&s, nix::sys::socket::sockopt::RcvBuf, &4096);
        }
        self.socket = s.try_clone().ok();
        let (tx, rx) = mpsc::channel();
        // Once, on its first connection only.
        let stall = self.stall.take();
        std::thread::spawn(move || {
            let mut n = 0usize;
            loop {
                match agents::frame(&mut s) {
                    Ok((1, payload)) => {
                        if tx.send(Got::Text(payload)).is_err() {
                            return;
                        }
                        n += 1;
                        if let Some((after, d)) = stall
                            && n == after
                        {
                            std::thread::sleep(d);
                            // Fallen behind: from now on it reads as a viewer does.
                            let _ = nix::sys::socket::setsockopt(&s, nix::sys::socket::sockopt::RcvBuf, &(1 << 20));
                        }
                    }
                    Ok((8, payload)) => {
                        let _ = tx.send(Got::Ended(format!("closed by the server: {}", String::from_utf8_lossy(&payload))));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(Got::Ended(format!("{e:#}")));
                        return;
                    }
                    Ok(_) => {}
                }
            }
        });
        self.rx = Some(rx);
        self.bytes = 0;
        self.beyond = 0;
        self.frames = 0;
        self.whole = false;
        self.connects += 1;
        // What it sent and hasn't heard back about goes again, under its id.
        let again: Vec<String> = self.asked.iter().filter(|(_, a)| a.answer().is_none()).map(|(id, _)| id.clone()).collect();
        for id in again {
            self.resend(&id);
        }
        Ok(())
    }

    /// Sends command `c` under `id`, kept until it's answered (sent again on each next socket).
    pub fn command(&mut self, id: &str, c: &Value) {
        let frame = serde_json::json!({"t": "cmd", "id": id, "c": c}).to_string();
        let message = matches!(c.get("type").and_then(Value::as_str), Some("send" | "skill_run"));
        self.asked.insert(id.to_owned(), Asked { frame, sockets: 0, answers: Vec::new(), message, sent_at: Instant::now() });
        self.resend(id);
    }

    /// Command `id` on this socket, if it has one now (else on its next).
    fn resend(&mut self, id: &str) {
        let (Some(s), Some(a)) = (self.socket.as_mut(), self.asked.get_mut(id)) else { return };
        if agents::send_text(s, &a.frame).is_ok() {
            a.sockets += 1;
            a.sent_at = Instant::now();
        }
    }

    /// The commands it sent that have no answer that settles them yet.
    pub fn unanswered(&self) -> usize {
        self.asked.values().filter(|a| a.answer().is_none()).count()
    }

    /// Its socket ends, as a page reload or a lost Wi-Fi would end it.
    pub fn drop_socket(&mut self) {
        if let Some(s) = self.socket.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Takes in what its socket brought; reconnects once it ended.
    pub fn pump(&mut self, port: u16, admin: &str) -> anyhow::Result<()> {
        let mut ended = false;
        let got: Vec<Got> = self.rx.as_ref().map(|rx| std::iter::from_fn(|| rx.try_recv().ok()).collect()).unwrap_or_default();
        for g in got {
            match g {
                Got::Text(payload) => {
                    self.bytes += payload.len();
                    // Every frame reads as the session socket's down type (typed messages, step 6).
                    if let Err(e) = serde_json::from_slice::<kiln::wire::ui::SessionDown>(&payload) {
                        anyhow::bail!("{} got a frame that isn't a SessionDown ({e}): {}", self.name, String::from_utf8_lossy(&payload).chars().take(300).collect::<String>());
                    }
                    let f: Value = serde_json::from_slice(&payload)?;
                    self.apply(&f, payload.len());
                }
                Got::Ended(why) => {
                    self.ends.push(format!("{why}, after {} frames, the last {:?}", self.frames, self.last));
                    ended = true;
                }
            }
        }
        // A command answered for now a while ago goes again, under its id.
        let due: Vec<String> = self.asked.iter().filter(|(_, a)| a.waits() && a.sent_at.elapsed() >= AGAIN_AFTER).map(|(id, _)| id.clone()).collect();
        for id in due {
            self.resend(&id);
        }
        if ended || self.rx.is_none() {
            self.rx = None;
            // The server may be starting again: a refused connection is tried again next time.
            if let Err(e) = self.connect(port, admin) {
                self.failed = (self.failed.0 + 1, format!("{e:#}"));
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, f: &Value, len: usize) {
        let t = f.get("t").and_then(Value::as_str).unwrap_or_default();
        match t {
            "open" => {
                self.events.clear();
                self.born = f.get("born").and_then(Value::as_u64).unwrap_or(self.born);
                self.whole = true;
            }
            "resumed" => {
                self.whole = false;
                // Mid-socket, not as it connected: it fell behind and was caught up.
                if self.frames > 0 {
                    self.caught_up += 1;
                }
            }
            "reset" => self.events.clear(),
            "ack" | "nack" => {
                if let Some(a) = f.get("id").and_then(Value::as_str).and_then(|id| self.asked.get_mut(id)) {
                    a.answers.push(f.clone());
                }
            }
            "events" => {
                for e in f.get("events").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(id) = e.get("id").and_then(Value::as_str) {
                        self.events.insert(id.to_owned(), e.clone());
                    }
                }
            }
            _ => {}
        }
        if !matches!(t, "events" | "state" | "state_patch" | "meta" | "changes") {
            self.beyond += len;
        }
        self.frames += 1;
        self.last.push_back(t.to_owned());
        while self.last.len() > 4 {
            self.last.pop_front();
        }
        self.rev = self.rev.max(f.get("rev").and_then(Value::as_u64).unwrap_or(0));
    }
}

/// The stand-in's lines in `events`, by the process and turn that said them, in the order the
/// copy holds them (an event's id is `msg_<pid>_<turn>_<line>:0`); and any line said twice.
pub fn turns(events: &[Value]) -> BTreeMap<(u64, u64), Vec<u64>> {
    let mut out: BTreeMap<(u64, u64), Vec<u64>> = BTreeMap::new();
    for id in events.iter().filter_map(|e| e.get("id").and_then(Value::as_str)) {
        let Some(rest) = id.strip_prefix("msg_").and_then(|r| r.strip_suffix(":0")) else { continue };
        let n: Vec<u64> = rest.split('_').filter_map(|p| p.parse().ok()).collect();
        if let [pid, turn, line] = n.as_slice() {
            out.entry((*pid, *turn)).or_default().push(*line);
        }
    }
    out
}

/// A field of a log line (`key=value`), as a number.
pub fn field(line: &str, key: &str) -> Option<usize> {
    line.split_whitespace().find_map(|w| w.strip_prefix(&format!("{key}="))).and_then(|v| v.trim_matches('"').parse().ok())
}

/// The copy's events of session `id`, as a viewer attaching now gets them.
pub fn copy(port: u16, admin: &str, id: &str) -> anyhow::Result<Vec<Value>> {
    let (frames, _) = agents::attach(port, admin, id, "")?;
    Ok(agents::events(&frames))
}

