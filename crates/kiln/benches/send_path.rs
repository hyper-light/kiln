//! bench send_path (CP-T3.2, AC-T3.2.3): what a session's inbox adds to a send, on a real disk. It
//! times `Inbox::accept`, a message's one durable commit on kiln's records store (Turso, every commit
//! synced), and beside each take the baseline it's measured against: the same store's plain durable
//! commit of one record, which a send costs anyway once its session's record is journaled before
//! it's acknowledged (CP-T3.1). What the inbox adds is, within each run, the p50, p99 and p99.9 of
//! its takes less those of the plain commits (a difference of percentiles); the criterion reads the
//! p99 of the median run, and every run is shown. The cases, each read by the criterion:
//! - one session at a time: each message's take and its plain commit in turn, the take first on one
//!   and second on the next (so neither has the other's place), and each message's own difference
//!   (take less its plain commit: the paired one) beside it;
//! - a session at its most ids, whose oldest go once a take's answer has gone, as the host tidies
//!   (the next take waits for that, and its time counts it); the same on a busy host, eight
//!   sessions at their most, the others taking messages all along; and on a session with a long
//!   history, 25,600 ids let go before, which its scans read through until a compaction;
//! - 32 sessions at once, in whole phases of takes, and of plain commits alone, their order
//!   alternating, so a take's check never rides in a plain commit's shared write: fresh sessions,
//!   and sessions at their most ids tidying as the host does, each passing its bound once in its
//!   takes, at its own point;
//! - takes across a compaction of the store's history (pushed to its first by plain commits of
//!   their own), its steps run between them;
//! - a first take, its session's head not in memory (as after a start), on an inbox fresh each round.
//!
//! Beside them: a session's opening, its ids counted (once a process, on its lane, which a take
//! waits on only if it comes that moment); a session's arrival with its most ids, taken in (its
//! first message waits for it); a repeat (a message taken before, answered with the first
//! time's answer); and the disk's own floor, a 4 KiB append and `fdatasync`. Not here: a head read again after another take came
//! between (a conflict, rare), and the round trip to the session's runner, which a send over the
//! network adds whatever its inbox does. The master plan's bar for a recording: the release binary,
//! the hardware and the drive, the commit, the date, the load (refused at a load average of 2 or
//! more).
//!
//! ```text
//! cargo bench -p kiln --bench send_path -- --dir <a folder on the disk to measure>
//!     [--runs 5] [--messages 2000] [--sessions 32] [--commit <sha>] [--out <file.json>]
//!     [--wait-quiet <seconds>] [--any-load]
//! ```
//!
//! `--any-load` records on a loaded machine too, marked so: a smoke run, never evidence.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiln::harness::inbox::{Inbox, KEEP, Limits, TRIM, digest};
use kiln::records::turso::{COMPACT_EVERY, KEEP_REVS, Stats, TursoRecords};
use kiln_seams::Records as _;
use serde_json::{Value, json};

#[allow(dead_code)]
mod common;
use common::{QUIET, disk, load, machine, now, summary, summary_ms};

struct Args {
    dir: PathBuf,
    runs: usize,
    messages: usize,
    sessions: usize,
    commit: String,
    out: Option<PathBuf>,
    wait_quiet: u64,
    any_load: bool,
}

fn args() -> anyhow::Result<Args> {
    let mut a = Args { dir: std::env::temp_dir(), runs: 5, messages: 2000, sessions: 32, commit: String::new(), out: None, wait_quiet: 0, any_load: false };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| anyhow::anyhow!("{k} takes a value"));
        match k.as_str() {
            "--dir" => a.dir = PathBuf::from(v()?),
            "--runs" => a.runs = v()?.parse()?,
            "--messages" => a.messages = v()?.parse()?,
            "--sessions" => a.sessions = v()?.parse()?,
            "--commit" => a.commit = v()?,
            "--out" => a.out = Some(PathBuf::from(v()?)),
            "--wait-quiet" => a.wait_quiet = v()?.parse()?,
            "--any-load" => a.any_load = true,
            // What cargo bench passes along.
            "--bench" => {}
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(a.runs > 0 && a.messages > 0 && a.sessions > 0, "--runs, --messages and --sessions are at least 1");
    Ok(a)
}

/// What the store did since `was` (its stats as they were): its durable writes, the commits they
/// held, and the time they took to apply and to sync, each on average per write.
fn store(now: &Stats, was: [u64; 4]) -> (Value, [u64; 4]) {
    let get = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Acquire);
    let cur = [get(&now.writes), get(&now.commits), get(&now.apply_ns), get(&now.sync_ns)];
    let d: Vec<u64> = cur.iter().zip(was).map(|(c, w)| c.saturating_sub(w)).collect();
    let (writes, commits, apply, sync) = (d.first().copied().unwrap_or(0), d.get(1).copied().unwrap_or(0), d.get(2).copied().unwrap_or(0), d.get(3).copied().unwrap_or(0));
    #[allow(clippy::cast_precision_loss)]
    let per = |x: u64| if writes == 0 { None } else { Some(x as f64 / writes as f64) };
    let ms = |x: Option<f64>| x.map(|v| v / 1e6);
    (json!({"writes": writes, "commits": commits, "commits_per_write": per(commits), "apply_ms_per_write": ms(per(apply)), "sync_ms_per_write": ms(per(sync))}), cur)
}

/// The disk's own floor: a 4 KiB append and `fdatasync`, 400 times.
fn floor(dir: &Path) -> anyhow::Result<Value> {
    use std::io::Write as _;
    let mut f = std::fs::File::create(dir.join("floor.bin"))?;
    let page = [b'f'; 4096];
    let mut took = Vec::with_capacity(400);
    for _ in 0..400 {
        f.write_all(&page)?;
        let t = Instant::now();
        f.sync_data()?;
        took.push(t.elapsed());
    }
    Ok(summary(took))
}

/// The other sessions of a busy host, each at its most ids (and its tag for them).
const BUSY: [(&str, &str); 7] = [("busy1", "b1"), ("busy2", "b2"), ("busy3", "b3"), ("busy4", "b4"), ("busy5", "b5"), ("busy6", "b6"), ("busy7", "b7")];

/// The cases, each read by the criterion: what the inbox adds to each.
const CASES: [&str; 8] = ["sequential", "at_its_most", "a_busy_host", "a_long_history", "concurrent", "concurrent_at_their_most", "across_a_compaction", "a_first_take"];

/// The plain commit a take is measured against: one record, synced (what a send writes anyway once
/// its session's record is journaled).
fn plain_commit(id: &str) -> kiln_seams::records::Tx {
    kiln_seams::records::Tx { conds: Vec::new(), writes: vec![kiln_seams::records::Write::Put { family: "journal".into(), key: id.as_bytes().to_vec(), value: vec![b'q'; 120] }] }
}

/// Session `session` opened and given `count` ids of its own (as a session that came with them has
/// them), then tidied once: its ids counted, and past its bound its oldest let go. What a session's
/// arrival does before its first message on this host: how long it took.
async fn fill(inbox: &Inbox, session: &str, tag: &str, count: usize) -> anyhow::Result<Duration> {
    let t = Instant::now();
    inbox.open(session, 0).await?;
    let kept: Vec<String> = (0..count).map(|m| format!("{tag}-{m}")).collect();
    inbox.import(session, &kept, 0).await?;
    let _ = inbox.tidy(session).await;
    Ok(t.elapsed())
}

/// Takes on session `session` (already at its most ids), `count` of them, each with its plain
/// commit; past its bound its oldest go once a take's answer has gone, as the host tidies, and the
/// next take waits for that (as the next message on its lane would): each take's time, its plain
/// commit's, their difference, and each tidying's own time.
async fn at_its_bound(inbox: &Inbox, turso: &TursoRecords, session: &str, prefix: &str, words: &str, count: usize) -> anyhow::Result<(Vec<Duration>, Vec<Duration>, Vec<f64>, Vec<Duration>)> {
    let (mut took, mut base, mut paired, mut tidied) = (Vec::with_capacity(count), Vec::with_capacity(count), Vec::with_capacity(count), Vec::new());
    let mut owed = Duration::ZERO;
    for m in 0..count {
        let (t, b, _) = pair(inbox, turso, session, format!("{prefix}-{m}"), words, m % 2 == 0).await?;
        let t = t.saturating_add(std::mem::take(&mut owed));
        took.push(t);
        base.push(b);
        paired.push((t.as_secs_f64() - b.as_secs_f64()) * 1000.0);
        let started = Instant::now();
        if inbox.tidy(session).await > 0 {
            owed = started.elapsed();
            tidied.push(owed);
        }
    }
    Ok((took, base, paired, tidied))
}

/// One message's take and its plain commit, the take first when `first` (else second): each's
/// time, and the take less the plain commit (the paired difference, in milliseconds).
async fn pair(inbox: &Inbox, turso: &TursoRecords, session: &str, id: String, words: &str, first: bool) -> anyhow::Result<(Duration, Duration, f64)> {
    let plain = plain_commit(&id);
    let take = async {
        let t = Instant::now();
        inbox.accept(session, &id, words, Some(0), 0).await?;
        anyhow::Ok(t.elapsed())
    };
    let base = async {
        let t = Instant::now();
        turso.commit(plain).await?;
        anyhow::Ok(t.elapsed())
    };
    let (took, base) = if first {
        let took = take.await?;
        (took, base.await?)
    } else {
        let base = base.await?;
        (take.await?, base)
    };
    Ok((took, base, (took.as_secs_f64() - base.as_secs_f64()) * 1000.0))
}

/// How [`phases`] goes: its rounds, the commits each session makes a phase, and whether it tidies.
struct Shape {
    rounds: usize,
    per: usize,
    tidy: bool,
}

/// What [`phases`] measured: the takes' times, the plain commits', the tidies', and how long it all
/// took.
type Phased = (Vec<Duration>, Vec<Duration>, Vec<Duration>, Duration);

/// Takes and plain commits by `sessions` at once, each with one commit in flight: whole phases of
/// each, their order alternating, so a take's check never rides in a plain commit's shared write.
/// Tidying, each session tidies after each take, as the host does, and its next take in the phase
/// waits for that (its time counts it).
async fn phases(inbox: &Arc<Inbox>, turso: &Arc<TursoRecords>, sessions: &[String], prefix: &str, words: &str, shape: &Shape) -> anyhow::Result<Phased> {
    let (mut takes, mut plains, mut tidied) = (Vec::new(), Vec::new(), Vec::new());
    let started = Instant::now();
    for r in 0..shape.rounds {
        for taking in if r % 2 == 0 { [true, false] } else { [false, true] } {
            let senders = sessions.iter().map(|session| {
                let (inbox, turso) = (Arc::clone(inbox), Arc::clone(turso));
                let (session, words, prefix) = (session.clone(), words.to_owned(), format!("{prefix}-{r}"));
                let (per, tidy) = (shape.per, shape.tidy);
                async move {
                    let (mut took, mut tidies) = (Vec::with_capacity(per), Vec::new());
                    let mut owed = Duration::ZERO;
                    for m in 0..per {
                        let id = format!("{prefix}-{session}-{m}");
                        let t = Instant::now();
                        if taking {
                            inbox.accept(&session, &id, &words, Some(0), 0).await?;
                            took.push(t.elapsed().saturating_add(std::mem::take(&mut owed)));
                            let t = Instant::now();
                            if tidy && inbox.tidy(&session).await > 0 {
                                owed = t.elapsed();
                                tidies.push(owed);
                            }
                        } else {
                            turso.commit(plain_commit(&id)).await?;
                            took.push(t.elapsed());
                        }
                    }
                    anyhow::Ok((took, tidies))
                }
            });
            for (took, tidies) in futures::future::try_join_all(senders).await? {
                if taking {
                    takes.extend(took);
                    tidied.extend(tidies);
                } else {
                    plains.extend(took);
                }
            }
        }
    }
    Ok((takes, plains, tidied, started.elapsed()))
}

/// Commits a second, of `n` made in `wall`.
fn rate(n: usize, wall: Duration) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    n / wall.as_secs_f64().max(f64::MIN_POSITIVE)
}

/// A case as recorded: its takes, its plain commits, what the inbox adds, and (paired) each take
/// less its own plain commit.
fn case(takes: Vec<Duration>, plains: Vec<Duration>, paired: Option<Vec<f64>>) -> serde_json::Map<String, Value> {
    let (latency, plain) = (summary(takes), summary(plains));
    let mut m = serde_json::Map::new();
    m.insert("added".into(), added(&latency, &plain));
    m.insert("latency".into(), latency);
    m.insert("plain_commit".into(), plain);
    if let Some(p) = paired {
        m.insert("paired".into(), summary_ms(p));
    }
    m
}

/// `case` with `more` beside what it measured.
fn with(mut m: serde_json::Map<String, Value>, more: Value) -> Value {
    if let Value::Object(more) = more {
        m.extend(more);
    }
    Value::Object(m)
}

/// One run: a fresh records file in `dir`, and on it each case (the module's doc), the repeats and
/// the disk's floor.
fn run(dir: &Path, messages: usize, sessions: usize, n: usize) -> anyhow::Result<Value> {
    let scratch = tempfile::Builder::new().prefix("send-path-").tempdir_in(dir)?;
    let turso = Arc::new(TursoRecords::open(&scratch.path().join(kiln::records::FILE))?);
    let inbox = Arc::new(Inbox::new(turso.clone(), Limits::default()));
    let (_, mut was) = store(turso.stats(), [0; 4]);
    let words = digest("a message of the size people send, more or less: deploy the fix to staging", &[]);
    let (keep, trim) = (usize::try_from(KEEP).unwrap_or(4096), usize::try_from(TRIM).unwrap_or(256));
    futures::executor::block_on(async {
        // One session, a message at a time.
        inbox.open("solo", 0).await?;
        let (mut alone, mut base_alone, mut paired_alone) = (Vec::with_capacity(messages), Vec::with_capacity(messages), Vec::with_capacity(messages));
        for m in 0..messages {
            let (took, base, diff) = pair(&inbox, &turso, "solo", format!("r{n}-solo-{m}"), &words, m % 2 == 0).await?;
            alone.push(took);
            base_alone.push(base);
            paired_alone.push(diff);
        }
        let (alone_store, now) = store(turso.stats(), was);
        was = now;
        // A repeat of each: answered with the first time's answer.
        let mut repeats = Vec::with_capacity(messages.min(500));
        for m in 0..messages.min(500) {
            let id = format!("r{n}-solo-{m}");
            let t = Instant::now();
            let a = inbox.accept("solo", &id, &words, Some(0), 0).await?;
            repeats.push(t.elapsed());
            anyhow::ensure!(a.repeat, "{id} wasn't a repeat");
        }
        // A session that keeps its most ids: each take with its plain commit, as above.
        let most = keep.saturating_add(trim);
        // Each arrival with its most ids, one at a time, timed (its first message waits for it).
        let mut arrived = vec![fill(&inbox, "full", &format!("r{n}-kept"), most).await?];
        let at_most = messages.min(1024);
        let (full, base_full, paired_full, tidied) = at_its_bound(&inbox, &turso, "full", &format!("r{n}-full"), &words, at_most).await?;
        // A busy host: more sessions at their most ids (and what their tidying left), the others
        // taking messages all along (and tidying, as the host does) while one is measured, each
        // take of its with its plain commit.
        for (session, tag) in BUSY {
            arrived.push(fill(&inbox, session, &format!("r{n}-{tag}"), most).await?);
        }
        let measuring = std::sync::atomic::AtomicBool::new(true);
        let others = BUSY.iter().skip(1).map(|(session, tag)| {
            let (inbox, words, measuring) = (Arc::clone(&inbox), words.clone(), &measuring);
            async move {
                let mut m = 0usize;
                while measuring.load(std::sync::atomic::Ordering::Acquire) {
                    inbox.accept(session, &format!("r{n}-{tag}-all-along-{m}"), &words, Some(0), 0).await?;
                    let _ = inbox.tidy(session).await;
                    m += 1;
                }
                anyhow::Ok(m)
            }
        });
        let measured = async {
            let r = at_its_bound(&inbox, &turso, "busy1", &format!("r{n}-busy"), &words, at_most).await;
            measuring.store(false, std::sync::atomic::Ordering::Release);
            r
        };
        let (measured, others) = futures::join!(measured, futures::future::try_join_all(others));
        let (busy, base_busy, paired_busy, tidied_busy) = measured?;
        let others_took: usize = others?.into_iter().sum();
        // A session with a long history: one that let go of 25,600 ids over its life (its bound's
        // 256, a hundred times), which its scans read through until a compaction takes them; takes
        // on it, each with its plain commit, tidying as the host does.
        let long = "long";
        arrived.push(fill(&inbox, long, &format!("r{n}-long-kept"), most).await?);
        for cycle in 0..100usize {
            let more: Vec<String> = (0..trim).map(|m| format!("r{n}-long-{cycle}-{m}")).collect();
            inbox.import(long, &more, 0).await?;
            let _ = inbox.tidy(long).await;
        }
        let (longer, base_long, paired_long, tidied_long) = at_its_bound(&inbox, &turso, long, &format!("r{n}-long-take"), &words, at_most).await?;
        let (_, now) = store(turso.stats(), was);
        was = now;
        // Many sessions at once, each with one commit in flight, in whole phases apart.
        let rounds = 4usize;
        let per = messages.div_ceil(sessions).max(1).div_ceil(rounds).max(1);
        let fresh: Vec<String> = (0..sessions).map(|s| format!("s{s}")).collect();
        for s in &fresh {
            inbox.open(s, 0).await?;
        }
        let (together, base_together, _, wall) = phases(&inbox, &turso, &fresh, &format!("r{n}-s"), &words, &Shape { rounds, per, tidy: false }).await?;
        let (together_store, now) = store(turso.stats(), was);
        was = now;
        let together_rate = rate(together.len().saturating_add(base_together.len()), wall);
        // Many sessions at once, each at its most ids, tidying as the host does: each passes its
        // bound once in its takes, at its own point (as a host's sessions do, not all at once).
        let crowd: Vec<String> = (0..sessions).map(|s| format!("c{s}")).collect();
        let fills = crowd.iter().enumerate().map(|(s, session)| {
            let (inbox, session) = (Arc::clone(&inbox), session.clone());
            // Past its bound after (trim - this) of its takes.
            let count = keep.saturating_add(s.saturating_mul(2).saturating_add(1).saturating_mul(trim) / sessions.saturating_mul(2).max(1));
            async move { fill(&inbox, &session, &format!("r{n}-{session}-kept"), count).await.map(|_| ()) }
        });
        futures::future::try_join_all(fills).await?;
        let (_, now) = store(turso.stats(), was);
        was = now;
        let crowd_shape = Shape { rounds, per: trim.div_ceil(rounds).max(1), tidy: true };
        let (crowded, base_crowded, tidied_crowded, crowd_wall) = phases(&inbox, &turso, &crowd, &format!("r{n}-c"), &words, &crowd_shape).await?;
        let (crowd_store, now) = store(turso.stats(), was);
        was = now;
        let crowd_rate = rate(crowded.len().saturating_add(base_crowded.len()), crowd_wall);
        // Across a compaction: the store's history pushed by plain commits of their own to just
        // short of its first compaction, which begins under takes on a session at its most ids,
        // each with its plain commit, its steps run between them.
        let first = KEEP_REVS.saturating_add(COMPACT_EVERY);
        let head = turso.head().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let short = usize::try_from(first.saturating_sub(64).saturating_sub(head)).unwrap_or(0);
        let pushers = (0..sessions).map(|s| {
            let turso = Arc::clone(&turso);
            let each = short / sessions.max(1);
            async move {
                for m in 0..each {
                    let w = kiln_seams::records::Write::Put { family: "history".into(), key: format!("r{n}-h{s}-{m}").into_bytes(), value: vec![b'h'; 60] };
                    turso.commit(kiln_seams::records::Tx { conds: Vec::new(), writes: vec![w] }).await?;
                }
                anyhow::Ok(())
            }
        });
        futures::future::try_join_all(pushers).await?;
        let compactions = || turso.stats().compactions.load(std::sync::atomic::Ordering::Acquire);
        let (before, history) = (compactions(), turso.head().await.map_err(|e| anyhow::anyhow!("{e}"))?);
        let (_, now) = store(turso.stats(), was);
        was = now;
        let (compacting, base_compacting, paired_compacting, tidied_compacting) = at_its_bound(&inbox, &turso, "full", &format!("r{n}-compacting"), &words, at_most).await?;
        let during = compactions().saturating_sub(before);
        let (compacting_store, _) = store(turso.stats(), was);
        // The one under way finished before what follows.
        turso.compact(0).await;
        // A first take, its session's head not in memory (as after a start, or as another process
        // takes it): an inbox fresh each round over the same records, every session's first take
        // there with its plain commit.
        let mut every: Vec<String> = vec!["solo".into(), "full".into(), long.into()];
        every.extend(BUSY.iter().map(|(s, _)| (*s).to_owned()));
        every.extend(fresh.iter().cloned());
        every.extend(crowd.iter().cloned());
        let cold_rounds = at_most.div_ceil(every.len().max(1)).max(1);
        let (mut cold, mut base_cold, mut paired_cold) = (Vec::new(), Vec::new(), Vec::new());
        for r in 0..cold_rounds {
            let fresh_inbox = Inbox::new(turso.clone(), Limits::default());
            for (k, session) in every.iter().enumerate() {
                let (t, b, d) = pair(&fresh_inbox, &turso, session, format!("r{n}-cold-{r}-{k}"), &words, (r + k) % 2 == 0).await?;
                cold.push(t);
                base_cold.push(b);
                paired_cold.push(d);
            }
        }
        // Each session's ids counted once a process, as it opens (its lane's first tidy, which no
        // take waits on unless one comes that moment), on an inbox fresh for it: timed apart.
        let counting = Inbox::new(turso.clone(), Limits::default());
        let mut opened = Vec::with_capacity(every.len());
        for session in &every {
            let t = Instant::now();
            counting.open(session, 0).await?;
            let _ = counting.tidy(session).await;
            opened.push(t.elapsed());
        }
        let crowd_kept = |s: usize| keep.saturating_add(s.saturating_mul(2).saturating_add(1).saturating_mul(trim) / sessions.saturating_mul(2).max(1));
        Ok(json!({
            "sequential": with(case(alone, base_alone, Some(paired_alone)), json!({"store": alone_store})),
            "repeat": summary(repeats),
            "at_its_most": with(case(full, base_full, Some(paired_full)), json!({"kept_before": most, "tidied": {"what": "its oldest ids let go, after a take's answer has gone (the next take waits for it, and its time counts it)", "latency": summary(tidied)}})),
            "a_busy_host": with(case(busy, base_busy, Some(paired_busy)), json!({"sessions_at_their_most": BUSY.len() + 1, "kept_each": most, "others_taking": BUSY.len() - 1, "others_took": others_took, "tidied": {"latency": summary(tidied_busy)}})),
            "a_long_history": with(case(longer, base_long, Some(paired_long)), json!({"let_go_before": 100 * trim, "kept": most, "tidied": {"what": "its oldest ids let go, reading through what it let go before (the next take waits for it, and its time counts it)", "latency": summary(tidied_long)}})),
            "concurrent": with(case(together, base_together, None), json!({"sessions": sessions, "per_session": per * rounds, "phases": "takes, and plain commits alone, in whole phases, their order alternating", "commits_per_second": together_rate, "store": together_store})),
            "concurrent_at_their_most": with(case(crowded, base_crowded, None), json!({"sessions": sessions, "kept_each": format!("{} to {}", crowd_kept(0), crowd_kept(sessions.saturating_sub(1))), "takes_each": crowd_shape.per * rounds, "tidied": {"what": "each session's oldest ids let go once, as it passed its bound (its next take in the phase waits for it, and its time counts it)", "latency": summary(tidied_crowded)}, "commits_per_second": crowd_rate, "store": crowd_store})),
            "across_a_compaction": with(case(compacting, base_compacting, Some(paired_compacting)), json!({"history_at": history, "compaction_at": first, "compactions_during": during, "tidied": {"latency": summary(tidied_compacting)}, "store": compacting_store})),
            "a_first_take": with(case(cold, base_cold, Some(paired_cold)), json!({"sessions": every.len(), "rounds": cold_rounds})),
            "opened": {"what": "a session opened on an inbox fresh for it, its ids counted (its lane's first tidy: once a process, a take waiting on it only if one comes that moment)", "sessions": every.len(), "latency": summary(opened)},
            "arrived": {"what": "a session arriving with its most ids (4,352): its inbox opened, the ids it brought taken in (a read each, a synced commit each 400), counted; its first message here waits for it", "sessions": arrived.len(), "latency": summary(arrived)},
            "disk_floor": floor(scratch.path())?,
        }))
    })
}

/// What `with` adds to `without`, at p50, p99 and p99.9, in milliseconds.
fn added(with: &Value, without: &Value) -> Value {
    let p = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let d = |k: &str| p(with, k).zip(p(without, k)).map(|(a, b)| a - b);
    json!({"p50_ms": d("p50_ms"), "p99_ms": d("p99_ms"), "p999_ms": d("p999_ms")})
}

/// The median of `xs` (the middle one; of two in the middle, the higher).
fn median(mut xs: Vec<f64>) -> Option<f64> {
    xs.sort_by(f64::total_cmp);
    xs.get(xs.len() / 2).copied()
}

fn main() -> anyhow::Result<()> {
    let a = args()?;
    // The load discipline: a loaded machine's numbers aren't recorded.
    let (before, loaded) = common::quiet("send_path", a.wait_quiet, a.any_load)?;
    let mut runs = Vec::with_capacity(a.runs);
    for n in 0..a.runs {
        runs.push(run(&a.dir, a.messages, a.sessions, n)?);
    }
    let after = load();
    // What the inbox adds, case by case, run by run (each against its own baseline), and the median.
    let every = |path: &str| -> Vec<f64> { runs.iter().filter_map(|r| r.pointer(path).and_then(Value::as_f64)).collect() };
    let mut added_by = serde_json::Map::new();
    let mut paired = serde_json::Map::new();
    let mut median_run = serde_json::Map::new();
    // The median run's own figures at `path`, for scale: the run whose p99 there is the middle one.
    let middle = |path: &str| -> Value {
        let mut each: Vec<(f64, &Value)> = runs.iter().filter_map(|r| Some((r.pointer(path)?.get("p99_ms")?.as_f64()?, r))).collect();
        each.sort_by(|x, y| x.0.total_cmp(&y.0));
        each.get(each.len() / 2).and_then(|(_, r)| r.pointer(path)).cloned().unwrap_or(Value::Null)
    };
    for c in CASES {
        let at = |k: &str| every(&format!("/{c}/added/{k}"));
        added_by.insert(c.into(), json!({"p50_ms": median(at("p50_ms")), "p99_ms": median(at("p99_ms")), "p999_ms": median(at("p999_ms")), "p99_ms_every_run": at("p99_ms")}));
        let p = every(&format!("/{c}/paired/p99_ms"));
        if !p.is_empty() {
            paired.insert(c.into(), json!({"p99_ms": median(p.clone()), "p99_ms_every_run": p}));
        }
        median_run.insert(c.into(), json!({"latency": middle(&format!("/{c}/latency")), "plain_commit": middle(&format!("/{c}/plain_commit"))}));
    }
    for (name, path) in [("at_its_most_tidied", "/at_its_most/tidied/latency"), ("a_long_history_tidied", "/a_long_history/tidied/latency"), ("concurrent_at_their_most_tidied", "/concurrent_at_their_most/tidied/latency"), ("opened", "/opened/latency"), ("arrived", "/arrived/latency"), ("repeat", "/repeat"), ("disk_floor", "/disk_floor")] {
        median_run.insert(name.into(), middle(path));
    }
    let within = CASES.iter().all(|c| added_by.get(*c).and_then(|v| v.get("p99_ms")).and_then(Value::as_f64).is_some_and(|x| x <= 2.0));
    let record = json!({
        "bench": "send_path",
        "criterion": "AC-T3.2.3: added send latency from the inbox check, p99 <= 2 ms: in each case, each run's p99 of takes less its p99 of plain commits (interleaved, order alternating; for many at once, in phases apart), the median run; the cases: one session at a time, a session at its most ids, one on a busy host (8 sessions at their most, the others taking all along), one with a long history (25,600 ids let go before), 32 at once, 32 at once at their most ids tidying, across a compaction, and a first take with its head not in memory; beside them, a session's opening (its ids counted)",
        "commit": a.commit,
        "date": now(),
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "machine": machine(),
        "disk": disk(&a.dir),
        "load": {"before": before, "after": after, "quiet_below": QUIET, "loaded": loaded},
        "runs": a.runs,
        "messages": a.messages,
        "added_by_the_inbox": added_by,
        "paired_difference": {"what": "each message's take less its own plain commit: its p99, the median run (none for many at once: they go in phases apart)", "cases": paired},
        "median_run": median_run,
        "every_run": runs,
        "within": within,
    });
    let text = serde_json::to_string_pretty(&record)?;
    if let Some(out) = &a.out {
        std::fs::write(out, &text)?;
    }
    println!("{text}");
    Ok(())
}
