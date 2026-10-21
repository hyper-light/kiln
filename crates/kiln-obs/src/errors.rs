//! Error Tracking (`docs/observe.md` §8.8): the issue an error is of, as Datadog groups errors
//! into issues.
//!
//! An error is tracked as Datadog tracks it: a span's when it is its service's entry and says the
//! error's type, message and stack (the stack a frame at least); a log's when its status is an
//! error's or worse, it names its service, and it says the error's kind or a stack with a frame.
//! Its issue is its service's, never another's; within one, the errors of one type, whose messages
//! have the same words, thrown at one place. Its words are Datadog's: numbers, punctuation and
//! whatever is quoted or bracketed left out, a word with a digit in it too (an id, a hash), so
//! `order 42 not found (shard 'eu-1')` and `order 7 not found` are one. Its place is a frame of its
//! stack: the innermost of the application's own (Sentry's in-app frames: a library's, a runtime's
//! or a framework's frame is passed over, so an error a library raises is where the application
//! called it), else the innermost with a file; its file's name and its function, each with what
//! varies from build to build left out (versions, hashes, a compiler's numbering of lambdas and
//! closures), never its line (an edit above it would move every issue). A user's own grouping
//! (`error.fingerprint`) takes the place of all but the service.
//!
//! The stacks read are each language's as its tracers and SDKs report them, recorded from the
//! runtimes themselves (`fixtures/errors/`, and `fixtures/errors/gen/` the programs that made
//! them): Python's tracebacks, the JVM's, V8's (and Firefox's), .NET's, Go's (dd-trace-go's and a
//! panic's), Ruby's, PHP's and Rust's. A stack is read to [`STACK_BYTES`] at the most.
//!
//! An issue's id is a hash of its parts that never changes (FNV-1a, then splitmix64's finalizer:
//! an issue's id is kept, replicated and searched for, so changing it would split every issue).

use crate::query::eval::{Lease, QueryError};

/// The most of a stack read for its frame: a frame is near its top, and a stack can be megabytes.
pub const STACK_BYTES: usize = 64 << 10;
/// A stack's attributes (Datadog's `error.stack`, OpenTelemetry's `exception.stacktrace`): kept
/// longer than other attributes' values, and cut keeping both its ends ([`stack_ends`]).
pub const STACK_KEYS: [&str; 2] = ["error.stack", "exception.stacktrace"];
/// A stack kept at the most (bytes): Datadog's Agent's bound on a span's tag's value.
pub const STACK_VALUE_BYTES: usize = 25_000;
/// What stands for the lines a cut stack leaves out.
pub const STACK_GAP: &str = "\n…\n";

/// `stack`'s first and last lines that `max` bytes hold with [`STACK_GAP`] between them, each part
/// whole lines (a frame is at one end or the other: Python's innermost last, every other language's
/// first); `(stack, "")` where it fits whole.
pub fn stack_ends(stack: &str, max: usize) -> (&str, &str) {
    if stack.len() <= max {
        return (stack, "");
    }
    let half = max.saturating_sub(STACK_GAP.len()) / 2;
    let mut h = half.min(stack.len());
    while !stack.is_char_boundary(h) {
        h -= 1;
    }
    let head = stack.get(..h).unwrap_or("");
    let head = head.rfind('\n').and_then(|i| head.get(..i)).unwrap_or(head);
    let mut t = stack.len().saturating_sub(half);
    while !stack.is_char_boundary(t) {
        t += 1;
    }
    let tail = stack.get(t..).unwrap_or("");
    let tail = tail.find('\n').and_then(|i| tail.get(i + 1..)).unwrap_or(tail);
    (head, tail)
}
/// The most of a message read for its words, and the most words kept.
pub const MESSAGE_BYTES: usize = 4 << 10;
pub const MESSAGE_WORDS: usize = 64;

/// Where an error was said: a span's, or a log's (their issues kept apart, as Datadog's
/// products keep theirs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Span,
    Log,
}

impl Source {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Span => b"span",
            Self::Log => b"log",
        }
    }
}

/// What a record says of its error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Error<'a> {
    pub kind: Option<&'a str>,
    pub message: Option<&'a str>,
    pub stack: Option<&'a str>,
    /// `error.fingerprint`: the user's grouping.
    pub fingerprint: Option<&'a str>,
}

/// The attributes an error's parts are read from, each part's in order of preference: Datadog's
/// (`error.type`, or a log's `error.kind`; `error.message`, older tracers' `error.msg`;
/// `error.stack`), else OpenTelemetry's (`exception.*`).
const KIND: [&str; 3] = ["error.type", "error.kind", "exception.type"];
const MESSAGE: [&str; 3] = ["error.message", "error.msg", "exception.message"];
const STACK: [&str; 2] = ["error.stack", "exception.stacktrace"];

impl<'a> Error<'a> {
    /// From a record's attributes, in one pass (each part the most preferred key's, a blank value
    /// none).
    pub fn of(attrs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut e = Self::default();
        let mut rank = [usize::MAX; 3];
        for (k, v) in attrs {
            if v.trim().is_empty() {
                continue;
            }
            for (slot, keys, r) in [(&mut e.kind, &KIND[..], 0), (&mut e.message, &MESSAGE[..], 1), (&mut e.stack, &STACK[..], 2)] {
                if let Some(at) = keys.iter().position(|x| *x == k)
                    && let Some(best) = rank.get_mut(r)
                    && at < *best
                {
                    *best = at;
                    *slot = Some(v);
                }
            }
            if k == "error.fingerprint" && e.fingerprint.is_none() {
                e.fingerprint = Some(v);
            }
        }
        e
    }

    /// Whether it says anything of an error.
    pub fn any(&self) -> bool {
        self.kind.is_some() || self.message.is_some() || self.stack.is_some()
    }
}

/// A stack's frame: its file (as the stack has it) and its function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub file: &'a str,
    pub function: &'a str,
}

/// A function no stack names (a JavaScript function's without a name).
pub const ANONYMOUS: &str = "<anonymous>";

/// The issue of a span's error: a service's entry span's (`entry`: its first span, a server's or
/// a consumer's, or a span whose parent is another service's) whose error says its type, its
/// message and its stack (two lines at the least, a frame among them); or with `error.fingerprint`,
/// any of them; else none.
pub fn span_issue(service: &str, entry: bool, e: &Error<'_>) -> Option<u64> {
    if !entry || service.is_empty() {
        return None;
    }
    if let Some(fp) = e.fingerprint
        && e.any()
    {
        return Some(custom(Source::Span, service, fp));
    }
    let (kind, message, stack) = (e.kind?, e.message?, e.stack?);
    if stack.lines().filter(|l| !l.trim().is_empty()).take(2).count() < 2 {
        return None;
    }
    let frame = frame(stack)?;
    Some(grouped(Source::Span, service, kind, message, Some(frame)))
}

/// The issue of a log's error: a log of an error's status or worse (`severity`, OpenTelemetry's
/// number: 17 and up) that names its service, and says its error's kind, or a stack with a frame;
/// or with `error.fingerprint`, any of it; else none.
pub fn log_issue(service: &str, severity: u8, e: &Error<'_>) -> Option<u64> {
    if severity < 17 || service.is_empty() {
        return None;
    }
    if let Some(fp) = e.fingerprint
        && e.any()
    {
        return Some(custom(Source::Log, service, fp));
    }
    let frame = e.stack.and_then(frame);
    if e.kind.is_none() && frame.is_none() {
        return None;
    }
    Some(grouped(Source::Log, service, e.kind.unwrap_or(""), e.message.unwrap_or(""), frame))
}

/// The metric each issue's errors are counted in, each window of ten seconds, by issue, service,
/// env and version: counted on the server as metrics from logs and from spans are
/// (`crate::logs::metrics`), each error once whichever node took it. Spans' and logs' issues are
/// never the same, so their counts are never each other's series.
pub const METRIC: &str = "error_tracking.errors";

/// Error Tracking's metric's definition, of spans (`spans`) or of logs: their errors that are
/// issues (an error's status, or worse, and an `issue.id`).
pub fn metrics(spans: bool) -> Vec<crate::logs::metrics::LogMetric> {
    use crate::logs::store::Facet;
    let query = if spans { "@issue.id:* status:error" } else { "@issue.id:* (status:error OR status:fatal)" };
    let search = if spans { crate::logs::search::parse_spans(query) } else { crate::logs::search::parse(query) };
    let Ok(search) = search else { return Vec::new() };
    let by = ["@issue.id", "service", "@env", "@version"].iter().map(|b| (if spans { Facet::parse_span(b) } else { Facet::parse(b) }, b.trim_start_matches('@').to_owned())).collect();
    vec![crate::logs::metrics::LogMetric { name: METRIC.to_owned(), search, by, compute: crate::logs::metrics::Compute::Count, since: 0 }]
}

/// What [`METRIC`] holds of one of an issue's series (its service's, an env's, a version's) over a
/// range: its errors, and the first and last window it has any in (ms).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seen {
    pub issue: String,
    pub service: String,
    pub env: String,
    pub version: String,
    pub errors: f64,
    pub first: i64,
    pub last: i64,
}

/// Each series of [`METRIC`] whose tags hold `filter`, over `[from, to)` (ms), with an error in it.
/// A long range is read from the series' rollups as [`crate::apm::red`] reads them (its first and
/// last windows then a tier's, where the range's edges are not). Of a store held throughout, or of
/// a shared one read a round of series at a time (`query::eval::Source`).
pub fn seen<'a>(store: impl Into<crate::query::eval::Source<'a>>, filter: &[(&str, &str)], from: i64, to: i64) -> Result<Vec<Seen>, QueryError> {
    let mut sel = crate::index::Selector::metric(METRIC);
    for (k, v) in filter {
        sel = sel.tag(k, v);
    }
    store.into().with(|lease| {
        let mut ids = Vec::new();
        lease.hold()?.select(&sel, &mut ids);
        read_seen(lease, &ids, from, to)
    })
}

/// [`seen`] of these issues' series alone, each issue's found in the store's index by its tag: what
/// a monitor watching a few issues reads.
pub fn seen_of<'a>(store: impl Into<crate::query::eval::Source<'a>>, issues: &[&str], from: i64, to: i64) -> Result<Vec<Seen>, QueryError> {
    store.into().with(|lease| {
        let (mut ids, mut one) = (Vec::new(), Vec::new());
        {
            let store = lease.hold()?;
            for issue in issues {
                one.clear();
                store.select(&crate::index::Selector::metric(METRIC).tag("issue.id", issue), &mut one);
                ids.extend_from_slice(&one);
            }
        }
        read_seen(lease, &ids, from, to)
    })
}

/// `ids`' errors over `[from, to)`, a round of series a hold (`query::eval::ROUND`).
fn read_seen(lease: &Lease<'_>, ids: &[u32], from: i64, to: i64) -> Result<Vec<Seen>, QueryError> {
    use crate::rollup::{Col, Rolled, TIERS};
    let width = TIERS.iter().rev().copied().find(|&w| w.saturating_mul(crate::apm::RED_WINDOWS) <= to.saturating_sub(from));
    let (mut ts, mut vs, mut rolled, mut scratch) = (Vec::new(), Vec::new(), Rolled::default(), crate::store::Scratch::default());
    let mut out = Vec::new();
    for round in ids.chunks(crate::query::eval::ROUND) {
        let store = lease.hold()?;
        for &id in round {
            let Some(series) = store.series(id) else { continue };
            ts.clear();
            vs.clear();
            rolled.clear();
            match width {
                Some(width) => {
                    let roll = crate::store::Roll { width, from, to, kind: series.kind(), cols: &[Col::Sum] };
                    store.read_rolled(id, from, to.saturating_sub(1), &roll, &mut scratch, &mut ts, &mut vs, &mut rolled)?;
                }
                None => store.read_with(id, from, to.saturating_sub(1), &mut scratch, &mut ts, &mut vs)?,
            }
            let (mut errors, mut first, mut last) = (0.0, i64::MAX, i64::MIN);
            for (&t, &v) in ts.iter().zip(&vs).chain(rolled.start.iter().zip(&rolled.sum)) {
                if v > 0.0 {
                    errors += v;
                    first = first.min(t);
                    last = last.max(t);
                }
            }
            if errors > 0.0 {
                let tag = |k: &str| series.tag(k).unwrap_or(crate::logs::metrics::NONE).to_owned();
                out.push(Seen { issue: tag("issue.id"), service: tag("service"), env: tag("env"), version: tag("version"), errors, first, last });
            }
        }
    }
    Ok(out)
}

/// Buckets an issue's timeline has at the most.
pub const TIMELINE_BUCKETS: usize = 1_500;

/// The metric query of the errors an Error Tracking monitor of `search` counts, by issue, for a
/// chart of them: when the search says no more than tags of [`METRIC`], each one value, all to
/// hold (`service:shop env:prod`, `*`); none when it says more (a type, a word, an `OR`).
pub fn monitor_query(search: &str) -> Option<String> {
    use crate::logs::search::{AttrMatch, Field, Leaf, Node, Text};
    let s = crate::logs::search::parse(search).ok()?;
    let mut tags: Vec<String> = Vec::new();
    let plain = |v: &str| !v.is_empty() && !v.contains([',', '{', '}', ' ', '"']);
    let mut todo = vec![s.root];
    while let Some(n) = todo.pop() {
        match s.nodes.get(n)? {
            Node::And(a, b) => todo.extend([*a, *b]),
            Node::Leaf(Leaf::All) => {}
            Node::Leaf(Leaf::Field(Field::Service, Text::Exact(v))) if plain(v) => tags.push(format!("service:{v}")),
            Node::Leaf(Leaf::Attr(k, AttrMatch::Text(Text::Exact(v)))) if matches!(k.as_str(), "env" | "version" | "issue.id") && plain(v) => tags.push(format!("{k}:{v}")),
            _ => return None,
        }
    }
    tags.sort();
    let scope = if tags.is_empty() { "*".to_owned() } else { tags.join(",") };
    Some(format!("sum:{METRIC}{{{scope}}} by {{issue.id}}.as_count()"))
}

/// Issue `id`'s errors over `[from, to)` (ms) in buckets of `step` (ms; `from` a multiple of it):
/// each bucket's start and its errors, [`TIMELINE_BUCKETS`] at the most. A step a rollup tier
/// divides reads the tier where the range is long enough (as [`crate::apm::red`] does). Of a store
/// held throughout, or of a shared one read a round of series at a time (`query::eval::Source`).
pub fn timeline<'a>(store: impl Into<crate::query::eval::Source<'a>>, id: &str, from: i64, to: i64, step: i64) -> Result<Vec<(i64, f64)>, QueryError> {
    store.into().with(|lease| timeline_on(lease, id, from, to, step))
}

fn timeline_on(lease: &Lease<'_>, id: &str, from: i64, to: i64, step: i64) -> Result<Vec<(i64, f64)>, QueryError> {
    use crate::rollup::{Col, Rolled, TIERS};
    let step = step.max(crate::logs::metrics::WINDOW / 1_000_000);
    let n = usize::try_from(to.saturating_sub(from).saturating_add(step - 1) / step).unwrap_or(0).min(TIMELINE_BUCKETS);
    let mut out: Vec<(i64, f64)> = (0..n).map(|i| (from.saturating_add(i64::try_from(i).unwrap_or(0).saturating_mul(step)), 0.0)).collect();
    let to = out.last().map_or(from, |b| b.0.saturating_add(step)).min(to);
    let mut ids = Vec::new();
    lease.hold()?.select(&crate::index::Selector::metric(METRIC).tag("issue.id", id), &mut ids);
    let width = TIERS.iter().rev().copied().find(|&w| step % w == 0 && w.saturating_mul(crate::apm::RED_WINDOWS) <= to.saturating_sub(from));
    let (mut ts, mut vs, mut rolled, mut scratch) = (Vec::new(), Vec::new(), Rolled::default(), crate::store::Scratch::default());
    for round in ids.chunks(crate::query::eval::ROUND) {
        let store = lease.hold()?;
        for &sid in round {
            let Some(series) = store.series(sid) else { continue };
            ts.clear();
            vs.clear();
            rolled.clear();
            match width {
                Some(width) => {
                    let roll = crate::store::Roll { width, from, to, kind: series.kind(), cols: &[Col::Sum] };
                    store.read_rolled(sid, from, to.saturating_sub(1), &roll, &mut scratch, &mut ts, &mut vs, &mut rolled)?;
                }
                None => store.read_with(sid, from, to.saturating_sub(1), &mut scratch, &mut ts, &mut vs)?,
            }
            for (&t, &v) in ts.iter().zip(&vs).chain(rolled.start.iter().zip(&rolled.sum)) {
                if let Some(b) = usize::try_from(t.saturating_sub(from) / step).ok().and_then(|i| out.get_mut(i)) {
                    b.1 += v;
                }
            }
        }
    }
    Ok(out)
}

/// An issue's id as it is written (`issue.id`): sixteen hex digits.
pub fn hex(id: u64) -> String {
    format!("{id:016x}")
}

/// A log's error's issue as its `issue.id` attribute, where it is one ([`log_issue`]); one it came
/// with left out (an issue's id is kiln's alone). A log below an error's status is left as it is.
pub fn put_log_issue(r: &mut crate::logs::record::Record) {
    if r.severity < 17 {
        return;
    }
    r.attrs.retain(|(k, _)| k != "issue.id");
    let id = log_issue(&r.service, r.severity, &Error::of(r.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str()))));
    if let Some(id) = id {
        r.attrs.push(("issue.id".to_owned(), hex(id)));
    }
}

/// An issue's id from its parts, each hashed as it is read (its words, its frame's file and
/// function never made whole): the id [`stable`] gives of them whole.
fn grouped(source: Source, service: &str, kind: &str, message: &str, frame: Option<Frame<'_>>) -> u64 {
    let mut h = Stable::new();
    for part in [&b"kiln.issue.v1"[..], source.tag(), service.as_bytes(), kind.trim().as_bytes()] {
        h.part(part);
    }
    words_to(message, &mut h);
    h.end();
    if let Some(f) = frame {
        normal_file_to(f.file, &mut h);
    }
    h.end();
    if let Some(f) = frame {
        normal_function_to(f.function, &mut h);
    }
    h.end();
    h.finish()
}

fn custom(source: Source, service: &str, fingerprint: &str) -> u64 {
    stable(&[b"kiln.issue.v1.custom", source.tag(), service.as_bytes(), fingerprint.as_bytes()])
}

/// FNV-1a 64 over the parts, each ended by a unit separator, then splitmix64's finalizer (FNV's
/// last bytes mix poorly alone). Frozen: an issue's id is kept.
fn stable(parts: &[&[u8]]) -> u64 {
    let mut h = Stable::new();
    for p in parts {
        h.part(p);
    }
    h.finish()
}

/// [`stable`]'s hash, a part's bytes at a time.
struct Stable(u64);

impl Stable {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// The end of a part: a unit separator.
    fn end(&mut self) {
        self.bytes(&[0x1f]);
    }

    fn part(&mut self, bytes: &[u8]) {
        self.bytes(bytes);
        self.end();
    }

    fn finish(self) -> u64 {
        let mut h = self.0;
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^ (h >> 31)
    }
}

/// What text is written to as it is made: a `String`, or an id's hash.
trait Sink {
    fn push_str(&mut self, s: &str);
    fn push(&mut self, c: char);
}

impl Sink for String {
    fn push_str(&mut self, s: &str) {
        String::push_str(self, s);
    }
    fn push(&mut self, c: char) {
        String::push(self, c);
    }
}

impl Sink for Stable {
    fn push_str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    fn push(&mut self, c: char) {
        self.bytes(c.encode_utf8(&mut [0; 4]).as_bytes());
    }
}

/// A message's words into `out` (cleared), each after a space but the first: Datadog's, its
/// numbers, its punctuation and what is quoted (`'`, `"`, `` ` ``, a quote's opening where a word
/// does not go on before it) or bracketed (`()`, `[]`, `{}`) left out, a word with a digit in it too;
/// [`MESSAGE_WORDS`] of its first [`MESSAGE_BYTES`] at the most. An unclosed quote or bracket is
/// read as punctuation.
pub fn words(message: &str, out: &mut String) {
    out.clear();
    words_to(message, out);
}

fn words_to(message: &str, out: &mut impl Sink) {
    let text = cut(message, MESSAGE_BYTES);
    let bytes = text.as_bytes();
    let (mut i, mut n) = (0usize, 0usize);
    let mut prev_word = false;
    while let Some(&b) = bytes.get(i) {
        if n >= MESSAGE_WORDS {
            break;
        }
        // What is quoted or bracketed, whole.
        if let Some(close) = match b {
            b'\'' | b'"' | b'`' if !prev_word => Some(b),
            b'(' => Some(b')'),
            b'[' => Some(b']'),
            b'{' => Some(b'}'),
            _ => None,
        } && let Some(end) = closing(bytes, i, b, close)
        {
            i = end + 1;
            prev_word = false;
            continue;
        }
        let Some(c) = text.get(i..).and_then(|t| t.chars().next()) else { break };
        if c.is_alphanumeric() {
            let start = i;
            let mut digit = false;
            let mut j = i;
            while let Some(c) = text.get(j..).and_then(|t| t.chars().next()).filter(|c| c.is_alphanumeric()) {
                digit |= c.is_numeric();
                j += c.len_utf8();
            }
            if !digit && let Some(w) = text.get(start..j) {
                if n > 0 {
                    out.push(' ');
                }
                out.push_str(w);
                n += 1;
            }
            i = j;
            prev_word = true;
        } else {
            i += c.len_utf8();
            prev_word = false;
        }
    }
}

/// Where what opens at `open` (`b`) closes (`close`): a quote's next of its kind that no word goes
/// on after; a bracket's match, those of its kind within it counted. None, unclosed.
fn closing(bytes: &[u8], open: usize, b: u8, close: u8) -> Option<usize> {
    let alnum = |at: usize| bytes.get(at).is_some_and(|c| c.is_ascii_alphanumeric() || *c >= 0x80);
    if b == close {
        let mut j = open + 1;
        while let Some(&c) = bytes.get(j) {
            if c == close && !alnum(j + 1) {
                return Some(j);
            }
            j += 1;
        }
        return None;
    }
    let mut depth = 0usize;
    for (j, &c) in bytes.iter().enumerate().skip(open) {
        if c == b {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(j);
            }
        }
    }
    None
}

/// `s` cut to `max` bytes at the most, at a character's boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("")
}

/// The stack's languages, by how their frames are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// `  File "/app/x.py", line 3, in f` (the innermost last).
    Python,
    /// `\tat a.b.C.m(C.java:12)`, `Caused by:` ending the error's own.
    Jvm,
    /// `    at f (/app/x.js:1:2)`, `    at /app/x.js:1:2`; Firefox's `f@/app/x.js:1:2`.
    JavaScript,
    /// `   at A.B.M(T x) in /app/X.cs:line 3`, the inner exceptions' first.
    DotNet,
    /// `main.f(...)` then `\t/app/x.go:3 +0x1d`.
    Go,
    /// `/app/x.rb:3:in 'f'`, `\tfrom /app/x.rb:3:in 'g'`.
    Ruby,
    /// `#0 /app/x.php(3): f()`.
    Php,
    /// `   0: x::f` then `             at ./src/x.rs:3:5`.
    Rust,
}

/// The frame an error is grouped by, from its stack: its innermost of the application's own, else
/// its innermost with a file and a function; none where its stack has no frame. The stack is
/// walked once, innermost first (Python's from its end), and its frames kept nowhere: to the first
/// of the application's (all of it for Go's, whose frames a panic's begin again).
pub fn frame(stack: &str) -> Option<Frame<'_>> {
    let (mut own, mut any) = (None, None);
    walk(stack, true, |lang, step| {
        match step {
            Step::Restart => (own, any) = (None, None),
            Step::Frame(f) if f.file.is_empty() => {}
            Step::Frame(f) => {
                any.get_or_insert(f);
                if own.is_none() && !library(lang, &f) {
                    own = Some(f);
                    if lang != Lang::Go {
                        return std::ops::ControlFlow::Break(());
                    }
                }
            }
        }
        std::ops::ControlFlow::Continue(())
    })?;
    own.or(any)
}

/// The frame of frames listed innermost first, as [`frame`] picks it: the first of the
/// application's, else the first with a file.
#[cfg(test)]
fn pick<'a, 'b>(lang: Lang, innermost_first: impl Iterator<Item = &'b Frame<'a>>) -> Option<Frame<'a>>
where
    'a: 'b,
{
    let mut first = None;
    for f in innermost_first {
        if f.file.is_empty() {
            continue;
        }
        if !library(lang, f) {
            return Some(*f);
        }
        first.get_or_insert(*f);
    }
    first
}

/// A stack's language and its frames as written (its error's own: an inner error's, or a cause's,
/// left out), each with its file (empty where it has none: a builtin's) and its function.
pub fn frames(stack: &str) -> Option<(Lang, Vec<Frame<'_>>)> {
    let mut out = Vec::new();
    let lang = walk(stack, false, |_, step| {
        match step {
            Step::Frame(f) => out.push(f),
            Step::Restart => out.clear(),
        }
        std::ops::ControlFlow::Continue(())
    })?;
    (!out.is_empty()).then_some((lang, out))
}

/// A step of a stack's walk: a frame as written; or a restart, the frames before it not the
/// error's (a Go panic's: what recovered it, before its `panic(`).
#[derive(Debug, Clone, Copy)]
enum Step<'a> {
    Frame(Frame<'a>),
    Restart,
}

/// A stack's language, its frames told to `visit` as written (its error's own: an inner error's,
/// or a cause's, left out) until it breaks; with `innermost_first`, Python's from its end (a frame
/// of Python's is a line of its own, so its last is its innermost). None where no line is a frame.
fn walk<'a>(stack: &'a str, innermost_first: bool, mut visit: impl FnMut(Lang, Step<'a>) -> std::ops::ControlFlow<()>) -> Option<Lang> {
    let whole = cut(stack, STACK_BYTES);
    // .NET: the error's own frames follow its inner exceptions' (where it has any).
    let text = match whole.rfind("--- End of inner exception stack trace ---") {
        Some(at) => whole.get(at..).and_then(|t| t.split_once('\n')).map_or("", |(_, rest)| rest),
        None => whole,
    };
    let (text, lang) = match text.lines().find_map(lang_of) {
        Some(lang) => (text, lang),
        None => (whole, whole.lines().find_map(lang_of)?),
    };
    match lang {
        Lang::Go => go_walk(text, |step| visit(Lang::Go, step)),
        Lang::Rust => rust_walk(text, |step| visit(Lang::Rust, step)),
        Lang::Python if innermost_first => {
            for f in text.lines().rev().filter_map(|line| python(line.trim_start())) {
                if visit(lang, Step::Frame(f)).is_break() {
                    break;
                }
            }
        }
        _ => {
            let mut ruby_lines = 0;
            for line in text.lines() {
                let t = line.trim_start();
                if lang == Lang::Jvm && t.starts_with("Caused by:") {
                    break;
                }
                let frame = match lang {
                    Lang::Python => python(t),
                    Lang::Jvm => jvm(t),
                    Lang::JavaScript => javascript(t),
                    Lang::DotNet => dotnet(t),
                    Lang::Php => php(t),
                    Lang::Ruby => {
                        // A cause's frames begin with a header line of their own (no `from`).
                        let f = ruby(t);
                        if f.is_some() && !t.starts_with("from ") {
                            ruby_lines += 1;
                            if ruby_lines > 1 {
                                break;
                            }
                        }
                        f
                    }
                    Lang::Go | Lang::Rust => None,
                };
                if let Some(f) = frame
                    && visit(lang, Step::Frame(f)).is_break()
                {
                    break;
                }
            }
        }
    }
    Some(lang)
}

/// The language a line is a frame of, if it is one.
fn lang_of(line: &str) -> Option<Lang> {
    let t = line.trim_start();
    if python(t).is_some() {
        return Some(Lang::Python);
    }
    if t.starts_with('#') && php(t).is_some() {
        return Some(Lang::Php);
    }
    if let Some(rest) = t.strip_prefix("at ") {
        if rest.contains(") in ") && rest.contains(":line ") {
            return Some(Lang::DotNet);
        }
        if let Some((name, loc)) = call(rest)
            && !name.contains(' ')
        {
            return Some(if jvm_location(loc) { Lang::Jvm } else { Lang::DotNet });
        }
        return javascript(t).map(|_| Lang::JavaScript);
    }
    if line.starts_with('\t') && go_location(t).is_some() {
        return Some(Lang::Go);
    }
    if ruby(t).is_some() {
        return Some(Lang::Ruby);
    }
    if rust_number(t).is_some() {
        return Some(Lang::Rust);
    }
    if firefox(t).is_some() {
        return Some(Lang::JavaScript);
    }
    None
}

/// `File "/app/x.py", line 3, in f`.
fn python(t: &str) -> Option<Frame<'_>> {
    let rest = t.strip_prefix("File \"")?;
    let (file, rest) = rest.split_once("\", line ")?;
    let (_, function) = rest.split_once(", in ")?;
    Some(Frame { file, function: function.trim_end() })
}

/// `name(location)`, its name before the first `(` at the top level and its location within the
/// last `)`.
fn call(rest: &str) -> Option<(&str, &str)> {
    let open = rest.find('(')?;
    let close = rest.rfind(')')?;
    (close > open && rest.get(close + 1..).is_some_and(|after| after.trim().is_empty())).then(|| (rest.get(..open).unwrap_or(""), rest.get(open + 1..close).unwrap_or("")))
}

/// A JVM frame's location: `C.java:12`, `C.kt`, `Native Method`, `Unknown Source`.
fn jvm_location(loc: &str) -> bool {
    if matches!(loc, "Native Method" | "Unknown Source") || loc.starts_with("Unknown Source:") {
        return true;
    }
    let file = loc.split_once(':').map_or(loc, |(f, line)| if line.bytes().all(|b| b.is_ascii_digit()) { f } else { "" });
    [".java", ".kt", ".scala", ".groovy", ".clj", ".jav"].iter().any(|ext| file.ends_with(ext))
}

/// `at a.b.C.m(C.java:12)`: its function the class's and the method's (a module's prefix, `java.base/`,
/// left out), its file the location's.
fn jvm(t: &str) -> Option<Frame<'_>> {
    let (name, loc) = call(t.strip_prefix("at ")?)?;
    // A module's prefix (`java.base/`, `app@1.0/`), not a generated class's `$$Lambda$14/0x…`.
    let function = name.split_once('/').filter(|(module, _)| !module.contains('$')).map_or(name, |(_, f)| f);
    let file = if jvm_location(loc) && !matches!(loc, "Native Method" | "Unknown Source") { loc.split_once(':').map_or(loc, |(f, _)| f) } else { "" };
    Some(Frame { file, function })
}

/// `at f (/app/x.js:1:2)`, `at async f (...)`, `at new F (...)`, `at /app/x.js:1:2` (a function
/// without a name), `at JSON.parse (<anonymous>)` (a builtin's: no file).
fn javascript(t: &str) -> Option<Frame<'_>> {
    let rest = t.strip_prefix("at ")?;
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let rest = rest.strip_prefix("new ").unwrap_or(rest);
    let (function, loc) = match rest.strip_suffix(')').and_then(|r| r.rsplit_once(" (")) {
        Some((f, loc)) => (f, loc),
        None => (ANONYMOUS, rest),
    };
    let file = js_file(loc).unwrap_or("");
    if file.is_empty() && function == ANONYMOUS {
        return None;
    }
    Some(Frame { file, function })
}

/// A JavaScript location's file: its `:line:column` left off; none for `<anonymous>`, `native`.
fn js_file(loc: &str) -> Option<&str> {
    let (rest, col) = loc.rsplit_once(':')?;
    let (file, line) = rest.rsplit_once(':')?;
    (col.bytes().all(|b| b.is_ascii_digit()) && line.bytes().all(|b| b.is_ascii_digit()) && !file.is_empty()).then_some(file)
}

/// Firefox's and Safari's `f@/app/x.js:1:2`.
fn firefox(t: &str) -> Option<Frame<'_>> {
    let (function, loc) = t.split_once('@')?;
    let file = js_file(loc)?;
    (!function.contains(' ')).then_some(Frame { file, function: if function.is_empty() { ANONYMOUS } else { function } })
}

/// `at A.B.M(T x) in /app/X.cs:line 3`; a framework's `at A.B.M(T x)` (no file).
fn dotnet(t: &str) -> Option<Frame<'_>> {
    let rest = t.strip_prefix("at ")?;
    let (method, file) = match rest.rsplit_once(") in ") {
        Some((m, loc)) => (m, loc.rsplit_once(":line ").map_or(loc, |(f, _)| f)),
        None => (rest.strip_suffix(')')?, ""),
    };
    let function = method.split_once('(').map_or(method, |(f, _)| f);
    Some(Frame { file, function })
}

/// `#0 /app/x.php(3): f()`; `#1 [internal function]: f()` (no file); `#2 {main}` none.
fn php(t: &str) -> Option<Frame<'_>> {
    let rest = t.strip_prefix('#')?;
    let (n, rest) = rest.split_once(' ')?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (loc, call) = rest.split_once("): ").or_else(|| rest.strip_prefix("[internal function]: ").map(|c| ("", c)))?;
    let file = loc.rsplit_once('(').map_or("", |(f, _)| f);
    let function = call.split_once('(').map_or(call, |(f, _)| f);
    Some(Frame { file, function })
}

/// `/app/x.rb:3:in 'f'` (Ruby 3.4), `` /app/x.rb:3:in `f' `` (before), the first with its message
/// after it; `from ` before each after the first.
fn ruby(t: &str) -> Option<Frame<'_>> {
    let t = t.strip_prefix("from ").unwrap_or(t);
    let at = t.find(":in '").or_else(|| t.find(":in `"))?;
    let (head, rest) = (t.get(..at)?, t.get(at + 5..)?);
    let (file, line) = head.rsplit_once(':')?;
    if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let function = rest.split_once('\'')?.0;
    Some(Frame { file, function })
}

/// A Go frame's location line: `\t/app/x.go:3` (dd-trace-go's), `\t/app/x.go:3 +0x1d` (a panic's).
fn go_location(t: &str) -> Option<&str> {
    let loc = t.split_once(" +0x").map_or(t, |(l, _)| l);
    let (file, line) = loc.rsplit_once(':')?;
    (!line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) && (file.contains('/') || file.ends_with(".go"))).then_some(file)
}

/// Go's frames: each a function's line, then its location's (a tab before it); a panic's frames
/// those after its `panic(`, what recovered it before (a restart).
fn go_walk<'a>(text: &'a str, mut visit: impl FnMut(Step<'a>) -> std::ops::ControlFlow<()>) {
    let mut function: Option<&str> = None;
    for line in text.lines() {
        if let Some(loc) = line.strip_prefix('\t') {
            if let (Some(f), Some(file)) = (function.take(), go_location(loc)) {
                let step =
                    if f.starts_with("panic(") { Step::Restart } else { Step::Frame(Frame { file, function: f.rsplit_once('(').filter(|(_, args)| args.ends_with(')')).map_or(f, |(name, _)| name) }) };
                if visit(step).is_break() {
                    return;
                }
            }
        } else if !line.trim().is_empty() && !line.starts_with("goroutine ") {
            function = Some(line.trim());
        }
    }
}

/// `   0: x::f`: its number's line.
fn rust_number(t: &str) -> Option<&str> {
    let (n, function) = t.split_once(": ")?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(function)
}

/// Rust's frames: each a numbered function's line, then (where its file is known) `at file:line:col`:
/// a frame told once the next begins, or the stack ends.
fn rust_walk<'a>(text: &'a str, mut visit: impl FnMut(Step<'a>) -> std::ops::ControlFlow<()>) {
    let mut pending: Option<Frame<'a>> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(function) = rust_number(t) {
            if let Some(done) = pending.replace(Frame { file: "", function })
                && visit(Step::Frame(done)).is_break()
            {
                return;
            }
        } else if let Some(loc) = t.strip_prefix("at ")
            && let Some(last) = pending.as_mut()
            && last.file.is_empty()
        {
            let mut file = loc;
            for _ in 0..2 {
                if let Some((f, n)) = file.rsplit_once(':')
                    && !n.is_empty()
                    && n.bytes().all(|b| b.is_ascii_digit())
                {
                    file = f;
                }
            }
            last.file = file;
        }
    }
    if let Some(done) = pending {
        let _ = visit(Step::Frame(done));
    }
}

/// Paths of what is not the application's: a language's own library and runtime, and what it
/// installed (packages, modules, gems, vendored code).
const LIBRARY_PATHS: [&str; 20] = [
    "/site-packages/",
    "/dist-packages/",
    "/lib/python",
    "<frozen ",
    "/node_modules/",
    "node:",
    "/usr/local/go/src/",
    "/usr/lib/go",
    "/pkg/mod/",
    "/vendor/",
    "<internal:",
    "/gems/",
    "/lib/ruby/",
    "/rustc/",
    "/.cargo/registry/",
    "/.cargo/git/",
    "/.rustup/",
    "/usr/share/dotnet/",
    "<anonymous>",
    "[native code]",
];

/// Functions of what is not the application's, by their names: the JVM's and .NET's libraries and
/// Rust's.
const LIBRARY_FUNCTIONS: [&str; 18] =
    ["java.", "javax.", "jdk.", "sun.", "com.sun.", "kotlin.", "kotlinx.", "scala.", "groovy.", "System.", "Microsoft.", "std::", "core::", "alloc::", "<std::", "<core::", "<alloc::", "__rust"];

/// Whether a frame is a library's (a language's own, or what the application installed), not the
/// application's.
fn library(lang: Lang, f: &Frame<'_>) -> bool {
    if LIBRARY_PATHS.iter().any(|p| f.file.contains(p)) || f.file.starts_with("internal/") || LIBRARY_FUNCTIONS.iter().any(|p| f.function.starts_with(p)) {
        return true;
    }
    // Go's standard library: a package whose path's first element has no dot (`net/http`, not
    // `github.com/…`), not `main`, its file in its package's place under `src/` (or there, a
    // `-trimpath` build's).
    if lang == Lang::Go {
        let last = f.function.rfind('/').map_or(0, |i| i + 1);
        let dot = f.function.get(last..).and_then(|t| t.find('.')).map_or(f.function.len(), |d| last + d);
        let package = f.function.get(..dot).unwrap_or("");
        let first = package.split('/').next().unwrap_or("");
        let placed = f.file.rsplit_once("/src/").map_or(f.file, |(_, rest)| rest);
        return !first.is_empty() && !first.contains('.') && package != "main" && placed.strip_prefix(package).is_some_and(|r| r.starts_with('/'));
    }
    false
}

/// A frame's file as an issue knows it: its name alone (where a build puts it varies), with what
/// varies from build to build left out ([`mask`]).
pub fn normal_file(file: &str, out: &mut String) {
    out.clear();
    normal_file_to(file, out);
}

fn normal_file_to(file: &str, out: &mut impl Sink) {
    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
    mask_to(name, out);
}

/// A frame's function as an issue knows it: its arguments left off (`f(0x1d)`, `f(42)`), what a
/// compiler numbers left unnumbered (Java's `lambda$main$3` and `$$Lambda$14/0x…`, .NET's
/// `<Main>b__5_0` and `<F>d__3`, Go's `func2`, Rust's `{closure#1}` and `::h0123…`, PHP's
/// `{closure:/app/x.php:30}`), and what else varies masked ([`mask`]).
pub fn normal_function(function: &str, out: &mut String) {
    out.clear();
    normal_function_to(function, out);
}

/// A compiler's numbering: a run of digits after these, left out.
const NUMBERED: [&str; 7] = ["lambda$", "$Lambda$", "b__", "d__", ".func", "{closure#", "DisplayClass"];

/// [`normal_function`]'s, a copy of the function made only where it names a PHP closure's place or
/// a compiler numbers it.
fn normal_function_to(function: &str, out: &mut impl Sink) {
    let mut f = function.trim();
    // Rust's legacy symbols' hash.
    if let Some((head, h)) = f.rsplit_once("::h")
        && h.len() == 16
        && h.bytes().all(|b| b.is_ascii_hexdigit())
    {
        f = head;
    }
    let closed: std::borrow::Cow<'_, str> = if f.contains("{closure:") { std::borrow::Cow::Owned(closures(f)) } else { std::borrow::Cow::Borrowed(f) };
    let plain: std::borrow::Cow<'_, str> = if NUMBERED.iter().any(|p| closed.contains(p)) { std::borrow::Cow::Owned(unnumbered(&closed)) } else { closed };
    mask_to(&plain, out);
}

/// PHP's closures name their place: `{closure:/app/x.php:30}` as `{closure}`.
fn closures(f: &str) -> String {
    let mut s = String::with_capacity(f.len());
    let mut rest = f;
    while let Some(at) = rest.find("{closure:") {
        s.push_str(rest.get(..at).unwrap_or(""));
        s.push_str("{closure}");
        let after = rest.get(at..).unwrap_or("");
        let mut depth = 0usize;
        let mut end = after.len();
        for (i, c) in after.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = after.get(end..).unwrap_or("");
    }
    s.push_str(rest);
    s
}

/// A compiler's numbering ([`NUMBERED`]) left out.
fn unnumbered(s: &str) -> String {
    let mut numbered = String::with_capacity(s.len());
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        if let Some(p) = NUMBERED.iter().find(|p| s.get(i..).is_some_and(|t| t.starts_with(**p))) {
            numbered.push_str(p);
            i += p.len();
            // Java's `lambda$main$3`: its method's name, then its number.
            if *p == "lambda$"
                && let Some(end) = s.get(i..).and_then(|t| t.find('$'))
            {
                numbered.push_str(s.get(i..i + end).unwrap_or(""));
                i += end + 1;
            }
            while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'_') {
                i += 1;
            }
            // `$$Lambda$14/0x0000000800c0b000`: its address too.
            if s.get(i..).is_some_and(|t| t.starts_with("/0x")) {
                i += 3;
                while b.get(i).is_some_and(u8::is_ascii_hexdigit) {
                    i += 1;
                }
            }
            continue;
        }
        if let Some(c) = s.get(i..).and_then(|t| t.chars().next()) {
            numbered.push(c);
            i += c.len_utf8();
        } else {
            break;
        }
    }
    numbered
}

/// `s` into `out`, each run of letters and digits that is a hash, an address or a timestamp (six
/// or more hex digits, a digit among them) and each version (digits, then `.` and digits, twice
/// at the least: `1.2.3`) as `*`.
pub fn mask(s: &str, out: &mut String) {
    mask_to(s, out);
}

fn mask_to(s: &str, out: &mut impl Sink) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        if b.get(i).is_some_and(u8::is_ascii_alphanumeric) {
            while b.get(i).is_some_and(u8::is_ascii_alphanumeric) {
                i += 1;
            }
            let run = b.get(start..i).unwrap_or_default();
            // A version: digits and dots, two dots at the least.
            if run.iter().all(u8::is_ascii_digit) {
                let mut j = i;
                let mut dots = 0;
                while b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                    while b.get(j).is_some_and(u8::is_ascii_digit) {
                        j += 1;
                    }
                    dots += 1;
                }
                if dots >= 2 {
                    out.push('*');
                    i = j;
                    continue;
                }
            }
            if run.len() >= 6 && run.iter().all(u8::is_ascii_hexdigit) && run.iter().any(u8::is_ascii_digit) {
                out.push('*');
            } else {
                out.push_str(s.get(start..i).unwrap_or(""));
            }
        } else if let Some(c) = s.get(i..).and_then(|t| t.chars().next()) {
            out.push(c);
            i += c.len_utf8();
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The errors' metric of a shared store, read a round of series at a time
    /// (`query::eval::Source`), reads as one held throughout does: every issue's series' errors and
    /// when (past two rounds of them), a few issues' alone, an issue's timeline, over a range read
    /// as samples and one read from rollups.
    #[test]
    fn the_errors_metric_read_a_round_at_a_time_reads_as_held() {
        use crate::batch::{Builder, Kind, SeriesKey};
        let mut st = crate::store::Store::new(crate::hash::Keyed::new());
        let mut b = Builder::new();
        for i in 0..150u32 {
            let issue = hex(u64::from(i) * 7_919);
            let k = SeriesKey::new(METRIC, [("issue.id", issue.as_str()), ("service", ["web", "api"][(i % 2) as usize]), ("env", "prod"), ("version", "1.2")], Kind::Count).unwrap();
            b.define(&k);
            b.run(k.fp, (0..360i64).map(|j| (j * 10_000, f64::from((i + j as u32) % 3))));
        }
        st.apply(&b.finish()).unwrap();
        let some = [hex(7_919), hex(2 * 7_919)];
        let some: Vec<&str> = some.iter().map(String::as_str).collect();
        for (from, to) in [(0, 300_000), (0, 3_600_000)] {
            let held = (seen(&st, &[], from, to).unwrap(), seen_of(&st, &some, from, to).unwrap(), timeline(&st, &hex(3 * 7_919), from, to, 60_000).unwrap());
            assert!(held.0.len() > 2 * crate::query::eval::ROUND, "{} series with errors", held.0.len());
            let shared = std::sync::RwLock::new(std::mem::take(&mut st));
            let apart = (seen(&shared, &[], from, to).unwrap(), seen_of(&shared, &some, from, to).unwrap(), timeline(&shared, &hex(3 * 7_919), from, to, 60_000).unwrap());
            assert_eq!(apart, held, "{from}..{to}");
            st = shared.into_inner().unwrap();
        }
    }

    /// A fixture's cases: each `---- name` and its stack.
    fn cases(text: &str) -> Vec<(&str, &str)> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("---- ") {
            let after = &rest[at + 5..];
            let (name, body) = after.split_once('\n').unwrap();
            let end = body.find("\n---- ").map_or(body.len(), |e| e + 1);
            out.push((name.trim(), &body[..end]));
            rest = &body[end..];
        }
        out
    }

    fn frame_of(stack: &str) -> (String, String) {
        let f = frame(stack).unwrap_or_else(|| panic!("no frame in\n{stack}"));
        let (mut file, mut function) = (String::new(), String::new());
        normal_file(f.file, &mut file);
        normal_function(f.function, &mut function);
        (file, function)
    }

    /// Each language's stacks, as its runtime wrote them (`fixtures/errors/`): the frame each is
    /// grouped by, the innermost of the application's own, as an issue knows it.
    #[test]
    fn each_language_s_stack_gives_the_application_s_innermost_frame() {
        let want: &[(&str, &str, &[(&str, &str, &str)])] = &[
            (
                "python",
                include_str!("fixtures/errors/python.txt"),
                &[
                    ("app", "errors.py", "total"),
                    ("app-again", "errors.py", "total"),
                    // json's own frames passed over: where the application called it.
                    ("stdlib", "errors.py", "parse"),
                    // The error raised (the last traceback), not its cause.
                    ("chained", "errors.py", "load"),
                    ("lambda", "errors.py", "<lambda>"),
                ],
            ),
            (
                "node",
                include_str!("fixtures/errors/node.txt"),
                &[
                    ("app", "errors.js", "Cart.total"),
                    ("app-again", "errors.js", "Cart.total"),
                    ("async", "errors.js", "fetchOrder"),
                    // `JSON.parse (<anonymous>)`: a builtin's, no file.
                    ("builtin", "errors.js", "parse"),
                    ("anonymous", "errors.js", ANONYMOUS),
                ],
            ),
            (
                "go",
                include_str!("fixtures/errors/go.txt"),
                &[
                    ("app", "errors.go", "main.checkout"),
                    ("app-again", "errors.go", "main.checkout"),
                    // After `panic(`: where it panicked, not what recovered it; its closure unnumbered.
                    ("panic", "errors.go", "main.handler.func"),
                    ("parse", "errors.go", "main.parse"),
                ],
            ),
            (
                "java",
                include_str!("fixtures/errors/java.txt"),
                &[
                    ("app", "Errors.java", "Errors$Cart.total"),
                    ("app-again", "Errors.java", "Errors$Cart.total"),
                    // The JDK's frames (`java.base/java.lang.Integer.parseInt`) passed over.
                    ("jdk", "Errors.java", "Errors.parse"),
                    // The error's own frames, not its cause's (`Caused by:`).
                    ("chained", "Errors.java", "Errors.load"),
                    ("lambda", "Errors.java", "Errors.lambda$main"),
                ],
            ),
            (
                "ruby",
                include_str!("fixtures/errors/ruby.txt"),
                &[
                    ("app", "errors.rb", "block in Cart#total"),
                    ("app-again", "errors.rb", "block in Cart#total"),
                    // `<internal:kernel>`: Ruby's own.
                    ("core", "errors.rb", "Object#parse"),
                    ("chained", "errors.rb", "Object#load"),
                    ("block", "errors.rb", "Integer#/"),
                ],
            ),
            (
                "php",
                include_str!("fixtures/errors/php.txt"),
                &[
                    ("app", "errors.php", "Shop\\Cart->total"),
                    ("app-string", "errors.php", "Shop\\Cart->total"),
                    ("app-again", "errors.php", "Shop\\Cart->total"),
                    ("app-again-string", "errors.php", "Shop\\Cart->total"),
                    ("builtin", "errors.php", "json_decode"),
                    ("builtin-string", "errors.php", "json_decode"),
                    ("closure", "errors.php", "intdiv"),
                    ("closure-string", "errors.php", "intdiv"),
                ],
            ),
            (
                "dotnet",
                include_str!("fixtures/errors/dotnet.txt"),
                &[
                    ("app", "Program.cs", "Shop.Cart.Total"),
                    ("app-again", "Program.cs", "Shop.Cart.Total"),
                    // The framework's frames have no file.
                    ("bcl", "Program.cs", "Shop.Program.Parse"),
                    ("async", "Program.cs", "Shop.Program.Fetch"),
                    // The outer exception's frames, after its inner one's.
                    ("chained", "Program.cs", "Shop.Program.Load"),
                    ("lambda", "Program.cs", "Shop.Program.<>c.<Main>b__"),
                ],
            ),
            (
                "rust",
                include_str!("fixtures/errors/rust.txt"),
                &[
                    ("app", "errors.rs", "<errors::Cart>::total"),
                    ("app-again", "errors.rs", "<errors::Cart>::total"),
                    ("parse", "errors.rs", "errors::parse::{closure#}"),
                    ("closure", "errors.rs", "errors::main::{closure#}"),
                ],
            ),
        ];
        for (lang, text, expected) in want {
            let got = cases(text);
            assert_eq!(got.len(), expected.len(), "{lang}: {:?}", got.iter().map(|c| c.0).collect::<Vec<_>>());
            for ((name, stack), (want_name, file, function)) in got.iter().zip(expected.iter()) {
                assert_eq!(name, want_name, "{lang}");
                assert_eq!(frame_of(stack), ((*file).to_owned(), (*function).to_owned()), "{lang} {name}");
            }
        }
    }

    #[test]
    fn a_message_s_words_are_datadog_s() {
        let w = |m: &str| {
            let mut out = String::new();
            words(m, &mut out);
            out
        };
        assert_eq!(w("bad item -42 in cart 'c--294'"), "bad item in cart");
        assert_eq!(w("bad item -7 in cart \"c--49\""), "bad item in cart");
        assert_eq!(w("Expected property name or '}' in JSON at position 1 (line 1 column 2)"), "Expected property name or in JSON at position");
        assert_eq!(w("invalid value for Integer(): \"x1\""), "invalid value for Integer");
        assert_eq!(w("order 550e8400-e29b-41d4-a716-446655440000 timed out after 30s [retry 3]"), "order timed out after");
        assert_eq!(w("user42 can't reach {\"host\": \"db-1\"}"), "can t reach");
        // Unclosed: punctuation, the rest read.
        assert_eq!(w("missing ( here and 'there"), "missing here and there");
        assert_eq!(w("Ünïcode wörds ok"), "Ünïcode wörds ok");
        assert_eq!(w(""), "");
        let long = "word ".repeat(1_000);
        assert_eq!(w(&long).split(' ').count(), MESSAGE_WORDS);
    }

    #[test]
    fn what_varies_from_build_to_build_is_masked() {
        let f = |s: &str| {
            let mut out = String::new();
            normal_file(s, &mut out);
            out
        };
        let g = |s: &str| {
            let mut out = String::new();
            normal_function(s, &mut out);
            out
        };
        assert_eq!(f("/app/releases/20240101123456/app/models/user.rb"), "user.rb");
        assert_eq!(f("/static/js/main.3f2a9b1c.js"), "main.*.js");
        assert_eq!(f("C:\\app\\Cart.cs"), "Cart.cs");
        assert_eq!(f("lodash-4.17.21.js"), "lodash-*.js");
        assert_eq!(g("Errors.lambda$main$4"), "Errors.lambda$main");
        assert_eq!(g("com.shop.Cart$$Lambda$14/0x0000000800c0b000.apply"), "com.shop.Cart$$Lambda$.apply");
        assert_eq!(g("Shop.Program.<Fetch>d__3.MoveNext"), "Shop.Program.<Fetch>d__.MoveNext");
        assert_eq!(g("Shop.Program.<>c__DisplayClass2_0.<Main>b__1"), "Shop.Program.<>c__DisplayClass.<Main>b__");
        assert_eq!(g("main.handler.func12"), "main.handler.func");
        assert_eq!(g("errors::main::{closure#1}"), "errors::main::{closure#}");
        assert_eq!(g("myapp::cart::total::h0123456789abcdef"), "myapp::cart::total");
        assert_eq!(g("{closure:{closure:/app/errors.php:30}:30}"), "{closure}");
        assert_eq!(g("Object.<anonymous>"), "Object.<anonymous>");
    }

    fn err<'a>(kind: &'a str, message: &'a str, stack: &'a str) -> Error<'a> {
        Error { kind: Some(kind), message: Some(message), stack: Some(stack), fingerprint: None }
    }

    /// One issue for an error thrown at one place whatever its message's variables; another for
    /// another place, type or service; a span's apart from a log's; a user's fingerprint its own.
    #[test]
    fn errors_alike_are_one_issue_and_errors_apart_are_not() {
        let py = cases(include_str!("fixtures/errors/python.txt"));
        let stack = |n: &str| py.iter().find(|c| c.0 == n).unwrap().1;
        let issue = |service: &str, e: &Error<'_>| span_issue(service, true, e).unwrap();
        let a = issue("shop", &err("ValueError", "bad item -42 in cart 'c--294'", stack("app")));
        assert_eq!(a, issue("shop", &err("ValueError", "bad item -7 in cart 'c--49'", stack("app-again"))), "its variables and its call's lines differ");
        assert_ne!(a, issue("cart", &err("ValueError", "bad item -42 in cart 'c--294'", stack("app"))), "another service");
        assert_ne!(a, issue("shop", &err("TypeError", "bad item -42 in cart 'c--294'", stack("app"))), "another type");
        assert_ne!(a, issue("shop", &err("ValueError", "bad item -42 in cart 'c--294'", stack("chained"))), "another place");
        assert_ne!(a, issue("shop", &err("ValueError", "no such cart", stack("app"))), "another message");
        let log = log_issue("shop", 17, &err("ValueError", "bad item -42 in cart 'c--294'", stack("app"))).unwrap();
        assert_ne!(a, log, "a log's issue is not a span's");
        let fp = Error { fingerprint: Some("cart-failures"), ..err("ValueError", "x", stack("app")) };
        let other = Error { fingerprint: Some("cart-failures"), ..err("KeyError", "y", stack("chained")) };
        assert_eq!(issue("shop", &fp), issue("shop", &other), "a user's grouping");
        assert_ne!(issue("shop", &fp), issue("cart", &fp), "still each service's own");
        assert_eq!(hex(a).len(), 16);
    }

    /// Datadog's: a span's error is an issue only at its service's entry and with its type, message
    /// and stack (two lines, a frame); a log's with an error's status, a service, and a kind or a
    /// stack with a frame.
    #[test]
    fn only_the_errors_datadog_tracks_are_issues() {
        let stack = cases(include_str!("fixtures/errors/java.txt"))[0].1;
        let e = err("java.lang.IllegalStateException", "bad item", stack);
        assert!(span_issue("shop", true, &e).is_some());
        assert!(span_issue("shop", false, &e).is_none(), "not its service's entry");
        assert!(span_issue("", true, &e).is_none(), "no service");
        assert!(span_issue("shop", true, &Error { stack: None, ..e }).is_none(), "no stack");
        assert!(span_issue("shop", true, &Error { message: None, ..e }).is_none(), "no message");
        assert!(span_issue("shop", true, &Error { stack: Some("java.lang.IllegalStateException: bad item"), ..e }).is_none(), "one line");
        assert!(span_issue("shop", true, &Error { stack: Some("one\ntwo"), ..e }).is_none(), "no frame");
        assert!(log_issue("shop", 17, &e).is_some());
        assert!(log_issue("shop", 13, &e).is_none(), "a warning");
        assert!(log_issue("", 17, &e).is_none(), "no service");
        assert!(log_issue("shop", 21, &Error { kind: Some("Timeout"), message: None, stack: None, fingerprint: None }).is_some(), "a kind alone");
        assert!(log_issue("shop", 17, &Error { kind: None, message: Some("bad"), stack: None, fingerprint: None }).is_none(), "a message alone");
        assert!(log_issue("shop", 17, &Error { kind: None, message: None, stack: Some(stack), fingerprint: None }).is_some(), "a stack with a frame");
    }

    #[test]
    fn an_error_s_parts_are_read_from_datadog_s_attributes_then_opentelemetry_s() {
        let e = Error::of([("exception.type", "otel.Type"), ("error.type", "dd.Type"), ("exception.message", "m"), ("error.msg", "old"), ("error.stack", " "), ("exception.stacktrace", "s")]);
        assert_eq!((e.kind, e.message, e.stack, e.fingerprint), (Some("dd.Type"), Some("old"), Some("s"), None));
        let e = Error::of([("error.kind", "k"), ("error.message", "m"), ("error.fingerprint", "f")]);
        assert_eq!((e.kind, e.message, e.fingerprint), (Some("k"), Some("m"), Some("f")));
        assert!(!Error::of([("http.status_code", "500")]).any());
    }

    /// A stack past its bound keeps both its ends, whole lines: Python's innermost frame (its last)
    /// and the JVM's (its first) both kept; one within it, whole.
    #[test]
    fn a_long_stack_keeps_both_its_ends() {
        let mut py = String::from("Traceback (most recent call last):\n");
        for i in 0..2_000 {
            py.push_str(&format!("  File \"/usr/lib/python3.12/site-packages/lib{i}.py\", line {i}, in f{i}\n    call()\n"));
        }
        py.push_str("  File \"/app/cart.py\", line 9, in total\n    raise ValueError(x)\nValueError: bad item\n");
        let (head, tail) = stack_ends(&py, STACK_VALUE_BYTES);
        assert!(head.len() + STACK_GAP.len() + tail.len() <= STACK_VALUE_BYTES);
        assert!(head.starts_with("Traceback") && !head.ends_with('\n') && tail.starts_with("  ") && tail.ends_with("ValueError: bad item\n"), "whole lines");
        let kept = format!("{head}{STACK_GAP}{tail}");
        assert_eq!(frame(&kept), Some(Frame { file: "/app/cart.py", function: "total" }));
        let short = "a\nb\n";
        assert_eq!(stack_ends(short, STACK_VALUE_BYTES), (short, ""));
    }

    /// A stack of megabytes is read to its first [`STACK_BYTES`]: its frame where a frame is.
    #[test]
    fn a_huge_stack_is_read_in_bounded_work() {
        let mut s = String::from("java.lang.StackOverflowError\n");
        for i in 0..200_000 {
            s.push_str(&format!("\tat com.shop.Deep.f{i}(Deep.java:{i})\n"));
        }
        let f = frame(&s).unwrap();
        assert_eq!((f.file, f.function), ("Deep.java", "com.shop.Deep.f0"));
    }

    /// Every fixture's stacks, by language.
    const ALL: [(&str, &str); 8] = [
        ("python", include_str!("fixtures/errors/python.txt")),
        ("node", include_str!("fixtures/errors/node.txt")),
        ("go", include_str!("fixtures/errors/go.txt")),
        ("java", include_str!("fixtures/errors/java.txt")),
        ("ruby", include_str!("fixtures/errors/ruby.txt")),
        ("php", include_str!("fixtures/errors/php.txt")),
        ("dotnet", include_str!("fixtures/errors/dotnet.txt")),
        ("rust", include_str!("fixtures/errors/rust.txt")),
    ];

    /// Issue ids are kept (in issues.json, written on every error): how they are made never
    /// changes them. Every fixture's issue, of a span and of a log, as the first build made them.
    #[test]
    fn issue_ids_are_frozen() {
        let mut digest = 0u64;
        let mut n = 0;
        for (_, text) in ALL {
            for (_, stack) in cases(text) {
                let e = Error { kind: Some("ValueError"), message: Some("bad item -42 in cart 'c-294' after 3 retries [eu-1] (timeout)"), stack: Some(stack), fingerprint: None };
                let (span, log) = (span_issue("shop", true, &e).unwrap(), log_issue("shop", 17, &e).unwrap());
                digest = digest.rotate_left(7) ^ span ^ log.rotate_left(29);
                n += 1;
            }
        }
        let custom = span_issue("shop", true, &Error { kind: Some("E"), message: None, stack: None, fingerprint: Some("checkout-timeouts") }).unwrap();
        // Made as they are read, the same as made of their parts whole: each stack's frame walked
        // is its list's, and its id the hash of its words, file and function made first.
        for (_, text) in ALL {
            for (name, stack) in cases(text) {
                let listed = frames(stack).and_then(|(lang, fs)| if lang == Lang::Python { pick(lang, fs.iter().rev()) } else { pick(lang, fs.iter()) });
                assert_eq!(frame(stack), listed, "{name}");
                for message in ["bad item -42 in cart 'c-294'", "", "order 550e8400-e29b-41d4-a716-446655440000 failed after 3 retries (timeout 30s) [shard eu-1] \u{e9}t\u{e9}"] {
                    let (mut w, mut file, mut function) = (String::new(), String::new(), String::new());
                    words(message, &mut w);
                    let f = frame(stack);
                    if let Some(f) = f {
                        normal_file(f.file, &mut file);
                        normal_function(f.function, &mut function);
                    }
                    let whole = stable(&[b"kiln.issue.v1", Source::Log.tag(), b"shop", b"ValueError", w.as_bytes(), file.as_bytes(), function.as_bytes()]);
                    assert_eq!(grouped(Source::Log, "shop", " ValueError ", message, f), whole, "{name}: {message}");
                }
            }
        }
        println!("{n} stacks: digest {digest:#018x}, custom {custom:#018x}");
        assert_eq!((n, digest, custom), (42, 0xd15a_caeb_d7c0_42c1, 0x82cc_f51d_a3bf_56c0));
    }

    /// A monitor's search of the metric's tags alone is a chart's query of what it counts; one
    /// that says more is not.
    #[test]
    fn a_monitor_s_search_of_tags_alone_is_a_chart_s_query() {
        assert_eq!(monitor_query("*").as_deref(), Some("sum:error_tracking.errors{*} by {issue.id}.as_count()"));
        assert_eq!(monitor_query("service:shop env:prod").as_deref(), Some("sum:error_tracking.errors{env:prod,service:shop} by {issue.id}.as_count()"));
        assert_eq!(monitor_query("@version:2.0 AND service:shop").as_deref(), Some("sum:error_tracking.errors{service:shop,version:2.0} by {issue.id}.as_count()"));
        for more in ["service:shop OR service:cart", "-env:dev", "service:shop ValueError", "@error.type:ValueError", "service:shop*", "\"bad item\"", "(x"] {
            assert_eq!(monitor_query(more), None, "{more}");
        }
    }
}
