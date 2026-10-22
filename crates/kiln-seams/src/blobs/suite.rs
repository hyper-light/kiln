//! The `Blobs` contract, clause by clause (docs/seams.md, "Blobs"): the conformance suite every
//! implementation passes, the simulator, kiln's local store and its S3 one alike (CP-T10.1,
//! AC-T10.1.2). Each case runs against an empty store of its own. A case that waits on time asks
//! the fixture to let it pass (`pass`): the simulator's clock moves at once, an S3's is waited out,
//! so those waits are seconds at most.

use std::time::Duration;

use futures::future::BoxFuture;

use super::{Access, Blobs, BlobsFixture, Cond, LIST_MAX, Listed, Listing, Meta, Method, PARTS_MAX, PRESIGN_MAX, Put, Range, Refusal};
use crate::conformance::{Case, Failure, ensure, fine, refused, same};
use crate::sha256_hex;

/// `n` bytes that don't repeat or compress, from `seed` (xorshift64*): the same `n` and `seed`,
/// the same bytes.
pub fn noise(n: usize, seed: u64) -> Vec<u8> {
    // Each seed its own state (an odd multiplier and an XOR are both one to one), never 0.
    let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xd1b5_4a32_d192_ed03;
    if s == 0 {
        s = 1;
    }
    let mut out = Vec::with_capacity(n.saturating_add(8));
    while out.len() < n {
        s ^= s.wrapping_shr(12);
        s ^= s.wrapping_shl(25);
        s ^= s.wrapping_shr(27);
        out.extend_from_slice(&s.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    }
    out.truncate(n);
    out
}

type Out<'a> = BoxFuture<'a, Result<(), Failure>>;

/// A case's check: `$f` the fixture, `$b` its store.
macro_rules! clause {
    ($name:ident, $f:ident, $b:ident, $body:block) => {
        fn $name<F: BlobsFixture>($f: &F) -> Out<'_> {
            Box::pin(async move {
                #[allow(unused_variables)]
                let $b = $f.blobs();
                $body
            })
        }
    };
}

// ------------------------------------------------------------------------------------ helpers

fn len64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

async fn put(b: &dyn Blobs, key: &str, bytes: &[u8]) -> Result<Meta, Failure> {
    fine(&format!("put {key:?}"), b.put(key, bytes.to_vec(), Put::default()).await)
}

async fn put_as(b: &dyn Blobs, key: &str, bytes: &[u8], how: Put) -> Result<Meta, Refusal> {
    b.put(key, bytes.to_vec(), how).await
}

async fn get(b: &dyn Blobs, key: &str) -> Result<Vec<u8>, Failure> {
    Ok(fine(&format!("get {key:?}"), b.get(key, None).await)?.1)
}

async fn head(b: &dyn Blobs, key: &str) -> Result<Meta, Failure> {
    fine(&format!("head {key:?}"), b.head(key).await)
}

async fn range(b: &dyn Blobs, key: &str, start: u64, end: Option<u64>) -> Result<Vec<u8>, Refusal> {
    b.get(key, Some(Range { start, end })).await.map(|(_, v)| v)
}

fn when(cond: Cond) -> Put {
    Put { cond, sha256: None }
}

fn if_match(etag: &str) -> Put {
    when(Cond::IfMatch(etag.to_owned()))
}

fn checked(sha: String) -> Put {
    Put { cond: Cond::Always, sha256: Some(sha) }
}

/// What a put's answer and head say of an object, its time left out (a put says when the store
/// acknowledged it, head when it wrote it: the same on most stores, a moment apart on an S3).
fn ident(m: &Meta) -> (String, u64, String) {
    (m.key.clone(), m.size, m.etag.clone())
}

fn keys(l: &Listing) -> Vec<String> {
    l.objects.iter().map(|o| o.key.clone()).collect()
}

fn owned(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn slice(v: &[u8], from: usize, to: usize) -> Vec<u8> {
    v.get(from..to).map(<[u8]>::to_vec).unwrap_or_default()
}

/// Every object under `prefix`, a page of `page` at a time.
async fn listed(b: &dyn Blobs, prefix: &str, page: usize) -> Result<Vec<Listed>, Failure> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10_000 {
        let l = fine("list", b.list(prefix, after.as_deref(), page).await)?;
        let last = l.objects.last().map(|o| o.key.clone());
        out.extend(l.objects);
        match (l.truncated, last) {
            (true, Some(k)) => after = Some(k),
            (true, None) => return Err("a page said truncated with nothing in it".into()),
            (false, _) => return Ok(out),
        }
    }
    Err("the listing never ended".into())
}

async fn listed_keys(b: &dyn Blobs, prefix: &str) -> Result<Vec<String>, Failure> {
    Ok(listed(b, prefix, LIST_MAX).await?.into_iter().map(|o| o.key).collect())
}

fn kind<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>, want: &str, matches: fn(&Refusal) -> bool) -> Result<(), Failure> {
    match got {
        Err(e) if matches(&e) => Ok(()),
        other => Err(format!("{what}: {other:?}, want {want}")),
    }
}

fn invalid<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>) -> Result<(), Failure> {
    kind(what, got, "Invalid", |e| matches!(e, Refusal::Invalid { .. }))
}

fn not_found<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>) -> Result<(), Failure> {
    kind(what, got, "NotFound", |e| matches!(e, Refusal::NotFound(_)))
}

fn precondition<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>) -> Result<(), Failure> {
    kind(what, got, "PreconditionFailed", |e| matches!(e, Refusal::PreconditionFailed(_)))
}

fn bad_parts<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>) -> Result<(), Failure> {
    kind(what, got, "BadParts", |e| matches!(e, Refusal::BadParts(_)))
}

fn no_upload<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>) -> Result<(), Failure> {
    kind(what, got, "NoSuchUpload", |e| matches!(e, Refusal::NoSuchUpload(_)))
}

fn too_large<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>) -> Result<(), Failure> {
    kind(what, got, "TooLarge", |e| matches!(e, Refusal::TooLarge { .. }))
}

fn bad_range<T: std::fmt::Debug>(what: &str, got: Result<T, Refusal>, size: u64) -> Result<(), Failure> {
    match got {
        Err(Refusal::InvalidRange { size: s }) if s == size => Ok(()),
        other => Err(format!("{what}: {other:?}, want InvalidRange of {size} bytes")),
    }
}

/// A key of `n` bytes of `k`s in segments of at most 200.
fn ascii_key(n: usize) -> String {
    let mut out = String::with_capacity(n);
    let mut left = n;
    while left > 0 {
        let mut take = left.min(200);
        // Never leave a single byte: it would be a '/' and nothing after it.
        if left.saturating_sub(take) == 1 {
            take = take.saturating_sub(1);
        }
        out.push_str(&"k".repeat(take));
        left = left.saturating_sub(take);
        if left > 0 {
            out.push('/');
            left = left.saturating_sub(1);
        }
    }
    out
}

/// A key of exactly 960 bytes: two-byte characters, `/`s and one `x`.
fn wide_key() -> String {
    let segment = "é".repeat(120);
    format!("{segment}/{segment}/{segment}/{}x", "é".repeat(118))
}

/// Uploads `parts` (numbered from 1) to a new upload of `key`, completes it, and answers its
/// object.
async fn upload(b: &dyn Blobs, key: &str, parts: &[Vec<u8>]) -> Result<Meta, Failure> {
    let id = fine("start", b.start_upload(key).await)?;
    let mut named = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let n = u32::try_from(i).unwrap_or(u32::MAX).saturating_add(1);
        named.push((n, fine(&format!("part {n}"), b.upload_part(&id, n, p.clone()).await)?));
    }
    fine("complete", b.complete_upload(&id, named).await)
}

/// Parts of a multipart object: two of the store's minimum, then `tail` bytes.
fn parts_for<F: BlobsFixture>(f: &F, seed: u64, tail: usize) -> Vec<Vec<u8>> {
    let min = f.part_min();
    vec![noise(min, seed), noise(min, seed.wrapping_add(1)), noise(tail, seed.wrapping_add(2))]
}

// ------------------------------------------------------------------- the first fifteen (CP-M2)

clause!(c1, f, b, {
    let meta = put(b, "c/a.txt", b"hello").await?;
    same("its size and etag", (meta.size, meta.etag.clone()), (5, sha256_hex(b"hello")))?;
    let (m, bytes) = fine("get", b.get("c/a.txt", None).await)?;
    same("what's read back", (ident(&m), bytes), (ident(&meta), b"hello".to_vec()))
});

clause!(c2, f, b, {
    put(b, "r", b"0123456789").await?;
    same("bytes 2-4", fine("get", range(b, "r", 2, Some(4)).await)?, b"234".to_vec())?;
    same("bytes 7-", fine("get", range(b, "r", 7, None).await)?, b"789".to_vec())?;
    same("bytes 8-100", fine("get", range(b, "r", 8, Some(100)).await)?, b"89".to_vec())?;
    bad_range("bytes 10-", range(b, "r", 10, None).await, 10)?;
    bad_range("bytes 5-3", range(b, "r", 5, Some(3)).await, 10)
});

clause!(c3, f, b, {
    let meta = put(b, "h", b"abc").await?;
    same("head", ident(&head(b, "h").await?), ident(&meta))?;
    same("head of nothing", b.head("missing").await, Err(Refusal::NotFound("missing".into())))?;
    same("get of nothing", b.get("missing", None).await.map(|x| x.1), Err(Refusal::NotFound("missing".into())))
});

clause!(c4, f, b, {
    let first = put(b, "o", b"first").await?;
    let second = put(b, "o", b"second").await?;
    ensure(first.etag != second.etag, || "new content kept its etag".into())?;
    same("after the overwrite", get(b, "o").await?, b"second".to_vec())
});

clause!(c5, f, b, {
    fine("a create", put_as(b, "n", b"one", when(Cond::IfNoneMatch)).await)?;
    same("a second create", put_as(b, "n", b"two", when(Cond::IfNoneMatch)).await.map(|m| m.size), Err(Refusal::PreconditionFailed("n".into())))?;
    same("the object", get(b, "n").await?, b"one".to_vec())
});

clause!(c6, f, b, {
    let first = put(b, "m", b"one").await?;
    fine("a replace at its etag", put_as(b, "m", b"two", if_match(&first.etag)).await)?;
    same("a replace at a stale etag", put_as(b, "m", b"three", if_match(&first.etag)).await.map(|m| m.size), Err(Refusal::PreconditionFailed("m".into())))?;
    same("a replace of nothing", put_as(b, "none", b"x", if_match(&first.etag)).await.map(|m| m.size), Err(Refusal::PreconditionFailed("none".into())))?;
    same("the object", get(b, "m").await?, b"two".to_vec())
});

clause!(c7, f, b, {
    put(b, "d", b"x").await?;
    fine("delete", b.delete("d").await)?;
    same("after the delete", b.head("d").await, Err(Refusal::NotFound("d".into())))?;
    fine("a delete of nothing", b.delete("d").await)
});

clause!(c8, f, b, {
    for k in ["p/b", "p/a", "q/a", "p/c", "p/d"] {
        put(b, k, b"v").await?;
    }
    let first = fine("list", b.list("p/", None, 3).await)?;
    same("the first page", (keys(&first), first.truncated), (owned(&["p/a", "p/b", "p/c"]), true))?;
    let rest = fine("list on", b.list("p/", Some("p/c"), 3).await)?;
    same("the rest", (keys(&rest), rest.truncated), (owned(&["p/d"]), false))?;
    invalid("a listing of 0", b.list("", None, 0).await)
});

clause!(c9, f, b, {
    fine("a put with its checksum", put_as(b, "s", b"data", checked(sha256_hex(b"data"))).await)?;
    let e = refused("a put with a wrong checksum", put_as(b, "t", b"data", checked(sha256_hex(b"other"))).await)?;
    ensure(matches!(e, Refusal::BadChecksum { .. }), || format!("refused as {e:?}"))?;
    same("what the refused put stored", b.head("t").await, Err(Refusal::NotFound("t".into())))
});

clause!(c10, f, b, {
    let min = f.part_min();
    let up = fine("start", b.start_upload("big").await)?;
    let (one, two, three) = (vec![b'a'; min], vec![b'b'; min], b"end".to_vec());
    // Uploaded out of order: joined in part order.
    let e3 = fine("part 3", b.upload_part(&up, 3, three.clone()).await)?;
    let e1 = fine("part 1", b.upload_part(&up, 1, one.clone()).await)?;
    let e2 = fine("part 2", b.upload_part(&up, 2, two.clone()).await)?;
    let meta = fine("complete", b.complete_upload(&up, vec![(1, e1), (2, e2), (3, e3)]).await)?;
    let whole: Vec<u8> = [one, two, three].concat();
    same("its size and etag", (meta.size, meta.etag), (len64(whole.len()), sha256_hex(&whole)))?;
    same("the object", get(b, "big").await?, whole)
});

clause!(c11, f, b, {
    let min = f.part_min();
    let up = fine("start", b.start_upload("x").await)?;
    let small = fine("a small first part", b.upload_part(&up, 1, b"tiny".to_vec()).await)?;
    let last = fine("the last part", b.upload_part(&up, 2, vec![b'z'; min]).await)?;
    bad_parts("a part under the minimum, not last", b.complete_upload(&up, vec![(1, small.clone()), (2, last.clone())]).await)?;
    bad_parts("parts named out of order", b.complete_upload(&up, vec![(2, last), (1, small)]).await)?;
    invalid("part 0", b.upload_part(&up, 0, b"x".to_vec()).await)?;
    invalid("a part past 10,000", b.upload_part(&up, PARTS_MAX.saturating_add(1), b"x".to_vec()).await)?;
    same("a part for no upload", b.upload_part("no-such-upload", 1, b"x".to_vec()).await, Err(Refusal::NoSuchUpload("no-such-upload".into())))?;
    same("what the failed completes left", b.head("x").await, Err(Refusal::NotFound("x".into())))?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c12, f, b, {
    let up = fine("start", b.start_upload("gone").await)?;
    let e1 = fine("part 1", b.upload_part(&up, 1, b"abc".to_vec()).await)?;
    fine("abort", b.abort_upload(&up).await)?;
    same("completing it after", b.complete_upload(&up, vec![(1, e1)]).await.map(|m| m.size), Err(Refusal::NoSuchUpload(up.clone())))?;
    same("the object", b.head("gone").await, Err(Refusal::NotFound("gone".into())))
});

clause!(c13, f, b, {
    put(b, "shared", b"for you").await?;
    let url = fine("presign", b.presign("shared", Access::Get, Duration::from_secs(3)).await)?;
    same("a get through it", f.redeem(&url, Method::Get, None).await, Ok(Some(b"for you".to_vec())))?;
    f.pass(Duration::from_secs(4)).await;
    same("a get once it has expired", f.redeem(&url, Method::Get, None).await, Err(Refusal::Expired))?;
    invalid("a URL for more than an hour", b.presign("shared", Access::Get, PRESIGN_MAX.saturating_add(Duration::from_secs(1))).await)?;
    invalid("a URL for no time", b.presign("shared", Access::Get, Duration::ZERO).await)?;
    fine("a URL for an hour", b.presign("shared", Access::Get, PRESIGN_MAX).await).map(|_| ())
});

clause!(c14, f, b, {
    let url = fine("presign", b.presign("dropped", Access::Put { length: 9 }, Duration::from_secs(60)).await)?;
    same("a put through it", f.redeem(&url, Method::Put, Some(b"left here".to_vec())).await, Ok(None))?;
    same("the object", get(b, "dropped").await?, b"left here".to_vec())?;
    // Its last character changed (a presigned URL ends in its signature, S3's as this one's).
    let mut tampered = url.clone();
    let last = tampered.pop();
    tampered.push(if last == Some('0') { '1' } else { '0' });
    // A body of the length declared, so the signature alone refuses it.
    ensure(f.redeem(&tampered, Method::Put, Some(b"tampered!".to_vec())).await.is_err(), || "a tampered URL was taken".into())?;
    same("the object after", get(b, "dropped").await?, b"left here".to_vec())
});

clause!(c15, f, b, {
    invalid("an empty key", put_as(b, "", b"x", Put::default()).await)?;
    invalid("a key of 961 bytes", put_as(b, &ascii_key(961), b"x", Put::default()).await)?;
    put(b, &ascii_key(960), b"x").await?;
    put(b, "k", b"x").await.map(|_| ())
});

// ----------------------------------------------------------------------------------------- keys

clause!(c16, f, b, {
    let key = "ünï/日本/😀.txt";
    put(b, key, b"wide").await?;
    same("its bytes", get(b, key).await?, b"wide".to_vec())?;
    same("its head's key", head(b, key).await?.key, key.to_owned())
});

clause!(c17, f, b, {
    let ks = ["a b", "a+b", "a%20b", "q?x=1&y=2", "h#frag", "sym;,:@$!'()*~[]", "tilde~and-under_score.dot"];
    for (i, k) in ks.iter().enumerate() {
        put(b, k, format!("v{i}").as_bytes()).await?;
    }
    for (i, k) in ks.iter().enumerate() {
        same(&format!("{k:?}"), get(b, k).await?, format!("v{i}").into_bytes())?;
    }
    let mut want: Vec<String> = owned(&ks);
    want.sort();
    same("the listing", listed_keys(b, "").await?, want)
});

clause!(c18, f, b, {
    for (k, v) in [("a b", "space"), ("a+b", "plus"), ("a/b", "slash"), ("a%2Fb", "escaped")] {
        put(b, k, v.as_bytes()).await?;
    }
    for (k, v) in [("a b", "space"), ("a+b", "plus"), ("a/b", "slash"), ("a%2Fb", "escaped")] {
        same(k, get(b, k).await?, v.as_bytes().to_vec())?;
    }
    Ok(())
});

clause!(c19, f, b, {
    put(b, "Case", b"upper").await?;
    put(b, "case", b"lower").await?;
    same("Case", get(b, "Case").await?, b"upper".to_vec())?;
    same("case", get(b, "case").await?, b"lower".to_vec())?;
    not_found("CASE", b.head("CASE").await)
});

clause!(c20, f, b, {
    let (nfc, nfd) = ("caf\u{e9}", "cafe\u{301}");
    put(b, nfc, b"composed").await?;
    put(b, nfd, b"decomposed").await?;
    same("NFC", get(b, nfc).await?, b"composed".to_vec())?;
    same("NFD", get(b, nfd).await?, b"decomposed".to_vec())
});

clause!(c21, f, b, {
    for k in ["nul\0here", "tab\there", "new\nline", "del\u{7f}", "c1\u{85}control"] {
        invalid(&format!("{k:?}"), put_as(b, k, b"x", Put::default()).await)?;
    }
    Ok(())
});

clause!(c22, f, b, {
    invalid("a backslash", put_as(b, "win\\dows", b"x", Put::default()).await)?;
    invalid("a leading backslash", put_as(b, "\\a", b"x", Put::default()).await)
});

clause!(c23, f, b, {
    for k in ["/lead", "trail/", "dou//bled", "/"] {
        invalid(&format!("{k:?}"), put_as(b, k, b"x", Put::default()).await)?;
    }
    Ok(())
});

clause!(c24, f, b, {
    for k in [".", "..", "a/./b", "a/../b", "a/..", "./a"] {
        invalid(&format!("{k:?}"), put_as(b, k, b"x", Put::default()).await)?;
    }
    for k in ["a.b", "..c", "c..", "...", ".hidden/x"] {
        put(b, k, b"x").await?;
    }
    Ok(())
});

clause!(c25, f, b, {
    put(b, &format!("a/{}/z", "s".repeat(254)), b"x").await?;
    put(b, &format!("a/{}", "s".repeat(254)), b"x").await?;
    invalid("a segment of 255 bytes", put_as(b, &format!("a/{}/z", "s".repeat(255)), b"x", Put::default()).await)?;
    invalid("a last segment of 255 bytes", put_as(b, &format!("a/{}", "s".repeat(255)), b"x", Put::default()).await)
});

clause!(c26, f, b, {
    let key = wide_key();
    ensure(key.len() == 960, || format!("the key is {} bytes", key.len()))?;
    put(b, &key, b"wide").await?;
    same("its bytes", get(b, &key).await?, b"wide".to_vec())?;
    invalid("a byte more", put_as(b, &format!("{key}x"), b"x", Put::default()).await)
});

clause!(c27, f, b, {
    invalid("get", b.get("a//b", None).await)?;
    invalid("head", b.head("a//b").await)?;
    invalid("delete", b.delete("a//b").await)?;
    invalid("start_upload", b.start_upload("a//b").await)?;
    invalid("presign", b.presign("a//b", Access::Get, Duration::from_secs(60)).await)
});

clause!(c28, f, b, {
    let _ = put_as(b, "/x", b"x", Put::default()).await;
    let _ = put_as(b, "x/", b"x", Put::default()).await;
    same("the listing", listed_keys(b, "").await?, Vec::<String>::new())
});

clause!(c29, f, b, {
    put(b, "a", b"alpha").await?;
    not_found("a with a leading space", b.head(" a").await)?;
    invalid("a with a trailing space", b.head("a ").await)
});

clause!(c30, f, b, {
    for (k, v) in [("a", "1"), ("a/b", "2"), ("ab", "3"), ("a b", "4")] {
        put(b, k, v.as_bytes()).await?;
    }
    put(b, "a/b", b"changed").await?;
    same("a", get(b, "a").await?, b"1".to_vec())?;
    same("ab", get(b, "ab").await?, b"3".to_vec())?;
    same("a b", get(b, "a b").await?, b"4".to_vec())
});

// ------------------------------------------------------------------------------- puts and gets

clause!(c31, f, b, {
    let meta = put(b, "empty", b"").await?;
    same("its size and etag", (meta.size, meta.etag), (0, sha256_hex(b"")))?;
    same("its bytes", get(b, "empty").await?, Vec::new())
});

clause!(c32, f, b, {
    let mut all: Vec<u8> = (0..=255u8).collect();
    all.extend((0..=255u8).rev());
    put(b, "bytes", &all).await?;
    same("its bytes", get(b, "bytes").await?, all)
});

clause!(c33, f, b, {
    for (i, n) in [1usize, 64 << 10, 1 << 20, (1 << 20) | 1].into_iter().enumerate() {
        let data = noise(n, u64::try_from(i).unwrap_or(0).saturating_add(30));
        let key = format!("size/{n}");
        put(b, &key, &data).await?;
        same(&key, get(b, &key).await?, data)?;
    }
    Ok(())
});

clause!(c34, f, b, {
    let data = noise(9 << 20, 34);
    let meta = put(b, "nine", &data).await?;
    same("its etag", meta.etag, sha256_hex(&data))?;
    same("its bytes", get(b, "nine").await?, data)
});

clause!(c35, f, b, {
    let max = usize::try_from(f.object_max()).unwrap_or(usize::MAX);
    put(b, "max", &vec![7u8; max]).await?;
    too_large("a byte more", put_as(b, "over", &vec![7u8; max.saturating_add(1)], Put::default()).await)?;
    not_found("what it stored", b.head("over").await)
});

clause!(c36, f, b, {
    let meta = put(b, "said", b"what it says").await?;
    same("head", ident(&head(b, "said").await?), ident(&meta))
});

clause!(c37, f, b, {
    for (k, v) in [("e1", &b"one"[..]), ("e2", b""), ("e3", &noise(1000, 37)[..])] {
        let etag = put(b, k, v).await?.etag;
        ensure(etag.len() == 64 && etag.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), || format!("{k}'s etag {etag:?}"))?;
    }
    Ok(())
});

clause!(c38, f, b, {
    let meta = put(b, "g", b"got").await?;
    let (m, _) = fine("get", b.get("g", None).await)?;
    same("its metadata", ident(&m), ident(&meta))?;
    same("head's", ident(&m), ident(&head(b, "g").await?))
});

clause!(c39, f, b, {
    let first = put(b, "same", b"content").await?;
    let again = put(b, "same", b"content").await?;
    same("the etag", again.etag, first.etag)?;
    same("its bytes", get(b, "same").await?, b"content".to_vec())
});

clause!(c40, f, b, {
    put(b, "shrink", &noise(100_000, 40)).await?;
    put(b, "shrink", b"short").await?;
    same("its bytes", get(b, "shrink").await?, b"short".to_vec())?;
    same("its size", head(b, "shrink").await?.size, 5)
});

clause!(c41, f, b, {
    put(b, "emptied", b"something").await?;
    let meta = put(b, "emptied", b"").await?;
    same("its size", meta.size, 0)?;
    same("its bytes", get(b, "emptied").await?, Vec::new())
});

clause!(c42, f, b, {
    let data = noise(300_000, 42);
    put(b, "twin/one", &data).await?;
    put(b, "twin/two", &data).await?;
    fine("delete one", b.delete("twin/one").await)?;
    same("the other", get(b, "twin/two").await?, data)
});

clause!(c43, f, b, {
    let data = noise(300_000, 43);
    put(b, "pair/one", &data).await?;
    put(b, "pair/two", &data).await?;
    put(b, "pair/one", b"replaced").await?;
    same("the other", get(b, "pair/two").await?, data)?;
    same("the replaced", get(b, "pair/one").await?, b"replaced".to_vec())
});

clause!(c44, f, b, {
    let a = put(b, "one-bit/a", &[0b0000_0000]).await?;
    let c = put(b, "one-bit/b", &[0b0000_0001]).await?;
    ensure(a.etag != c.etag, || "content a bit apart has one etag".into())
});

clause!(c45, f, b, {
    let a = put(b, "alike/a", b"identical").await?;
    let c = put(b, "alike/b", b"identical").await?;
    same("their etags", a.etag, c.etag)
});

// ------------------------------------------------------------------------------------ checksums

clause!(c46, f, b, {
    fine("lower case", put_as(b, "lower", b"abc", checked(sha256_hex(b"abc"))).await)?;
    fine("upper case", put_as(b, "upper", b"abc", checked(sha256_hex(b"abc").to_ascii_uppercase())).await)?;
    same("the object", get(b, "upper").await?, b"abc".to_vec())
});

clause!(c47, f, b, {
    for bad in ["", "abc", &"g".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
        invalid(&format!("{bad:?}"), put_as(b, "notsha", b"x", checked(bad.to_owned())).await)?;
    }
    not_found("what they stored", b.head("notsha").await)
});

clause!(c48, f, b, {
    put(b, "kept", b"original").await?;
    let e = refused("a wrong checksum", put_as(b, "kept", b"changed", checked(sha256_hex(b"other"))).await)?;
    ensure(matches!(e, Refusal::BadChecksum { .. }), || format!("refused as {e:?}"))?;
    same("the object", get(b, "kept").await?, b"original".to_vec())
});

clause!(c49, f, b, {
    let meta = fine("an empty put with its checksum", put_as(b, "zero", b"", checked(sha256_hex(b""))).await)?;
    same("its size", meta.size, 0)
});

clause!(c50, f, b, {
    put(b, "both", b"there").await?;
    let wrong = Put { cond: Cond::IfNoneMatch, sha256: Some(sha256_hex(b"not this")) };
    let e = refused("a create with a wrong checksum", put_as(b, "both", b"new", wrong).await)?;
    ensure(matches!(e, Refusal::BadChecksum { .. }), || format!("refused as {e:?}, not BadChecksum"))
});

clause!(c51, f, b, {
    let right = Put { cond: Cond::IfNoneMatch, sha256: Some(sha256_hex(b"fresh")) };
    fine("a create with its checksum", put_as(b, "fresh", b"fresh", right).await)?;
    same("the object", get(b, "fresh").await?, b"fresh".to_vec())
});

clause!(c52, f, b, {
    let base = put(b, "matched", b"v1").await?;
    let right = Put { cond: Cond::IfMatch(base.etag), sha256: Some(sha256_hex(b"v2")) };
    fine("a replace with its checksum", put_as(b, "matched", b"v2", right).await)?;
    same("the object", get(b, "matched").await?, b"v2".to_vec())
});

// ------------------------------------------------------------------------------------ conditions

clause!(c53, f, b, {
    put(b, "stay", b"first").await?;
    let before = head(b, "stay").await?;
    f.pass(f.resolution().saturating_mul(2)).await;
    precondition("a create over it", put_as(b, "stay", b"second", when(Cond::IfNoneMatch)).await)?;
    let after = head(b, "stay").await?;
    same("its etag and modified time", (after.etag, after.modified_ms), (before.etag, before.modified_ms))?;
    same("its bytes", get(b, "stay").await?, b"first".to_vec())
});

clause!(c54, f, b, {
    put(b, "again", b"one").await?;
    fine("delete", b.delete("again").await)?;
    fine("a create after", put_as(b, "again", b"two", when(Cond::IfNoneMatch)).await)?;
    same("the object", get(b, "again").await?, b"two".to_vec())
});

clause!(c55, f, b, {
    let first = put(b, "rewritten", b"same").await?;
    put(b, "rewritten", b"same").await?;
    fine("If-Match the etag of the same content", put_as(b, "rewritten", b"next", if_match(&first.etag)).await)?;
    same("the object", get(b, "rewritten").await?, b"next".to_vec())
});

clause!(c56, f, b, {
    let real = put(b, "guarded", b"v").await?;
    for etag in [String::new(), "x".to_owned(), "0".repeat(64), sha256_hex(b"another")] {
        if etag == real.etag {
            continue;
        }
        precondition(&format!("If-Match {etag:?}"), put_as(b, "guarded", b"w", if_match(&etag)).await)?;
    }
    same("the object", get(b, "guarded").await?, b"v".to_vec())
});

clause!(c57, f, b, {
    let base = put(b, "vanished", b"v").await?;
    fine("delete", b.delete("vanished").await)?;
    precondition("If-Match a deleted object's etag", put_as(b, "vanished", b"w", if_match(&base.etag)).await)?;
    not_found("the object", b.head("vanished").await)
});

// T-T10.1.1: a conditional create racing others of the same key; exactly one succeeds, on every
// store the suite runs on.
clause!(c58, f, b, {
    let attempts = (0..8u8).map(|i| async move { (i, b.put("race", vec![i; 1000], when(Cond::IfNoneMatch)).await) });
    let outcomes = futures::future::join_all(attempts).await;
    let won: Vec<u8> = outcomes.iter().filter(|(_, r)| r.is_ok()).map(|(i, _)| *i).collect();
    let lost = outcomes.iter().filter(|(_, r)| matches!(r, Err(Refusal::PreconditionFailed(_)))).count();
    same("creates taken, and refused PreconditionFailed", (won.len(), lost), (1, 7))?;
    let winner = won.first().copied().unwrap_or_default();
    same("the object", get(b, "race").await?, vec![winner; 1000])
});

clause!(c59, f, b, {
    let base = put(b, "cas", b"base").await?;
    let attempts = (0..8u8).map(|i| {
        let etag = base.etag.clone();
        async move { (i, b.put("cas", vec![i; 100], if_match(&etag)).await) }
    });
    let outcomes = futures::future::join_all(attempts).await;
    let won: Vec<u8> = outcomes.iter().filter(|(_, r)| r.is_ok()).map(|(i, _)| *i).collect();
    let lost = outcomes.iter().filter(|(_, r)| matches!(r, Err(Refusal::PreconditionFailed(_)))).count();
    same("replaces taken, and refused PreconditionFailed", (won.len(), lost), (1, 7))?;
    let winner = won.first().copied().unwrap_or_default();
    same("the object", get(b, "cas").await?, vec![winner; 100])
});

clause!(c60, f, b, {
    let attempts = (0..8u8).map(|i| b.put("free-for-all", vec![i; 50_000], Put::default()));
    for r in futures::future::join_all(attempts).await {
        fine("a put", r)?;
    }
    let got = get(b, "free-for-all").await?;
    ensure(got.len() == 50_000 && got.iter().all(|x| Some(x) == got.first()), || "the object is no one put's whole".into())
});

clause!(c61, f, b, {
    let up = fine("start", b.start_upload("left").await)?;
    fine("a part", b.upload_part(&up, 1, b"partial".to_vec()).await)?;
    fine("abort", b.abort_upload(&up).await)?;
    fine("a create after", put_as(b, "left", b"made", when(Cond::IfNoneMatch)).await)?;
    same("the object", get(b, "left").await?, b"made".to_vec())
});

clause!(c62, f, b, {
    put(b, "deleted-guard", b"v").await?;
    fine("delete", b.delete("deleted-guard").await)?;
    precondition("If-Match anything after a delete", put_as(b, "deleted-guard", b"w", if_match(&sha256_hex(b"v"))).await)
});

// -------------------------------------------------------------------------------------- ranges

clause!(c63, f, b, {
    put(b, "ends", b"abcdef").await?;
    same("bytes 0-0", fine("get", range(b, "ends", 0, Some(0)).await)?, b"a".to_vec())?;
    same("bytes 5-5", fine("get", range(b, "ends", 5, Some(5)).await)?, b"f".to_vec())?;
    same("bytes 5-", fine("get", range(b, "ends", 5, None).await)?, b"f".to_vec())
});

clause!(c64, f, b, {
    let data = noise(70_000, 64);
    put(b, "whole", &data).await?;
    same("bytes 0-69999", fine("get", range(b, "whole", 0, Some(69_999)).await)?, data.clone())?;
    same("bytes 0-", fine("get", range(b, "whole", 0, None).await)?, data)
});

clause!(c65, f, b, {
    let meta = put(b, "said-whole", &noise(5000, 65)).await?;
    let (m, part) = fine("a range", b.get("said-whole", Some(Range { start: 100, end: Some(199) })).await)?;
    same("its metadata", ident(&m), ident(&meta))?;
    same("the range's length", part.len(), 100)
});

clause!(c66, f, b, {
    put(b, "nothing-in", b"").await?;
    bad_range("bytes 0-", range(b, "nothing-in", 0, None).await, 0)?;
    bad_range("bytes 0-0", range(b, "nothing-in", 0, Some(0)).await, 0)
});

clause!(c67, f, b, {
    put(b, "edge", b"0123456789").await?;
    same("bytes 9-9", fine("get", range(b, "edge", 9, Some(9)).await)?, b"9".to_vec())?;
    bad_range("bytes 10-10", range(b, "edge", 10, Some(10)).await, 10)?;
    bad_range("bytes 11-", range(b, "edge", 11, None).await, 10)?;
    bad_range("bytes 1000-2000", range(b, "edge", 1000, Some(2000)).await, 10)
});

clause!(c68, f, b, {
    not_found("a range of nothing", range(b, "absent", 0, Some(10)).await)?;
    not_found("an open range of nothing", range(b, "absent", 5, None).await)
});

clause!(c69, f, b, {
    let data = noise(3 << 20, 69);
    put(b, "spread", &data).await?;
    let size = len64(data.len());
    let mut s = 69u64;
    for i in 0..40u64 {
        s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let start = s.wrapping_shr(16).checked_rem(size).unwrap_or(0);
        let len = s.wrapping_shr(40).checked_rem(300_000).unwrap_or(0);
        let end = start.saturating_add(len).min(size.saturating_sub(1));
        let want = slice(&data, usize::try_from(start).unwrap_or(0), usize::try_from(end).unwrap_or(0).saturating_add(1));
        same(&format!("range {i}: bytes {start}-{end}"), fine("get", range(b, "spread", start, Some(end)).await)?, want)?;
    }
    Ok(())
});

clause!(c70, f, b, {
    let data = noise((4 << 20) | 12_345, 70);
    put(b, "pieces", &data).await?;
    let mut whole = Vec::with_capacity(data.len());
    let mut at = 0u64;
    while at < len64(data.len()) {
        whole.extend(fine("a piece", range(b, "pieces", at, Some(at.saturating_add((1 << 20) - 1))).await)?);
        at = at.saturating_add(1 << 20);
    }
    same("the pieces put together", whole, data)
});

clause!(c71, f, b, {
    let parts = parts_for(f, 71, 1000);
    let whole: Vec<u8> = parts.concat();
    upload(b, "joined", &parts).await?;
    let min = len64(f.part_min());
    for start in [min.saturating_sub(10), min.saturating_mul(2).saturating_sub(10)] {
        let end = start.saturating_add(20);
        let want = slice(&whole, usize::try_from(start).unwrap_or(0), usize::try_from(end).unwrap_or(0).saturating_add(1));
        same(&format!("bytes {start}-{end}"), fine("get", range(b, "joined", start, Some(end)).await)?, want)?;
    }
    Ok(())
});

clause!(c72, f, b, {
    let data = noise(2000, 72);
    put(b, "middle", &data).await?;
    same("bytes 1000-1000", fine("get", range(b, "middle", 1000, Some(1000)).await)?, slice(&data, 1000, 1001))?;
    same("bytes 999-1001", fine("get", range(b, "middle", 999, Some(1001)).await)?, slice(&data, 999, 1002))
});

// ---------------------------------------------------------------------------------------- head

clause!(c73, f, b, {
    put(b, "was", b"here").await?;
    fine("delete", b.delete("was").await)?;
    not_found("head after the delete", b.head("was").await)
});

clause!(c74, f, b, {
    let up = fine("start", b.start_upload("coming").await)?;
    fine("a part", b.upload_part(&up, 1, b"soon".to_vec()).await)?;
    not_found("head while it uploads", b.head("coming").await)?;
    not_found("get while it uploads", b.get("coming", None).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c75, f, b, {
    put(b, "times", b"t").await?;
    let h = head(b, "times").await?;
    let (g, _) = fine("get", b.get("times", None).await)?;
    let l = fine("list", b.list("times", None, 10).await)?;
    let listed = l.objects.first().map(|o| (o.size, o.modified_ms));
    same("get's time", g.modified_ms, h.modified_ms)?;
    same("the listing's size and time", listed, Some((h.size, h.modified_ms)))
});

clause!(c76, f, b, {
    let first = put(b, "later", b"one").await?;
    let first = head(b, &first.key).await?;
    f.pass(f.resolution().saturating_mul(2)).await;
    put(b, "later", b"two").await?;
    let second = head(b, "later").await?;
    ensure(second.modified_ms > first.modified_ms, || format!("written again, its time went from {} to {}", first.modified_ms, second.modified_ms))
});

clause!(c77, f, b, {
    put(b, "a", b"top").await?;
    put(b, "a/b/c", b"deep").await?;
    not_found("a/b, between them", b.head("a/b").await)
});

// -------------------------------------------------------------------------------------- delete

clause!(c78, f, b, {
    for k in ["a", "a/b", "ab"] {
        put(b, k, k.as_bytes()).await?;
    }
    fine("delete a", b.delete("a").await)?;
    same("a/b", get(b, "a/b").await?, b"a/b".to_vec())?;
    same("ab", get(b, "ab").await?, b"ab".to_vec())
});

clause!(c79, f, b, {
    put(b, "reborn", b"old").await?;
    fine("delete", b.delete("reborn").await)?;
    let meta = put(b, "reborn", b"new and longer").await?;
    same("its size and etag", (meta.size, meta.etag), (14, sha256_hex(b"new and longer")))?;
    same("its bytes", get(b, "reborn").await?, b"new and longer".to_vec())
});

clause!(c80, f, b, {
    fine("a delete of nothing", b.delete("never").await)?;
    fine("and again", b.delete("never").await)
});

clause!(c81, f, b, {
    put(b, "listed/one", b"1").await?;
    put(b, "listed/two", b"2").await?;
    fine("delete", b.delete("listed/one").await)?;
    same("the listing", listed_keys(b, "listed/").await?, owned(&["listed/two"]))
});

clause!(c82, f, b, {
    let data = noise(200_000, 82);
    put(b, "shared/a", &data).await?;
    put(b, "shared/b", &data).await?;
    fine("delete a", b.delete("shared/a").await)?;
    put(b, "shared/a", &data).await?;
    same("a, put again", get(b, "shared/a").await?, data.clone())?;
    same("b", get(b, "shared/b").await?, data)
});

// ---------------------------------------------------------------------------------------- list

clause!(c83, f, b, {
    let l = fine("list", b.list("", None, 10).await)?;
    same("an empty store's listing", (keys(&l), l.truncated), (Vec::new(), false))
});

clause!(c84, f, b, {
    for k in ["z", "a", "m/n"] {
        put(b, k, b"v").await?;
    }
    same("every object", listed_keys(b, "").await?, owned(&["a", "m/n", "z"]))
});

clause!(c85, f, b, {
    for k in ["p", "p/x", "pa", "pb", "q"] {
        put(b, k, b"v").await?;
    }
    same("prefix p", listed_keys(b, "p").await?, owned(&["p", "p/x", "pa", "pb"]))
});

clause!(c86, f, b, {
    for k in ["é", "z", "a", "Z", "A", "aa", "a0", "a/b"] {
        put(b, k, b"v").await?;
    }
    same("byte order", listed_keys(b, "").await?, owned(&["A", "Z", "a", "a/b", "a0", "aa", "z", "é"]))
});

clause!(c87, f, b, {
    put(b, "there", b"v").await?;
    let l = fine("list", b.list("nowhere", None, 10).await)?;
    same("a prefix that names nothing", (keys(&l), l.truncated), (Vec::new(), false))
});

clause!(c88, f, b, {
    for k in ["one/a", "one/b"] {
        put(b, k, b"v").await?;
    }
    let l = fine("list", b.list("one/", None, 1).await)?;
    same("a limit of 1", (keys(&l), l.truncated), (owned(&["one/a"]), true))
});

clause!(c89, f, b, {
    for k in ["exact/a", "exact/b", "exact/c"] {
        put(b, k, b"v").await?;
    }
    let l = fine("list", b.list("exact/", None, 3).await)?;
    same("a limit of what's there", (keys(&l), l.truncated), (owned(&["exact/a", "exact/b", "exact/c"]), false))?;
    let more = fine("list", b.list("exact/", None, 10).await)?;
    same("a limit past it", (keys(&more), more.truncated), (owned(&["exact/a", "exact/b", "exact/c"]), false))
});

clause!(c90, f, b, {
    let names: Vec<String> = (0..1001).map(|i| format!("many/{i:04}")).collect();
    for batch in names.chunks(50) {
        for r in futures::future::join_all(batch.iter().map(|k| b.put(k, Vec::new(), Put::default()))).await {
            fine("a put", r)?;
        }
    }
    let l = fine("list", b.list("many/", None, 5000).await)?;
    same("a limit over 1000", (l.objects.len(), l.truncated), (LIST_MAX, true))?;
    same("in all", listed(b, "many/", 5000).await?.len(), 1001)
});

clause!(c91, f, b, {
    for k in ["c/a", "c/b", "c/d"] {
        put(b, k, b"v").await?;
    }
    same("after c/a", keys(&fine("list", b.list("c/", Some("c/a"), 10).await)?), owned(&["c/b", "c/d"]))?;
    same("after c/c, which isn't there", keys(&fine("list", b.list("c/", Some("c/c"), 10).await)?), owned(&["c/d"]))
});

clause!(c92, f, b, {
    for k in ["m/a", "m/b", "a"] {
        put(b, k, b"v").await?;
    }
    same("after a, with prefix m/", keys(&fine("list", b.list("m/", Some("a"), 10).await)?), owned(&["m/a", "m/b"]))
});

clause!(c93, f, b, {
    put(b, "end/a", b"v").await?;
    let l = fine("list", b.list("end/", Some("end/a"), 10).await)?;
    same("after the last", (keys(&l), l.truncated), (Vec::new(), false))?;
    let past = fine("list", b.list("end/", Some("end/zzz"), 10).await)?;
    same("after past the last", (keys(&past), past.truncated), (Vec::new(), false))
});

clause!(c94, f, b, {
    let ks: Vec<String> = (0..7).map(|i| format!("paged/{i}")).collect();
    for k in &ks {
        put(b, k, b"v").await?;
    }
    same("every page", listed(b, "paged/", 2).await?.into_iter().map(|o| o.key).collect::<Vec<_>>(), ks)
});

clause!(c95, f, b, {
    put(b, "sized/a", b"four").await?;
    put(b, "sized/b", &noise(12_345, 95)).await?;
    for o in listed(b, "sized/", 10).await? {
        let h = head(b, &o.key).await?;
        same(&o.key, (o.size, o.modified_ms), (h.size, h.modified_ms))?;
    }
    Ok(())
});

clause!(c96, f, b, {
    put(b, "grow", b"x").await?;
    put(b, "grow", b"xyz").await?;
    same("the listed size", listed(b, "grow", 10).await?.first().map(|o| o.size), Some(3))
});

clause!(c97, f, b, {
    let up = fine("start", b.start_upload("pending/obj").await)?;
    let e = fine("a part", b.upload_part(&up, 1, b"bits".to_vec()).await)?;
    same("while it uploads", listed_keys(b, "pending/").await?, Vec::<String>::new())?;
    fine("complete", b.complete_upload(&up, vec![(1, e)]).await)?;
    same("once completed", listed_keys(b, "pending/").await?, owned(&["pending/obj"]))
});

clause!(c98, f, b, {
    put(b, "zero/len", b"").await?;
    same("the listed size", listed(b, "zero/", 10).await?.first().map(|o| o.size), Some(0))
});

clause!(c99, f, b, {
    let ks = ["ünï/日本/😀.txt", "q?x=1&y=2", "a b", "a+b"];
    for k in ks {
        put(b, k, b"v").await?;
    }
    let mut want = owned(&ks);
    want.sort();
    same("the listed keys", listed_keys(b, "").await?, want)
});

clause!(c100, f, b, {
    let url = fine("presign", b.presign("via/presigned", Access::Put { length: 1 }, Duration::from_secs(60)).await)?;
    fine("a put through it", f.redeem(&url, Method::Put, Some(b"p".to_vec())).await)?;
    upload(b, "via/parts", &[b"q".to_vec()]).await?;
    put(b, "via/put", b"r").await?;
    same("the listing", listed_keys(b, "via/").await?, owned(&["via/parts", "via/presigned", "via/put"]))
});

clause!(c101, f, b, {
    put(b, "refused/there", b"v").await?;
    let _ = put_as(b, "refused/there", b"w", when(Cond::IfNoneMatch)).await;
    let _ = put_as(b, "refused/sum", b"w", checked(sha256_hex(b"other"))).await;
    let _ = put_as(b, "refused/big", &vec![0u8; usize::try_from(f.object_max()).unwrap_or(0).saturating_add(1)], Put::default()).await;
    let _ = put_as(b, "refused/cond", b"w", if_match(&sha256_hex(b"none"))).await;
    same("the listing", listed_keys(b, "refused/").await?, owned(&["refused/there"]))
});

clause!(c102, f, b, {
    put(b, "full", b"v").await?;
    put(b, "full/under", b"v").await?;
    put(b, "fullness", b"v").await?;
    same("prefix full", listed_keys(b, "full").await?, owned(&["full", "full/under", "fullness"]))?;
    same("prefix full/", listed_keys(b, "full/").await?, owned(&["full/under"]))
});

clause!(c103, f, b, {
    for k in ["s/q?", "s/q?x", "s/qx"] {
        put(b, k, b"v").await?;
    }
    same("prefix s/q?", listed_keys(b, "s/q?").await?, owned(&["s/q?", "s/q?x"]))
});

clause!(c104, f, b, {
    for k in ["a/m", "a/n", "b/a"] {
        put(b, k, b"v").await?;
    }
    same("after a/m in a/", keys(&fine("list", b.list("a/", Some("a/m"), 10).await)?), owned(&["a/n"]))
});

clause!(c105, f, b, {
    put(b, "seen/a", b"v").await?;
    let first = fine("list", b.list("seen/", None, 1).await)?;
    put(b, "seen/b", b"v").await?;
    let next = fine("list on", b.list("seen/", first.objects.last().map(|o| o.key.as_str()), 10).await)?;
    same("an object put after the first page, past its cursor", keys(&next), owned(&["seen/b"]))
});

clause!(c106, f, b, {
    let key = ascii_key(960);
    put(b, &key, b"long").await?;
    same("listed", listed_keys(b, "k").await?, vec![key.clone()])?;
    fine("deleted", b.delete(&key).await)?;
    same("listed after", listed_keys(b, "k").await?, Vec::<String>::new())
});

// ----------------------------------------------------------------------------------- multipart

clause!(c107, f, b, {
    let parts = parts_for(f, 107, 777);
    let whole: Vec<u8> = parts.concat();
    let meta = upload(b, "in-order", &parts).await?;
    same("its size and etag", (meta.size, meta.etag), (len64(whole.len()), sha256_hex(&whole)))?;
    same("the object", get(b, "in-order").await?, whole)
});

clause!(c108, f, b, {
    let meta = upload(b, "single", &[b"just one small part".to_vec()]).await?;
    same("its size", meta.size, 19)?;
    same("the object", get(b, "single").await?, b"just one small part".to_vec())
});

clause!(c109, f, b, {
    let parts = parts_for(f, 109, 4321);
    let whole: Vec<u8> = parts.concat();
    let in_parts = upload(b, "two-ways/parts", &parts).await?;
    let whole_put = put(b, "two-ways/whole", &whole).await?;
    same("the etags", in_parts.etag, whole_put.etag)
});

clause!(c110, f, b, {
    let min = f.part_min();
    let up = fine("start", b.start_upload("redone").await)?;
    let old = fine("part 1", b.upload_part(&up, 1, noise(min, 110)).await)?;
    let new = fine("part 1 again", b.upload_part(&up, 1, noise(min, 111)).await)?;
    let last = fine("part 2", b.upload_part(&up, 2, b"tail".to_vec()).await)?;
    bad_parts("completing with the old etag", b.complete_upload(&up, vec![(1, old), (2, last.clone())]).await)?;
    fine("completing with the new", b.complete_upload(&up, vec![(1, new), (2, last)]).await)?;
    same("the object", get(b, "redone").await?, [noise(min, 111), b"tail".to_vec()].concat())
});

clause!(c111, f, b, {
    let up = fine("start", b.start_upload("twice").await)?;
    let e = fine("part 1", b.upload_part(&up, 1, b"x".to_vec()).await)?;
    bad_parts("a part named twice", b.complete_upload(&up, vec![(1, e.clone()), (1, e)]).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c112, f, b, {
    let up = fine("start", b.start_upload("none-named").await)?;
    fine("part 1", b.upload_part(&up, 1, b"x".to_vec()).await)?;
    bad_parts("no parts named", b.complete_upload(&up, Vec::new()).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c113, f, b, {
    let up = fine("start", b.start_upload("missing-part").await)?;
    let e = fine("part 1", b.upload_part(&up, 1, b"x".to_vec()).await)?;
    bad_parts("a part never uploaded", b.complete_upload(&up, vec![(1, e.clone()), (2, e)]).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c114, f, b, {
    let up = fine("start", b.start_upload("wrong-etag").await)?;
    fine("part 1", b.upload_part(&up, 1, b"x".to_vec()).await)?;
    bad_parts("a wrong etag", b.complete_upload(&up, vec![(1, sha256_hex(b"something else"))]).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c115, f, b, {
    let up = fine("start", b.start_upload("numbers").await)?;
    fine("part 1", b.upload_part(&up, 1, b"first".to_vec()).await)?;
    fine("part 10,000", b.upload_part(&up, PARTS_MAX, b"last".to_vec()).await)?;
    invalid("part 0", b.upload_part(&up, 0, b"x".to_vec()).await)?;
    invalid("part 10,001", b.upload_part(&up, PARTS_MAX.saturating_add(1), b"x".to_vec()).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c116, f, b, {
    no_upload("complete", b.complete_upload("no-such", vec![(1, sha256_hex(b"x"))]).await)?;
    no_upload("abort", b.abort_upload("no-such").await)
});

clause!(c117, f, b, {
    let up = fine("start", b.start_upload("done").await)?;
    let e = fine("part 1", b.upload_part(&up, 1, b"x".to_vec()).await)?;
    fine("complete", b.complete_upload(&up, vec![(1, e.clone())]).await)?;
    no_upload("completing it again", b.complete_upload(&up, vec![(1, e)]).await)?;
    no_upload("aborting it", b.abort_upload(&up).await)?;
    no_upload("a part for it", b.upload_part(&up, 2, b"y".to_vec()).await)
});

clause!(c118, f, b, {
    let up = fine("start", b.start_upload("abandoned").await)?;
    fine("abort", b.abort_upload(&up).await)?;
    no_upload("a part after the abort", b.upload_part(&up, 1, b"x".to_vec()).await)
});

clause!(c119, f, b, {
    put(b, "replaced-by-parts", b"before").await?;
    upload(b, "replaced-by-parts", &[b"after".to_vec()]).await?;
    same("the object", get(b, "replaced-by-parts").await?, b"after".to_vec())
});

clause!(c120, f, b, {
    put(b, "kept-on-abort", b"stays").await?;
    let up = fine("start", b.start_upload("kept-on-abort").await)?;
    fine("a part", b.upload_part(&up, 1, b"never".to_vec()).await)?;
    fine("abort", b.abort_upload(&up).await)?;
    same("the object", get(b, "kept-on-abort").await?, b"stays".to_vec())
});

clause!(c121, f, b, {
    let one = fine("start one", b.start_upload("contested").await)?;
    let two = fine("start two", b.start_upload("contested").await)?;
    let e1 = fine("one's part", b.upload_part(&one, 1, b"from one".to_vec()).await)?;
    let e2 = fine("two's part", b.upload_part(&two, 1, b"from two".to_vec()).await)?;
    fine("complete two", b.complete_upload(&two, vec![(1, e2)]).await)?;
    same("after two", get(b, "contested").await?, b"from two".to_vec())?;
    fine("complete one", b.complete_upload(&one, vec![(1, e1)]).await)?;
    same("after one, completed last", get(b, "contested").await?, b"from one".to_vec())
});

clause!(c122, f, b, {
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(fine("start", b.start_upload("ids").await)?);
    }
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    same("distinct ids", unique.len(), ids.len())?;
    for id in &ids {
        fine("abort", b.abort_upload(id).await)?;
    }
    Ok(())
});

clause!(c123, f, b, {
    let up = fine("start", b.start_upload("big-part").await)?;
    let over = usize::try_from(f.object_max()).unwrap_or(0).saturating_add(1);
    too_large("a part over the object limit", b.upload_part(&up, 1, vec![1u8; over]).await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c124, f, b, {
    let max = f.object_max();
    // Five parts of a quarter of the limit each (at least the minimum): more than it in all.
    let part = usize::try_from(max.checked_div(4).unwrap_or(0)).unwrap_or(0).max(f.part_min()).max(1);
    ensure(len64(part).saturating_mul(5) > max, || "five parts don't pass the limit".into())?;
    let up = fine("start", b.start_upload("too-much").await)?;
    let mut named = Vec::new();
    for n in 1..=5u32 {
        named.push((n, fine("a part", b.upload_part(&up, n, vec![2u8; part]).await)?));
    }
    too_large("parts summing past the limit", b.complete_upload(&up, named).await)?;
    not_found("what it stored", b.head("too-much").await)?;
    fine("abort", b.abort_upload(&up).await)
});

clause!(c125, f, b, {
    let min = f.part_min();
    let parts = vec![noise(min, 125), noise(min, 126), vec![9u8]];
    let meta = upload(b, "at-minimum", &parts).await?;
    same("its size", meta.size, len64(min).saturating_mul(2).saturating_add(1))
});

clause!(c126, f, b, {
    let parts = parts_for(f, 127, 10);
    let whole: Vec<u8> = parts.concat();
    let up = fine("start", b.start_upload("at-once").await)?;
    let sends = parts.iter().enumerate().map(|(i, p)| {
        let (up, p) = (up.clone(), p.clone());
        async move {
            let n = u32::try_from(i).unwrap_or(0).saturating_add(1);
            (n, b.upload_part(&up, n, p).await)
        }
    });
    let mut named = Vec::new();
    for (n, r) in futures::future::join_all(sends).await {
        named.push((n, fine(&format!("part {n}"), r)?));
    }
    fine("complete", b.complete_upload(&up, named).await)?;
    same("the object", get(b, "at-once").await?, whole)
});

clause!(c127, f, b, {
    let up = fine("start", b.start_upload("outlives").await)?;
    let e = fine("a part", b.upload_part(&up, 1, b"survivor".to_vec()).await)?;
    put(b, "outlives", b"meanwhile").await?;
    fine("delete", b.delete("outlives").await)?;
    fine("complete", b.complete_upload(&up, vec![(1, e)]).await)?;
    same("the object", get(b, "outlives").await?, b"survivor".to_vec())
});

clause!(c128, f, b, {
    let up = fine("start", b.start_upload("racing-put").await)?;
    let e = fine("a part", b.upload_part(&up, 1, b"upload".to_vec()).await)?;
    put(b, "racing-put", b"a put while it uploads").await?;
    fine("complete", b.complete_upload(&up, vec![(1, e)]).await)?;
    same("the object", get(b, "racing-put").await?, b"upload".to_vec())
});

clause!(c129, f, b, {
    let min = f.part_min();
    let up = fine("start", b.start_upload("many-times").await)?;
    let mut last = String::new();
    for i in 0..3u64 {
        last = fine("part 1, again", b.upload_part(&up, 1, noise(min, 129u64.wrapping_add(i))).await)?;
    }
    let tail = fine("part 2", b.upload_part(&up, 2, b"!".to_vec()).await)?;
    fine("complete", b.complete_upload(&up, vec![(1, last), (2, tail)]).await)?;
    same("the object", get(b, "many-times").await?, [noise(min, 131), b"!".to_vec()].concat())
});

clause!(c130, f, b, {
    let up = fine("start", b.start_upload("aborted/after-parts").await)?;
    fine("a part", b.upload_part(&up, 1, b"one".to_vec()).await)?;
    fine("another", b.upload_part(&up, 2, b"two".to_vec()).await)?;
    fine("abort", b.abort_upload(&up).await)?;
    same("the listing", listed_keys(b, "aborted/").await?, Vec::<String>::new())
});

clause!(c131, f, b, {
    let meta = upload(b, "parts/timed", &[b"t".to_vec()]).await?;
    let h = head(b, "parts/timed").await?;
    same("head", ident(&h), ident(&meta))?;
    same("the listing's time", listed(b, "parts/", 10).await?.first().map(|o| o.modified_ms), Some(h.modified_ms))
});

clause!(c132, f, b, {
    let up = fine("start", b.start_upload("etag-per-content").await)?;
    let a = fine("part 1", b.upload_part(&up, 1, b"aaaa".to_vec()).await)?;
    let c = fine("part 2", b.upload_part(&up, 2, b"bbbb".to_vec()).await)?;
    ensure(!a.is_empty() && a != c, || format!("parts of different content: etags {a:?} and {c:?}"))?;
    fine("abort", b.abort_upload(&up).await)
});

// ------------------------------------------------------------------------------------- presign

clause!(c133, f, b, {
    let url = fine("presign", b.presign("not-yet", Access::Get, Duration::from_secs(60)).await)?;
    not_found("a get of nothing", f.redeem(&url, Method::Get, None).await)
});

clause!(c134, f, b, {
    put(b, "changing", b"before").await?;
    let url = fine("presign", b.presign("changing", Access::Get, Duration::from_secs(60)).await)?;
    put(b, "changing", b"after").await?;
    same("a get through it", f.redeem(&url, Method::Get, None).await, Ok(Some(b"after".to_vec())))
});

clause!(c135, f, b, {
    put(b, "over-it", b"old").await?;
    let url = fine("presign", b.presign("over-it", Access::Put { length: 3 }, Duration::from_secs(60)).await)?;
    fine("a put through it", f.redeem(&url, Method::Put, Some(b"new".to_vec())).await)?;
    same("the object", get(b, "over-it").await?, b"new".to_vec())
});

clause!(c136, f, b, {
    let body = noise(10_000, 136);
    let url = fine("presign", b.presign("hashed", Access::Put { length: 10000 }, Duration::from_secs(60)).await)?;
    fine("a put through it", f.redeem(&url, Method::Put, Some(body.clone())).await)?;
    let h = head(b, "hashed").await?;
    same("its size and etag", (h.size, h.etag), (10_000, sha256_hex(&body)))
});

clause!(c137, f, b, {
    let url = fine("presign", b.presign("late", Access::Put { length: 1 }, Duration::from_secs(3)).await)?;
    f.pass(Duration::from_secs(4)).await;
    same("a put once it has expired", f.redeem(&url, Method::Put, Some(b"x".to_vec())).await, Err(Refusal::Expired))?;
    not_found("what it stored", b.head("late").await)
});

clause!(c138, f, b, {
    put(b, "either-way", b"v").await?;
    let get_url = fine("presign a get", b.presign("either-way", Access::Get, Duration::from_secs(60)).await)?;
    let put_url = fine("presign a put", b.presign("either-way", Access::Put { length: 1 }, Duration::from_secs(60)).await)?;
    ensure(f.redeem(&get_url, Method::Put, Some(b"w".to_vec())).await.is_err(), || "a get's URL put".into())?;
    ensure(f.redeem(&put_url, Method::Get, None).await.is_err(), || "a put's URL got".into())?;
    same("the object", get(b, "either-way").await?, b"v".to_vec())
});

clause!(c139, f, b, {
    put(b, "k-one", b"one's").await?;
    put(b, "k-two", b"two's").await?;
    let url = fine("presign", b.presign("k-one", Access::Get, Duration::from_secs(60)).await)?;
    // The key as the URL carries it: every store's URL names it once, as it is.
    let other = url.replacen("k-one", "k-two", 1);
    ensure(other != url, || format!("the key isn't in the URL {url:?} as k-one"))?;
    let got = f.redeem(&other, Method::Get, None).await;
    ensure(got.is_err(), || format!("a URL changed to name k-two gave {got:?}"))
});

clause!(c140, f, b, {
    fine("presign", b.presign("unmade", Access::Put { length: 0 }, Duration::from_secs(60)).await)?;
    not_found("the object", b.head("unmade").await)
});

clause!(c141, f, b, {
    put(b, "removed", b"v").await?;
    let url = fine("presign", b.presign("removed", Access::Get, Duration::from_secs(60)).await)?;
    fine("delete", b.delete("removed").await)?;
    not_found("a get through it", f.redeem(&url, Method::Get, None).await)
});

clause!(c142, f, b, {
    let url = fine("presign", b.presign("void", Access::Put { length: 0 }, Duration::from_secs(60)).await)?;
    fine("an empty put through it", f.redeem(&url, Method::Put, Some(Vec::new())).await)?;
    same("its size", head(b, "void").await?.size, 0)
});

clause!(c143, f, b, {
    let key = "ünï/日本/😀.txt";
    put(b, key, b"far").await?;
    let url = fine("presign", b.presign(key, Access::Get, Duration::from_secs(60)).await)?;
    same("a get through it", f.redeem(&url, Method::Get, None).await, Ok(Some(b"far".to_vec())))
});

clause!(c144, f, b, {
    let key = "q?x=1&y=2 #frag+plus";
    put(b, key, b"reserved").await?;
    let url = fine("presign", b.presign(key, Access::Get, Duration::from_secs(60)).await)?;
    same("a get through it", f.redeem(&url, Method::Get, None).await, Ok(Some(b"reserved".to_vec())))
});

clause!(c145, f, b, {
    let data = noise(2 << 20, 145);
    put(b, "big-share", &data).await?;
    let url = fine("presign", b.presign("big-share", Access::Get, Duration::from_secs(60)).await)?;
    same("a get through it", f.redeem(&url, Method::Get, None).await, Ok(Some(data)))
});

// --------------------------------------------------------------------------------- concurrency

clause!(c146, f, b, {
    let puts = (0..16u8).map(|i| async move { (i, b.put(&format!("many-at-once/{i}"), vec![i; 2000], Put::default()).await) });
    for (i, r) in futures::future::join_all(puts).await {
        fine(&format!("put {i}"), r)?;
    }
    for i in 0..16u8 {
        same(&format!("object {i}"), get(b, &format!("many-at-once/{i}")).await?, vec![i; 2000])?;
    }
    Ok(())
});

clause!(c147, f, b, {
    let (old, new) = (vec![1u8; 200_000], vec![2u8; 200_000]);
    put(b, "flip", &old).await?;
    let reads = (0..6).map(|_| b.get("flip", None));
    let write = b.put("flip", new.clone(), Put::default());
    let (reads, written) = futures::future::join(futures::future::join_all(reads), write).await;
    fine("the replacement", written)?;
    for r in reads {
        let (_, got) = fine("a get", r)?;
        ensure(got == old || got == new, || "a get gave neither version whole".into())?;
    }
    same("after", get(b, "flip").await?, new)
});

clause!(c148, f, b, {
    let ks: Vec<String> = (0..10).map(|i| format!("deleting/{i}")).collect();
    for k in &ks {
        put(b, k, b"v").await?;
    }
    for r in futures::future::join_all(ks.iter().map(|k| b.delete(k))).await {
        fine("a delete", r)?;
    }
    same("the listing", listed_keys(b, "deleting/").await?, Vec::<String>::new())
});

clause!(c149, f, b, {
    let base = put(b, "guarded-race", b"base").await?;
    let tries = (0..4u8).map(|i| {
        let etag = base.etag.clone();
        async move {
            if i.is_multiple_of(2) { b.put("guarded-race", vec![i; 10], if_match(&etag)).await } else { b.put("guarded-race", vec![i; 10], when(Cond::IfNoneMatch)).await }
        }
    });
    let taken = futures::future::join_all(tries).await.into_iter().filter(Result::is_ok).count();
    same("writes taken: one If-Match, no create", taken, 1)
});

clause!(c150, f, b, {
    let ks: Vec<String> = (0..12).map(|i| format!("starts/{i}")).collect();
    let starts = ks.iter().map(|k| b.start_upload(k));
    let ids: Vec<String> = futures::future::join_all(starts).await.into_iter().map(|r| fine("start", r)).collect::<Result<_, _>>()?;
    for (id, k) in ids.iter().zip(&ks) {
        let e = fine("a part", b.upload_part(id, 1, k.as_bytes().to_vec()).await)?;
        fine("complete", b.complete_upload(id, vec![(1, e)]).await)?;
    }
    for k in &ks {
        same(k, get(b, k).await?, k.as_bytes().to_vec())?;
    }
    Ok(())
});

// ------------------------------------------------------------------------------- what's left

clause!(c151, f, b, {
    let meta = put(b, "keyed", b"k").await?;
    same("the key a put answers", meta.key, "keyed".to_owned())?;
    same("the key head answers", head(b, "keyed").await?.key, "keyed".to_owned())
});

clause!(c152, f, b, {
    let parts = parts_for(f, 152, 3);
    let whole = parts.concat();
    upload(b, "read-parts", &parts).await?;
    let (m, bytes) = fine("get", b.get("read-parts", None).await)?;
    same("its metadata", (m.size, m.etag), (len64(whole.len()), sha256_hex(&whole)))?;
    same("its bytes", bytes, whole)
});

clause!(c153, f, b, {
    let url = fine("presign", b.presign("ranged-share", Access::Put { length: 100000 }, Duration::from_secs(60)).await)?;
    let body = noise(100_000, 153);
    fine("a put through it", f.redeem(&url, Method::Put, Some(body.clone())).await)?;
    same("bytes 500-599", fine("get", range(b, "ranged-share", 500, Some(599)).await)?, slice(&body, 500, 600))
});

clause!(c154, f, b, {
    let data = noise(1 << 20, 154);
    put(b, "overwritten-big", &data).await?;
    let smaller = noise(1000, 155);
    put(b, "overwritten-big", &smaller).await?;
    bad_range("a range past the new end", range(b, "overwritten-big", 5000, None).await, 1000)?;
    same("the new bytes", get(b, "overwritten-big").await?, smaller)
});

clause!(c155, f, b, {
    let up = fine("start", b.start_upload("odd/parts").await)?;
    // A byte each: under any store's minimum.
    let e2 = fine("part 2", b.upload_part(&up, 2, b"2".to_vec()).await)?;
    let e7 = fine("part 7", b.upload_part(&up, 7, b"7".to_vec()).await)?;
    let e5 = fine("part 5", b.upload_part(&up, 5, b"5".to_vec()).await)?;
    bad_parts("small parts that aren't last", b.complete_upload(&up, vec![(2, e2.clone()), (5, e5), (7, e7)]).await)?;
    fine("only part 2", b.complete_upload(&up, vec![(2, e2)]).await)?;
    same("the object: the parts named, no others", get(b, "odd/parts").await?, b"2".to_vec())
});

clause!(c156, f, b, {
    let min = f.part_min();
    let up = fine("start", b.start_upload("gaps").await)?;
    let e1 = fine("part 1", b.upload_part(&up, 1, noise(min, 156)).await)?;
    let e9 = fine("part 9", b.upload_part(&up, 9, b"nine".to_vec()).await)?;
    fine("complete 1 and 9", b.complete_upload(&up, vec![(1, e1), (9, e9)]).await)?;
    same("the object", get(b, "gaps").await?, [noise(min, 156), b"nine".to_vec()].concat())
});

clause!(c157, f, b, {
    let up = fine("start", b.start_upload("unnamed/rest").await)?;
    let e1 = fine("part 1", b.upload_part(&up, 1, b"used".to_vec()).await)?;
    fine("part 2, never named", b.upload_part(&up, 2, b"unused".to_vec()).await)?;
    fine("complete part 1", b.complete_upload(&up, vec![(1, e1)]).await)?;
    same("the object", get(b, "unnamed/rest").await?, b"used".to_vec())?;
    no_upload("the upload after", b.abort_upload(&up).await)
});

clause!(c158, f, b, {
    let first = put(b, "chain", b"1").await?;
    let second = fine("If-Match the first", put_as(b, "chain", b"2", if_match(&first.etag)).await)?;
    let third = fine("If-Match the second", put_as(b, "chain", b"3", if_match(&second.etag)).await)?;
    precondition("If-Match the first again", put_as(b, "chain", b"4", if_match(&first.etag)).await)?;
    same("the object's etag", head(b, "chain").await?.etag, third.etag)
});

clause!(c159, f, b, {
    let a = put(b, "same-size/a", b"aaaa").await?;
    put(b, "same-size/a", b"bbbb").await?;
    precondition("If-Match the etag of what was there, same size", put_as(b, "same-size/a", b"cccc", if_match(&a.etag)).await)?;
    same("the object", get(b, "same-size/a").await?, b"bbbb".to_vec())
});

clause!(c160, f, b, {
    let ks = ["mixed/a", "mixed/b", "mixed/c"];
    put(b, "mixed/a", b"put").await?;
    upload(b, "mixed/b", &[b"parts".to_vec()]).await?;
    let url = fine("presign", b.presign("mixed/c", Access::Put { length: 9 }, Duration::from_secs(60)).await)?;
    fine("a put through it", f.redeem(&url, Method::Put, Some(b"presigned".to_vec())).await)?;
    for (k, v) in ks.iter().zip([&b"put"[..], b"parts", b"presigned"]) {
        let h = head(b, k).await?;
        same(&format!("{k}'s etag"), h.etag, sha256_hex(v))?;
    }
    Ok(())
});

clause!(c161, f, b, {
    let url = fine("presign", b.presign("declared", Access::Put { length: 5 }, Duration::from_secs(60)).await)?;
    ensure(f.redeem(&url, Method::Put, Some(b"123456".to_vec())).await.is_err(), || "a body longer than its URL declares was taken".into())?;
    ensure(f.redeem(&url, Method::Put, Some(b"1234".to_vec())).await.is_err(), || "a body shorter than its URL declares was taken".into())?;
    not_found("what they stored", b.head("declared").await)?;
    same("a body of the length declared", f.redeem(&url, Method::Put, Some(b"12345".to_vec())).await, Ok(None))?;
    same("the object", get(b, "declared").await?, b"12345".to_vec())
});

clause!(c162, f, b, {
    for k in ["sp", "sp q", "spa"] {
        put(b, k, k.as_bytes()).await?;
    }
    same("the keys a prefix ending in a space lists", listed_keys(b, "sp ").await?, vec!["sp q".to_owned()])?;
    same("the keys its first two bytes list", listed_keys(b, "sp").await?, vec!["sp".to_owned(), "sp q".to_owned(), "spa".to_owned()])
});

clause!(c163, f, b, {
    for k in ["a ", "a /b", "a/b ", " "] {
        invalid(&format!("{k:?}"), put_as(b, k, b"x", Put::default()).await)?;
    }
    for k in ["a", "a b/c", " a/b c"] {
        put(b, k, b"x").await?;
    }
    same("the listing", listed_keys(b, "").await?, vec![" a/b c".to_owned(), "a".to_owned(), "a b/c".to_owned()])
});

/// The `Blobs` contract, clause by clause (docs/seams.md, "Blobs").
pub fn cases<F: BlobsFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "BLOBS-1", says: "an object put is got back whole, with its size and its content's SHA-256 as its etag", check: c1::<F> },
        Case { id: "BLOBS-2", says: "a get with a byte range gives those bytes (an end past the object, to its end); a range outside it is refused InvalidRange", check: c2::<F> },
        Case { id: "BLOBS-3", says: "head gives an object's metadata; head or get of a key with no object is refused NotFound", check: c3::<F> },
        Case { id: "BLOBS-4", says: "a put over an object replaces it, and new content has a new etag", check: c4::<F> },
        Case { id: "BLOBS-5", says: "a put If-None-Match creates only where there's no object; else it's refused PreconditionFailed and stores nothing", check: c5::<F> },
        Case { id: "BLOBS-6", says: "a put If-Match replaces only the object with that etag; else it's refused PreconditionFailed", check: c6::<F> },
        Case { id: "BLOBS-7", says: "a deleted object is gone; deleting a key with no object succeeds", check: c7::<F> },
        Case { id: "BLOBS-8", says: "a listing gives a prefix's objects in key order, after its cursor, at most its limit, saying when it's truncated; a limit of 0 is refused", check: c8::<F> },
        Case { id: "BLOBS-9", says: "a put with a SHA-256 its content doesn't have is refused BadChecksum and stores nothing", check: c9::<F> },
        Case { id: "BLOBS-10", says: "a multipart upload's parts, uploaded in any order, are joined in part order into one object", check: c10::<F> },
        Case { id: "BLOBS-11", says: "a multipart upload is refused when a part but the last is under the minimum or parts are named out of order; part numbers run 1 to 10,000", check: c11::<F> },
        Case { id: "BLOBS-12", says: "an aborted upload is gone: completing it is refused NoSuchUpload", check: c12::<F> },
        Case { id: "BLOBS-13", says: "a presigned get works until it expires by the store's clock, then is refused Expired; a URL's lifetime is more than 0 and at most an hour", check: c13::<F> },
        Case { id: "BLOBS-14", says: "a presigned put stores what its holder sends; a URL changed after signing is refused", check: c14::<F> },
        Case { id: "BLOBS-15", says: "keys are 1 to 960 bytes: an empty or longer one is refused Invalid", check: c15::<F> },
        Case { id: "BLOBS-16", says: "a key of multibyte UTF-8 is kept as it is: its object is got back by it, and head names it so", check: c16::<F> },
        Case { id: "BLOBS-17", says: "keys with spaces and URL-reserved characters are kept exactly, each its own object, and listed as they were put", check: c17::<F> },
        Case { id: "BLOBS-18", says: "a space, a plus, a slash and an escaped slash make different keys: \"a b\", \"a+b\", \"a/b\" and \"a%2Fb\" are four objects", check: c18::<F> },
        Case { id: "BLOBS-19", says: "keys that differ only in case are different objects", check: c19::<F> },
        Case { id: "BLOBS-20", says: "keys that differ only in Unicode normalization (NFC and NFD) are different objects: keys are their bytes", check: c20::<F> },
        Case { id: "BLOBS-21", says: "a key with a control character (NUL, tab, newline, DEL, a C1 control) is refused Invalid", check: c21::<F> },
        Case { id: "BLOBS-22", says: "a key with a backslash is refused Invalid, so keys are the same on every platform's file system", check: c22::<F> },
        Case { id: "BLOBS-23", says: "a key that starts or ends with '/', or holds an empty segment ('//'), is refused Invalid", check: c23::<F> },
        Case { id: "BLOBS-24", says: "a key with a '.' or '..' segment is refused Invalid; dots within a segment's name ('a.b', '..c', '...') are taken", check: c24::<F> },
        Case { id: "BLOBS-25", says: "a key's segments are 1 to 254 bytes: one of 254 is taken, last or not; one of 255 refused Invalid", check: c25::<F> },
        Case { id: "BLOBS-26", says: "a key's bytes are counted, not its characters: 960 bytes, most of them two-byte characters, are taken, one byte more refused Invalid", check: c26::<F> },
        Case { id: "BLOBS-27", says: "every request that names a key refuses a bad one Invalid: get, head, delete, start_upload and presign as put does", check: c27::<F> },
        Case { id: "BLOBS-28", says: "a key refused stores nothing: nothing is listed after", check: c28::<F> },
        Case { id: "BLOBS-29", says: "keys are exact: \" a\" isn't \"a\", and \"a \" is no key at all (a segment doesn't end in a space)", check: c29::<F> },
        Case { id: "BLOBS-30", says: "objects are apart: a put to one key changes no other, however alike their keys ('a', 'a/b', 'ab', 'a b')", check: c30::<F> },
        Case { id: "BLOBS-31", says: "an empty object is kept: its size is 0, its etag the SHA-256 of nothing, and a get gives no bytes", check: c31::<F> },
        Case { id: "BLOBS-32", says: "every byte value round-trips: 0 to 255 and back down are got back exactly", check: c32::<F> },
        Case { id: "BLOBS-33", says: "objects of 1 byte, 64 KiB, 1 MiB and 1 MiB and a byte are each got back exactly", check: c33::<F> },
        Case { id: "BLOBS-34", says: "an object of 9 MiB of noise is got back exactly, its etag its SHA-256", check: c34::<F> },
        Case { id: "BLOBS-35", says: "an object as large as the store takes is kept; one byte more is refused TooLarge and stores nothing", check: c35::<F> },
        Case { id: "BLOBS-36", says: "a put's answer says the key, size and etag head then says", check: c36::<F> },
        Case { id: "BLOBS-37", says: "an etag is 64 lowercase hex digits", check: c37::<F> },
        Case { id: "BLOBS-38", says: "a get's answer carries the object's key, size and etag, as head says them", check: c38::<F> },
        Case { id: "BLOBS-39", says: "the same content put again keeps its etag, and the object is got back unchanged", check: c39::<F> },
        Case { id: "BLOBS-40", says: "a smaller object put over a larger leaves exactly the new bytes, no tail of the old", check: c40::<F> },
        Case { id: "BLOBS-41", says: "an empty object put over another leaves it empty", check: c41::<F> },
        Case { id: "BLOBS-42", says: "two keys may hold the same content, each its own object: deleting one leaves the other whole", check: c42::<F> },
        Case { id: "BLOBS-43", says: "replacing one of two keys that held the same content leaves the other's as it was", check: c43::<F> },
        Case { id: "BLOBS-44", says: "content a bit apart has etags apart", check: c44::<F> },
        Case { id: "BLOBS-45", says: "the same content under two keys has one etag", check: c45::<F> },
        Case { id: "BLOBS-46", says: "a put with its content's SHA-256 is taken, in lower or upper case hex", check: c46::<F> },
        Case { id: "BLOBS-47", says: "a put whose SHA-256 isn't 64 hex digits is refused Invalid and stores nothing", check: c47::<F> },
        Case { id: "BLOBS-48", says: "a put with a wrong SHA-256 over an object leaves the object as it was", check: c48::<F> },
        Case { id: "BLOBS-49", says: "an empty object put with the SHA-256 of nothing is taken", check: c49::<F> },
        Case { id: "BLOBS-50", says: "a put's checksum is checked before its condition: a wrong one is refused BadChecksum even where If-None-Match fails too", check: c50::<F> },
        Case { id: "BLOBS-51", says: "a create (If-None-Match) with its content's SHA-256 is taken", check: c51::<F> },
        Case { id: "BLOBS-52", says: "a replace (If-Match) with its content's SHA-256 is taken", check: c52::<F> },
        Case { id: "BLOBS-53", says: "a create refused leaves the object as it was: its bytes, its etag and its time", check: c53::<F> },
        Case { id: "BLOBS-54", says: "a key whose object was deleted can be created again with If-None-Match", check: c54::<F> },
        Case { id: "BLOBS-55", says: "If-Match an object's etag holds after the same content is put again", check: c55::<F> },
        Case { id: "BLOBS-56", says: "If-Match an etag the object hasn't (empty, garbage, another's) is refused PreconditionFailed, and the object stays", check: c56::<F> },
        Case { id: "BLOBS-57", says: "If-Match the etag of an object since deleted is refused PreconditionFailed, and nothing is stored", check: c57::<F> },
        Case { id: "BLOBS-58", says: "of 8 creates (If-None-Match) of one key at once, exactly one is taken, the rest refused PreconditionFailed, and the object is the one taken", check: c58::<F> },
        Case { id: "BLOBS-59", says: "of 8 replaces at once, each If-Match the same etag, exactly one is taken, the rest refused PreconditionFailed, and the object is the one taken", check: c59::<F> },
        Case { id: "BLOBS-60", says: "of 8 unconditional puts of one key at once, every one is taken and the object is one of them whole", check: c60::<F> },
        Case { id: "BLOBS-61", says: "a create where an upload was aborted is taken: the upload left nothing", check: c61::<F> },
        Case { id: "BLOBS-62", says: "after a delete, If-Match any etag is refused PreconditionFailed", check: c62::<F> },
        Case { id: "BLOBS-63", says: "bytes 0-0 give the first byte; the last byte's range, closed or open, gives the last", check: c63::<F> },
        Case { id: "BLOBS-64", says: "a range from 0 to the last byte, closed or open, gives the whole object", check: c64::<F> },
        Case { id: "BLOBS-65", says: "a range's answer says the whole object's key, size and etag, not the range's", check: c65::<F> },
        Case { id: "BLOBS-66", says: "any range of an empty object is refused InvalidRange, naming size 0", check: c66::<F> },
        Case { id: "BLOBS-67", says: "a range that starts at or past the end is refused InvalidRange, naming the object's size; one on the last byte gives it", check: c67::<F> },
        Case { id: "BLOBS-68", says: "a range of a key with no object is refused NotFound, not InvalidRange", check: c68::<F> },
        Case { id: "BLOBS-69", says: "40 ranges spread over 3 MiB of noise each give exactly their bytes", check: c69::<F> },
        Case { id: "BLOBS-70", says: "an object read in 1 MiB ranges, put together, is the object", check: c70::<F> },
        Case { id: "BLOBS-71", says: "a range across the joins of a multipart object's parts gives its bytes", check: c71::<F> },
        Case { id: "BLOBS-72", says: "ranges of one byte and of three in an object's middle give exactly those", check: c72::<F> },
        Case { id: "BLOBS-73", says: "head after a delete is refused NotFound", check: c73::<F> },
        Case { id: "BLOBS-74", says: "an object being uploaded in parts isn't there until its upload completes: head and get are refused NotFound", check: c74::<F> },
        Case { id: "BLOBS-75", says: "head, get and list say the same size and modified time of an object", check: c75::<F> },
        Case { id: "BLOBS-76", says: "an object written again later has a later modified time, by the store's clock", check: c76::<F> },
        Case { id: "BLOBS-77", says: "a key between two objects' ('a' and 'a/b/c' put, 'a/b' not) is refused NotFound: keys aren't folders", check: c77::<F> },
        Case { id: "BLOBS-78", says: "deleting a key leaves keys that start with it ('a/b', 'ab') as they were", check: c78::<F> },
        Case { id: "BLOBS-79", says: "a key put again after a delete has the new content, size and etag", check: c79::<F> },
        Case { id: "BLOBS-80", says: "deleting a key with no object succeeds, again and again", check: c80::<F> },
        Case { id: "BLOBS-81", says: "a deleted object leaves the listing at once", check: c81::<F> },
        Case { id: "BLOBS-82", says: "content deleted under one key and put again stays whole under another that holds it too", check: c82::<F> },
        Case { id: "BLOBS-83", says: "an empty store's listing is empty and not truncated", check: c83::<F> },
        Case { id: "BLOBS-84", says: "a listing of the prefix \"\" gives every object", check: c84::<F> },
        Case { id: "BLOBS-85", says: "a prefix is a string, not a folder: \"p\" lists \"p\", \"p/x\", \"pa\" and \"pb\", not \"q\"", check: c85::<F> },
        Case { id: "BLOBS-86", says: "keys list in the order of their UTF-8 bytes: A, Z, a, a/b, a0, aa, z, é", check: c86::<F> },
        Case { id: "BLOBS-87", says: "a prefix that names nothing lists nothing, and isn't truncated", check: c87::<F> },
        Case { id: "BLOBS-88", says: "a listing's limit of 1 gives one object, truncated when more follow", check: c88::<F> },
        Case { id: "BLOBS-89", says: "a listing whose limit is what's there, or more, gives it all and isn't truncated", check: c89::<F> },
        Case { id: "BLOBS-90", says: "a limit over 1000 gives 1000 and says the rest follow; paged, 1001 objects are each listed", check: c90::<F> },
        Case { id: "BLOBS-91", says: "a listing after a key starts past it, whether or not the key is there", check: c91::<F> },
        Case { id: "BLOBS-92", says: "a listing after a key before its prefix starts at the prefix", check: c92::<F> },
        Case { id: "BLOBS-93", says: "a listing after the last key, or past it, is empty and not truncated", check: c93::<F> },
        Case { id: "BLOBS-94", says: "paging with a limit of 2, each page after the one before's last key, visits every object once, in order", check: c94::<F> },
        Case { id: "BLOBS-95", says: "a listing says each object's size and modified time as head says them", check: c95::<F> },
        Case { id: "BLOBS-96", says: "a listing after an overwrite says the new size", check: c96::<F> },
        Case { id: "BLOBS-97", says: "an upload under way isn't listed; once completed, its object is", check: c97::<F> },
        Case { id: "BLOBS-98", says: "an empty object is listed, with size 0", check: c98::<F> },
        Case { id: "BLOBS-99", says: "multibyte and reserved keys are listed as they were put", check: c99::<F> },
        Case { id: "BLOBS-100", says: "objects stored by a presigned put and by an upload are listed like any other", check: c100::<F> },
        Case { id: "BLOBS-101", says: "a put refused for its condition, checksum or size lists nothing new", check: c101::<F> },
        Case { id: "BLOBS-102", says: "a prefix that is a whole key lists that object and those that go on from it", check: c102::<F> },
        Case { id: "BLOBS-103", says: "a prefix with reserved characters (\"s/q?\") matches them exactly", check: c103::<F> },
        Case { id: "BLOBS-104", says: "a cursor within a prefix lists only the prefix's later keys", check: c104::<F> },
        Case { id: "BLOBS-105", says: "an object put after a page was read is on a later page when its key is past the cursor", check: c105::<F> },
        Case { id: "BLOBS-106", says: "a key of 960 bytes is listed and deleted like any other", check: c106::<F> },
        Case { id: "BLOBS-107", says: "parts uploaded in order and completed are one object, its etag the SHA-256 of the whole", check: c107::<F> },
        Case { id: "BLOBS-108", says: "an upload of one part smaller than the minimum is one object: the last part may be any size", check: c108::<F> },
        Case { id: "BLOBS-109", says: "an object put in parts has the etag of the same bytes put whole", check: c109::<F> },
        Case { id: "BLOBS-110", says: "a part uploaded again replaces the one before: completing with the old etag is refused BadParts, with the new it's taken", check: c110::<F> },
        Case { id: "BLOBS-111", says: "completing with a part named twice is refused BadParts", check: c111::<F> },
        Case { id: "BLOBS-112", says: "completing with no parts named is refused BadParts", check: c112::<F> },
        Case { id: "BLOBS-113", says: "completing with a part never uploaded is refused BadParts", check: c113::<F> },
        Case { id: "BLOBS-114", says: "completing with a part's etag wrong is refused BadParts", check: c114::<F> },
        Case { id: "BLOBS-115", says: "parts 1 and 10,000 are taken; 0 and 10,001 are refused Invalid", check: c115::<F> },
        Case { id: "BLOBS-116", says: "completing or aborting an upload that was never started is refused NoSuchUpload", check: c116::<F> },
        Case { id: "BLOBS-117", says: "a completed upload is gone: completing it again, aborting it or a part for it is refused NoSuchUpload", check: c117::<F> },
        Case { id: "BLOBS-118", says: "a part for an aborted upload is refused NoSuchUpload", check: c118::<F> },
        Case { id: "BLOBS-119", says: "a completed upload replaces the object at its key", check: c119::<F> },
        Case { id: "BLOBS-120", says: "aborting an upload leaves the object at its key as it was", check: c120::<F> },
        Case { id: "BLOBS-121", says: "two uploads to one key are apart, each with its own parts; the one completed last is the object", check: c121::<F> },
        Case { id: "BLOBS-122", says: "each upload started has an id of its own", check: c122::<F> },
        Case { id: "BLOBS-123", says: "a part over the store's object limit is refused TooLarge", check: c123::<F> },
        Case { id: "BLOBS-124", says: "completing an upload whose parts sum past the object limit is refused TooLarge and stores nothing", check: c124::<F> },
        Case { id: "BLOBS-125", says: "parts of exactly the minimum, then a last of one byte, are taken", check: c125::<F> },
        Case { id: "BLOBS-126", says: "parts of one upload uploaded at once are each kept and joined in order", check: c126::<F> },
        Case { id: "BLOBS-127", says: "deleting a key doesn't end an upload to it: the upload completes after, and is the object", check: c127::<F> },
        Case { id: "BLOBS-128", says: "a put while an upload is under way is replaced when the upload completes", check: c128::<F> },
        Case { id: "BLOBS-129", says: "a part uploaded again and again is joined as its last upload", check: c129::<F> },
        Case { id: "BLOBS-130", says: "an upload aborted after parts lists nothing", check: c130::<F> },
        Case { id: "BLOBS-131", says: "a completed upload's object is headed and listed like any other, with one modified time", check: c131::<F> },
        Case { id: "BLOBS-132", says: "parts of different content have different etags", check: c132::<F> },
        Case { id: "BLOBS-133", says: "a presigned get of a key with no object is refused NotFound", check: c133::<F> },
        Case { id: "BLOBS-134", says: "a presigned get gives the object as it is when used: put again since signing, the new bytes", check: c134::<F> },
        Case { id: "BLOBS-135", says: "a presigned put replaces the object at its key", check: c135::<F> },
        Case { id: "BLOBS-136", says: "an object stored by a presigned put has its content's SHA-256 as its etag", check: c136::<F> },
        Case { id: "BLOBS-137", says: "a presigned put that has expired is refused Expired and stores nothing", check: c137::<F> },
        Case { id: "BLOBS-138", says: "a get's URL can't put and a put's can't get: each is refused", check: c138::<F> },
        Case { id: "BLOBS-139", says: "a URL changed to name another key is refused", check: c139::<F> },
        Case { id: "BLOBS-140", says: "presigning makes nothing: a presigned put's key has no object until its URL is used", check: c140::<F> },
        Case { id: "BLOBS-141", says: "a presigned get of an object deleted since signing is refused NotFound", check: c141::<F> },
        Case { id: "BLOBS-142", says: "a presigned put of no bytes stores an empty object", check: c142::<F> },
        Case { id: "BLOBS-143", says: "a presigned URL works for a multibyte key", check: c143::<F> },
        Case { id: "BLOBS-144", says: "a presigned URL works for a key with spaces and reserved characters", check: c144::<F> },
        Case { id: "BLOBS-145", says: "a presigned get gives an object of 2 MiB whole", check: c145::<F> },
        Case { id: "BLOBS-146", says: "puts to 16 keys at once all land, each got back as put", check: c146::<F> },
        Case { id: "BLOBS-147", says: "gets during a replacement each give one version whole, the old or the new", check: c147::<F> },
        Case { id: "BLOBS-148", says: "deletes of 10 keys at once leave none listed", check: c148::<F> },
        Case { id: "BLOBS-149", says: "of If-Match and If-None-Match puts racing on one object, exactly one If-Match is taken", check: c149::<F> },
        Case { id: "BLOBS-150", says: "12 uploads started at once to 12 keys each complete as their own object", check: c150::<F> },
        Case { id: "BLOBS-151", says: "a put's answer and head name the object's key", check: c151::<F> },
        Case { id: "BLOBS-152", says: "a multipart object got whole says its whole size and etag", check: c152::<F> },
        Case { id: "BLOBS-153", says: "ranges of an object stored by a presigned put give its bytes", check: c153::<F> },
        Case { id: "BLOBS-154", says: "after a large object is replaced by a small one, a range past the small one's end is refused InvalidRange naming its size", check: c154::<F> },
        Case { id: "BLOBS-155", says: "an upload completes with the parts it names, in their order, gaps allowed; small parts not last are refused BadParts", check: c155::<F> },
        Case { id: "BLOBS-156", says: "parts 1 and 9, with none between, complete into one object of the two", check: c156::<F> },
        Case { id: "BLOBS-157", says: "parts uploaded but not named are left out of the object, and gone with the upload", check: c157::<F> },
        Case { id: "BLOBS-158", says: "If-Match chains: each replace names the etag the last answered, and an older etag is refused PreconditionFailed", check: c158::<F> },
        Case { id: "BLOBS-159", says: "If-Match compares content, not size: an etag of same-sized, other content is refused PreconditionFailed", check: c159::<F> },
        Case { id: "BLOBS-160", says: "objects stored by a put, an upload and a presigned put each have their content's SHA-256 as their etag", check: c160::<F> },
        Case { id: "BLOBS-161", says: "a presigned put takes a body of exactly the length its URL declares: one longer or shorter is refused and stores nothing", check: c161::<F> },
        Case { id: "BLOBS-162", says: "a listing by a prefix ending in a space gives only the keys that start with it, the space included", check: c162::<F> },
        Case { id: "BLOBS-163", says: "a key with a segment that ends in a space is refused Invalid; spaces elsewhere in a segment are taken, and listed as they were put", check: c163::<F> },
    ]
}
