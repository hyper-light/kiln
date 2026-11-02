//! A runner's agent-session updates and terminal output over the channel (docs/wire.md, plan
//! step 4).
//!
//! Each session's updates are the items of a stream of its own, kept until the server has them;
//! each terminal's output is too (its replay, the picture of it now, starts a stream that starts
//! over).
//! After a reconnect they're sent again from where the server says it has them; the whole session
//! is never sent again. A stream starts with the session's snapshot, so a server that has none of
//! it gets the session whole, once (it restarted, or never knew it). A server that has some gets
//! only what it lacks.
//!
//! A stream's name is its key (`s:<session>`, `t:<terminal>`), this runner process's incarnation
//! and the stream's generation: `s:<session>#<boot>.<gen>`. A new process's streams are new, and
//! a stream that starts over (its backlog grew past [`ROOM`], or the server lost what it had) is
//! new too. The server lets go of the one it replaces.
//!
//! A stream's picture goes whole, however large ([`Updates::picture`]): [`ROOM`] and
//! [`TOTAL_ROOM`] bound only the backlog after it. A long session's snapshot is larger than
//! [`ROOM`]; counted against it, its stream went stale while its picture was pushed, started over
//! at the next frame from the server with the same picture, and so on without end: the server
//! never heard from the session again (a stop, a force stop, its inbox's answers), and the runner
//! built the snapshot again at every frame.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::channel::{Event, Frame, Sender};

/// What one stream keeps unanswered past its picture, at the most. Past it the stream starts over
/// with its picture once the server is back, rather than keep a backlog without end while the
/// server is away.
pub const ROOM: usize = 8 << 20;
/// What all of a runner's streams keep unanswered past their pictures together, at the most: past
/// it, the stream an item is pushed to starts over as one past its own room does. Many busy
/// sessions and a long outage never hold more than this (a stream starts over only while the
/// server is there, so an outage holds no new pictures).
pub const TOTAL_ROOM: usize = 64 << 20;
/// How long an update sent on a live link waits for its answer before it goes again.
pub const PATIENCE: Duration = Duration::from_secs(15);

/// A session's key: its updates' stream is named for it.
pub fn session_key(id: &str) -> String {
    format!("s:{id}")
}

/// A terminal's key: its output's stream is named for it.
pub fn terminal_key(id: &str) -> String {
    format!("t:{id}")
}

/// The stream a key's items go on: `<key>#<boot>.<gen>`.
pub fn stream_name(key: &str, boot: &str, generation: u32) -> String {
    format!("{key}#{boot}.{generation}")
}

/// The key a stream's name is of (`s:<session>`, `t:<terminal>`).
pub fn key_of(stream: &str) -> Option<&str> {
    stream.rsplit_once('#').map(|(key, _)| key).filter(|k| k.starts_with("s:") || k.starts_with("t:"))
}

/// The session a stream's name is of (`None`: not a session's).
pub fn session_of(stream: &str) -> Option<&str> {
    key_of(stream)?.strip_prefix("s:")
}

/// A runner's session and terminal streams, by key.
pub struct Updates {
    tx: Sender,
    /// What all its streams keep unanswered together, at the most.
    total: usize,
    /// This runner process's incarnation.
    boot: String,
    /// Each key's stream generation, once its stream has started.
    generation: BTreeMap<String, u32>,
    /// Keys whose stream starts over at the next chance (a new generation, its picture first).
    stale: BTreeSet<String>,
    /// Keys whose stream ends once its last items (a `Closed`, a `TermExit`) are answered.
    closing: BTreeSet<String>,
}

impl Updates {
    pub fn new(boot: impl Into<String>) -> Self {
        Self::with(boot, ROOM, PATIENCE)
    }

    pub fn with(boot: impl Into<String>, room: usize, patience: Duration) -> Self {
        Self::bounded(boot, room, TOTAL_ROOM, patience)
    }

    /// Streams of `room` bytes each, `total` together.
    pub fn bounded(boot: impl Into<String>, room: usize, total: usize, patience: Duration) -> Self {
        Self { tx: Sender::new(room, patience), total, boot: boot.into(), generation: BTreeMap::new(), stale: BTreeSet::new(), closing: BTreeSet::new() }
    }

    /// The stream `key`'s items go on now.
    pub fn stream(&self, key: &str) -> String {
        stream_name(key, &self.boot, self.generation.get(key).copied().unwrap_or(0))
    }

    /// Whether `key`'s stream has started (its picture went first).
    pub fn started(&self, key: &str) -> bool {
        self.generation.contains_key(key)
    }

    /// Starts `key`'s stream, or starts it over under its next generation: the caller pushes its
    /// picture next (a session's snapshot, a terminal's replay: [`Updates::picture`]), then its
    /// items as they come ([`Updates::push`]).
    pub fn start(&mut self, key: &str) {
        let next = match self.generation.get(key) {
            Some(g) => {
                self.tx.drop_stream(&stream_name(key, &self.boot, *g));
                g.saturating_add(1)
            }
            None => 0,
        };
        self.generation.insert(key.to_owned(), next);
        self.stale.remove(key);
        self.closing.remove(key);
    }

    /// One item of `key` (its bytes: an encoded `AgentMsg`). False when its stream isn't taking
    /// items: not started, starting over, or full (then it starts over later, its picture first,
    /// and this item is in that picture).
    pub fn push(&mut self, key: &str, body: Vec<u8>) -> bool {
        if !self.started(key) || self.stale.contains(key) {
            return false;
        }
        let stream = self.stream(key);
        let over = self.tx.backlog().saturating_add(body.len()) > self.total;
        if over || self.tx.push(&stream, body).is_err() {
            self.stale.insert(key.to_owned());
            self.tx.drop_stream(&stream);
            return false;
        }
        true
    }

    /// One item of `key`'s picture, pushed after [`Updates::start`] and before its other items: it
    /// goes whatever the stream's room and the total, which bound what comes after the picture (a
    /// picture larger than the room went stale as it was pushed, every time). False when its stream
    /// isn't taking items: not started, or starting over.
    pub fn picture(&mut self, key: &str, body: Vec<u8>) -> bool {
        if !self.started(key) || self.stale.contains(key) {
            return false;
        }
        let stream = self.stream(key);
        self.tx.push_picture(&stream, body);
        true
    }

    /// The session closed, the terminal exited: its stream ends once what it has left is answered.
    pub fn closing(&mut self, key: &str) {
        self.closing.insert(key.to_owned());
    }

    /// Keys whose stream has to start over now, each to be started (and its picture pushed) by
    /// the caller.
    pub fn take_stale(&mut self) -> Vec<String> {
        std::mem::take(&mut self.stale).into_iter().collect()
    }

    /// A frame from the server for this side's streams: its answers, its wants, what it has.
    pub fn on_frame(&mut self, frame: Frame) -> Vec<Event> {
        let events = self.tx.on_frame(frame);
        for e in &events {
            // The server has less of a stream than it had answered: it lost it (a restart). The
            // session starts over, whole, once.
            if let Event::Regressed { stream, .. } = e
                && let Some(key) = key_of(stream)
                && self.stream(key) == *stream
            {
                self.stale.insert(key.to_owned());
            }
        }
        // A closed session's stream, an exited terminal's, all answered, is done with.
        let done: Vec<String> = self.closing.iter().filter(|k| self.tx.pending(&self.stream(k)).is_empty()).cloned().collect();
        for key in done {
            self.tx.drop_stream(&self.stream(&key));
            self.closing.remove(&key);
            self.generation.remove(&key);
        }
        events
    }

    /// The frames to send now.
    pub fn poll(&mut self, now: Instant) -> Vec<Frame> {
        self.tx.poll(now)
    }

    /// The link went down: what was on it goes again once the server says what it has.
    pub fn down(&mut self) {
        self.tx.down();
    }

    /// Updates sent and not yet answered, over every stream.
    pub fn unanswered(&self) -> usize {
        self.tx.unanswered()
    }
}

#[cfg(test)]
mod tests {
    use super::super::channel::{Arrival, Receiver};
    use super::*;

    /// The server's side, as `crate::wire::link` keeps it: what it has of each stream, and the
    /// mirror the items build (the bodies in the order they were taken).
    struct Server {
        rx: Receiver,
        mirror: BTreeMap<String, Vec<Vec<u8>>>,
        taken_bytes: usize,
    }

    impl Server {
        fn new() -> Self {
            Self { rx: Receiver::new(), mirror: BTreeMap::new(), taken_bytes: 0 }
        }

        fn take(&mut self, frames: Vec<Frame>) -> Vec<Frame> {
            let mut back = Vec::new();
            for f in frames {
                let Frame::Item { stream, seq, low, body } = f else { continue };
                match self.rx.on_item(&stream, seq, low, body) {
                    Arrival::Take(body) => {
                        self.taken_bytes += body.len();
                        let session = session_of(&stream).unwrap_or_default().to_owned();
                        // A stream's first item is its snapshot's: the mirror starts over.
                        if seq == 1 {
                            self.mirror.insert(session.clone(), Vec::new());
                        }
                        self.mirror.entry(session).or_default().push(body);
                        back.push(self.rx.took(&stream, seq));
                    }
                    Arrival::Answered(Some(a)) | Arrival::Gap(a) => back.push(a),
                    Arrival::Answered(None) => {}
                }
            }
            back
        }
    }

    fn update(n: usize) -> Vec<u8> {
        format!("{{\"type\":\"events\",\"n\":{n}}}").into_bytes()
    }

    /// The runner: a session streaming `count` updates, its snapshot (`snapshot` updates) first.
    fn start(u: &mut Updates, key: &str, snapshot: usize) {
        u.start(key);
        for n in 0..snapshot {
            assert!(u.picture(key, format!("snap {n}").into_bytes()));
        }
    }

    #[test]
    fn a_cut_link_sends_only_the_gap_after_it() {
        let base = Instant::now();
        let mut u = Updates::with("boot1", ROOM, PATIENCE);
        let mut s = Server::new();
        start(&mut u, "s:x", 50);
        for a in s.take(Vec::new()) {
            u.on_frame(a);
        }
        u.on_frame(s.rx.have());
        let mut session: Vec<Vec<u8>> = (0..50).map(|n| format!("snap {n}").into_bytes()).collect();
        // Streaming, the link up.
        for n in 0..100 {
            assert!(u.push("s:x", update(n)));
            session.push(update(n));
        }
        for a in s.take(u.poll(base)) {
            u.on_frame(a);
        }
        assert_eq!(s.mirror.get("x"), Some(&session));
        // The link drops for thirty seconds while it goes on streaming; what was in flight is lost.
        for n in 100..130 {
            u.push("s:x", update(n));
            session.push(update(n));
        }
        let _lost = u.poll(base + Duration::from_secs(1));
        u.down();
        for n in 130..400 {
            u.push("s:x", update(n));
            session.push(update(n));
        }
        let before = s.taken_bytes;
        // Back: the server says what it has; only what it lacks comes, once.
        u.on_frame(s.rx.have());
        let sent = u.poll(base + Duration::from_secs(31));
        let sent_bytes: usize = sent.iter().map(|f| if let Frame::Item { body, .. } = f { body.len() } else { 0 }).sum();
        for a in s.take(sent) {
            u.on_frame(a);
        }
        assert_eq!(s.mirror.get("x"), Some(&session), "the mirror equals the session");
        let gap: usize = (100..400).map(|n| update(n).len()).sum();
        assert_eq!(s.taken_bytes - before, gap, "the server took only the gap");
        assert!(sent_bytes <= gap + 1024, "sent {sent_bytes} bytes after the reconnect for a gap of {gap}");
        assert_eq!(u.unanswered(), 0);
    }

    #[test]
    fn a_server_that_restarted_gets_the_session_whole_once() {
        let base = Instant::now();
        let mut u = Updates::with("boot1", ROOM, PATIENCE);
        let mut s = Server::new();
        start(&mut u, "s:x", 3);
        u.on_frame(s.rx.have());
        for n in 0..5 {
            u.push("s:x", update(n));
        }
        for a in s.take(u.poll(base)) {
            u.on_frame(a);
        }
        // The server restarts: it has nothing of the stream, short of what it had answered.
        let mut fresh = Server::new();
        u.down();
        u.on_frame(fresh.rx.have());
        let stale = u.take_stale();
        assert_eq!(stale, vec!["s:x".to_owned()]);
        // Its next generation starts with the snapshot, which holds what the session is now.
        start(&mut u, "s:x", 8);
        assert_eq!(u.stream("s:x"), "s:x#boot1.1");
        for a in fresh.take(u.poll(base)) {
            u.on_frame(a);
        }
        assert_eq!(fresh.mirror.get("x").map(Vec::len), Some(8));
        assert_eq!(u.unanswered(), 0);
    }

    #[test]
    fn a_backlog_past_its_room_starts_over_with_a_snapshot() {
        let mut u = Updates::with("boot1", 64, PATIENCE);
        start(&mut u, "s:x", 1);
        // Away, the backlog grows past the room: the stream gives up, keeping nothing.
        let mut taken = 0;
        while u.push("s:x", vec![0; 16]) {
            taken += 1;
        }
        assert!(taken < 10);
        assert!(!u.push("s:x", vec![0; 16]), "a stale stream takes nothing more");
        assert_eq!(u.take_stale(), vec!["s:x".to_owned()]);
        assert_eq!(u.unanswered(), 0, "what it kept went with it");
        start(&mut u, "s:x", 1);
        assert!(u.push("s:x", vec![0; 16]));
    }

    /// A session whose snapshot is larger than a stream's room (a long Codex session's was 10 to
    /// 16 MB against 8 MiB): its stream starts over with all of it, and the server gets it whole.
    /// Counted against the room, the picture went stale as it was pushed, and the stream started
    /// over at each frame from the server with the same picture, the server never hearing from the
    /// session again.
    #[test]
    fn a_snapshot_larger_than_the_room_starts_its_stream_over_once() {
        let base = Instant::now();
        let mut u = Updates::with("boot1", 64, PATIENCE);
        let mut s = Server::new();
        start(&mut u, "s:x", 1);
        u.on_frame(s.rx.have());
        // Its backlog passes its room while the server doesn't answer: it starts over.
        while u.push("s:x", vec![0; 16]) {}
        assert_eq!(u.take_stale(), vec!["s:x".to_owned()]);
        // Its snapshot now is ten times the room.
        let mut session: Vec<Vec<u8>> = (0..40).map(|n| format!("snapshot part {n:02}").into_bytes()).collect();
        u.start("s:x");
        for part in &session {
            assert!(u.picture("s:x", part.clone()), "a picture always goes");
        }
        // What follows the picture has the room to itself, the picture not answered yet.
        assert!(u.push("s:x", update(0)));
        session.push(update(0));
        for a in s.take(u.poll(base)) {
            u.on_frame(a);
        }
        assert!(u.take_stale().is_empty(), "it doesn't start over again");
        assert_eq!(s.mirror.get("x"), Some(&session), "the server has the session whole");
        assert_eq!(u.unanswered(), 0);
    }

    /// Other streams' pictures don't count against the total: only backlogs do.
    #[test]
    fn the_total_bounds_backlogs_not_pictures() {
        let mut u = Updates::bounded("boot1", 1 << 20, 100, PATIENCE);
        u.start("s:a");
        assert!(u.picture("s:a", vec![0; 500]));
        start(&mut u, "s:b", 0);
        assert!(u.push("s:b", vec![0; 60]), "a's picture leaves b the total");
        assert!(u.push("s:a", vec![0; 30]));
        // Together past the total: the one pushed to starts over.
        assert!(!u.push("s:b", vec![0; 30]));
        assert_eq!(u.take_stale(), vec!["s:b".to_owned()]);
    }

    #[test]
    fn all_streams_together_keep_no_more_than_their_total() {
        let mut u = Updates::bounded("boot1", 1 << 20, 100, PATIENCE);
        start(&mut u, "s:a", 0);
        start(&mut u, "s:b", 0);
        assert!(u.push("s:a", vec![0; 60]));
        // b's own room is ample, but together they'd pass the total: b starts over.
        assert!(!u.push("s:b", vec![0; 60]));
        assert_eq!(u.take_stale(), vec!["s:b".to_owned()]);
        assert!(u.push("s:a", vec![0; 30]), "a goes on within the total");
    }

    #[test]
    fn a_closed_sessions_stream_ends_once_answered() {
        let base = Instant::now();
        let mut u = Updates::with("boot1", ROOM, PATIENCE);
        let mut s = Server::new();
        start(&mut u, "s:x", 1);
        u.on_frame(s.rx.have());
        u.push("s:x", b"closed".to_vec());
        u.closing("s:x");
        for a in s.take(u.poll(base)) {
            u.on_frame(a);
        }
        assert!(!u.started("s:x"), "its stream is done with");
    }

    #[test]
    fn stream_names_say_their_session() {
        assert_eq!(stream_name(&session_key("01ab"), "f00d", 2), "s:01ab#f00d.2");
        assert_eq!(session_of("s:01ab#f00d.2"), Some("01ab"));
        assert_eq!(key_of("t:7#f00d.0"), Some("t:7"));
        assert_eq!(session_of("t:7#f00d.0"), None);
        assert_eq!(key_of("cmd"), None);
    }
}
