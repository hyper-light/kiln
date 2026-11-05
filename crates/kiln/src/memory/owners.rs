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

/// Kiln's receipts of runners' conversation-feed batches waiting for the task that signs and keeps them
/// (`crate::custody::feeds`): at most [`crate::custody::feeds::RECEIPTS_BYTES`], as much as the intake's
/// queue their hand-ons came in. At the bound a receipt is let go, said so, its hand-on unanswered.
pub static CUSTODY_RECEIPTS: Owner = Owner::new("custody_receipts");

/// Finished messages' mentions of files waiting for the task that keeps them (`crate::harness::kept`): at
/// most `QUEUE_BYTES` there, an owner's inbox's bytes ([`crate::net::EVENTS_BYTES`]), where the messages
/// came from. At the bound a message's files aren't kept as it finishes: they're read when someone looks.
pub static KEPT_FILES: Owner = Owner::new("kept_files");

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

/// The server's events as their JSON texts (`crate::hub::Hub`'s `events`: what `GET /api/events` sockets and
/// kiln's own listeners hear, on one lane; and its `harness_events`, agent sessions' own, on each owner shard's
/// lane), each listener's own channels (`crate::memory::lanes`; goal 4, plan 01a1a59c2a9c step 5): their texts,
/// each counted once until its last listener takes it, within [`crate::net::EVENTS_BYTES`] for each of the two
/// (as much as the hub's queue of runners' messages most of them are made from), and each listener's places,
/// made as it subscribes (`crate::hub::EVENTS` in all its lanes, as many as the hub's ring held for its
/// slowest; push's sessions' `EVENTS` a lane). Past either, what comes is let go of for that listener and it's
/// told how many: a page's socket fetches what it shows again (`Resync`), and kiln's own listeners are told how
/// many they missed.
pub static UI_EVENTS: Owner = Owner::new("ui_events");

/// Each live job's log on its way to the job's viewers (`crate::logs`), kept until their slowest takes
/// it (`crate::memory::ring`): each job's at most `FEED` pieces, and `FEED` x [`crate::proto::LOG_CHUNK`]
/// of their bytes, what the job's writer tells its viewers at once (`FEED` pieces a batch) of the largest
/// chunks a runner sends. At the bound the oldest go: a viewer is told how many it skipped, and the job's
/// log file has them all.
pub static LOG_FEEDS: Owner = Owner::new("log_feeds");

/// Each terminal session's output on its way to its attached viewers (`crate::termsess`), kept until
/// their slowest takes it (`crate::memory::ring`): each session's at most `FANOUT` frames, and
/// `BACKLOG_BYTES` of their bytes, the backlog a viewer that falls behind is repainted from (more would
/// take a viewer longer to catch up through than the repaint). At the bound the oldest go: their viewer
/// is repainted from the session's backlog and screen.
pub static TERMINAL_FEEDS: Owner = Owner::new("terminal_feeds");

/// Each viewer's queue of a browser stream's chunks and events (`crate::harness::streams`), every viewer's
/// together: each at most [`crate::harness::streams::VIEWER_BYTES`], the time a viewer may be behind (240
/// pictures at the stream's 60 a second: 4 s) at its encoder's top rate (25 Mb/s): 12.5 MB. At the bound
/// the chunk isn't sent to that viewer, as one past 240 chunks wasn't: its next key frame mends the
/// picture.
pub static STREAM_VIEWERS: Owner = Owner::new("stream_viewers");

/// Outgoing webhooks' deliveries waiting for their subscriber's worker (`crate::ci::webhooks`), every
/// subscriber's together: each at most 1,000 events and [`crate::net::EVENTS_BYTES`] of them, as much as the
/// hub's events ring they're taken from holds. At the bound a delivery is logged as dropped, its body kept
/// in its row, to send again (`POST /api/ci/webhooks/deliveries/:id/redeliver`).
pub static WEBHOOK_QUEUES: Owner = Owner::new("webhook_queues");

/// Syslog records received and not yet offered to the observe node (`crate::observe::collect::syslog`),
/// each source's queue together: each at most [`crate::observe::ingest::WAITING_LOG_BYTES`], what the node
/// takes waiting to be written before it refuses more, where they go next. At the bound a datagram is
/// dropped and counted in the source's state (syslog over UDP has no acknowledgement, as it never had),
/// and a TCP sender waits: its connection isn't read until there's room.
pub static SYSLOG_WAITING: Owner = Owner::new("syslog_waiting");

/// Messages waiting for the CI engine's dispatcher (`crate::ci::dispatch::Handle`): at most
/// [`crate::net::EVENTS_BYTES`], as much as the hub's queue of runners' messages its runners' messages are
/// made from, one a message. At the bound the hub's loop waits to hand it a runner's message, and behind
/// it the net thread and QUIC's flow control hold the runners. A person's request (a run started, a
/// cancel, a rerun) never waits: one message each, counted all the same.
pub static CI_INBOX: Owner = Owner::new("ci_inbox");

/// Small files kept whole (`crate::keep`: a registry's names and epochs, its holders, the terminals'
/// names), waiting for their writer: each file's newest bytes alone, so at most one copy of each kept
/// file waits (and one is written), the bytes before them let go as they come. No bound of its own past
/// that: the files' sizes are the registries'.
pub static KEEP_WAITING: Owner = Owner::new("keep_waiting");

/// Ship's read model (`crate::ship::ShipCache`): each repository's newest commits' rows, their records and
/// their JSON drawn: at most `CAP` (5,000) rows a repository, so its bound is that many as large as its
/// largest row held (each measured as it's drawn). Past the count the oldest go: older pages come from
/// the store, as they did.
pub static SHIP_CACHE: Owner = Owner::new("ship_cache");

/// The package graphs kept for `affected()` (`crate::ci::graphs`): at most 64 graphs and `MAX_BYTES` of
/// them (64 MiB, each counted as its JSON's bytes), as much as one commit's manifests may be read from.
/// Past either the oldest go: read again from the repository's mirror when a run asks.
pub static CI_GRAPHS: Owner = Owner::new("ci_graphs");

/// Hosts' open pull requests kept for the inbox (`crate::githost::pulls`): each repository's list for its
/// minute, and at most 32 MiB of them, one host response's worth as kiln reads them at the most (GitHub's
/// are read up to 32 MiB). Past it the lists read longest ago go: read from the host again when asked.
pub static PULL_LISTS: Owner = Owner::new("pull_lists");

/// observe's grok rules compiled (`crate::observe::pipeline::grok::Lazy`), each counted at what its regex's
/// lazy DFA may keep for one thread searching it (forward and reverse caches at regex's capacity, 2 MiB
/// each; regex reports no figure of what they hold). For each further thread searching a rule at once it
/// may keep that again. Let go with their pipeline.
pub static GROK_DFA: Owner = Owner::new("grok_dfa");

/// What the deployments engine is told and hasn't taken yet (`crate::cd::Cd::input`): its clusters'
/// objects (a kind's list, each change a watch sees, each apply's answer), its renders, and people's and
/// the source poll's word, each counted by what it takes in memory (an object as serde_json keeps it). At
/// most [`crate::cd::INPUTS_BYTES`], as much as kiln reads of one answer of a cluster's API server
/// (`crate::cd::kube::BODY_LIMIT`, 256 MiB). At the bound whoever sends waits for room, as a task: a
/// watch stops reading its stream (the API server holds what follows), a render or an answer waits to be
/// handed in, a person's request waits its turn. None sends past it: the poll's and a rollback's inputs,
/// sent under the gate that keeps them in order, wait for their room before they take it.
pub static CD_INPUTS: Owner = Owner::new("cd_inputs");

/// What the deployments engine asked for and its dispatcher hasn't taken yet (`crate::cd`'s `dispatch`):
/// applies (each its object), deletes, renders, watches, apps' statuses and syncs' records. At most
/// [`crate::cd::ACTIONS_BYTES`], as much as its inputs, which what it asks for is made of (an apply's
/// object is a render's). At the bound the engine's thread waits (its own, not the runtime's), and its
/// inputs behind it.
pub static CD_ACTIONS: Owner = Owner::new("cd_actions");

/// The owners counted here, in the order the exposition says them.
pub(super) const ALL: [&Owner; 25] = [&NET_OUT, &NET_EVENTS, &SESSION_INBOXES, &LOCAL_UPDATES, &RECORDS_QUEUE, &FEED_INTAKE, &MIRRORS_OUT, &WIRE_CHECKS, &CUSTODY_RECEIPTS, &KEPT_FILES, &SQLITE, &UI_EVENTS, &LOG_FEEDS, &TERMINAL_FEEDS, &STREAM_VIEWERS, &WEBHOOK_QUEUES, &SYSLOG_WAITING, &CI_INBOX, &KEEP_WAITING, &SHIP_CACHE, &CI_GRAPHS, &PULL_LISTS, &GROK_DFA, &CD_INPUTS, &CD_ACTIONS];

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
        for name in ["net_out", "net_events", "session_inboxes", "local_updates", "records_queue", "feed_intake", "mirrors_out", "custody_receipts", "kept_files", "sqlite", "ui_events", "log_feeds", "terminal_feeds"] {
            anyhow::ensure!(o.contains(&format!("kiln_memory_bytes{{owner=\"{name}\"}} ")), "{name} isn't on /metrics:\n{o}");
        }
        anyhow::ensure!(o.contains("kiln_memory_bound_bytes{owner=\"local_updates\"} "), "{o}");
        anyhow::ensure!(KEPT_FILES.bound().is_some_and(|b| b >= crate::net::EVENTS_BYTES), "kept files' bound: {:?}", KEPT_FILES.bound());
        let job = |paths: usize| crate::harness::kept::Job { session: "s1".into(), seq: 1, paths: vec!["p".repeat(100); paths], location: crate::harness::registry::Location::Local };
        anyhow::ensure!(job(10).weight() - job(0).weight() == 10 * (size_of::<String>() + 100), "a job weighs its paths");
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
