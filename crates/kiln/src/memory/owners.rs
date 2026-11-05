//! The server's queues and caches bounded in bytes (goal 11, plan 01a1a87b2e56 step 3), each an [`Owner`]
//! on `/metrics`: what it holds now (`kiln_memory_bytes{owner}`) and the most it may
//! (`kiln_memory_bound_bytes{owner}`). Each says what it holds, its bound and the bound's basis, and what
//! happens at the bound: a sender waits (or is told it's full where it has something better to do), or
//! what's kept is let go of where it can be had again. Nothing a person sent is lost at a bound.

use super::Owner;

/// Commands waiting for the net thread, frames to runners mostly (`crate::net::NetHandle`): at most
/// [`crate::net::OUT_BYTES`], one connection's receive window, the most a peer takes unread. At the bound
/// whoever sends waits, or (`try_send`) is told it's full.
pub static NET_OUT: Owner = Owner::new("net_out");

/// What the net thread has taken off runners' links and not yet handed to the hub's loop: at most
/// [`crate::net::EVENTS_BYTES`], one connection's receive window. At the bound the net thread holds what's
/// past it itself, then (another window on) its peers' lanes: QUIC's flow control holds the runners.
pub static NET_EVENTS: Owner = Owner::new("net_events");

/// Runners' and people's messages waiting for the shard that owns a session's copy
/// (`crate::harness::copies`), every shard's inbox together: each at most [`crate::net::EVENTS_BYTES`], as
/// much as the hub's queue of runner events it's handed on from. At the bound the hub's loop waits, and
/// behind it the net thread and QUIC's flow control.
pub static SESSION_INBOXES: Owner = Owner::new("session_inboxes");

/// The server's own sessions' updates waiting for the loop that hands them to their owners
/// (`crate::harness::registry`'s `local_rx`): at most [`crate::net::EVENTS_BYTES`], as much as the owner's
/// inbox they go to next. At the bound a session waits to send.
pub static LOCAL_UPDATES: Owner = Owner::new("local_updates");

/// Requests waiting for the records store's thread (`crate::records::turso`): at most
/// [`crate::records::turso::QUEUE_BYTES`], what the thread writes together in one commit. At the bound a
/// caller waits its turn.
pub static RECORDS_QUEUE: Owner = Owner::new("records_queue");

/// Runners' conversation-feed messages (batches' parts, mostly) waiting for the intake's thread
/// (`crate::convo::feed::Intake`): at most [`crate::convo::feed::HEARD_BYTES`], one connection's receive
/// window. At the bound a message is dropped, as it always was past the queue's end: its runner holds the
/// batch until the server has it and sends it again.
pub static FEED_INTAKE: Owner = Owner::new("feed_intake");

/// What the server sends its conversation store's mirrors, waiting for the loop that hands it to the net
/// thread (`crate::convo::mirror`): at most [`crate::convo::mirror::OUT_BYTES`], as much as the net
/// thread's queue it goes into. At the bound a mirror's feed waits.
pub static MIRRORS_OUT: Owner = Owner::new("mirrors_out");

/// SQLite's page caches of kiln.db (`crate::store`'s connections: the writer, and the read connections the
/// pool keeps), each counted after each use (`SQLITE_DBSTATUS_CACHE_USED`). SQLite keeps every page cache
/// of the process in one group (pcache1's unified mode, as libsqlite3-sys builds it, with
/// `SQLITE_ENABLE_MEMORY_MANAGEMENT`): their pages together are at most the sum of their `cache_size`s, a
/// connection holding more than its own while others hold less. So the bound is that sum over kiln.db's
/// open connections (`crate::store::cache_bound` each: SQLite's default `cache_size`, -2000 KiB, about 2 MB,
/// kiln sets none, with each page's header, for its main database and as much again for a temp one a
/// statement may make: about 4.3 MB); 25 connections on the server's profile (1 writer, 24 readers: the
/// blocking pool's `store` share), about 108 MB, half of it reached only by statements that make temp
/// databases. At the bound SQLite lets the group's least recently used page go: it's read again from the
/// file (or the OS's cache of it) when asked.
pub static SQLITE: Owner = Owner::new("sqlite");

/// Runners' payloads read on a shard, waiting for the thread that looks for fields this build doesn't know
/// in them (`crate::wire::payload`'s checker; goal 9, plan 01a1a87b2cec step 5): at most
/// [`crate::net::EVENTS_BYTES`], as much as the hub's loop keeps waiting from runners. At the bound a
/// payload is let go unlooked at, counted and said: the message itself was taken whole already.
pub static WIRE_CHECKS: Owner = Owner::new("wire_checks");

/// The owners counted here, in the order the exposition says them.
pub(super) const ALL: [&Owner; 9] = [&NET_OUT, &NET_EVENTS, &SESSION_INBOXES, &LOCAL_UPDATES, &RECORDS_QUEUE, &FEED_INTAKE, &MIRRORS_OUT, &WIRE_CHECKS, &SQLITE];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::queue::{PLACE, Weigh};

    /// Goal 11, plan 01a1a87b2e56 step 3: the net thread's queues count a frame by its bytes, so a
    /// megabyte frame takes a megabyte of their bound and an empty one its place alone; their bounds are
    /// one receive window each.
    #[test]
    fn g11_s3_the_net_queues_count_frames_by_their_bytes() {
        let frame = |n: usize| crate::net::NetEvent::Frame { peer: crate::net::SERVER_PEER, lane: 0, kind: crate::net::Kind::Control, data: vec![0; n], at: std::time::Instant::now() };
        assert_eq!(frame(1 << 20).weight() - frame(0).weight(), 1 << 20);
        let out = |n: usize| crate::net::Out::Frame { peer: crate::net::SERVER_PEER, lane: 0, kind: crate::net::Kind::Control, data: vec![0; n] };
        assert_eq!(out(4096).weight() - out(0).weight(), 4096);
        assert_eq!(crate::net::EVENTS_BYTES, usize::try_from(crate::net::limits(true).window_ceiling).unwrap_or(0), "the events' bound is the window the net thread holds lanes at");
        assert_eq!(crate::net::OUT_BYTES, usize::try_from(crate::net::WINDOW).unwrap_or(0));
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a message to a session's owner weighs the texts its update
    /// carries, so a snapshot's events count for what they say, not as one message among 16,384.
    #[test]
    fn g11_s3_a_session_inbox_message_weighs_its_update() -> anyhow::Result<()> {
        use crate::harness::copies::{Ingest, Msg};
        use crate::harness::core::Update;
        let said: crate::harness::model::Event = serde_json::from_value(serde_json::json!({"id": "e1", "seq": 1, "at": 0, "kind": "assistant", "text": "x".repeat(100_000)}))?;
        let msg = |u: Update| Msg::Update(Box::new(Ingest { from: crate::harness::registry::Location::Local, id: "s1".into(), u, shipped: false, reported: String::new() }));
        let (big, small) = (msg(Update::Events { events: vec![said] }).weight(), msg(Update::Closed).weight());
        anyhow::ensure!(big >= small + 100_000, "{big} against {small}");
        // The server's own sessions' updates go into a queue counted the same way, ahead of the inbox.
        anyhow::ensure!(("s1".to_owned(), Update::Closed).weight() >= size_of::<Update>());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a feed's message waiting for the intake weighs the batch's part
    /// it carries.
    #[test]
    fn g11_s3_a_feed_message_weighs_its_part() {
        let heard = |n: usize| (crate::net::SERVER_PEER, "m1".to_owned(), crate::convo::feed::Msg::Hello { store: "s".into(), restore: false }, vec![0u8; n]).weight();
        assert_eq!(heard(3 << 20) - heard(0), 3 << 20);
        assert_eq!(crate::convo::feed::HEARD_BYTES, usize::try_from(crate::net::WINDOW).unwrap_or(0));
        // What goes to a mirror weighs the same way, and is bounded as the net thread's queue it goes into.
        let to_mirror = |n: usize| -> crate::convo::mirror::Out { (crate::net::SERVER_PEER, "{}".to_owned(), vec![0u8; n]) };
        assert_eq!(to_mirror(3 << 20).weight() - to_mirror(0).weight(), 3 << 20);
        assert_eq!(crate::convo::mirror::OUT_BYTES, crate::net::OUT_BYTES);
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: the queues made with a registry are bounded in bytes on
    /// `/metrics`: its local updates at an inbox's bound, and its sessions' inboxes at one each.
    #[test]
    fn g11_s3_a_registrys_queues_show_their_bounds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let _registry = crate::harness::registry::Registry::new(dir.path().to_path_buf());
        anyhow::ensure!(LOCAL_UPDATES.bound().is_some_and(|b| b >= crate::net::EVENTS_BYTES), "local updates' bound: {:?}", LOCAL_UPDATES.bound());
        anyhow::ensure!(SESSION_INBOXES.bound().is_some_and(|b| b >= crate::net::EVENTS_BYTES), "inboxes' bound: {:?}", SESSION_INBOXES.bound());
        let mut o = String::new();
        crate::memory::render(&mut o, &[]);
        for name in ["net_out", "net_events", "session_inboxes", "local_updates", "records_queue", "feed_intake", "mirrors_out", "sqlite"] {
            anyhow::ensure!(o.contains(&format!("kiln_memory_bytes{{owner=\"{name}\"}} ")), "{name} isn't on /metrics:\n{o}");
        }
        anyhow::ensure!(o.contains("kiln_memory_bound_bytes{owner=\"local_updates\"} "), "{o}");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: kiln.db's page caches are counted on `/metrics` as its
    /// connections use them, and bounded for each connection open.
    #[test]
    fn g11_s3_sqlites_page_caches_are_counted_and_bounded() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
            let rows: i64 = store.read(|c| Ok(c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(rows > 0, "kiln.db has its schema");
            anyhow::ensure!(SQLITE.bytes() > 0, "nothing counted of the caches a read and the writer used");
            let each = crate::store::cache_bound(&rusqlite::Connection::open_in_memory()?);
            anyhow::ensure!(each >= 2 * 2000 * 1024, "SQLite's default cache_size, -2000 KiB, for the main and a temp database: {each}");
            anyhow::ensure!(SQLITE.bound().is_some_and(|b| b >= 2 * each), "the writer's and a reader's bounds: {:?}", SQLITE.bound());
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: what SQLite counts of each page a cache holds is the page and a
    /// header within `crate::store::PAGE_HEADER` (its basis): a connection reading a hundred pages more of
    /// a table holds a hundred pages and their headers more.
    #[test]
    fn g11_s3_a_cached_pages_header_is_within_the_bound() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let c = rusqlite::Connection::open(dir.path().join("pages.db"))?;
        let page: i64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        let page = usize::try_from(page)?;
        // A row a page: its blob fills a page but for the cell's and page's own few bytes.
        let blob = page - 100;
        c.execute_batch(&format!("CREATE TABLE t (b BLOB); WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200) INSERT INTO t SELECT zeroblob({blob}) FROM n"))?;
        drop(c);
        let c = rusqlite::Connection::open(dir.path().join("pages.db"))?;
        let read = |upto: i64| -> anyhow::Result<usize> {
            let n: i64 = c.query_row("SELECT count(length(b)) FROM t WHERE rowid <= ?1", [upto], |r| r.get(0))?;
            anyhow::ensure!(n == upto);
            Ok(crate::store::capture::cache_used(&c))
        };
        let (fifty, one_fifty) = (read(50)?, read(150)?);
        let each = one_fifty.saturating_sub(fifty) / 100;
        eprintln!("a cached page: {each} bytes counted for a page of {page}");
        anyhow::ensure!(each >= page, "a page counted at {each}, less than its {page} bytes");
        anyhow::ensure!(each - page <= 512, "a page's header counted at {} bytes, past crate::store::PAGE_HEADER", each - page);
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: an item's count includes its place in the queue.
    #[test]
    fn g11_s3_a_place_is_counted_with_its_item() {
        assert_eq!(PLACE, 2 * size_of::<usize>());
    }
}
