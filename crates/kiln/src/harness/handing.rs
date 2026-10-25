//! Handing an agent session to this machine's newer runner process, which takes this one's place
//! (a take-over: `crate::upgrade`, docs/releases.md "One runner a machine").
//!
//! The server tells the older process to (a local `Command::MoveTo`). Once the session's turn is
//! over, `super::session` packs it — its timeline, state and queue — and [`send`] sends it to the
//! server in pieces ([`AgentMsg::HarnessHandOver`]), which passes it on to the newer process as a
//! `HarnessPackage`. That process takes it over and says how that went ([`adopted`],
//! [`AgentMsg::HarnessAdopted`]); the server tells this one (`Command::HandedOver`, [`answered`]),
//! and only then does it let go of the session. Both processes share the machine's disk and home,
//! so nothing of the session's folder or its harness's record travels: the newer process resumes
//! the harness's own session from them, as it would after a restart.
//!
//! Two processes never run one session: until it's known how it went, the session waits here (its
//! queue holds what is sent meanwhile). Each try at handing it over is decided once for the machine,
//! by whichever of its processes comes first (`super::host::Decided`, CP-T3.2, the nineteenth
//! adversarial check's), so the server's word, which may be an earlier try's come late, says when to
//! look: refused, it takes the try back unless a newer process took it first, and goes on here.
//! With no word of it (the link dropped, or nothing came, or the newer process went away once its
//! whole package went to it), the newer process may run it now: it stays waiting here, and its
//! package goes again until it's decided (CP-T3.2, the eighteenth adversarial check's). A hand-over's
//! word is waited for across its tries ([`done`] once it's known), so one that comes between two
//! tries isn't lost.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use futures::FutureExt as _;
use parking_lot::Mutex;

use crate::exec::Up;
use crate::proto::AgentMsg;

/// The pieces a package goes in, as the server sends packages (`super::registry`).
const PIECE: usize = 3 << 20;

/// How long a try waits for the server's word before it's taken as not known.
const ANSWER_WITHIN: Duration = Duration::from_secs(600);

/// Why a hand-over didn't go through (CP-T3.2: `super::session` keeps the session's inbox fenced,
/// and the session waiting, while it isn't known).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotHanded {
    /// The newer process said no.
    Refused(String),
    /// This try went to no process: it never all went out, or the server couldn't pass it on. Sure
    /// only when no try before it went without a word.
    NotPassed(String),
    /// Not known: the link dropped, no word came, or the server can't say (the newer process went
    /// away once its whole package went to it). The newer process may have taken it.
    Unknown(String),
}

impl std::fmt::Display for NotHanded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) | Self::NotPassed(why) | Self::Unknown(why) => f.write_str(why),
        }
    }
}

/// The server's word on a hand-over.
type Word = Result<(), NotHanded>;

/// This runner's link to the server, and the hand-overs under way, each waiting for its word.
struct Relay {
    up: Mutex<Option<async_channel::Sender<Up>>>,
    waiting: Mutex<HashMap<String, (async_channel::Sender<Word>, async_channel::Receiver<Word>)>>,
}

static RELAY: LazyLock<Relay> = LazyLock::new(|| Relay { up: Mutex::new(None), waiting: Mutex::new(HashMap::new()) });

/// This runner's link to the server, set as it starts.
pub fn install(up: async_channel::Sender<Up>) {
    // A test holding this process's link keeps it: a runner another test starts here meanwhile (a
    // server's local agent) doesn't take it over.
    #[cfg(test)]
    if HELD.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    *RELAY.up.lock() = Some(up);
}

/// Send session `id`, packed (`bytes`), to the server for this machine's newer runner process, and
/// wait for the server's word on it: Ok once that process has taken it over. Sent again, it waits
/// for the same word: one that came since the last try is its answer at once.
pub async fn send(id: &str, bytes: &[u8]) -> Word {
    let Some(up) = RELAY.up.lock().clone() else { return Err(NotHanded::NotPassed("this runner has no link to the server".to_owned())) };
    let rx = RELAY.waiting.lock().entry(id.to_owned()).or_insert_with(async_channel::unbounded).1.clone();
    if let Ok(word) = rx.try_recv() {
        return word;
    }
    let pieces: Vec<&[u8]> = bytes.chunks(PIECE).collect();
    let mut offset = 0u64;
    for (n, piece) in pieces.iter().enumerate() {
        let msg = AgentMsg::HarnessHandOver { session: id.to_owned(), offset, data: piece.to_vec(), done: n + 1 == pieces.len() };
        if up.send(Up::Convo(msg)).await.is_err() {
            // Its last piece never went: the server can't have passed this try on.
            return Err(NotHanded::NotPassed("this runner's link to the server closed".into()));
        }
        offset += piece.len() as u64;
    }
    futures::select! {
        w = rx.recv().fuse() => w.unwrap_or_else(|_| Err(NotHanded::Unknown("its hand-over was ended here".into()))),
        () = crate::rt::sleep(ANSWER_WITHIN).fuse() => Err(NotHanded::Unknown(format!("the server said nothing of it in {} minutes", ANSWER_WITHIN.as_secs() / 60))),
    }
}

/// Session `id`'s hand-over is over (its word known, or the session gone): nothing waits for its
/// word any more.
pub fn done(id: &str) {
    RELAY.waiting.lock().remove(id);
}

/// The server's word on session `id`'s hand-over (`Command::HandedOver`: `error`, and whether the
/// newer process said it, `by_newest`, or the server can't say, `unsure`): whether a hand-over was
/// waiting for it.
pub fn answered(id: &str, error: Option<String>, unsure: bool, by_newest: bool) -> bool {
    let Some(tx) = RELAY.waiting.lock().get(id).map(|(tx, _)| tx.clone()) else { return false };
    let word = match error {
        None => Ok(()),
        Some(why) if by_newest => Err(NotHanded::Refused(why)),
        Some(why) if unsure => Err(NotHanded::Unknown(why)),
        Some(why) => Err(NotHanded::NotPassed(why)),
    };
    tx.try_send(word).is_ok()
}

/// The link to the server dropped: no word comes now for what was on its way. Each waits on (the
/// server's word, parked, comes once the link is back) and is asked again.
pub fn link_lost() {
    let waiting: Vec<_> = RELAY.waiting.lock().values().map(|(tx, _)| tx.clone()).collect();
    for tx in waiting {
        let _ = tx.try_send(Err(NotHanded::Unknown("this runner's link to the server dropped".into())));
    }
}

/// This runner's link to the server dropped (`crate::agent::run`): [`link_lost`], but for a runner a
/// test starts in this process while another test holds the link, which is that test's alone.
pub fn runner_link_lost() {
    #[cfg(test)]
    if HELD.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    link_lost();
}

/// This process took over session `id` from an older one of its machine, or couldn't (`error`):
/// said to the server, which tells that process.
pub async fn adopted(id: &str, error: Option<String>) {
    let Some(up) = RELAY.up.lock().clone() else { return };
    let _ = up.send(Up::Msg(AgentMsg::HarnessAdopted { session: id.to_owned(), error })).await;
}

/// Tests that use this process's link to the server (it's one a process) hold it ([`hold_link`]).
#[cfg(test)]
static TEST_LINK: Mutex<()> = Mutex::new(());

/// A test holds this process's link to the server.
#[cfg(test)]
static HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This process's link to the server is the calling test's until what's returned goes, one test at a
/// time ([`set_held_link`] sets it): a runner another test starts in this process meanwhile (a
/// server's local agent, `crate::agent::run`) neither takes it over nor says it dropped
/// ([`runner_link_lost`]).
#[cfg(test)]
pub fn hold_link() -> HeldLink {
    let guard = TEST_LINK.lock();
    HELD.store(true, std::sync::atomic::Ordering::Release);
    HeldLink { _guard: guard }
}

/// The link of the test holding it ([`hold_link`]).
#[cfg(test)]
pub fn set_held_link(up: async_channel::Sender<Up>) {
    *RELAY.up.lock() = Some(up);
}

#[cfg(test)]
pub struct HeldLink {
    _guard: parking_lot::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for HeldLink {
    fn drop(&mut self) {
        HELD.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_over_goes_in_pieces_and_waits_for_the_servers_word() -> anyhow::Result<()> {
        let _link = hold_link();
        crate::rt::block_on(async {
            let (tx, rx) = async_channel::unbounded();
            set_held_link(tx);
            let bytes = vec![7u8; PIECE + 10];
            let sending = crate::rt::spawn(async move { send("s1", &bytes).await });
            let mut got = Vec::new();
            while got.len() < 2 {
                match rx.recv().await? {
                    Up::Convo(AgentMsg::HarnessHandOver { session, offset, data, done }) => got.push((session, offset, data.len(), done)),
                    other => anyhow::bail!("unexpected {other:?}"),
                }
            }
            assert_eq!(got, vec![("s1".to_owned(), 0, PIECE, false), ("s1".to_owned(), PIECE as u64, 10, true)]);
            // Nothing for a session no hand-over waits on.
            assert!(!answered("s2", None, false, false));
            assert!(answered("s1", None, false, false));
            assert_eq!(sending.await?, Ok(()));
            done("s1");
            // The newer process's no, the server's own, and the server unsure.
            for (unsure, by_newest, word) in [
                (false, true, Err(NotHanded::Refused("no room".into()))),
                (false, false, Err(NotHanded::NotPassed("no room".into()))),
                (true, false, Err(NotHanded::Unknown("no room".into()))),
            ] {
                let failing = crate::rt::spawn(async move { send("s3", &[1]).await });
                let _ = rx.recv().await?;
                while !answered("s3", Some("no room".into()), unsure, by_newest) {
                    crate::rt::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(failing.await?, word);
            }
            done("s3");
            // The link drops: the word isn't known. The hand-over waits on: the word that comes
            // once the link is back (between two tries, here) answers its next try at once.
            let dropped = crate::rt::spawn(async move { send("s4", &[1]).await });
            let _ = rx.recv().await?;
            while !RELAY.waiting.lock().contains_key("s4") {
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            link_lost();
            assert!(matches!(dropped.await?, Err(NotHanded::Unknown(_))), "the link dropped: not known whether it was taken");
            assert!(answered("s4", None, false, false), "still waited for");
            assert_eq!(send("s4", &[1]).await, Ok(()), "the word that came between the tries");
            assert!(rx.try_recv().is_err(), "nothing sent again once the word was in");
            done("s4");
            assert!(!answered("s4", None, false, false), "done: nothing waits for it");
            Ok(())
        })
    }
}
