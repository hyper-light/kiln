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
//! Two processes never run one session: until the server's word comes, the session waits here
//! (its queue holds what is sent meanwhile); without it, it goes on here — and should the newer
//! process have taken it over after all, the server's word reaches this one later and it lets go
//! then (`super::host::Host::handed_over`).

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use futures::FutureExt as _;
use futures::channel::oneshot;
use parking_lot::Mutex;

use crate::exec::Up;
use crate::proto::AgentMsg;

/// The pieces a package goes in, as the server sends packages (`super::registry`).
const PIECE: usize = 3 << 20;

/// How long a hand-over waits for the server's word before the session goes on here.
const ANSWER_WITHIN: Duration = Duration::from_secs(600);

struct Relay {
    up: async_channel::Sender<Up>,
    waiting: Mutex<HashMap<String, oneshot::Sender<Result<(), String>>>>,
}

static RELAY: OnceLock<Relay> = OnceLock::new();

/// This runner's link to the server, set once as it starts.
pub fn install(up: async_channel::Sender<Up>) {
    let _ = RELAY.set(Relay { up, waiting: Mutex::new(HashMap::new()) });
}

/// Send session `id`, packed (`bytes`), to the server for this machine's newer runner process, and
/// wait for the server's word on it: Ok once that process has taken it over.
pub async fn send(id: &str, bytes: Vec<u8>) -> Result<(), String> {
    let relay = RELAY.get().ok_or_else(|| "this runner has no link to the server".to_owned())?;
    let (tx, rx) = oneshot::channel();
    relay.waiting.lock().insert(id.to_owned(), tx);
    let pieces: Vec<&[u8]> = bytes.chunks(PIECE).collect();
    let mut offset = 0u64;
    for (n, piece) in pieces.iter().enumerate() {
        let msg = AgentMsg::HarnessHandOver { session: id.to_owned(), offset, data: piece.to_vec(), done: n + 1 == pieces.len() };
        if relay.up.send(Up::Convo(msg)).await.is_err() {
            relay.waiting.lock().remove(id);
            return Err("this runner's link to the server closed".into());
        }
        offset += piece.len() as u64;
    }
    let word = futures::select! {
        w = rx.fuse() => w.unwrap_or_else(|_| Err("this runner's link to the server dropped".into())),
        () = crate::rt::sleep(ANSWER_WITHIN).fuse() => Err(format!("the server said nothing of it in {} minutes", ANSWER_WITHIN.as_secs() / 60)),
    };
    relay.waiting.lock().remove(id);
    word
}

/// The server's word on session `id`'s hand-over (`Command::HandedOver`): whether a hand-over was
/// waiting for it.
pub fn answered(id: &str, error: Option<String>) -> bool {
    let Some(relay) = RELAY.get() else { return false };
    let Some(tx) = relay.waiting.lock().remove(id) else { return false };
    tx.send(error.map_or(Ok(()), Err)).is_ok()
}

/// The link to the server dropped: no word comes for what was on its way, and those sessions go
/// on here. Taken over meanwhile, one is let go of when the server's word comes after all.
pub fn link_lost() {
    let Some(relay) = RELAY.get() else { return };
    let waiting: Vec<_> = relay.waiting.lock().drain().collect();
    for (_, tx) in waiting {
        let _ = tx.send(Err("this runner's link to the server dropped".into()));
    }
}

/// This process took over session `id` from an older one of its machine, or couldn't (`error`):
/// said to the server, which tells that process.
pub async fn adopted(id: &str, error: Option<String>) {
    let Some(relay) = RELAY.get() else { return };
    let _ = relay.up.send(Up::Msg(AgentMsg::HarnessAdopted { session: id.to_owned(), error })).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_over_goes_in_pieces_and_waits_for_the_servers_word() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (tx, rx) = async_channel::unbounded();
            install(tx);
            let bytes = vec![7u8; PIECE + 10];
            let sending = crate::rt::spawn(async move { send("s1", bytes).await });
            let mut got = Vec::new();
            while got.len() < 2 {
                match rx.recv().await? {
                    Up::Convo(AgentMsg::HarnessHandOver { session, offset, data, done }) => got.push((session, offset, data.len(), done)),
                    other => anyhow::bail!("unexpected {other:?}"),
                }
            }
            assert_eq!(got, vec![("s1".to_owned(), 0, PIECE, false), ("s1".to_owned(), PIECE as u64, 10, true)]);
            // Nothing for a session no hand-over waits on.
            assert!(!answered("s2", None));
            while !answered("s1", None) {
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(sending.await?, Ok(()));
            // Its word, an error: the session goes on there, saying why.
            let failing = crate::rt::spawn(async move { send("s3", vec![1]).await });
            let _ = rx.recv().await?;
            while !answered("s3", Some("no room".into())) {
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(failing.await?, Err("no room".to_owned()));
            // The link drops: what waits goes on here.
            let dropped = crate::rt::spawn(async move { send("s4", vec![1]).await });
            let _ = rx.recv().await?;
            while RELAY.get().is_some_and(|r| !r.waiting.lock().contains_key("s4")) {
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            link_lost();
            assert!(dropped.await?.is_err());
            Ok(())
        })
    }
}
