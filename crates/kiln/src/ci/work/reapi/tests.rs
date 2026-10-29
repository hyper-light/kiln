//! W24-R26's tests: the cache's methods on kiln's store (a scratch hub's) and on either ledger, and
//! the whole of it over kiln's own listener and kiln-http's HTTP/2, trailers and all.

use std::collections::VecDeque;

use anyhow::Context as _;
use super::pb::{Out, each};

use super::*;

fn caller(repo: &str, domain: &'static str) -> Caller {
    Caller { repo: repo.into(), domain, read: true, write: true, run: "run-1".into(), leg: "leg-1".into() }
}

fn cache_on(hub: &Arc<Hub>, records: bool) -> Cache {
    Cache { hub: Arc::clone(hub), ledger: if records { Ledger::Records { records: Arc::clone(&hub.records), audit: hub.store.clone() } } else { Ledger::Table(hub.store.clone()) } }
}

fn message(f: impl FnOnce(&mut Out)) -> Vec<u8> {
    let mut o = Out::default();
    f(&mut o);
    o.into_bytes()
}

/// `google.rpc.Status`'s code.
fn code_of(status: &[u8]) -> anyhow::Result<u32> {
    let mut code = 0;
    each(status, |f, v| {
        if f == 1 {
            code = u32::try_from(v.uint()?).unwrap_or(u32::MAX);
        }
        Ok(())
    })?;
    Ok(code)
}

/// FindMissingBlobs: those missing.
async fn find(c: &Cache, who: &Caller, ds: &[Digest]) -> Result<Vec<Digest>, Status> {
    let answer = unary(c, who, CAS, "FindMissingBlobs", &message(|o| ds.iter().for_each(|d| o.message(2, &d.encode())))).await?;
    let mut out = Vec::new();
    each(&answer, |f, v| {
        if f == 2 {
            out.push(Digest::decode(v.bytes()?)?);
        }
        Ok(())
    })
    .map_err(invalid)?;
    Ok(out)
}

/// BatchUpdateBlobs: each blob's code.
async fn upload(c: &Cache, who: &Caller, blobs: &[(Digest, Vec<u8>)]) -> Result<Vec<u32>, Status> {
    let body = message(|o| {
        for (d, data) in blobs {
            o.message(2, &message(|r| {
                r.message(1, &d.encode());
                r.bytes(2, data);
            }));
        }
    });
    let answer = unary(c, who, CAS, "BatchUpdateBlobs", &body).await?;
    let mut codes = Vec::new();
    each(&answer, |f, v| {
        if f == 1 {
            each(v.bytes()?, |f, v| {
                if f == 2 {
                    codes.push(code_of(v.bytes()?).map_err(|e| pb::Bad(e.to_string()))?);
                }
                Ok(())
            })?;
        }
        Ok(())
    })
    .map_err(invalid)?;
    Ok(codes)
}

/// BatchReadBlobs: each blob's code and data.
async fn download(c: &Cache, who: &Caller, ds: &[Digest]) -> Result<Vec<(u32, Vec<u8>)>, Status> {
    let answer = unary(c, who, CAS, "BatchReadBlobs", &message(|o| ds.iter().for_each(|d| o.message(2, &d.encode())))).await?;
    let mut out = Vec::new();
    each(&answer, |f, v| {
        if f == 1 {
            let (mut code, mut data) = (0, Vec::new());
            each(v.bytes()?, |f, v| {
                match f {
                    2 => data = v.bytes()?.to_vec(),
                    3 => code = code_of(v.bytes()?).map_err(|e| pb::Bad(e.to_string()))?,
                    _ => {}
                }
                Ok(())
            })?;
            out.push((code, data));
        }
        Ok(())
    })
    .map_err(invalid)?;
    Ok(out)
}

/// W24-R26: blobs go in checked against their digests and come back whole; a repository is told
/// of, and reads, only what it put; the empty blob is everyone's; a leg reads and writes as its
/// cache mode lets it.
#[test]
fn w24_r26_blobs_go_in_checked_and_come_back_whole() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let c = cache_on(&hub, false);
        let app = caller("acme/app", "trusted");
        let (a, big) = (b"alpha".to_vec(), kiln_seams::blobs::noise(3 << 20, 7));
        let (da, dbig, empty) = (Digest::of(&a), Digest::of(&big), Digest::of(b""));
        assert_eq!(find(&c, &app, &[da.clone(), dbig.clone(), empty.clone(), da.clone()]).await?, [da.clone(), dbig.clone()], "missing, each once; the empty blob never");
        let wrong = Digest::of(b"something else");
        assert_eq!(upload(&c, &app, &[(da.clone(), a.clone()), (dbig.clone(), big.clone()), (wrong.clone(), a.clone())]).await?, [0, 0, Code::InvalidArgument as u32]);
        assert_eq!(find(&c, &app, &[da.clone(), dbig.clone(), wrong.clone()]).await?, std::slice::from_ref(&wrong));
        let got = download(&c, &app, &[da.clone(), dbig.clone(), wrong.clone(), empty.clone()]).await?;
        assert_eq!(got.iter().map(|(code, _)| *code).collect::<Vec<_>>(), [0, 0, Code::NotFound as u32, 0]);
        assert!(got.first().is_some_and(|(_, d)| *d == a) && got.get(1).is_some_and(|(_, d)| *d == big), "whole");
        // Put again: nothing moves, all's well.
        assert_eq!(upload(&c, &app, &[(da.clone(), a.clone())]).await?, [0]);
        // Another repository is told of none of it, and reads none of it, until it puts it itself.
        let other = caller("acme/other", "trusted");
        assert_eq!(find(&c, &other, std::slice::from_ref(&da)).await?, std::slice::from_ref(&da));
        assert_eq!(download(&c, &other, std::slice::from_ref(&da)).await?.first().map(|(code, _)| *code), Some(Code::NotFound as u32));
        assert_eq!(upload(&c, &other, &[(da.clone(), a.clone())]).await?, [0]);
        assert!(find(&c, &other, std::slice::from_ref(&da)).await?.is_empty());
        // Cache modes: read-only writes nothing; `none` does nothing at all.
        let reader = Caller { write: false, ..app.clone() };
        assert_eq!(upload(&c, &reader, &[(da.clone(), a.clone())]).await.err().map(|s| s.code), Some(Code::PermissionDenied));
        assert!(find(&c, &reader, std::slice::from_ref(&da)).await?.is_empty());
        let none = Caller { read: false, write: false, ..app.clone() };
        assert_eq!(find(&c, &none, std::slice::from_ref(&da)).await.err().map(|s| s.code), Some(Code::PermissionDenied));
        // A batch past what it takes is refused, pointing at ByteStream.
        let huge = Digest { hash: dbig.hash.clone(), size: 5 << 20 };
        assert!(download(&c, &app, &[huge]).await.is_err_and(|s| s.code == Code::InvalidArgument && s.message.contains("ByteStream")));
        Ok(())
    })
}

/// A ByteStream write's messages, `pieces` of `data` at their offsets, the last finishing it.
fn writes(resource: &str, data: &[u8], pieces: usize) -> VecDeque<Bytes> {
    let size = data.len().div_ceil(pieces.max(1)).max(1);
    let chunks: Vec<&[u8]> = data.chunks(size).collect();
    let last = chunks.len().saturating_sub(1);
    let mut at = 0usize;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let m = message(|o| {
                if i == 0 {
                    o.str(1, resource);
                }
                o.int(2, i64::try_from(at).unwrap_or(0));
                o.bool(3, i == last);
                o.bytes(10, chunk);
            });
            at += chunk.len();
            Bytes::from(m)
        })
        .collect()
}

impl Source for VecDeque<Bytes> {
    fn message(&mut self) -> impl std::future::Future<Output = Result<Option<Bytes>, Status>> + Send {
        std::future::ready(Ok(self.pop_front()))
    }
}

/// A streamed answer's messages, joined, and its status.
async fn drain(r: Response) -> anyhow::Result<(Vec<Bytes>, Option<String>)> {
    let status = r.headers().get("grpc-status").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let collected = r.into_body().collect().await?;
    let status = status.or_else(|| collected.trailers().and_then(|t| t.get("grpc-status")).and_then(|v| v.to_str().ok()).map(str::to_owned));
    let mut buf = BytesMut::from(&collected.to_bytes()[..]);
    let mut out = Vec::new();
    while let Some(m) = take_message(&mut buf)? {
        out.push(m);
    }
    anyhow::ensure!(buf.is_empty(), "{} bytes after the last message", buf.len());
    Ok((out, status))
}

/// A ByteStream read's data.
fn read_data(messages: &[Bytes]) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    for m in messages {
        each(m, |f, v| {
            if f == 10 {
                data.extend_from_slice(v.bytes()?);
            }
            Ok(())
        })?;
    }
    Ok(data)
}

/// W24-R26: ByteStream writes a blob in pieces, each at the offset the last left, checked whole
/// before it's kept; one held already is answered at once; reads give any range, a megabyte a
/// message, `OK` in the trailers.
#[test]
fn w24_r26_bytestream_writes_in_pieces_and_reads_ranges() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let c = cache_on(&hub, false);
        let app = caller("acme/app", "trusted");
        let blob = kiln_seams::blobs::noise((5 << 20) + 11, 3);
        let d = Digest::of(&blob);
        let upload = format!("main/uploads/u-1/blobs/{}/{}", d.hash, d.size);
        // Wrong bytes, a gap, no finish: refused, nothing kept.
        let mut forged = blob.clone();
        if let Some(b) = forged.get_mut(77) {
            *b ^= 1;
        }
        assert!(write_stream(&c, &app, writes(&upload, &forged, 3)).await.is_err_and(|s| s.code == Code::InvalidArgument && s.message.contains("aren't")));
        let mut gap = writes(&upload, &blob, 3);
        let _ = gap.remove(1);
        assert!(write_stream(&c, &app, gap).await.is_err_and(|s| s.message.contains("write_offset")));
        let mut cut = writes(&upload, &blob, 3);
        let _ = cut.pop_back();
        assert!(write_stream(&c, &app, cut).await.is_err_and(|s| s.message.contains("finish_write")));
        assert_eq!(find(&c, &app, std::slice::from_ref(&d)).await?, std::slice::from_ref(&d));
        assert!(write_stream(&c, &app, writes(&format!("main/blobs/{}/{}", d.hash, d.size), &blob, 1)).await.is_err_and(|s| s.message.contains("isn't an upload's")));
        // Whole, in three pieces.
        let committed = write_stream(&c, &app, writes(&upload, &blob, 3)).await?;
        assert_eq!(committed, pb::write_response(d.size));
        assert!(find(&c, &app, std::slice::from_ref(&d)).await?.is_empty());
        // Again: answered at its first message, the whole committed.
        let mut first = writes(&upload, &blob, 3);
        first.truncate(1);
        assert_eq!(write_stream(&c, &app, first).await?, pb::write_response(d.size));
        let status = unary(&c, &app, BYTESTREAM, "QueryWriteStatus", &message(|o| o.str(1, &upload))).await?;
        assert_eq!(status, pb::write_status(d.size, true));
        // A range, and the whole.
        let read = |offset: i64, limit: i64| message(|o| {
            o.str(1, &format!("main/blobs/{}/{}", d.hash, d.size));
            o.int(2, offset);
            o.int(3, limit);
        });
        let (messages, status) = drain(read_stream(&c, &app, &read(1000, 3 << 20)).await?).await?;
        assert_eq!(status.as_deref(), Some("0"));
        assert_eq!(read_data(&messages)?, blob.get(1000..1000 + (3 << 20)).unwrap_or_default());
        assert!(messages.iter().all(|m| m.len() <= READ_PIECE + 16), "a megabyte a message");
        let (messages, _) = drain(read_stream(&c, &app, &read(0, 0)).await?).await?;
        assert!(read_data(&messages)? == blob, "the whole blob");
        // Past its end; one not held.
        assert_eq!(read_stream(&c, &app, &read(d.size + 1, 0)).await.err().map(|s| s.code), Some(Code::OutOfRange));
        let other = caller("acme/other", "trusted");
        assert_eq!(read_stream(&c, &other, &read(0, 0)).await.err().map(|s| s.code), Some(Code::NotFound));
        Ok(())
    })
}

/// A `Directory` of files and subdirectories, by name.
fn directory(files: &[(&str, &Digest)], dirs: &[(&str, &Digest)]) -> Vec<u8> {
    let node = |name: &str, d: &Digest| {
        message(|n| {
            n.str(1, name);
            n.message(2, &d.encode());
        })
    };
    message(|o| {
        for (name, d) in files {
            o.message(1, &node(name, d));
        }
        for (name, d) in dirs {
            o.message(2, &node(name, d));
        }
    })
}

/// GetTree's directories (as digests) and next page token.
fn tree_page(answer: &[u8]) -> anyhow::Result<(Vec<Digest>, String)> {
    let (mut dirs, mut next) = (Vec::new(), String::new());
    each(answer, |f, v| {
        match f {
            1 => dirs.push(Digest::of(v.bytes()?)),
            2 => next = v.str()?.to_owned(),
            _ => {}
        }
        Ok(())
    })?;
    Ok((dirs, next))
}

/// W24-R26: GetTree walks a tree breadth first, each directory once, in pages; a directory not
/// held ends it, saying which.
#[test]
fn w24_r26_get_tree_walks_a_tree_in_pages() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let c = cache_on(&hub, false);
        let app = caller("acme/app", "trusted");
        let (f1, f2) = (Digest::of(b"one"), Digest::of(b"two"));
        let l1 = directory(&[("one.txt", &f1)], &[]);
        let l2 = directory(&[("two.txt", &f2)], &[]);
        let (dl1, dl2) = (Digest::of(&l1), Digest::of(&l2));
        let m = directory(&[], &[("l1", &dl1), ("l2", &dl2)]);
        let dm = Digest::of(&m);
        let root = directory(&[("zero.txt", &f1)], &[("l1", &dl1), ("m", &dm)]);
        let droot = Digest::of(&root);
        assert_eq!(upload(&c, &app, &[(droot.clone(), root), (dm.clone(), m), (dl1.clone(), l1), (dl2.clone(), l2)]).await?, [0, 0, 0, 0]);
        let ask = |page: i64, token: &str| message(|o| {
            o.message(2, &droot.encode());
            o.int(3, page);
            o.str(4, token);
        });
        let (all, next) = tree_page(&unary(&c, &app, CAS, "GetTree", &ask(0, "")).await?)?;
        assert_eq!((all.clone(), next.as_str()), (vec![droot.clone(), dl1.clone(), dm.clone(), dl2.clone()], ""), "breadth first, l1 once");
        let (first, next) = tree_page(&unary(&c, &app, CAS, "GetTree", &ask(3, "")).await?)?;
        assert_eq!((first, next.as_str()), (vec![droot.clone(), dl1.clone(), dm.clone()], "3"));
        let (rest, next) = tree_page(&unary(&c, &app, CAS, "GetTree", &ask(3, "3")).await?)?;
        assert_eq!((rest, next.as_str()), (vec![dl2.clone()], ""));
        assert!(unary(&c, &app, CAS, "GetTree", &ask(3, "x")).await.is_err_and(|s| s.code == Code::InvalidArgument));
        // A subdirectory nobody put: the walk stops there, saying which.
        let lost = Digest::of(b"never put");
        let top = directory(&[], &[("lost", &lost)]);
        let dtop = Digest::of(&top);
        assert_eq!(upload(&c, &app, &[(dtop.clone(), top)]).await?, [0]);
        let e = unary(&c, &app, CAS, "GetTree", &message(|o| o.message(2, &dtop.encode()))).await.err().context("a tree with a hole was walked")?;
        assert!(e.code == Code::NotFound && e.message.contains(&lost.hash), "{e}");
        Ok(())
    })
}

/// An `ActionResult`: an output file, an output directory's tree, stdout, an exit code.
fn action_result(file: &Digest, tree: &Digest, stdout: &Digest, exit: i64) -> Vec<u8> {
    message(|r| {
        r.message(2, &message(|f| {
            f.str(1, "bin/app");
            f.message(2, &file.encode());
            f.bool(4, true);
        }));
        r.message(3, &message(|d| {
            d.str(1, "gen");
            d.message(3, &tree.encode());
        }));
        r.int(4, exit);
        r.message(6, &stdout.encode());
    })
}

fn ask_action(action: &Digest) -> Vec<u8> {
    message(|o| o.message(2, &action.encode()))
}

fn update(action: &Digest, result: &[u8]) -> Vec<u8> {
    message(|o| {
        o.message(2, &action.encode());
        o.message(3, result);
    })
}

/// The action cache on `records` (or the table): kept on the ledger, answered while every blob
/// a result names is held.
async fn action_cache_on(records: bool) -> anyhow::Result<()> {
    let hub = crate::hub::tests::scratch_hub().await?;
    let c = cache_on(&hub, records);
    let app = caller("acme/app", "trusted");
    let (bin, x, out) = (b"the binary".to_vec(), b"generated".to_vec(), b"compiling\n".to_vec());
    let (dbin, dx, dout) = (Digest::of(&bin), Digest::of(&x), Digest::of(&out));
    let tree = message(|t| t.message(1, &directory(&[("x.rs", &dx)], &[])));
    let dtree = Digest::of(&tree);
    let action = Digest::of(b"an action: cargo build, on these inputs");
    let result = action_result(&dbin, &dtree, &dout, 0);
    assert_eq!(unary(&c, &app, AC, "GetActionResult", &ask_action(&action)).await.err().map(|s| s.code), Some(Code::NotFound));
    // Before its blobs are put; a failure; then as it should be.
    let e = unary(&c, &app, AC, "UpdateActionResult", &update(&action, &result)).await.err().context("a result naming nothing held was kept")?;
    assert!(e.code == Code::FailedPrecondition && e.message.contains("upload its outputs first"), "{e}");
    assert_eq!(upload(&c, &app, &[(dbin.clone(), bin), (dout.clone(), out), (dtree.clone(), tree)]).await?, [0, 0, 0]);
    let e = unary(&c, &app, AC, "UpdateActionResult", &update(&action, &result)).await.err().context("a result whose tree's file isn't held was kept")?;
    assert!(e.message.contains(&dx.hash), "the tree's file: {e}");
    assert_eq!(upload(&c, &app, &[(dx.clone(), x)]).await?, [0]);
    let failed = action_result(&dbin, &dtree, &dout, 2);
    assert!(unary(&c, &app, AC, "UpdateActionResult", &update(&action, &failed)).await.is_err_and(|s| s.code == Code::FailedPrecondition && s.message.contains("W24-R07")));
    assert_eq!(unary(&c, &app, AC, "UpdateActionResult", &update(&action, &result)).await?, result);
    assert_eq!(unary(&c, &app, AC, "GetActionResult", &ask_action(&action)).await?, result);
    // On the ledger's one path: the action's unit's result, naming its blobs (W24-R02, R30), keeping
    // the client's.
    let (sc, unit) = (app.scope(), action_unit(&action, "trusted"));
    let kept = c.ledger.run(move |rows| super::super::lookup(rows, &sc, &unit, &|p: &PathSet| p.clone())).await?.context("not on the ledger")?.recorded;
    assert_eq!((kept.outcome.as_str(), kept.exit, kept.domain.as_str(), kept.provenance.run.as_str(), kept.provenance.leg.as_str()), ("success", Some(0), "trusted", "run-1", "leg-1"));
    assert_eq!(kept.outputs.get("bin/app"), Some(&format!("sha256:{}", dbin.hash)));
    assert_eq!(kept.action_result.as_deref().and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok()), Some(result.clone()));
    let keys = super::super::ledger::blob_keys(&kept);
    assert!([&dbin, &dtree, &dout].iter().all(|d| keys.contains(&cas::key(&d.hash))), "{keys:?}");
    // Reads go down only (W24-R21): an untrusted leg reads the trusted result; its own goes to its
    // domain, which trusted legs never read; another repository's legs read neither.
    let fork = caller("acme/app", "untrusted");
    assert_eq!(unary(&c, &fork, AC, "GetActionResult", &ask_action(&action)).await?, result);
    let theirs = Digest::of(b"an action a fork ran");
    assert_eq!(unary(&c, &fork, AC, "UpdateActionResult", &update(&theirs, &result)).await?, result);
    assert_eq!(unary(&c, &fork, AC, "GetActionResult", &ask_action(&theirs)).await?, result);
    assert_eq!(unary(&c, &app, AC, "GetActionResult", &ask_action(&theirs)).await.err().map(|s| s.code), Some(Code::NotFound));
    assert_eq!(unary(&c, &caller("acme/other", "trusted"), AC, "GetActionResult", &ask_action(&action)).await.err().map(|s| s.code), Some(Code::NotFound));
    // A leg that may not write keeps nothing; one that may not read finds nothing.
    assert_eq!(unary(&c, &Caller { write: false, ..app.clone() }, AC, "UpdateActionResult", &update(&action, &result)).await.err().map(|s| s.code), Some(Code::PermissionDenied));
    assert_eq!(unary(&c, &Caller { read: false, ..app.clone() }, AC, "GetActionResult", &ask_action(&action)).await.err().map(|s| s.code), Some(Code::NotFound));
    // Entries are never overwritten (W24-R22): the same result again reproduces the entry; other
    // outputs for the same action are refused, and the entry, in doubt now, is judged again and
    // passed over (W24-R23), so the client runs the action itself.
    let other = b"another binary".to_vec();
    let dother = Digest::of(&other);
    assert_eq!(upload(&c, &app, &[(dother.clone(), other)]).await?, [0]);
    let twice = Digest::of(b"an action run twice");
    assert_eq!(unary(&c, &app, AC, "UpdateActionResult", &update(&twice, &result)).await?, result);
    assert_eq!(unary(&c, &app, AC, "UpdateActionResult", &update(&twice, &result)).await?, result);
    assert_eq!(unary(&c, &app, AC, "GetActionResult", &ask_action(&twice)).await?, result);
    let differs = action_result(&dother, &dtree, &dout, 0);
    let e = unary(&c, &app, AC, "UpdateActionResult", &update(&twice, &differs)).await.err().context("another result overwrote the entry")?;
    assert!(e.code == Code::FailedPrecondition && e.message.contains("never overwritten"), "{e}");
    assert_eq!(unary(&c, &app, AC, "GetActionResult", &ask_action(&twice)).await.err().map(|s| s.code), Some(Code::NotFound));
    // A blob the store let go of (its collection): the result is a miss, and the client runs it again.
    let gone = dx.hash.clone();
    hub.store.call(move |c| Ok(c.execute("DELETE FROM cas_index WHERE digest = ?1", [&gone])?)).await?;
    assert_eq!(unary(&c, &app, AC, "GetActionResult", &ask_action(&action)).await.err().map(|s| s.code), Some(Code::NotFound));
    Ok(())
}

/// W24-R26, W24-R02: action results are the ledger's, in the `work_records` table and in Records
/// alike.
#[test]
fn w24_r26_action_results_are_kept_on_the_ledger_while_their_blobs_are_held() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        action_cache_on(false).await.context("on the table")?;
        action_cache_on(true).await.context("on Records")
    })
}

/// W24-R26: what isn't served, or isn't well formed, is refused, saying why.
#[test]
fn w24_r26_what_isnt_served_is_refused() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        let c = cache_on(&hub, false);
        let app = caller("acme/app", "trusted");
        let e = unary(&c, &app, EXECUTION, "Execute", &[]).await.err().context("Execute was served")?;
        assert!(e.code == Code::Unimplemented && e.message.contains("--remote_cache"), "{e}");
        assert_eq!(unary(&c, &app, CAS, "Nothing", &[]).await.err().map(|s| s.code), Some(Code::Unimplemented));
        let sha1 = message(|o| o.uint(3, 2));
        assert!(unary(&c, &app, CAS, "FindMissingBlobs", &sha1).await.is_err_and(|s| s.code == Code::InvalidArgument && s.message.contains("SHA-256")));
        let bad = message(|o| o.message(2, &Digest { hash: "XYZ".into(), size: 1 }.encode()));
        assert_eq!(unary(&c, &app, CAS, "FindMissingBlobs", &bad).await.err().map(|s| s.code), Some(Code::InvalidArgument));
        assert_eq!(unary(&c, &app, CAS, "FindMissingBlobs", &[0x0a, 9]).await.err().map(|s| s.code), Some(Code::InvalidArgument));
        // Capabilities: SHA-256, updates as the leg may write.
        let caps = |who: &Caller| {
            let (c, who) = (c.clone(), who.clone());
            async move { unary(&c, &who, CAPABILITIES, "GetCapabilities", &[]).await }
        };
        assert_eq!(caps(&app).await?, pb::capabilities(true, 4 << 20));
        assert_eq!(caps(&Caller { write: false, ..app.clone() }).await?, pb::capabilities(false, 4 << 20));
        Ok(())
    })
}

/// W24-R26: gRPC's framing: a message whole or not yet, compressed or too large refused; a status
/// message percent-encoded; resources parsed as the API names them.
#[test]
fn w24_r26_grpc_framing_and_resource_names() -> anyhow::Result<()> {
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&frame(b"hello"));
    buf.extend_from_slice(frame(b"next").get(..3).unwrap_or_default());
    assert_eq!(take_message(&mut buf)?, Some(Bytes::from_static(b"hello")));
    assert_eq!(take_message(&mut buf)?, None, "not all here yet");
    buf.extend_from_slice(frame(b"next").get(3..).unwrap_or_default());
    assert_eq!(take_message(&mut buf)?, Some(Bytes::from_static(b"next")));
    assert!(buf.is_empty());
    let mut compressed = BytesMut::from(&[1u8, 0, 0, 0, 1, 7][..]);
    assert_eq!(take_message(&mut compressed).err().map(|s| s.code), Some(Code::Unimplemented));
    let mut huge = BytesMut::from(&[0u8, 0xff, 0xff, 0xff, 0xff][..]);
    assert_eq!(take_message(&mut huge).err().map(|s| s.code), Some(Code::ResourceExhausted));
    assert_eq!(grpc_message("100% done: naïve\n"), "100%25 done: na%C3%AFve%0A");
    let h = "ab".repeat(32);
    assert_eq!(blob_of(&format!("blobs/{h}/12"), false)?, Digest { hash: h.clone(), size: 12 });
    assert_eq!(blob_of(&format!("a/b/blobs/{h}/12"), false)?.size, 12);
    assert_eq!(blob_of(&format!("main/uploads/0f2a/blobs/{h}/12/meta/data"), true)?.size, 12);
    for (resource, upload) in [(format!("blobs/{h}/12/x"), false), (format!("blobs/{h}"), false), (format!("blobs/{h}/-1"), false), (format!("compressed-blobs/zstd/{h}/12"), false), (format!("blobs/{h}/12"), true), ("uploads/u/blobs/xyz/1".to_owned(), true)] {
        assert!(blob_of(&resource, upload).is_err(), "{resource} was taken");
    }
    Ok(())
}

/// The Remote Execution API's methods, at kiln's listener, over HTTP/2 with prior knowledge as a
/// `grpc://` client speaks it.
struct Client {
    send: kiln_http::client::http2::SendRequest<srv::Body>,
    token: Option<String>,
}

impl Client {
    async fn call(&mut self, path: &str, body: srv::Body) -> anyhow::Result<(http::response::Parts, Vec<Bytes>, Option<String>, Option<String>)> {
        let mut req = http::Request::post(format!("http://kiln.test{path}")).header(header::CONTENT_TYPE, "application/grpc").header(header::TE, "trailers");
        if let Some(t) = &self.token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = self.send.send_request(req.body(body)?).await?;
        anyhow::ensure!(resp.version() == http::Version::HTTP_2, "answered over {:?}", resp.version());
        let (parts, body) = resp.into_parts();
        let collected = body.collect().await?;
        let said = |h: Option<&HeaderMap>, k: &str| h.and_then(|h| h.get(k)).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let (status, msg) = match said(Some(&parts.headers), "grpc-status") {
            Some(s) => (Some(s), said(Some(&parts.headers), "grpc-message")),
            None => (said(collected.trailers(), "grpc-status"), said(collected.trailers(), "grpc-message")),
        };
        let mut buf = BytesMut::from(&collected.to_bytes()[..]);
        let mut messages = Vec::new();
        while let Some(m) = take_message(&mut buf)? {
            messages.push(m);
        }
        Ok((parts, messages, status, msg))
    }

    async fn unary(&mut self, path: &str, msg: &[u8]) -> anyhow::Result<(Vec<Bytes>, Option<String>, Option<String>)> {
        let (_, messages, status, said) = self.call(path, srv::full(frame(msg))).await?;
        Ok((messages, status, said))
    }
}

/// W24-R26: the cache over kiln's own listener and router, HTTP/2 in the clear: a leg's token
/// opens it; each call ends with its status in the trailers (or, alone, in the headers);
/// ByteStream streams both ways.
#[test]
fn w24_r26_the_cache_over_kiln_http2_with_trailers() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub().await?;
        hub.store.call(|c| Ok(c.execute("INSERT INTO repos (id, name, clone_url, created_at) VALUES ('repo-1', 'acme/app', '', 0)", [])?)).await?;
        let token = crate::ci::token::mint(
            &hub,
            &crate::ci::token::Leg { leg: "leg-1", run: "run-1", repo: "repo-1", cache: vec![crate::ci::token::Access { scope: "refs/heads/main".into(), permission: crate::ci::token::READ | crate::ci::token::WRITE }], cache_mode: "write", ttl: 600 },
        );
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        let api = Arc::new(crate::api::Api { hub: Arc::clone(&hub) });
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let (api, remote) = (Arc::clone(&api), stream.peer_addr().ok());
                crate::rt::spawn(async move { srv::serve_accepted(stream, api, remote, None).await });
            }
        });
        let tcp = hyper_rt::tcp::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let (send, conn) = kiln_http::client::http2::handshake::<_, srv::Body>(crate::http::io::TcpIo::new(tcp)).await?;
        crate::rt::spawn(conn);
        let mut anon = Client { send: send.clone(), token: None };
        let mut leg = Client { send, token: Some(token) };

        // No token: UNAUTHENTICATED, alone in the headers.
        let (messages, status, said) = anon.unary("/build.bazel.remote.execution.v2.Capabilities/GetCapabilities", &[]).await?;
        assert_eq!((messages.len(), status.as_deref()), (0, Some("16")), "{said:?}");
        // Capabilities: one message, OK in the trailers.
        let (parts, messages, status, _) = leg.call("/build.bazel.remote.execution.v2.Capabilities/GetCapabilities", srv::full(frame(&[]))).await?;
        assert_eq!((parts.status, parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())), (StatusCode::OK, Some("application/grpc")));
        assert!(parts.headers.get("grpc-status").is_none(), "its status comes in the trailers");
        assert_eq!((messages, status.as_deref()), (vec![Bytes::from(pb::capabilities(true, 4 << 20))], Some("0")));
        // An action not cached: NOT_FOUND, alone.
        let action = Digest::of(b"an action");
        let (_, status, _) = leg.unary("/build.bazel.remote.execution.v2.ActionCache/GetActionResult", &ask_action(&action)).await?;
        assert_eq!(status.as_deref(), Some("5"));
        // A blob in a batch, and one streamed up in three messages, then down.
        let (small, big) = (b"stdout of the action".to_vec(), kiln_seams::blobs::noise(3 << 20, 9));
        let (dsmall, dbig) = (Digest::of(&small), Digest::of(&big));
        let batch = message(|o| {
            o.message(2, &message(|r| {
                r.message(1, &dsmall.encode());
                r.bytes(2, &small);
            }))
        });
        let (messages, status, _) = leg.unary("/build.bazel.remote.execution.v2.ContentAddressableStorage/BatchUpdateBlobs", &batch).await?;
        assert_eq!((messages.len(), status.as_deref()), (1, Some("0")));
        let up: Vec<Result<Frame<Bytes>, std::io::Error>> = writes(&format!("uploads/u-9/blobs/{}/{}", dbig.hash, dbig.size), &big, 3).into_iter().map(|m| Ok(Frame::data(frame(&m)))).collect();
        let (_, messages, status, said) = leg.call("/google.bytestream.ByteStream/Write", http_body_util::StreamBody::new(futures::stream::iter(up)).boxed()).await?;
        assert_eq!((messages, status.as_deref()), (vec![Bytes::from(pb::write_response(dbig.size))], Some("0")), "{said:?}");
        let (messages, status, _) = leg.unary("/google.bytestream.ByteStream/Read", &message(|o| o.str(1, &format!("blobs/{}/{}", dbig.hash, dbig.size)))).await?;
        assert_eq!(status.as_deref(), Some("0"));
        assert!(messages.len() >= 3 && read_data(&messages)? == big, "streamed down whole, {} messages", messages.len());
        let missing = message(|o| {
            o.message(2, &dsmall.encode());
            o.message(2, &dbig.encode());
        });
        let (messages, status, _) = leg.unary("/build.bazel.remote.execution.v2.ContentAddressableStorage/FindMissingBlobs", &missing).await?;
        assert_eq!((messages, status.as_deref()), (vec![Bytes::from(pb::missing(&[]))], Some("0")));
        // The action's result, kept and found.
        let result = message(|r| {
            r.message(2, &message(|f| {
                f.str(1, "out.bin");
                f.message(2, &dbig.encode());
            }));
            r.message(6, &dsmall.encode());
        });
        let (messages, status, said) = leg.unary("/build.bazel.remote.execution.v2.ActionCache/UpdateActionResult", &update(&action, &result)).await?;
        assert_eq!((messages, status.as_deref()), (vec![Bytes::from(result.clone())], Some("0")), "{said:?}");
        let (messages, status, _) = leg.unary("/build.bazel.remote.execution.v2.ActionCache/GetActionResult", &ask_action(&action)).await?;
        assert_eq!((messages, status.as_deref()), (vec![Bytes::from(result)], Some("0")));
        // Execute: not here, said as gRPC says it.
        let (_, status, said) = leg.unary("/build.bazel.remote.execution.v2.Execution/Execute", &[]).await?;
        assert_eq!(status.as_deref(), Some("12"));
        assert!(said.is_some_and(|m| m.contains("--remote_cache")));
        Ok(())
    })
}
