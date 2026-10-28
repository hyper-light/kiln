//! The protocol buffers of the Remote Execution API's cache (docs/ci.md W24-R26), written and read
//! by hand: `build.bazel.remote.execution.v2` (CAS, action cache, capabilities), `google.bytestream`
//! and `google.rpc.Status`, the fields kiln's server reads and writes, numbered as the `.proto`
//! files number them. Encoding is proto3's: scalars at their defaults aren't written, repeated
//! scalars are packed, and a reader takes them packed or not and skips fields it doesn't know. A
//! message it can't read (a length past its end, a varint past ten bytes, a group, a field named
//! 0) is refused, saying why, never a panic.

/// Why a message couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a malformed protocol buffer: {0}")]
pub struct Bad(pub String);

fn bad<T>(why: impl Into<String>) -> Result<T, Bad> {
    Err(Bad(why.into()))
}

const VARINT: u64 = 0;
const I64: u64 = 1;
const LEN: u64 = 2;
const I32: u64 = 5;

/// A message being written.
#[derive(Debug, Default)]
pub struct Out(Vec<u8>);

impl Out {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v & 0x7f) as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn tag(&mut self, field: u32, wire: u64) {
        self.varint((u64::from(field) << 3) | wire);
    }

    /// An unsigned or enum field.
    pub fn uint(&mut self, field: u32, v: u64) {
        if v != 0 {
            self.tag(field, VARINT);
            self.varint(v);
        }
    }

    /// An `int64` or `int32` field (a negative one in ten bytes, as proto3 has it).
    pub fn int(&mut self, field: u32, v: i64) {
        self.uint(field, v as u64);
    }

    pub fn bool(&mut self, field: u32, v: bool) {
        self.uint(field, u64::from(v));
    }

    pub fn bytes(&mut self, field: u32, v: &[u8]) {
        if !v.is_empty() {
            self.message(field, v);
        }
    }

    pub fn str(&mut self, field: u32, v: &str) {
        self.bytes(field, v.as_bytes());
    }

    /// A message field (or an element of a repeated one), written even when it's empty: a
    /// message's presence is part of it.
    pub fn message(&mut self, field: u32, m: &[u8]) {
        self.tag(field, LEN);
        self.varint(m.len() as u64);
        self.0.extend_from_slice(m);
    }

    /// A repeated scalar field, packed.
    pub fn packed(&mut self, field: u32, vs: &[u64]) {
        if vs.is_empty() {
            return;
        }
        let mut inner = Out::default();
        for v in vs {
            inner.varint(*v);
        }
        self.message(field, &inner.0);
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

/// A field's value as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val<'a> {
    Varint(u64),
    Fixed64(u64),
    Len(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Val<'a> {
    pub fn uint(self) -> Result<u64, Bad> {
        match self {
            Val::Varint(v) => Ok(v),
            _ => bad("a number where its field takes one"),
        }
    }

    pub fn int(self) -> Result<i64, Bad> {
        self.uint().map(|v| v as i64)
    }

    pub fn bool(self) -> Result<bool, Bad> {
        self.uint().map(|v| v != 0)
    }

    pub fn bytes(self) -> Result<&'a [u8], Bad> {
        match self {
            Val::Len(b) => Ok(b),
            _ => bad("bytes where its field takes them"),
        }
    }

    pub fn str(self) -> Result<&'a str, Bad> {
        std::str::from_utf8(self.bytes()?).or_else(|_| bad("a string that isn't UTF-8"))
    }

    /// A repeated scalar field's values here: one, or a packed run of them.
    pub fn uints(self) -> Result<Vec<u64>, Bad> {
        match self {
            Val::Varint(v) => Ok(vec![v]),
            Val::Len(b) => {
                let mut r = In::new(b);
                let mut out = Vec::new();
                while !r.0.is_empty() {
                    out.push(r.varint()?);
                }
                Ok(out)
            }
            _ => bad("a number where its field takes one"),
        }
    }
}

/// A message being read, field by field.
pub struct In<'a>(&'a [u8]);

impl<'a> In<'a> {
    pub fn new(b: &'a [u8]) -> In<'a> {
        In(b)
    }

    fn varint(&mut self) -> Result<u64, Bad> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let Some((&b, rest)) = self.0.split_first() else { return bad("it ends inside a number") };
            self.0 = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        bad("a number longer than ten bytes")
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        let Some((v, rest)) = self.0.split_at_checked(n) else { return bad(format!("a field of {n} bytes, past the message's end")) };
        self.0 = rest;
        Ok(v)
    }

    /// The next field: its number and its value; `None` at the end.
    pub fn field(&mut self) -> Result<Option<(u32, Val<'a>)>, Bad> {
        if self.0.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let Ok(field) = u32::try_from(key >> 3) else { return bad("a field number past 2³²") };
        if field == 0 {
            return bad("a field numbered 0");
        }
        let val = match key & 7 {
            VARINT => Val::Varint(self.varint()?),
            I64 => Val::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().or_else(|_| bad("a short fixed64"))?)),
            LEN => {
                let n = usize::try_from(self.varint()?).or_else(|_| bad("a length past what memory holds"))?;
                Val::Len(self.take(n)?)
            }
            I32 => Val::Fixed32(u32::from_le_bytes(self.take(4)?.try_into().or_else(|_| bad("a short fixed32"))?)),
            other => return bad(format!("wire type {other}, which proto3 doesn't use")),
        };
        Ok(Some((field, val)))
    }
}

/// Calls `f` with each field of `b`, in order.
pub fn each<'a>(b: &'a [u8], mut f: impl FnMut(u32, Val<'a>) -> Result<(), Bad>) -> Result<(), Bad> {
    let mut r = In::new(b);
    while let Some((field, v)) = r.field()? {
        f(field, v)?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------- the types

/// `DigestFunction.Value.SHA256`: the one digest function kiln's store keeps content by.
pub const SHA256: u64 = 1;

/// A blob's digest (`Digest`): its content's SHA-256, lowercase hex, and its size.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    pub hash: String,
    pub size: i64,
}

impl Digest {
    /// The digest of `data`.
    pub fn of(data: &[u8]) -> Digest {
        Digest { hash: crate::util::sha256_hex(data), size: i64::try_from(data.len()).unwrap_or(i64::MAX) }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = Out::default();
        o.str(1, &self.hash);
        o.int(2, self.size);
        o.into_bytes()
    }

    pub fn decode(b: &[u8]) -> Result<Digest, Bad> {
        let mut d = Digest::default();
        each(b, |f, v| {
            match f {
                1 => d.hash = v.str()?.to_owned(),
                2 => d.size = v.int()?,
                _ => {}
            }
            Ok(())
        })?;
        Ok(d)
    }

    /// What's wrong with it as a digest of kiln's store, if anything: 64 lowercase hex digits (a
    /// SHA-256) and a size of 0 or more.
    pub fn check(&self) -> Result<(), String> {
        let hex = self.hash.len() == 64 && self.hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex {
            return Err(format!("`{}` isn't a SHA-256 digest (64 lowercase hex digits)", self.hash));
        }
        if self.size < 0 {
            return Err(format!("{}: a size of {}", self.hash, self.size));
        }
        Ok(())
    }

    /// The size, as a count of bytes.
    pub fn bytes(&self) -> u64 {
        u64::try_from(self.size).unwrap_or(0)
    }

    /// The empty blob's: always there, never stored.
    pub fn is_empty_blob(&self) -> bool {
        self.size == 0 && self.hash == EMPTY_SHA256
    }
}

/// The SHA-256 of nothing.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// `google.rpc.Status`: a code (gRPC's) and a message.
pub fn status(code: u32, message: &str) -> Vec<u8> {
    let mut o = Out::default();
    o.uint(1, u64::from(code));
    o.str(2, message);
    o.into_bytes()
}

/// The fields of a request every method of the CAS and the action cache sends (`instance_name`,
/// `digest_function`) and those it says which blobs or action it's about by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ask {
    pub instance: String,
    pub digest_function: u64,
    /// `blob_digests` (FindMissingBlobs), `digests` (BatchReadBlobs).
    pub digests: Vec<Digest>,
    /// `root_digest` (GetTree), `action_digest` (GetActionResult, UpdateActionResult).
    pub digest: Option<Digest>,
    /// BatchUpdateBlobs' requests: each blob's digest and data.
    pub blobs: Vec<(Digest, Vec<u8>, u64)>,
    /// GetTree's `page_size` and `page_token`.
    pub page_size: i64,
    pub page_token: String,
    /// UpdateActionResult's `action_result`, as it was sent.
    pub action_result: Option<Vec<u8>>,
}

/// Which request a method takes, for [`Ask::decode`]: where its fields are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    GetCapabilities,
    FindMissingBlobs,
    BatchUpdateBlobs,
    BatchReadBlobs,
    GetTree,
    GetActionResult,
    UpdateActionResult,
}

impl Ask {
    pub fn decode(m: Method, b: &[u8]) -> Result<Ask, Bad> {
        let mut a = Ask::default();
        // `digest_function`'s number in each request.
        let df = match m {
            Method::GetCapabilities => 0,
            Method::FindMissingBlobs => 3,
            Method::BatchReadBlobs => 4,
            Method::BatchUpdateBlobs | Method::GetTree | Method::UpdateActionResult => 5,
            Method::GetActionResult => 6,
        };
        each(b, |f, v| {
            match (m, f) {
                (_, 1) => a.instance = v.str()?.to_owned(),
                (_, n) if n == df => a.digest_function = v.uint()?,
                (Method::FindMissingBlobs | Method::BatchReadBlobs, 2) => a.digests.push(Digest::decode(v.bytes()?)?),
                (Method::GetTree | Method::GetActionResult | Method::UpdateActionResult, 2) => a.digest = Some(Digest::decode(v.bytes()?)?),
                (Method::BatchUpdateBlobs, 2) => {
                    let (mut digest, mut data, mut compressor) = (None, Vec::new(), 0);
                    each(v.bytes()?, |f, v| {
                        match f {
                            1 => digest = Some(Digest::decode(v.bytes()?)?),
                            2 => data = v.bytes()?.to_vec(),
                            3 => compressor = v.uint()?,
                            _ => {}
                        }
                        Ok(())
                    })?;
                    a.blobs.push((digest.unwrap_or_default(), data, compressor));
                }
                (Method::GetTree, 3) => a.page_size = v.int()?,
                (Method::GetTree, 4) => a.page_token = v.str()?.to_owned(),
                (Method::UpdateActionResult, 3) => a.action_result = Some(v.bytes()?.to_vec()),
                _ => {}
            }
            Ok(())
        })?;
        Ok(a)
    }
}

/// The digests a `Directory` names: its files' (`files`, each a `FileNode`) and its
/// subdirectories' (`directories`, each a `DirectoryNode`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectoryRefs {
    pub files: Vec<Digest>,
    pub directories: Vec<Digest>,
}

impl DirectoryRefs {
    pub fn decode(b: &[u8]) -> Result<DirectoryRefs, Bad> {
        let mut d = DirectoryRefs::default();
        each(b, |f, v| {
            if matches!(f, 1 | 2) {
                let mut digest = None;
                each(v.bytes()?, |nf, nv| {
                    if nf == 2 {
                        digest = Some(Digest::decode(nv.bytes()?)?);
                    }
                    Ok(())
                })?;
                let digest = digest.unwrap_or_default();
                if f == 1 { d.files.push(digest) } else { d.directories.push(digest) }
            }
            Ok(())
        })?;
        Ok(d)
    }
}

/// The digests a `Tree` names: those of the files in its root and children, every directory's.
pub fn tree_files(b: &[u8]) -> Result<Vec<Digest>, Bad> {
    let mut files = Vec::new();
    each(b, |f, v| {
        if matches!(f, 1 | 2) {
            files.extend(DirectoryRefs::decode(v.bytes()?)?.files);
        }
        Ok(())
    })?;
    Ok(files)
}

/// What an `ActionResult` says, as far as kiln keeps and checks it: its exit code, and the blobs
/// it names (each output file, each output directory's `Tree`, its stdout and stderr).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionResult {
    pub exit_code: i32,
    /// Output files by path, with whether each is executable.
    pub files: Vec<(String, Digest, bool)>,
    /// Output directories by path: each one's `Tree`.
    pub directories: Vec<(String, Digest)>,
    pub stdout: Option<Digest>,
    pub stderr: Option<Digest>,
}

impl ActionResult {
    pub fn decode(b: &[u8]) -> Result<ActionResult, Bad> {
        let mut r = ActionResult::default();
        each(b, |f, v| {
            match f {
                2 => {
                    let (mut path, mut digest, mut exec) = (String::new(), Digest::default(), false);
                    each(v.bytes()?, |f, v| {
                        match f {
                            1 => path = v.str()?.to_owned(),
                            2 => digest = Digest::decode(v.bytes()?)?,
                            4 => exec = v.bool()?,
                            _ => {}
                        }
                        Ok(())
                    })?;
                    r.files.push((path, digest, exec));
                }
                3 => {
                    let (mut path, mut tree) = (String::new(), Digest::default());
                    each(v.bytes()?, |f, v| {
                        match f {
                            1 => path = v.str()?.to_owned(),
                            3 => tree = Digest::decode(v.bytes()?)?,
                            _ => {}
                        }
                        Ok(())
                    })?;
                    r.directories.push((path, tree));
                }
                4 => r.exit_code = v.int()? as i32,
                6 => r.stdout = Some(Digest::decode(v.bytes()?)?),
                8 => r.stderr = Some(Digest::decode(v.bytes()?)?),
                _ => {}
            }
            Ok(())
        })?;
        Ok(r)
    }

    /// Every blob it names but the trees' files, each once (an unset stdout or stderr names none).
    pub fn blobs(&self) -> Vec<Digest> {
        let mut out: Vec<Digest> = self.files.iter().map(|(_, d, _)| d.clone()).chain(self.directories.iter().map(|(_, d)| d.clone())).chain(self.stdout.clone()).chain(self.stderr.clone()).filter(|d| !d.hash.is_empty()).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// `ServerCapabilities`: the cache's (SHA-256 only, updates when the caller may write, batches to
/// `max_batch`, no compression, absolute symlinks disallowed), no execution, and the API versions
/// served (2.0 to 2.3).
pub fn capabilities(update: bool, max_batch: i64) -> Vec<u8> {
    let semver = |major: u64, minor: u64| {
        let mut v = Out::default();
        v.uint(1, major);
        v.uint(2, minor);
        v.into_bytes()
    };
    let mut updates = Out::default();
    updates.bool(1, update);
    let mut cache = Out::default();
    cache.packed(1, &[SHA256]);
    cache.message(2, &updates.into_bytes());
    cache.int(4, max_batch);
    // SymlinkAbsolutePathStrategy.DISALLOWED.
    cache.uint(5, 1);
    let mut o = Out::default();
    o.message(1, &cache.into_bytes());
    o.message(4, &semver(2, 0));
    o.message(5, &semver(2, 3));
    o.into_bytes()
}

/// `FindMissingBlobsResponse`.
pub fn missing(digests: &[Digest]) -> Vec<u8> {
    let mut o = Out::default();
    for d in digests {
        o.message(2, &d.encode());
    }
    o.into_bytes()
}

/// `BatchUpdateBlobsResponse`: each blob's digest and status.
pub fn updated(answers: &[(Digest, u32, String)]) -> Vec<u8> {
    let mut o = Out::default();
    for (d, code, msg) in answers {
        let mut r = Out::default();
        r.message(1, &d.encode());
        r.message(2, &status(*code, msg));
        o.message(1, &r.into_bytes());
    }
    o.into_bytes()
}

/// `BatchReadBlobsResponse`: each blob's digest, data (when found) and status.
pub fn read(answers: &[(Digest, Vec<u8>, u32, String)]) -> Vec<u8> {
    let mut o = Out::default();
    for (d, data, code, msg) in answers {
        let mut r = Out::default();
        r.message(1, &d.encode());
        r.bytes(2, data);
        r.message(3, &status(*code, msg));
        o.message(1, &r.into_bytes());
    }
    o.into_bytes()
}

/// `GetTreeResponse`: directories as they're kept, and where the next page starts.
pub fn tree(directories: &[Vec<u8>], next: &str) -> Vec<u8> {
    let mut o = Out::default();
    for d in directories {
        o.message(1, d);
    }
    o.str(2, next);
    o.into_bytes()
}

// ------------------------------------------------------------------------------- google.bytestream

/// `ReadRequest`, `WriteRequest` and `QueryWriteStatusRequest`, in one: their resource, offset,
/// limit, data and whether a write is finished.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stream {
    pub resource: String,
    pub offset: i64,
    pub limit: i64,
    pub finish: bool,
    pub data: bytes::Bytes,
}

impl Stream {
    /// A `ReadRequest` (`resource_name` 1, `read_offset` 2, `read_limit` 3).
    pub fn read_request(b: &[u8]) -> Result<Stream, Bad> {
        let mut s = Stream::default();
        each(b, |f, v| {
            match f {
                1 => s.resource = v.str()?.to_owned(),
                2 => s.offset = v.int()?,
                3 => s.limit = v.int()?,
                _ => {}
            }
            Ok(())
        })?;
        Ok(s)
    }

    /// A `WriteRequest` (`resource_name` 1, `write_offset` 2, `finish_write` 3, `data` 10), its
    /// data a slice of `whole`'s bytes, not a copy.
    pub fn write_request(whole: &bytes::Bytes) -> Result<Stream, Bad> {
        let mut s = Stream::default();
        let mut data: Option<(usize, usize)> = None;
        let base = whole.as_ptr() as usize;
        each(whole, |f, v| {
            match f {
                1 => s.resource = v.str()?.to_owned(),
                2 => s.offset = v.int()?,
                3 => s.finish = v.bool()?,
                10 => {
                    let b = v.bytes()?;
                    let at = (b.as_ptr() as usize).saturating_sub(base);
                    data = Some((at, at.saturating_add(b.len())));
                }
                _ => {}
            }
            Ok(())
        })?;
        if let Some((from, to)) = data {
            if to > whole.len() {
                return bad("a write's data past its message");
            }
            s.data = whole.slice(from..to);
        }
        Ok(s)
    }

    /// A `QueryWriteStatusRequest` (`resource_name` 1).
    pub fn query_request(b: &[u8]) -> Result<Stream, Bad> {
        Stream::read_request(b)
    }
}

/// `ReadResponse`: a piece of the blob.
pub fn read_response(data: &[u8]) -> Vec<u8> {
    let mut o = Out::default();
    o.bytes(10, data);
    o.into_bytes()
}

/// `WriteResponse`.
pub fn write_response(committed: i64) -> Vec<u8> {
    let mut o = Out::default();
    o.int(1, committed);
    o.into_bytes()
}

/// `QueryWriteStatusResponse`.
pub fn write_status(committed: i64, complete: bool) -> Vec<u8> {
    let mut o = Out::default();
    o.int(1, committed);
    o.bool(2, complete);
    o.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W24-R26: a digest's bytes are protobuf's (field 1 a string, field 2 a varint), and read back.
    #[test]
    fn w24_r26_a_digest_is_written_as_protobuf_writes_it() -> Result<(), Bad> {
        let d = Digest { hash: "abc".into(), size: 3 };
        assert_eq!(d.encode(), [0x0a, 3, b'a', b'b', b'c', 0x10, 3]);
        assert_eq!(Digest::decode(&d.encode())?, d);
        // A size past one byte's varint, and none at all (proto3 writes no default).
        assert_eq!(Digest { hash: String::new(), size: 300 }.encode(), [0x10, 0xac, 0x02]);
        assert_eq!(Digest::default().encode(), Vec::<u8>::new());
        // An int32 below 0 is ten bytes, and reads back.
        let mut o = Out::default();
        o.int(4, -1);
        let b = o.into_bytes();
        assert_eq!(b.len(), 11);
        assert_eq!(In::new(&b).field()?.map(|(f, v)| (f, v.int().map(|i| i as i32))), Some((4, Ok(-1))));
        assert!(Digest::of(b"").is_empty_blob());
        assert_eq!(Digest::of(b"hello").check(), Ok(()));
        assert!(Digest { hash: "AB".repeat(32), size: 1 }.check().is_err() && Digest { hash: "ab".repeat(32), size: -1 }.check().is_err());
        Ok(())
    }

    /// W24-R26: what isn't a message is refused, saying why: truncated, overlong, a group, field 0.
    #[test]
    fn w24_r26_malformed_messages_are_refused() {
        for (b, says) in [
            (&[0x0a, 5, b'a'][..], "past the message's end"),
            (&[0x10, 0x80, 0x80][..], "ends inside a number"),
            (&[0x10, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..], "longer than ten bytes"),
            (&[0x0b][..], "wire type 3"),
            (&[0x02, 0][..], "numbered 0"),
            (&[0x0a, 2, 0xff, 0xfe][..], "isn't UTF-8"),
        ] {
            let e = Digest::decode(b).err().map(|e| e.0).unwrap_or_default();
            assert!(e.contains(says), "{b:?}: {e}");
        }
    }

    /// W24-R26: requests read as their `.proto` numbers them, unknown fields skipped, repeated
    /// scalars packed or not.
    #[test]
    fn w24_r26_requests_read_as_the_api_numbers_them() -> Result<(), Bad> {
        let (a, b) = (Digest::of(b"a"), Digest::of(b"b"));
        let mut o = Out::default();
        o.str(1, "main");
        o.message(2, &a.encode());
        o.message(2, &b.encode());
        o.uint(3, SHA256);
        o.str(99, "a field from a newer client");
        let ask = Ask::decode(Method::FindMissingBlobs, &o.into_bytes())?;
        assert_eq!((ask.instance.as_str(), ask.digests.clone(), ask.digest_function), ("main", vec![a.clone(), b.clone()], SHA256));
        let mut req = Out::default();
        req.message(1, &a.encode());
        req.bytes(2, b"a");
        let mut o = Out::default();
        o.message(2, &req.into_bytes());
        o.uint(5, SHA256);
        let ask = Ask::decode(Method::BatchUpdateBlobs, &o.into_bytes())?;
        assert_eq!((ask.blobs, ask.digest_function), (vec![(a.clone(), b"a".to_vec(), 0)], SHA256));
        let mut o = Out::default();
        o.message(2, &b.encode());
        o.int(3, 10);
        o.str(4, "20");
        let ask = Ask::decode(Method::GetTree, &o.into_bytes())?;
        assert_eq!((ask.digest, ask.page_size, ask.page_token.as_str()), (Some(b.clone()), 10, "20"));
        // Packed and unpacked: the same values.
        let mut packed = Out::default();
        packed.packed(3, &[0, 1, 2]);
        let mut loose = Out::default();
        for v in [0, 1, 2] {
            loose.tag(3, VARINT);
            loose.varint(v);
        }
        for bytes in [packed.into_bytes(), loose.into_bytes()] {
            let mut all = Vec::new();
            each(&bytes, |_, v| {
                all.extend(v.uints()?);
                Ok(())
            })?;
            assert_eq!(all, [0, 1, 2]);
        }
        Ok(())
    }

    /// W24-R26: an `ActionResult`'s exit code and every blob it names, a `Tree`'s files, and
    /// capabilities that say SHA-256 and whether updates are taken.
    #[test]
    fn w24_r26_action_results_trees_and_capabilities() -> Result<(), Bad> {
        let (f, t, out) = (Digest::of(b"binary"), Digest::of(b"tree"), Digest::of(b"stdout"));
        let mut file = Out::default();
        file.str(1, "bin/app");
        file.message(2, &f.encode());
        file.bool(4, true);
        let mut dir = Out::default();
        dir.str(1, "gen");
        dir.message(3, &t.encode());
        let mut r = Out::default();
        r.message(2, &file.into_bytes());
        r.message(3, &dir.into_bytes());
        r.int(4, 0);
        r.message(6, &out.encode());
        let got = ActionResult::decode(&r.into_bytes())?;
        assert_eq!(got.files, vec![("bin/app".to_owned(), f.clone(), true)]);
        assert_eq!(got.directories, vec![("gen".to_owned(), t.clone())]);
        let mut want = vec![f.clone(), t, out];
        want.sort();
        assert_eq!(got.blobs(), want);
        // A tree: its root's file and a child's.
        let (x, y) = (Digest::of(b"x"), Digest::of(b"y"));
        let node = |d: &Digest| {
            let mut n = Out::default();
            n.str(1, "f");
            n.message(2, &d.encode());
            n.into_bytes()
        };
        let mut root = Out::default();
        root.message(1, &node(&x));
        let mut child = Out::default();
        child.message(1, &node(&y));
        let mut tree = Out::default();
        tree.message(1, &root.into_bytes());
        tree.message(2, &child.into_bytes());
        assert_eq!(tree_files(&tree.into_bytes())?, vec![x, y]);
        // Capabilities: field 1 the cache's, which says SHA-256 (packed) and whether updates go.
        let caps = capabilities(true, 4 << 20);
        let mut cache = None;
        each(&caps, |f, v| {
            if f == 1 {
                cache = Some(v.bytes()?.to_vec());
            }
            Ok(())
        })?;
        let (mut functions, mut update) = (Vec::new(), None);
        each(&cache.unwrap_or_default(), |f, v| {
            match f {
                1 => functions.extend(v.uints()?),
                2 => each(v.bytes()?, |f, v| {
                    if f == 1 {
                        update = Some(v.bool()?);
                    }
                    Ok(())
                })?,
                _ => {}
            }
            Ok(())
        })?;
        assert_eq!((functions, update), (vec![SHA256], Some(true)));
        Ok(())
    }

    /// W24-R26: a write's data is a slice of the message it came in, not a copy.
    #[test]
    fn w24_r26_a_writes_data_is_its_messages_bytes() -> Result<(), Bad> {
        let mut o = Out::default();
        o.str(1, "uploads/u1/blobs/aa/2");
        o.int(2, 5);
        o.bool(3, true);
        o.bytes(10, b"hi");
        let whole = bytes::Bytes::from(o.into_bytes());
        let s = Stream::write_request(&whole)?;
        assert_eq!((s.resource.as_str(), s.offset, s.finish, &s.data[..]), ("uploads/u1/blobs/aa/2", 5, true, &b"hi"[..]));
        assert_eq!(s.data.as_ptr(), whole.get(whole.len() - 2..).map_or(std::ptr::null(), <[u8]>::as_ptr));
        Ok(())
    }
}
