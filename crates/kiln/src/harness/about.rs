//! A card's helper (`agents.about`, docs/agents.md): the agent that answers what a person tapped
//! in a conversation lives as long as a card reads it. Each read of its answer (`agents.result`)
//! keeps it; once none has come for [`UNREAD`] (the card closed, its page gone, or its answer read
//! to the end) kiln ends it, and anything it started. A helper is no row of Mission Control, and its
//! parent isn't told of it (`super::told`): it's the card's.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How long a helper goes unread before it's ended.
pub const UNREAD: Duration = Duration::from_secs(90);
/// How often a helper's last read is looked at.
const EVERY: Duration = Duration::from_secs(15);

/// Each helper still running, and when a card last read it.
static READ: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// What a helper is marked with (`Spec::helper`, `Info::helper`).
pub const ABOUT: &str = "about";

/// A card read helper `id`'s answer: it's kept a while longer.
pub fn seen(id: &str) {
    if let Some(at) = READ.lock().get_mut(id) {
        *at = Instant::now();
    }
}

/// Its card ended helper `id`: there's nothing left to watch.
pub fn ended(id: &str) {
    READ.lock().remove(id);
}

/// Whether helper `id`, last read at `at`, has gone unread long enough to end, at `now`.
fn stale(at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(at) >= UNREAD
}

/// Helper `id` started for a card, as `actor` asked: it's ended once no card has read it for
/// [`UNREAD`].
pub fn started(hub: Arc<crate::hub::Hub>, id: String, actor: String) {
    READ.lock().insert(id.clone(), Instant::now());
    drop(crate::rt::spawn(async move {
        loop {
            crate::rt::sleep(EVERY).await;
            let Some(at) = READ.lock().get(&id).copied() else { return };
            if !stale(at, Instant::now()) {
                continue;
            }
            READ.lock().remove(&id);
            let mut kids = super::lineage::descendants(&hub.harness.list(), &id);
            kids.reverse();
            for k in kids {
                super::ops::end(&hub, &k.id, &actor).await;
            }
            if !super::ops::end(&hub, &id, &actor).await {
                tracing::info!(agent = %id, "a card's helper no card reads didn't end (it had already?)");
            }
            return;
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_helper_is_kept_while_a_card_reads_it_and_ended_once_none_has() {
        let at = Instant::now();
        assert!(!stale(at, at));
        assert!(!stale(at, at + UNREAD - Duration::from_secs(1)));
        assert!(stale(at, at + UNREAD));
        // A clock that went back keeps it.
        assert!(!stale(at + UNREAD, at));
        READ.lock().insert("h1".into(), at);
        seen("h1");
        assert!(READ.lock().get("h1").is_some_and(|t| *t >= at));
        // Only helpers are kept: a read of any other agent changes nothing.
        seen("not-a-helper");
        assert!(!READ.lock().contains_key("not-a-helper"));
        ended("h1");
        assert!(!READ.lock().contains_key("h1"));
    }
}
