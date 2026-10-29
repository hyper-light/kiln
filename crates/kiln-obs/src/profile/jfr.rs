//! JFR recordings (`docs/observe.md` §8.11): Java Flight Recorder's format, as Datadog's Java tracer
//! uploads it (`main.jfr`: the JDK's chunks, then its own profiler's, ddprof's), read and turned into
//! pprof profiles of the kinds kiln shows: CPU and wall time, allocations, the live heap, locks,
//! exceptions, file and socket I/O, each a profile in Go's names, so everything that reads pprof
//! reads them.
//!
//! The format as the JDK, JMC, async-profiler and ddprof write and read it. A file is a run of
//! chunks, each read alone: its 68-byte header, its metadata (the classes its events and pools are
//! made of, by name: ids mean other things in another chunk, another writer), its constant pools
//! (a chain of checkpoints from the newest back), its events, each bound to its class's fields by
//! name and skipped by its size where it is not one kiln reads. Integers are JFR's varint (seven
//! bits a byte for eight bytes, a ninth of eight; padded forms are the norm), floats fixed
//! big-endian. Each chunk's stacks are resolved to strings before the next chunk. Every count,
//! length and offset is checked against what is left before anything is kept; a chunk that does
//! not read is dropped and the others kept.
//!
//! Read in place: the metadata's strings and the pools' symbols are the recording's bytes (UTF-16's
//! decoded where they are compared or named), the metadata walked once into each class's fields
//! and what kiln reads of each (by name, once, then by place); classes by a dense index, so a field
//! read is no lookup; an event read only as far as its last field kiln reads. A frame's name is made
//! once a chunk for each method its samples have, into one buffer; each profile's strings,
//! functions, locations and stacks interned in tables on kiln-obs's keyed hash, each stack's
//! locations one after another in one buffer.
//!
//! What each event weighs is its writer's (the format does not say): ddprof's CPU and wall samples
//! a weight of sampling intervals (its interval from the chunk's settings, else its configuration,
//! else 10 ms and 50 ms), its allocation and live-heap samples a weight of objects (Poisson
//! sampling's) and bytes their size times it; the JDK's samples one period each, its allocation
//! sample its weight in bytes, its locks and I/O their duration.

use std::ops::Range;

use hashbrown::{HashMap, HashSet, HashTable};

use crate::hash::{Keyed, KeyedState};

/// Bounds on what one recording may hold (past them, refused or dropped: never allocated).
pub const MAX_CHUNKS: usize = 256;
/// Strings in a chunk's metadata, depth of its element tree, of a struct in a struct; classes and
/// fields it declares.
const MAX_STRINGS: usize = 1 << 20;
const MAX_TREE_DEPTH: usize = 32;
const MAX_STRUCT_DEPTH: usize = 16;
const MAX_CLASSES: usize = 1 << 16;
const MAX_FIELDS: usize = 1 << 20;
/// Entries of a chunk's pools, frames of its stacks in all, frames a stack keeps.
pub const MAX_POOL_ENTRIES: usize = 1 << 22;
pub const MAX_FRAMES: usize = 1 << 25;
pub const MAX_STACK: usize = 2048;
/// A chunk's header.
const HEADER: usize = 68;
/// A chunk's first bytes, and so a recording's.
pub const MAGIC: [u8; 4] = *b"FLR\0";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JfrError {
    #[error("not a JFR recording: its magic")]
    Magic,
    #[error("a JFR chunk of version {0}.{1}: 2.x is read")]
    Version(u16, u16),
    #[error("a chunk's header: {0}")]
    Header(&'static str),
    #[error("cut short")]
    Truncated,
    #[error("past its bound: {0}")]
    Bound(&'static str),
    #[error("its metadata: {0}")]
    Metadata(&'static str),
    #[error("its constant pools: {0}")]
    Pool(&'static str),
    #[error("no chunk read")]
    Nothing,
}

use JfrError::{Bound, Truncated};

/// A map on kiln-obs's keyed hash (its keys a recording's, so chosen by whoever wrote it).
type Map<K, V> = HashMap<K, V, KeyedState>;

fn map<K, V>() -> Map<K, V> {
    HashMap::with_hasher(KeyedState::default())
}

/// Bytes read in place, within a bound.
#[derive(Clone, Copy)]
struct Cur<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }

    fn left(&self) -> usize {
        self.b.len().saturating_sub(self.at)
    }

    fn byte(&mut self) -> Result<u8, JfrError> {
        let v = *self.b.get(self.at).ok_or(Truncated)?;
        self.at += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], JfrError> {
        let end = self.at.checked_add(n).ok_or(Truncated)?;
        let s = self.b.get(self.at..end).ok_or(Truncated)?;
        self.at = end;
        Ok(s)
    }

    fn be_u16(&mut self) -> Result<u16, JfrError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| Truncated)?))
    }

    fn be_u64(&mut self) -> Result<u64, JfrError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| Truncated)?))
    }

    /// JFR's varint: seven bits a byte, least first, for eight bytes; a ninth holds eight bits.
    #[inline]
    fn varint(&mut self) -> Result<u64, JfrError> {
        let mut v = 0u64;
        for i in 0..8 {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b < 0x80 {
                return Ok(v);
            }
        }
        Ok(v | (u64::from(self.byte()?) << 56))
    }

    /// A count of what follows, each at least a byte: never past what is left.
    fn count(&mut self) -> Result<usize, JfrError> {
        let n = usize::try_from(self.varint()?).map_err(|_| Bound("a count"))?;
        if n > self.left() {
            return Err(Bound("a count past what is left"));
        }
        Ok(n)
    }

    fn f32(&mut self) -> Result<f32, JfrError> {
        Ok(f32::from_be_bytes(self.take(4)?.try_into().map_err(|_| Truncated)?))
    }

    fn f64(&mut self) -> Result<f64, JfrError> {
        Ok(f64::from_be_bytes(self.take(8)?.try_into().map_err(|_| Truncated)?))
    }

    /// A string: its encoding's byte, then its bytes (null, empty, a pool's, UTF-8, UTF-16 as
    /// varints, Latin-1), kept in place.
    fn string(&mut self) -> Result<JStr<'a>, JfrError> {
        Ok(match self.byte()? {
            0 => JStr::Null,
            1 => JStr::Empty,
            2 => JStr::Pool(self.varint()?),
            3 => {
                let n = self.count()?;
                JStr::Utf8(self.take(n)?)
            }
            4 => {
                let n = self.count()?;
                let from = self.at;
                for _ in 0..n {
                    self.varint()?;
                }
                JStr::Utf16(self.b.get(from..self.at).ok_or(Truncated)?)
            }
            5 => {
                let n = self.count()?;
                JStr::Latin1(self.take(n)?)
            }
            _ => return Err(Bound("a string's encoding")),
        })
    }
}

/// A string as read, in place: UTF-8 or Latin-1 bytes, UTF-16's units as their varints (each read
/// again where it is decoded); a pool's by its key.
#[derive(Debug, Clone, Copy, PartialEq)]
enum JStr<'a> {
    Null,
    Empty,
    Pool(u64),
    Utf8(&'a [u8]),
    Utf16(&'a [u8]),
    Latin1(&'a [u8]),
}

/// UTF-16's units from their varints.
struct Units<'a>(Cur<'a>);

impl Iterator for Units<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.0.left() == 0 {
            return None;
        }
        self.0.varint().ok().map(|v| (v & 0xffff) as u16)
    }
}

impl JStr<'_> {
    /// Calls `f` with each of its characters while it says go on: false where it said stop. Bytes
    /// not UTF-8 and lone surrogates are U+FFFD; a pool's string has none here.
    fn each(&self, mut f: impl FnMut(char) -> bool) -> bool {
        match *self {
            Self::Null | Self::Empty | Self::Pool(_) => true,
            Self::Utf8(b) => match std::str::from_utf8(b) {
                Ok(s) => s.chars().all(f),
                Err(_) => String::from_utf8_lossy(b).chars().all(f),
            },
            Self::Latin1(b) => b.iter().all(|&c| f(char::from(c))),
            Self::Utf16(b) => char::decode_utf16(Units(Cur::new(b))).all(|c| f(c.unwrap_or(char::REPLACEMENT_CHARACTER))),
        }
    }

    /// Its characters written after `out`'s.
    fn write(&self, out: &mut String) {
        if let Self::Utf8(b) = *self
            && let Ok(s) = std::str::from_utf8(b)
        {
            out.push_str(s);
            return;
        }
        self.each(|c| {
            out.push(c);
            true
        });
    }

    /// Whether it is `s`, no string made (a pool's, not yet resolved, is none).
    fn is(&self, s: &str) -> bool {
        match *self {
            Self::Utf8(b) => return b == s.as_bytes(),
            Self::Pool(_) => return false,
            _ => {}
        }
        let mut want = s.chars();
        self.each(|c| want.next() == Some(c)) && want.next().is_none()
    }
}

/// How a class's values are written: a primitive by its name, or a struct of its fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prim {
    Bool,
    Byte,
    Var,
    F32,
    F64,
    Str,
    Struct,
}

impl Prim {
    fn of(name: &str) -> Self {
        match name {
            "boolean" => Self::Bool,
            "byte" => Self::Byte,
            "char" | "short" | "int" | "long" => Self::Var,
            "float" => Self::F32,
            "double" => Self::F64,
            "java.lang.String" => Self::Str,
            _ => Self::Struct,
        }
    }
}

/// What kiln reads a class as: an event of a kind; a setting, ddprof's configuration; a pool's
/// entry stacks are made of; anything else, skipped by its fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Other,
    Event(Kind),
    Setting,
    Config,
    Stack,
    Frame,
    Method,
    Class,
    Symbol,
    FrameType,
    String,
}

impl Role {
    fn of(class: &str) -> Self {
        if let Some(k) = kind_of(class) {
            return Self::Event(k);
        }
        match class {
            "jdk.ActiveSetting" => Self::Setting,
            "datadog.DatadogProfilerConfig" => Self::Config,
            "jdk.types.StackTrace" => Self::Stack,
            "jdk.types.StackFrame" => Self::Frame,
            "jdk.types.Method" => Self::Method,
            "java.lang.Class" => Self::Class,
            "jdk.types.Symbol" => Self::Symbol,
            "jdk.types.FrameType" => Self::FrameType,
            "java.lang.String" => Self::String,
            _ => Self::Other,
        }
    }
}

/// What kiln reads a field as, in its class's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Other,
    Stack,
    Weight,
    Size,
    Duration,
    Period,
    Frames,
    Method,
    Line,
    Type,
    Name,
    Id,
    Value,
    Cpu,
    Wall,
}

impl Slot {
    fn of(role: Role, class: &str, field: &str) -> Self {
        match role {
            Role::Event(_) => match part_of(class, field) {
                Part::Stack => Self::Stack,
                Part::Weight => Self::Weight,
                Part::Size => Self::Size,
                Part::Duration => Self::Duration,
                Part::Period => Self::Period,
                Part::Other => Self::Other,
            },
            Role::Setting => match field {
                "id" => Self::Id,
                "name" => Self::Name,
                "value" => Self::Value,
                _ => Self::Other,
            },
            Role::Config => match field {
                "cpuInterval" => Self::Cpu,
                "wallInterval" => Self::Wall,
                _ => Self::Other,
            },
            Role::Stack if field == "frames" => Self::Frames,
            Role::Frame => match field {
                "method" => Self::Method,
                "lineNumber" => Self::Line,
                "type" => Self::Type,
                _ => Self::Other,
            },
            Role::Method => match field {
                "type" => Self::Type,
                "name" => Self::Name,
                _ => Self::Other,
            },
            Role::Class if field == "name" => Self::Name,
            _ => Self::Other,
        }
    }
}

/// A field: its class (by its place among the chunk's; `u32::MAX` one the metadata does not
/// declare), whether it is a pool's key, an array, one varint (a pool's key, an integer: most
/// fields, read with no more said), and what kiln reads it as.
#[derive(Debug, Clone, Copy)]
struct Field {
    class: u32,
    pool: bool,
    array: bool,
    var: bool,
    slot: Slot,
}

/// A class: how its values are written, what kiln reads it as, its fields (their place among the
/// chunk's), and how many of them an event of it is read through (to the last kiln reads).
#[derive(Debug, Clone)]
struct Class {
    prim: Prim,
    role: Role,
    fields: Range<u32>,
    need: u32,
}

/// A chunk's metadata: its classes by their place, each id's place.
#[derive(Default)]
struct Meta {
    classes: Vec<Class>,
    fields: Vec<Field>,
    index: Map<u64, u32>,
}

impl Meta {
    fn class(&self, id: u64) -> Option<(u32, &Class)> {
        let i = *self.index.get(&id)?;
        Some((i, self.classes.get(i as usize)?))
    }

    fn fields(&self, c: &Class) -> &[Field] {
        self.fields.get(c.fields.start as usize..c.fields.end as usize).unwrap_or_default()
    }
}

/// The metadata's strings kiln looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Word {
    None,
    Metadata,
    Class,
    Field,
    Id,
    Name,
    ConstantPool,
    Dimension,
}

impl Word {
    fn of(s: &JStr<'_>) -> Self {
        [(Self::Metadata, "metadata"), (Self::Class, "class"), (Self::Field, "field"), (Self::Id, "id"), (Self::Name, "name"), (Self::ConstantPool, "constantPool"), (Self::Dimension, "dimension")]
            .into_iter()
            .find(|(_, w)| s.is(w))
            .map_or(Self::None, |(w, _)| w)
    }
}

/// A class as the metadata declares it: its id, its name (a string's place), its fields' places.
struct Declared {
    id: u64,
    name: Option<u32>,
    fields: Range<usize>,
}

/// A field as the metadata declares it: its name, its class's id, a pool's key, an array.
struct DeclaredField {
    name: u32,
    class: u64,
    pool: bool,
    array: bool,
}

/// Where in the metadata's tree an element is: the root itself, or what it is under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Under {
    Top,
    Root,
    Metadata,
    Class,
    Elsewhere,
}

/// What a walk of the metadata's tree keeps.
struct Walk<'s, 'a> {
    strings: &'s [JStr<'a>],
    words: &'s [Word],
    classes: Vec<Declared>,
    fields: Vec<DeclaredField>,
}

impl Walk<'_, '_> {
    fn index(&self, cur: &mut Cur<'_>) -> Result<u32, JfrError> {
        let i = cur.varint()?;
        if i >= self.strings.len() as u64 {
            return Err(JfrError::Metadata("a string's index"));
        }
        u32::try_from(i).map_err(|_| JfrError::Metadata("a string's index"))
    }

    fn string(&self, i: u32) -> JStr<'_> {
        self.strings.get(i as usize).copied().unwrap_or(JStr::Null)
    }

    /// A string's digits as a number (none where it has none, or anything else).
    fn number(&self, i: u32) -> Option<u64> {
        let (mut v, mut any) = (Some(0u64), false);
        let digits = self.string(i).each(|c| {
            any = true;
            v = c.to_digit(10).and_then(|d| v?.checked_mul(10)?.checked_add(u64::from(d)));
            v.is_some()
        });
        v.filter(|_| digits && any)
    }

    /// An element read where `under` says, its children after it: a class under the metadata kept,
    /// a field under a class kept.
    fn element(&mut self, cur: &mut Cur<'_>, under: Under, depth: usize) -> Result<(), JfrError> {
        if depth > MAX_TREE_DEPTH {
            return Err(JfrError::Metadata("its tree past its depth"));
        }
        let name = self.index(cur)?;
        let word = self.words.get(name as usize).copied().unwrap_or(Word::None);
        let (mut id, mut named, mut class, mut pool, mut array) = (None, None, None, false, false);
        let n = cur.count()?;
        for _ in 0..n {
            let (k, v) = (self.index(cur)?, self.index(cur)?);
            match self.words.get(k as usize).copied().unwrap_or(Word::None) {
                Word::Id => id = self.number(v),
                Word::Name => named = Some(v),
                Word::Class => class = self.number(v),
                Word::ConstantPool => pool = !self.string(v).is("false"),
                Word::Dimension => array = self.string(v).is("1"),
                _ => {}
            }
        }
        let here = match (under, word) {
            (Under::Top, _) => Under::Root,
            (Under::Root, Word::Metadata) => Under::Metadata,
            (Under::Metadata, Word::Class) => {
                if let Some(id) = id {
                    if self.classes.len() >= MAX_CLASSES {
                        return Err(Bound("its classes"));
                    }
                    self.classes.push(Declared { id, name: named, fields: self.fields.len()..self.fields.len() });
                    Under::Class
                } else {
                    Under::Elsewhere
                }
            }
            (Under::Class, Word::Field) => {
                if let (Some(name), Some(class)) = (named, class)
                    && let Some(c) = self.classes.last_mut()
                {
                    if self.fields.len() >= MAX_FIELDS {
                        return Err(Bound("its fields"));
                    }
                    self.fields.push(DeclaredField { name, class, pool, array });
                    c.fields.end = self.fields.len();
                }
                Under::Elsewhere
            }
            _ => Under::Elsewhere,
        };
        let n = cur.count()?;
        for _ in 0..n {
            self.element(cur, here, depth + 1)?;
        }
        Ok(())
    }
}

/// The metadata event at `at` of the chunk: its strings, its classes and fields, each class's and
/// field's role.
fn metadata(chunk: &[u8], at: usize) -> Result<Meta, JfrError> {
    let mut cur = Cur::new(chunk);
    cur.at = at;
    let size = usize::try_from(cur.varint()?).map_err(|_| JfrError::Metadata("its size"))?;
    let end = at.checked_add(size).filter(|e| *e <= chunk.len()).ok_or(JfrError::Metadata("its size"))?;
    let mut cur = Cur { b: chunk.get(..end).ok_or(Truncated)?, at: cur.at };
    if cur.varint()? != 0 {
        return Err(JfrError::Metadata("not the metadata's type"));
    }
    // Its start, duration and id.
    for _ in 0..3 {
        cur.varint()?;
    }
    let n = cur.count()?;
    if n > MAX_STRINGS {
        return Err(JfrError::Metadata("past its strings' bound"));
    }
    let mut strings = Vec::with_capacity(n);
    for _ in 0..n {
        let s = cur.string()?;
        if matches!(s, JStr::Pool(_)) {
            return Err(JfrError::Metadata("a pool's string in its table"));
        }
        strings.push(s);
    }
    let words: Vec<Word> = strings.iter().map(Word::of).collect();
    let mut walk = Walk { strings: &strings, words: &words, classes: Vec::new(), fields: Vec::new() };
    walk.element(&mut cur, Under::Top, 0)?;
    let mut meta = Meta { classes: Vec::with_capacity(walk.classes.len()), fields: Vec::with_capacity(walk.fields.len()), index: map() };
    meta.index.reserve(walk.classes.len());
    for (i, d) in walk.classes.iter().enumerate() {
        meta.index.insert(d.id, i as u32);
    }
    // Each class's and field's name made once into a buffer, read for its role, let go.
    let (mut class_name, mut field_name) = (String::new(), String::new());
    for d in &walk.classes {
        class_name.clear();
        if let Some(n) = d.name {
            walk.string(n).write(&mut class_name);
        }
        let declared = walk.fields.get(d.fields.clone()).unwrap_or_default();
        let role = Role::of(&class_name);
        let prim = if declared.is_empty() { Prim::of(&class_name) } else { Prim::Struct };
        let from = meta.fields.len() as u32;
        let mut need = 0u32;
        for (j, f) in declared.iter().enumerate() {
            field_name.clear();
            walk.string(f.name).write(&mut field_name);
            let slot = Slot::of(role, &class_name, &field_name);
            if slot != Slot::Other {
                need = j as u32 + 1;
            }
            let class = meta.index.get(&f.class).copied().unwrap_or(u32::MAX);
            meta.fields.push(Field { class, pool: f.pool, array: f.array, var: false, slot });
        }
        // An event, a setting or a configuration read through its last field kiln reads; any other
        // class (a pool's) whole.
        let need = if matches!(role, Role::Event(_) | Role::Setting | Role::Config) { need } else { declared.len() as u32 };
        meta.classes.push(Class { prim, role, fields: from..meta.fields.len() as u32, need });
    }
    // Every class's way known now: each field one varint or not.
    let Meta { classes, fields, .. } = &mut meta;
    for f in fields.iter_mut() {
        f.var = !f.array && (f.pool || classes.get(f.class as usize).is_some_and(|c| c.prim == Prim::Var));
    }
    Ok(meta)
}

/// A value read: a number, a reference into a pool, a string; what kiln does not keep, skipped.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Val<'a> {
    Num(u64),
    F(f64),
    Bool(bool),
    Ref(u64),
    Str(JStr<'a>),
    Skipped,
}

impl Val<'_> {
    fn num(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n as f64),
            Self::F(f) => Some(*f),
            _ => None,
        }
    }
}

/// Field `f`'s value (an array's, a struct's: skipped).
#[inline]
fn value<'a>(cur: &mut Cur<'a>, meta: &Meta, f: &Field, depth: usize) -> Result<Val<'a>, JfrError> {
    if f.var {
        let v = cur.varint()?;
        return Ok(if f.pool { Val::Ref(v) } else { Val::Num(v) });
    }
    if f.array {
        let n = cur.count()?;
        for _ in 0..n {
            one(cur, meta, f.class, f.pool, depth)?;
        }
        return Ok(Val::Skipped);
    }
    one(cur, meta, f.class, f.pool, depth)
}

fn one<'a>(cur: &mut Cur<'a>, meta: &Meta, class: u32, pool: bool, depth: usize) -> Result<Val<'a>, JfrError> {
    if pool {
        return Ok(Val::Ref(cur.varint()?));
    }
    let c = meta.classes.get(class as usize).ok_or(JfrError::Metadata("a field of a class it does not declare"))?;
    Ok(match c.prim {
        Prim::Bool => Val::Bool(cur.byte()? != 0),
        Prim::Byte => Val::Num(u64::from(cur.byte()?)),
        Prim::Var => Val::Num(cur.varint()?),
        Prim::F32 => Val::F(f64::from(cur.f32()?)),
        Prim::F64 => Val::F(cur.f64()?),
        Prim::Str => Val::Str(cur.string()?),
        Prim::Struct => {
            if depth >= MAX_STRUCT_DEPTH {
                return Err(Bound("a struct's depth"));
            }
            for g in meta.fields(c) {
                value(cur, meta, g, depth + 1)?;
            }
            Val::Skipped
        }
    })
}

/// A frame of a stack: its method, its line, its type (pools' keys).
#[derive(Debug, Clone, Copy)]
struct Frame {
    method: u64,
    line: i32,
    kind: u64,
}

/// A chunk's pools, as much of them as stacks need, in place: each stack's frames in one arena;
/// strings and symbols the recording's bytes.
struct Pools<'a> {
    strings: Map<u64, JStr<'a>>,
    symbols: Map<u64, JStr<'a>>,
    frame_kinds: HashSet<u64, KeyedState>,
    /// Class: its name's symbol. Method: its class, its name's symbol.
    classes: Map<u64, u64>,
    methods: Map<u64, (u64, u64)>,
    stacks: Map<u64, (u32, u32)>,
    frames: Vec<Frame>,
    entries: usize,
}

impl<'a> Pools<'a> {
    fn new() -> Self {
        Self { strings: map(), symbols: map(), frame_kinds: HashSet::with_hasher(KeyedState::default()), classes: map(), methods: map(), stacks: map(), frames: Vec::new(), entries: 0 }
    }

    /// A string, a pool's resolved (a pool's string is no pool's again).
    fn text(&self, s: JStr<'a>) -> JStr<'a> {
        match s {
            JStr::Pool(k) => match self.strings.get(&k) {
                Some(JStr::Pool(_)) | None => JStr::Null,
                Some(s) => *s,
            },
            s => s,
        }
    }

    /// Symbol `key`'s text written after `out`'s.
    fn symbol(&self, key: u64, out: &mut String) {
        if let Some(&s) = self.symbols.get(&key) {
            self.text(s).write(out);
        }
    }
}

/// A pool's entry of class `c` (at `ci`; key `key`) kept where stacks need it, else skipped. A key a
/// newer checkpoint gave already (the chain is read newest first) keeps its value.
fn pool_entry<'a>(cur: &mut Cur<'a>, meta: &Meta, ci: u32, c: &Class, key: u64, p: &mut Pools<'a>) -> Result<(), JfrError> {
    match c.role {
        Role::Stack => {
            let start = p.frames.len();
            for f in meta.fields(c) {
                if f.slot == Slot::Frames && f.array {
                    let n = cur.count()?;
                    if n > MAX_STACK || p.frames.len() + n > MAX_FRAMES {
                        return Err(Bound("a stack's frames"));
                    }
                    let frame = meta.classes.get(f.class as usize).ok_or(JfrError::Metadata("a stack's frame"))?;
                    let parts = meta.fields(frame);
                    p.frames.reserve(n);
                    for _ in 0..n {
                        let mut fr = Frame { method: 0, line: 0, kind: u64::MAX };
                        for g in parts {
                            match (g.slot, value(cur, meta, g, 1)?) {
                                (Slot::Method, Val::Ref(k)) => fr.method = k,
                                (Slot::Line, Val::Num(n)) => fr.line = n as u32 as i32,
                                (Slot::Type, Val::Ref(k)) => fr.kind = k,
                                _ => {}
                            }
                        }
                        p.frames.push(fr);
                    }
                } else {
                    value(cur, meta, f, 1)?;
                }
            }
            // Its frames read again let go where a newer checkpoint gave it.
            if p.stacks.contains_key(&key) {
                p.frames.truncate(start);
            } else {
                let (s, n) = (u32::try_from(start).map_err(|_| Bound("frames"))?, u32::try_from(p.frames.len() - start).map_err(|_| Bound("frames"))?);
                p.stacks.insert(key, (s, n));
            }
        }
        Role::Method => {
            let (mut class, mut name) = (0, 0);
            for f in meta.fields(c) {
                match (f.slot, value(cur, meta, f, 1)?) {
                    (Slot::Type, Val::Ref(k)) => class = k,
                    (Slot::Name, Val::Ref(k)) => name = k,
                    _ => {}
                }
            }
            p.methods.entry(key).or_insert((class, name));
        }
        Role::Class => {
            let mut name = 0;
            for f in meta.fields(c) {
                if let (Slot::Name, Val::Ref(k)) = (f.slot, value(cur, meta, f, 1)?) {
                    name = k;
                }
            }
            p.classes.entry(key).or_insert(name);
        }
        Role::Symbol | Role::FrameType => {
            let mut text = None;
            for f in meta.fields(c) {
                if let Val::Str(s) = value(cur, meta, f, 1)? {
                    text = Some(s);
                }
            }
            match (c.role, text) {
                (Role::Symbol, Some(s)) => {
                    p.symbols.entry(key).or_insert(s);
                }
                (Role::FrameType, Some(_)) => {
                    p.frame_kinds.insert(key);
                }
                _ => {}
            }
        }
        Role::String => {
            let s = cur.string()?;
            p.strings.entry(key).or_insert(s);
        }
        _ => {
            if c.prim == Prim::Struct {
                for f in meta.fields(c) {
                    value(cur, meta, f, 1)?;
                }
            } else {
                one(cur, meta, ci, false, 1)?;
            }
        }
    }
    Ok(())
}

/// Every checkpoint of the chunk's chain, newest first, read into its pools.
fn pools<'a>(chunk: &'a [u8], first: usize, meta: &Meta) -> Result<Pools<'a>, JfrError> {
    let mut p = Pools::new();
    let mut at = first;
    let mut seen = 0usize;
    loop {
        if at < HEADER || at >= chunk.len() {
            return Err(JfrError::Pool("a checkpoint outside its chunk"));
        }
        seen += 1;
        // A checkpoint is seven bytes at the least: a chain longer than that allows loops.
        if seen > chunk.len() / 7 + 1 {
            return Err(JfrError::Pool("its chain loops"));
        }
        let mut cur = Cur::new(chunk);
        cur.at = at;
        let size = usize::try_from(cur.varint()?).map_err(|_| JfrError::Pool("a checkpoint's size"))?;
        let end = at.checked_add(size).filter(|e| *e <= chunk.len() && size > 0).ok_or(JfrError::Pool("a checkpoint's size"))?;
        let mut cur = Cur { b: chunk.get(..end).ok_or(Truncated)?, at: cur.at };
        if cur.varint()? != 1 {
            return Err(JfrError::Pool("not a checkpoint"));
        }
        // Its start and duration.
        cur.varint()?;
        cur.varint()?;
        let delta = cur.varint()?.cast_signed();
        // Its kind (a flush, the chunk's header, statics, threads).
        cur.varint()?;
        let n = cur.count()?;
        for _ in 0..n {
            let class = cur.varint()?;
            let entries = cur.count()?;
            p.entries += entries;
            if p.entries > MAX_POOL_ENTRIES {
                return Err(Bound("a chunk's pools' entries"));
            }
            let (ci, c) = meta.class(class).ok_or(JfrError::Pool("a pool of a class its metadata does not declare"))?;
            for _ in 0..entries {
                let key = cur.varint()?;
                pool_entry(&mut cur, meta, ci, c, key, &mut p)?;
            }
        }
        // A checkpoint read to its very size, or its pools are not what it says.
        if cur.at != end {
            return Err(JfrError::Pool("a checkpoint not read to its size"));
        }
        if delta == 0 {
            break;
        }
        at = i64::try_from(at).ok().and_then(|a| a.checked_add(delta)).and_then(|a| usize::try_from(a).ok()).ok_or(JfrError::Pool("its chain"))?;
    }
    Ok(p)
}

/// What kiln reads of a sample, by where it comes from: one source of a kind kept where several
/// wrote one (ddprof's over the JDK's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Kind {
    CpuDd,
    CpuJdk,
    CpuTime,
    Wall,
    AllocDd,
    AllocJdk,
    AllocTlab,
    LiveDd,
    LiveJdk,
    Lock,
    Exceptions,
    FileIo,
    SocketIo,
}

/// Kinds there are.
const KINDS: usize = 13;

impl Kind {
    fn ix(self) -> usize {
        self as usize
    }
}

/// How an event is read: each of its fields' part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Stack,
    Weight,
    Size,
    Duration,
    Period,
    Other,
}

fn kind_of(event: &str) -> Option<Kind> {
    Some(match event {
        "datadog.ExecutionSample" => Kind::CpuDd,
        "jdk.ExecutionSample" => Kind::CpuJdk,
        "jdk.CPUTimeSample" => Kind::CpuTime,
        "datadog.MethodSample" => Kind::Wall,
        "datadog.ObjectSample" => Kind::AllocDd,
        "jdk.ObjectAllocationSample" => Kind::AllocJdk,
        "jdk.ObjectAllocationInNewTLAB" | "jdk.ObjectAllocationOutsideTLAB" => Kind::AllocTlab,
        "datadog.HeapLiveObject" => Kind::LiveDd,
        "jdk.OldObjectSample" => Kind::LiveJdk,
        "jdk.JavaMonitorEnter" | "jdk.JavaMonitorWait" | "jdk.ThreadPark" => Kind::Lock,
        "datadog.ExceptionSample" => Kind::Exceptions,
        "jdk.FileRead" | "jdk.FileWrite" | "jdk.FileForce" => Kind::FileIo,
        "jdk.SocketRead" | "jdk.SocketWrite" => Kind::SocketIo,
        _ => return None,
    })
}

/// A field's part in an event of `event`'s kind.
fn part_of(event: &str, field: &str) -> Part {
    match (event, field) {
        (_, "stackTrace") => Part::Stack,
        ("datadog.ExecutionSample" | "datadog.MethodSample" | "datadog.ObjectSample" | "datadog.HeapLiveObject" | "jdk.ObjectAllocationSample", "weight") => Part::Weight,
        ("datadog.ObjectSample" | "datadog.HeapLiveObject", "size") => Part::Size,
        ("jdk.ObjectAllocationInNewTLAB", "tlabSize") | ("jdk.ObjectAllocationOutsideTLAB", "allocationSize") | ("jdk.OldObjectSample", "objectSize") => Part::Size,
        ("jdk.CPUTimeSample", "samplingPeriod") => Part::Period,
        (_, "duration") => Part::Duration,
        _ => Part::Other,
    }
}

/// A chunk's clock: ticks a second.
#[derive(Debug, Clone, Copy)]
struct Clock {
    per_second: u64,
}

impl Clock {
    fn ns(&self, ticks: u64) -> f64 {
        ticks as f64 * 1e9 / self.per_second as f64
    }
}

/// A duration as the JDK's settings write it (`9 ms`, `20 ms`, `1 s`), in nanoseconds.
fn period_ns(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let scale = match unit.trim() {
        "ns" => 1.0,
        "us" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        "m" => 60e9,
        "h" => 3600e9,
        "d" => 86_400e9,
        _ => return None,
    };
    Some(n * scale)
}

/// A frame as a profile names it: `package.Class.method` (a hidden class's address, lambdas' and
/// generated classes' numbers left out: each JVM gives them others, and each would be a function of
/// its own), a native one by its symbol.
#[cfg(test)]
fn label(class: &str, method: &str) -> String {
    let mut out = String::new();
    label_into(class, method, &mut String::new(), &mut out);
    out
}

/// [`label`] written after `out`'s, `scratch` the class's name's buffer.
fn label_into(class: &str, method: &str, scratch: &mut String, out: &mut String) {
    if class.is_empty() {
        out.push_str(method);
        return;
    }
    scratch.clear();
    scratch.extend(class.chars().map(|c| if c == '/' { '.' } else { c }));
    let c = scratch;
    // A hidden class (a lambda's, a method handle's form): its name, then the address its JVM gave
    // it, `/0x…` (`.0x…` in its internal form).
    if let Some(i) = c.rfind(".0x")
        && c.len() > i + 3
        && c.get(i + 3..).is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        c.truncate(i);
    }
    // Each search made only where its name could hold what it looks for.
    if c.contains("$$") {
        if let Some(i) = c.find("$$Lambda") {
            c.truncate(i + "$$Lambda".len());
        }
        if let Some(i) = c.find("$$EnhancerBySpringCGLIB$$") {
            c.truncate(i + "$$EnhancerBySpringCGLIB$$".len());
        }
    }
    if c.contains("Generated") {
        for generated in ["GeneratedMethodAccessor", "GeneratedConstructorAccessor", "GeneratedSerializationConstructorAccessor"] {
            if let Some(i) = c.find(generated) {
                let digits = c.get(i + generated.len()..).map_or(0, |r| r.bytes().take_while(u8::is_ascii_digit).count());
                c.replace_range(i + generated.len()..i + generated.len() + digits, "");
            }
        }
    }
    out.push_str(c);
    out.push('.');
    out.push_str(method);
}

/// A profile being built: its strings (one buffer), functions, locations and stacks interned on
/// kiln-obs's keyed hash, each stack's locations one after another in one buffer, its values two a
/// stack. Ids are places (pprof's are one more, a string's its own).
struct Built {
    keyed: Keyed,
    text: String,
    strings: Vec<(u32, u32)>,
    string_of: HashTable<u32>,
    functions: Vec<(u32, u32)>,
    function_of: HashTable<u32>,
    locations: Vec<(u32, i64)>,
    location_of: HashTable<u32>,
    stack_locs: Vec<u32>,
    samples: Vec<(u32, u32, [f64; 2])>,
    sample_of: HashTable<u32>,
    /// The chunk it was last given functions of, and each of that chunk's functions' and
    /// locations' here.
    seq: u64,
    local: Vec<u32>,
    local_loc: Vec<u32>,
}

impl Built {
    fn new() -> Self {
        let mut b = Self {
            keyed: Keyed::new(),
            text: String::new(),
            strings: Vec::new(),
            string_of: HashTable::new(),
            functions: Vec::new(),
            function_of: HashTable::new(),
            locations: Vec::new(),
            location_of: HashTable::new(),
            stack_locs: Vec::new(),
            samples: Vec::new(),
            sample_of: HashTable::new(),
            seq: u64::MAX,
            local: Vec::new(),
            local_loc: Vec::new(),
        };
        b.string("");
        b
    }

    /// Ready for chunk `seq`, whose `functions` functions and `locations` locations it is given,
    /// and `stacks` stacks at the most.
    fn begin(&mut self, seq: u64, functions: usize, locations: usize, stacks: usize) {
        if self.seq != seq {
            self.seq = seq;
            self.local.clear();
            self.local.resize(functions, u32::MAX);
            self.local_loc.clear();
            self.local_loc.resize(locations, u32::MAX);
            let Self { keyed, stack_locs, samples, sample_of, .. } = self;
            samples.reserve(stacks);
            let hash = |l: &[u32]| l.iter().fold(keyed.word(l.len() as u64), |h, &x| keyed.word(h ^ u64::from(x)));
            let (stack_locs, samples) = (&*stack_locs, &*samples);
            sample_of.reserve(stacks, |&j| hash(samples.get(j as usize).and_then(|s| stack_locs.get(s.0 as usize..(s.0 + s.1) as usize)).unwrap_or_default()));
        }
    }

    /// Its location of the chunk's location `local` (of the chunk's function `function`, named
    /// `name`, native or not, at `line`).
    fn location_of(&mut self, local: usize, function: usize, name: &str, native: bool, line: i64) -> u32 {
        if let Some(&l) = self.local_loc.get(local)
            && l != u32::MAX
        {
            return l;
        }
        let f = self.function(self.seq, function, name, native);
        let l = self.location(f, line);
        if let Some(slot) = self.local_loc.get_mut(local) {
            *slot = l;
        }
        l
    }

    fn string(&mut self, s: &str) -> u32 {
        let Self { keyed, text, strings, string_of, .. } = self;
        let at = |i: u32| strings.get(i as usize).and_then(|&(a, b)| text.get(a as usize..b as usize)).unwrap_or("");
        let h = keyed.bytes(s.as_bytes());
        if let Some(&i) = string_of.find(h, |&i| at(i) == s) {
            return i;
        }
        let i = strings.len() as u32;
        let from = text.len() as u32;
        text.push_str(s);
        strings.push((from, text.len() as u32));
        let (text, strings) = (&*text, &*strings);
        let at = |i: u32| strings.get(i as usize).and_then(|&(a, b)| text.get(a as usize..b as usize)).unwrap_or("");
        string_of.insert_unique(h, i, |&j| keyed.bytes(at(j).as_bytes()));
        i
    }

    /// Its function of chunk `seq`'s function `local` (`name`, native or not).
    fn function(&mut self, seq: u64, local: usize, name: &str, native: bool) -> u32 {
        if self.seq != seq {
            self.seq = seq;
            self.local.clear();
            self.local_loc.clear();
        }
        if let Some(&f) = self.local.get(local)
            && f != u32::MAX
        {
            return f;
        }
        let key = (self.string(name), self.string(if native { "[native]" } else { "" }));
        let Self { keyed, functions, function_of, .. } = self;
        let hash = |k: (u32, u32)| keyed.word((u64::from(k.0) << 32) | u64::from(k.1));
        let f = match function_of.find(hash(key), |&i| functions.get(i as usize) == Some(&key)) {
            Some(&i) => i,
            None => {
                let i = functions.len() as u32;
                functions.push(key);
                let functions = &*functions;
                function_of.insert_unique(hash(key), i, |&j| hash(functions.get(j as usize).copied().unwrap_or_default()));
                i
            }
        };
        if self.local.len() <= local {
            self.local.resize(local + 1, u32::MAX);
        }
        if let Some(slot) = self.local.get_mut(local) {
            *slot = f;
        }
        f
    }

    /// Its location of function `f` at `line`.
    fn location(&mut self, f: u32, line: i64) -> u32 {
        let Self { keyed, locations, location_of, .. } = self;
        let hash = |k: (u32, i64)| keyed.word(keyed.word(u64::from(k.0)) ^ k.1.cast_unsigned());
        let key = (f, line);
        if let Some(&i) = location_of.find(hash(key), |&i| locations.get(i as usize) == Some(&key)) {
            return i;
        }
        let i = locations.len() as u32;
        locations.push(key);
        let locations = &*locations;
        location_of.insert_unique(hash(key), i, |&j| hash(locations.get(j as usize).copied().unwrap_or_default()));
        i
    }

    /// `v` added to the stack of locations `locs` (leaf first).
    fn add(&mut self, locs: &[u32], v: [f64; 2]) {
        let Self { keyed, stack_locs, samples, sample_of, .. } = self;
        let hash = |l: &[u32]| l.iter().fold(keyed.word(l.len() as u64), |h, &x| keyed.word(h ^ u64::from(x)));
        let h = hash(locs);
        let at = |i: u32| samples.get(i as usize).and_then(|s| stack_locs.get(s.0 as usize..(s.0 + s.1) as usize)).unwrap_or_default();
        if let Some(&i) = sample_of.find(h, |&i| at(i) == locs) {
            if let Some(s) = samples.get_mut(i as usize) {
                s.2[0] += v[0];
                s.2[1] += v[1];
            }
            return;
        }
        let i = samples.len() as u32;
        let from = stack_locs.len() as u32;
        stack_locs.extend_from_slice(locs);
        samples.push((from, locs.len() as u32, v));
        let (stack_locs, samples) = (&*stack_locs, &*samples);
        let at = |i: u32| samples.get(i as usize).and_then(|s| stack_locs.get(s.0 as usize..(s.0 + s.1) as usize)).unwrap_or_default();
        sample_of.insert_unique(h, i, |&j| hash(at(j)));
    }
}

/// A profile made of a recording: its name, its sample types (type, unit) and each one's values
/// summed, its pprof (not compressed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub name: &'static str,
    pub types: &'static [(&'static str, &'static str)],
    pub totals: Vec<i64>,
    pub pprof: Vec<u8>,
}

/// What a recording became: its profiles (pprof, Go's names), and how its chunks went.
#[derive(Debug, Default)]
pub struct Converted {
    pub profiles: Vec<Made>,
    pub chunks: usize,
    pub chunks_failed: usize,
    pub events: u64,
    /// The first chunk's start and the last's end, nanoseconds since the epoch.
    pub start_ns: i64,
    pub end_ns: i64,
}

/// A chunk read: its events kiln kept, its intervals.
struct ChunkRead {
    events: u64,
    cpu_interval_ns: Option<f64>,
    wall_interval_ns: Option<f64>,
    jdk_period_ns: Option<f64>,
}

/// An event's parts kiln reads.
struct Sample {
    stack: u64,
    weight: Option<f64>,
    size: Option<f64>,
    duration_ns: Option<f64>,
    period_ns: Option<f64>,
}

impl Sample {
    /// Its two values in its kind's profile, before a kind's interval makes them whole.
    fn values(&self, kind: Kind) -> [f64; 2] {
        match kind {
            Kind::CpuDd | Kind::Wall => {
                let w = self.weight.filter(|w| *w > 0.0).unwrap_or(1.0);
                [w, w]
            }
            Kind::CpuJdk => [1.0, 1.0],
            Kind::CpuTime => [1.0, self.period_ns.unwrap_or(0.0)],
            Kind::AllocDd | Kind::LiveDd => {
                let w = self.weight.unwrap_or(1.0);
                [w, w * self.size.unwrap_or(0.0)]
            }
            Kind::AllocJdk => [1.0, self.weight.unwrap_or(0.0)],
            Kind::AllocTlab | Kind::LiveJdk => [1.0, self.size.unwrap_or(0.0)],
            Kind::Lock | Kind::FileIo | Kind::SocketIo => [1.0, self.duration_ns.unwrap_or(0.0)],
            Kind::Exceptions => [1.0, 0.0],
        }
    }
}

/// An event of `class` read through its last field kiln reads (the rest passed over by its size).
fn sample(cur: &mut Cur<'_>, meta: &Meta, class: &Class, clock: Clock) -> Result<Sample, JfrError> {
    let mut s = Sample { stack: 0, weight: None, size: None, duration_ns: None, period_ns: None };
    for f in meta.fields(class).iter().take(class.need as usize) {
        match (f.slot, value(cur, meta, f, 0)?) {
            (Slot::Stack, Val::Ref(k)) => s.stack = k,
            (Slot::Weight, v) => s.weight = v.num(),
            (Slot::Size, v) => s.size = v.num(),
            (Slot::Duration, Val::Num(t)) => s.duration_ns = Some(clock.ns(t)),
            (Slot::Period, Val::Num(t)) => s.period_ns = Some(clock.ns(t)),
            _ => {}
        }
    }
    Ok(s)
}

/// A `jdk.ActiveSetting`: the event type it is of, its name, its value.
fn setting<'a>(cur: &mut Cur<'a>, meta: &Meta, class: &Class) -> Result<(Option<u64>, JStr<'a>, JStr<'a>), JfrError> {
    let (mut id, mut name, mut value_s) = (None, JStr::Null, JStr::Null);
    for f in meta.fields(class).iter().take(class.need as usize) {
        match (f.slot, value(cur, meta, f, 0)?) {
            (Slot::Id, Val::Num(n)) => id = Some(n),
            (Slot::Name, Val::Str(s)) => name = s,
            (Slot::Value, Val::Str(s)) => value_s = s,
            _ => {}
        }
    }
    Ok((id, name, value_s))
}

/// ddprof's `datadog.DatadogProfilerConfig`: its CPU and wall intervals (milliseconds).
fn profiler_config(cur: &mut Cur<'_>, meta: &Meta, class: &Class) -> Result<(Option<f64>, Option<f64>), JfrError> {
    let (mut cpu, mut wall) = (None, None);
    for f in meta.fields(class).iter().take(class.need as usize) {
        match (f.slot, value(cur, meta, f, 0)?) {
            (Slot::Cpu, v) => cpu = v.num(),
            (Slot::Wall, v) => wall = v.num(),
            _ => {}
        }
    }
    Ok((cpu, wall))
}

/// One chunk (`chunk`, its bytes whole; `seq`, its place in the recording) read into the builders.
fn chunk(chunk: &[u8], seq: u64, built: &mut [Option<Built>; KINDS], start_ns: &mut i64, end_ns: &mut i64) -> Result<ChunkRead, JfrError> {
    let mut cur = Cur::new(chunk);
    if cur.take(4)? != MAGIC {
        return Err(JfrError::Magic);
    }
    let (major, minor) = (cur.be_u16()?, cur.be_u16()?);
    if major != 2 {
        return Err(JfrError::Version(major, minor));
    }
    let size = cur.be_u64()?;
    let (cp_off, meta_off) = (cur.be_u64()?, cur.be_u64()?);
    let (start, duration) = (cur.be_u64()?, cur.be_u64()?);
    let _start_ticks = cur.be_u64()?;
    let per_second = cur.be_u64()?;
    let flags = {
        cur.take(2)?;
        cur.be_u16()?
    };
    if size as usize != chunk.len() {
        return Err(JfrError::Header("its size"));
    }
    if per_second == 0 {
        return Err(JfrError::Header("its ticks a second"));
    }
    if flags & 1 == 0 {
        return Err(JfrError::Header("integers not compressed"));
    }
    let off = |o: u64| usize::try_from(o).ok().filter(|o| (HEADER..chunk.len()).contains(o)).ok_or(JfrError::Header("an offset outside it"));
    let (cp_at, meta_at) = (off(cp_off)?, off(meta_off)?);
    let clock = Clock { per_second };
    let meta = metadata(chunk, meta_at)?;
    let pools = pools(chunk, cp_at, &meta)?;
    let s = start.cast_signed();
    if *start_ns == 0 || s < *start_ns {
        *start_ns = s;
    }
    *end_ns = (*end_ns).max(s.saturating_add(duration.cast_signed()));

    let mut raw: Map<(Kind, u64), [f64; 2]> = map();
    let mut read = ChunkRead { events: 0, cpu_interval_ns: None, wall_interval_ns: None, jdk_period_ns: None };
    let mut text = String::new();
    let mut at = HEADER;
    while at < chunk.len() {
        let mut head = Cur::new(chunk);
        head.at = at;
        let size = usize::try_from(head.varint()?).map_err(|_| Bound("an event's size"))?;
        let end = at.checked_add(size).filter(|e| *e <= chunk.len() && size > 0).ok_or(Bound("an event's size"))?;
        let mut cur = Cur { b: chunk.get(..end).ok_or(Truncated)?, at: head.at };
        let ty = cur.varint()?;
        at = end;
        // The metadata and checkpoints (0 and 1) and events of classes kiln does not read: passed
        // over by their size. An event that does not read to its last field kiln reads (a writer's
        // quirk) is passed over alike.
        let Some((_, class)) = meta.class(ty).filter(|_| ty > 1) else { continue };
        match class.role {
            Role::Event(kind) => {
                if let Ok(ev) = sample(&mut cur, &meta, class, clock)
                    && ev.stack != 0
                {
                    read.events += 1;
                    let add = ev.values(kind);
                    let e = raw.entry((kind, ev.stack)).or_insert([0.0, 0.0]);
                    e[0] += add[0];
                    e[1] += add[1];
                }
            }
            Role::Setting => {
                // The interval ddprof samples at (ns), the period the JDK does.
                if let Ok((id, name, value_s)) = setting(&mut cur, &meta, class) {
                    let target = id.and_then(|i| meta.class(i)).map(|(_, c)| c.role);
                    let (name, value_s) = (pools.text(name), pools.text(value_s));
                    let want = match target {
                        Some(Role::Event(Kind::CpuDd | Kind::Wall)) => "interval",
                        Some(Role::Event(Kind::CpuJdk)) => "period",
                        _ => continue,
                    };
                    if !name.is(want) {
                        continue;
                    }
                    text.clear();
                    value_s.write(&mut text);
                    match target {
                        Some(Role::Event(Kind::CpuDd)) => read.cpu_interval_ns = text.trim().parse().ok(),
                        Some(Role::Event(Kind::Wall)) => read.wall_interval_ns = text.trim().parse().ok(),
                        _ => read.jdk_period_ns = period_ns(&text),
                    }
                }
            }
            Role::Config => {
                // ddprof's configuration: its intervals in milliseconds, where no setting said them.
                if let Ok((cpu, wall)) = profiler_config(&mut cur, &meta, class) {
                    read.cpu_interval_ns = read.cpu_interval_ns.or(cpu.filter(|v| *v > 0.0).map(|ms| ms * 1e6));
                    read.wall_interval_ns = read.wall_interval_ns.or(wall.filter(|v| *v > 0.0).map(|ms| ms * 1e6));
                }
            }
            _ => {}
        }
    }

    // The raw values made whole with the chunk's intervals, each stack resolved to its builder's
    // locations (the next chunk's ids mean other things), in the order of their kinds and stacks'
    // keys, not a table's: a recording read twice is the same profiles to the byte (its functions'
    // and locations' ids given in that order) and its sums the same to the bit.
    let cpu = read.cpu_interval_ns.unwrap_or(10e6);
    let wall = read.wall_interval_ns.unwrap_or(50e6);
    let jdk = read.jdk_period_ns.unwrap_or(10e6);
    let mut raw: Vec<((Kind, u64), [f64; 2])> = raw.into_iter().collect();
    raw.sort_unstable_by_key(|&(k, _)| k);
    // Shared by every kind, made once a chunk: each method a function of the chunk's, its name made
    // once into `labels` (a native frame, C, C++ or the kernel's: no class, its file said so); each
    // (method, line) a location of the chunk's; each stack its locations of the chunk's, one after
    // another in `arena`. Each kind's builder then finds its own of each by its place.
    let mut fn_of: Map<u64, u32> = map();
    let mut spans: Vec<(u32, u32, bool)> = Vec::new();
    let mut labels = String::new();
    let mut loc_of: Map<(u64, i32), u32> = map();
    let mut locals: Vec<(u32, i32)> = Vec::new();
    let mut stack_of: Map<u64, (u32, u32)> = map();
    let mut arena: Vec<u32> = Vec::new();
    let (mut class_name, mut method_name, mut scratch) = (String::new(), String::new(), String::new());
    for &((_, stack), _) in &raw {
        if stack_of.contains_key(&stack) {
            continue;
        }
        let Some(&(s, n)) = pools.stacks.get(&stack) else { continue };
        let frames = pools.frames.get(s as usize..(s + n) as usize).unwrap_or_default();
        let from = arena.len() as u32;
        for fr in frames {
            let line = fr.line.max(0);
            let l = match loc_of.get(&(fr.method, line)) {
                Some(&l) => l,
                None => {
                    let f = match fn_of.get(&fr.method) {
                        Some(&f) => f,
                        None => {
                            let (class_key, name_key) = pools.methods.get(&fr.method).copied().unwrap_or((0, 0));
                            class_name.clear();
                            if let Some(&sym) = pools.classes.get(&class_key) {
                                pools.symbol(sym, &mut class_name);
                            }
                            method_name.clear();
                            pools.symbol(name_key, &mut method_name);
                            let at = labels.len() as u32;
                            label_into(&class_name, &method_name, &mut scratch, &mut labels);
                            let native = class_name.is_empty() && pools.frame_kinds.contains(&fr.kind);
                            let f = spans.len() as u32;
                            spans.push((at, labels.len() as u32, native));
                            fn_of.insert(fr.method, f);
                            f
                        }
                    };
                    let l = locals.len() as u32;
                    locals.push((f, line));
                    loc_of.insert((fr.method, line), l);
                    l
                }
            };
            arena.push(l);
        }
        stack_of.insert(stack, (from, arena.len() as u32 - from));
    }
    // Each kind's stacks this chunk, room made for them at once.
    let mut per_kind = [0usize; KINDS];
    for &((kind, _), _) in &raw {
        if let Some(n) = per_kind.get_mut(kind.ix()) {
            *n += 1;
        }
    }
    let mut locs: Vec<u32> = Vec::new();
    for ((kind, stack), v) in raw {
        let Some(&(from, n)) = stack_of.get(&stack) else { continue };
        let scaled = match kind {
            Kind::CpuDd => [v[0], v[1] * cpu],
            Kind::Wall => [v[0], v[1] * wall],
            Kind::CpuJdk => [v[0], v[1] * jdk],
            _ => v,
        };
        let Some(slot) = built.get_mut(kind.ix()) else { continue };
        let b = slot.get_or_insert_with(Built::new);
        b.begin(seq, spans.len(), locals.len(), per_kind.get(kind.ix()).copied().unwrap_or(0));
        locs.clear();
        for &l in arena.get(from as usize..(from + n) as usize).unwrap_or_default() {
            let (f, line) = locals.get(l as usize).copied().unwrap_or((0, 0));
            let (at, to, native) = spans.get(f as usize).copied().unwrap_or((0, 0, false));
            let name = labels.get(at as usize..to as usize).unwrap_or("");
            locs.push(b.location_of(l as usize, f as usize, name, native, i64::from(line)));
        }
        b.add(&locs, scaled);
    }
    Ok(read)
}

/// A profile `b` as pprof: its sample types, its period, its time; and each type's values summed.
fn encode<'a>(b: &Built, types: &[(&'a str, &'a str)], period: Option<((&'a str, &'a str), i64)>, start_ns: i64, duration_ns: i64) -> (Vec<u8>, Vec<i64>) {
    use crate::varint::put;
    // The types' names among the profile's strings, those it has not written after them.
    let mut extra: Vec<&'a str> = Vec::new();
    let at = |i: u32| b.strings.get(i as usize).and_then(|&(f, t)| b.text.get(f as usize..t as usize)).unwrap_or("");
    let mut id = |s: &'a str| -> u64 {
        if let Some(&i) = b.string_of.find(b.keyed.bytes(s.as_bytes()), |&i| at(i) == s) {
            return u64::from(i);
        }
        let base = b.strings.len();
        let i = extra.iter().position(|x| *x == s).unwrap_or_else(|| {
            extra.push(s);
            extra.len() - 1
        });
        (base + i) as u64
    };
    let type_ids: Vec<(u64, u64)> = types.iter().map(|(t, u)| (id(t), id(u))).collect();
    let period_ids = period.map(|((t, u), p)| ((id(t), id(u)), p));
    let mut totals = vec![0i64; types.len()];
    // Its size foreseen, so it is written without growing: each sample's location ids (two bytes
    // each at most, mostly), each location's and function's message, the strings.
    let mut out = Vec::with_capacity(b.stack_locs.len() * 2 + b.samples.len() * 24 + b.locations.len() * 16 + b.functions.len() * 16 + b.text.len() + b.strings.len() * 2 + 128);
    let mut msg = Vec::new();
    let field = |out: &mut Vec<u8>, n: u32, wire: u8| put(out, (u64::from(n) << 3) | u64::from(wire));
    let bytes = |out: &mut Vec<u8>, n: u32, b: &[u8]| {
        field(out, n, 2);
        put(out, b.len() as u64);
        out.extend_from_slice(b);
    };
    let uint = |out: &mut Vec<u8>, n: u32, v: u64| {
        field(out, n, 0);
        put(out, v);
    };
    for &(t, u) in &type_ids {
        msg.clear();
        uint(&mut msg, 1, t);
        uint(&mut msg, 2, u);
        bytes(&mut out, 1, &msg);
    }
    // Samples as they were first summed (the order a recording gives them, so the same each read).
    let (mut ids, mut vals) = (Vec::new(), Vec::new());
    for &(from, n, v) in &b.samples {
        msg.clear();
        ids.clear();
        for &l in b.stack_locs.get(from as usize..(from + n) as usize).unwrap_or_default() {
            put(&mut ids, u64::from(l) + 1);
        }
        bytes(&mut msg, 1, &ids);
        vals.clear();
        for (x, t) in v.iter().zip(totals.iter_mut()) {
            let x = x.round() as i64;
            *t = t.saturating_add(x);
            put(&mut vals, x.cast_unsigned());
        }
        bytes(&mut msg, 2, &vals);
        bytes(&mut out, 2, &msg);
    }
    let mut line = Vec::new();
    for (i, &(f, l)) in b.locations.iter().enumerate() {
        msg.clear();
        uint(&mut msg, 1, i as u64 + 1);
        line.clear();
        uint(&mut line, 1, u64::from(f) + 1);
        uint(&mut line, 2, l.cast_unsigned());
        bytes(&mut msg, 4, &line);
        bytes(&mut out, 4, &msg);
    }
    for (i, &(name, file)) in b.functions.iter().enumerate() {
        msg.clear();
        uint(&mut msg, 1, i as u64 + 1);
        uint(&mut msg, 2, u64::from(name));
        uint(&mut msg, 3, u64::from(name));
        uint(&mut msg, 4, u64::from(file));
        bytes(&mut out, 5, &msg);
    }
    for i in 0..b.strings.len() as u32 {
        bytes(&mut out, 6, at(i).as_bytes());
    }
    for s in &extra {
        bytes(&mut out, 6, s.as_bytes());
    }
    uint(&mut out, 9, start_ns.cast_unsigned());
    uint(&mut out, 10, duration_ns.max(0).cast_unsigned());
    if let Some(((t, u), p)) = period_ids {
        msg.clear();
        uint(&mut msg, 1, t);
        uint(&mut msg, 2, u);
        bytes(&mut out, 11, &msg);
        uint(&mut out, 12, p.cast_unsigned());
    }
    (out, totals)
}

/// `jfr` (a recording inflated: chunks one after another) read into profiles, each a kind's from
/// the source that wrote it (ddprof's where both did): `cpu.pprof`, `wall.pprof`, `alloc.pprof`,
/// `live.pprof`, `lock.pprof`, `exceptions.pprof`, `file-io.pprof`, `socket-io.pprof`. A chunk that
/// does not read is counted and passed over; none reading at all is an error.
pub fn to_pprof(jfr: &[u8]) -> Result<Converted, JfrError> {
    let mut out = Converted::default();
    let mut built: [Option<Built>; KINDS] = std::array::from_fn(|_| None);
    let (mut cpu, mut wall, mut jdk) = (None, None, None);
    let mut at = 0usize;
    let mut last = None;
    while at < jfr.len() {
        if out.chunks + out.chunks_failed >= MAX_CHUNKS {
            break;
        }
        let rest = jfr.get(at..).unwrap_or_default();
        let mut head = Cur::new(rest);
        if head.take(4).ok() != Some(&MAGIC[..]) {
            last = Some(JfrError::Magic);
            break;
        }
        head.take(4)?;
        let size = usize::try_from(head.be_u64()?).map_err(|_| JfrError::Header("its size"))?;
        // A chunk past the file's end (a writer that did not finish it) ends the reading.
        if size < HEADER || size > rest.len() {
            last = Some(JfrError::Header("its size"));
            break;
        }
        let bytes = rest.get(..size).unwrap_or_default();
        let seq = (out.chunks + out.chunks_failed) as u64;
        match chunk(bytes, seq, &mut built, &mut out.start_ns, &mut out.end_ns) {
            Ok(r) => {
                out.chunks += 1;
                out.events += r.events;
                cpu = cpu.or(r.cpu_interval_ns);
                wall = wall.or(r.wall_interval_ns);
                jdk = jdk.or(r.jdk_period_ns);
            }
            Err(e) => {
                out.chunks_failed += 1;
                last = Some(e);
            }
        }
        at += size;
    }
    if out.chunks == 0 {
        return Err(last.unwrap_or(JfrError::Nothing));
    }
    let duration = out.end_ns.saturating_sub(out.start_ns);
    let first = |kinds: &[Kind]| kinds.iter().find(|k| built.get(k.ix()).and_then(Option::as_ref).is_some_and(|b| !b.samples.is_empty())).copied();
    let ns = |v: Option<f64>, d: f64| v.unwrap_or(d).round() as i64;
    let outputs: [(&'static str, &[Kind], &'static [(&'static str, &'static str)], Option<(&str, &str)>); 8] = [
        ("cpu.pprof", &[Kind::CpuDd, Kind::CpuTime, Kind::CpuJdk], &[("samples", "count"), ("cpu", "nanoseconds")], Some(("cpu", "nanoseconds"))),
        ("wall.pprof", &[Kind::Wall], &[("samples", "count"), ("wall", "nanoseconds")], Some(("wall", "nanoseconds"))),
        ("alloc.pprof", &[Kind::AllocDd, Kind::AllocJdk, Kind::AllocTlab], &[("alloc_objects", "count"), ("alloc_space", "bytes")], None),
        ("live.pprof", &[Kind::LiveDd, Kind::LiveJdk], &[("inuse_objects", "count"), ("inuse_space", "bytes")], None),
        ("lock.pprof", &[Kind::Lock], &[("contentions", "count"), ("delay", "nanoseconds")], None),
        ("exceptions.pprof", &[Kind::Exceptions], &[("exception-samples", "count")], None),
        ("file-io.pprof", &[Kind::FileIo], &[("file-io-events", "count"), ("file-io-time", "nanoseconds")], None),
        ("socket-io.pprof", &[Kind::SocketIo], &[("socket-io-events", "count"), ("socket-io-time", "nanoseconds")], None),
    ];
    for (name, kinds, types, period) in outputs {
        let Some(k) = first(kinds) else { continue };
        let Some(b) = built.get(k.ix()).and_then(Option::as_ref) else { continue };
        // The period of the samples it is made of: ddprof's interval, the JDK's period.
        let p = match k {
            Kind::CpuDd => ns(cpu, 10e6),
            Kind::CpuJdk | Kind::CpuTime => ns(jdk, 10e6),
            Kind::Wall => ns(wall, 50e6),
            _ => 0,
        };
        let (pprof, totals) = encode(b, types, period.map(|t| (t, p)), out.start_ns, duration);
        out.profiles.push(Made { name, types, totals, pprof });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::profile::{flame::Flame, pprof};
    use crate::testkit::Rng;

    /// JFR's varint, as short as it goes.
    fn var(out: &mut Vec<u8>, mut v: u64) {
        for _ in 0..8 {
            if v < 0x80 {
                out.push(v as u8);
                return;
            }
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// A varint padded to `width` bytes, as the JDK and ddprof back-patch sizes.
    fn var_padded(out: &mut Vec<u8>, mut v: u64, width: usize) {
        for i in 0..width {
            let last = i + 1 == width;
            out.push(if last { (v & 0x7f) as u8 } else { ((v & 0x7f) as u8) | 0x80 });
            v >>= 7;
        }
    }

    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(3);
        var(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }

    /// A string as the JDK writes its metadata's: UTF-16 units, each a varint.
    fn chars(out: &mut Vec<u8>, s: &str) {
        let units: Vec<u16> = s.encode_utf16().collect();
        out.push(4);
        var(out, units.len() as u64);
        for u in units {
            var(out, u64::from(u));
        }
    }

    /// A class of a recording's metadata: its id, name and fields (name, class, in a pool, an array).
    struct C {
        id: u64,
        name: &'static str,
        fields: Vec<(&'static str, u64, bool, bool)>,
    }

    /// A record: its size (padded to five bytes, as ddprof writes it) and its bytes.
    fn record(type_id: u64, body: &[u8]) -> Vec<u8> {
        let mut inner = Vec::new();
        var(&mut inner, type_id);
        inner.extend_from_slice(body);
        let mut out = Vec::new();
        var_padded(&mut out, (inner.len() + 5) as u64, 5);
        out.extend_from_slice(&inner);
        out
    }

    /// The metadata event of `classes`, its strings UTF-16 (the JDK's way) or UTF-8 (ddprof's).
    fn metadata_event(classes: &[C], utf16: bool) -> Vec<u8> {
        let mut strings: Vec<String> = Vec::new();
        let mut ix = |s: &str| -> u64 {
            if let Some(i) = strings.iter().position(|x| x == s) {
                return i as u64;
            }
            strings.push(s.to_owned());
            strings.len() as u64 - 1
        };
        let mut tree = Vec::new();
        let el = |tree: &mut Vec<u8>, name: u64, attrs: &[(u64, u64)], kids: u64| {
            var(tree, name);
            var(tree, attrs.len() as u64);
            for (k, v) in attrs {
                var(tree, *k);
                var(tree, *v);
            }
            var(tree, kids);
        };
        let (root, meta, region, class, field) = (ix("root"), ix("metadata"), ix("region"), ix("class"), ix("field"));
        el(&mut tree, root, &[], 2);
        el(&mut tree, meta, &[], classes.len() as u64);
        for c in classes {
            let attrs = [(ix("id"), ix(&c.id.to_string())), (ix("name"), ix(c.name))];
            el(&mut tree, class, &attrs, c.fields.len() as u64);
            for &(name, cls, pool, array) in &c.fields {
                let mut attrs = vec![(ix("name"), ix(name)), (ix("class"), ix(&cls.to_string()))];
                if pool {
                    attrs.push((ix("constantPool"), ix("true")));
                }
                if array {
                    attrs.push((ix("dimension"), ix("1")));
                }
                el(&mut tree, field, &attrs, 0);
            }
        }
        el(&mut tree, region, &[], 0);
        let mut body = Vec::new();
        for _ in 0..3 {
            var(&mut body, 0);
        }
        var(&mut body, strings.len() as u64);
        for s in &strings {
            if utf16 { chars(&mut body, s) } else { utf8(&mut body, s) }
        }
        body.extend_from_slice(&tree);
        record(0, &body)
    }

    /// A checkpoint of `pools` (each its class and entries, an entry its key and its value's bytes),
    /// `delta` back to the one before it.
    fn checkpoint(pools: &[(u64, Vec<(u64, Vec<u8>)>)], delta: i64) -> Vec<u8> {
        let mut body = Vec::new();
        var(&mut body, 0);
        var(&mut body, 0);
        var(&mut body, delta.cast_unsigned());
        var(&mut body, 1);
        var(&mut body, pools.len() as u64);
        for (class, entries) in pools {
            var(&mut body, *class);
            var(&mut body, entries.len() as u64);
            for (key, value) in entries {
                var(&mut body, *key);
                body.extend_from_slice(value);
            }
        }
        record(1, &body)
    }

    /// A chunk: its header, its metadata, its events, its checkpoint last (as ddprof writes it).
    fn chunk_of(meta: &[u8], events: &[Vec<u8>], cp: &[u8], start_ns: u64) -> Vec<u8> {
        let mut body = Vec::new();
        let meta_at = HEADER + body.len();
        body.extend_from_slice(meta);
        for e in events {
            body.extend_from_slice(e);
        }
        let cp_at = HEADER + body.len();
        body.extend_from_slice(cp);
        let size = HEADER + body.len();
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&2u16.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        for v in [size as u64, cp_at as u64, meta_at as u64, start_ns, 60_000_000_000, 0, 1_000_000_000] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&body);
        out
    }

    fn sym(s: &str) -> Vec<u8> {
        let mut v = Vec::new();
        utf8(&mut v, s);
        v
    }

    fn refs(keys: &[u64]) -> Vec<u8> {
        let mut v = Vec::new();
        for k in keys {
            var(&mut v, *k);
        }
        v
    }

    /// A stack's frames, leaf first: each (method, line), JIT compiled.
    fn stack(frames: &[(u64, u32)]) -> Vec<u8> {
        let mut v = vec![0u8];
        var(&mut v, frames.len() as u64);
        for &(m, line) in frames {
            var(&mut v, m);
            var(&mut v, u64::from(line));
            var(&mut v, 0);
            var(&mut v, 1);
        }
        v
    }

    /// ddprof's way: CPU and wall samples (a weight of intervals), allocations (a weight and a size),
    /// its interval in a setting; `shift` moves every id, as another chunk's would be.
    fn ddprof_chunk(shift: u64, start_ns: u64) -> Vec<u8> {
        let id = |i: u64| i + shift;
        let classes = [
            C { id: id(1), name: "long", fields: vec![] },
            C { id: id(2), name: "int", fields: vec![] },
            C { id: id(3), name: "boolean", fields: vec![] },
            C { id: id(4), name: "float", fields: vec![] },
            C { id: id(5), name: "java.lang.String", fields: vec![] },
            C { id: id(10), name: "jdk.types.Symbol", fields: vec![("string", id(5), false, false)] },
            C { id: id(11), name: "java.lang.Class", fields: vec![("classLoader", id(1), true, false), ("name", id(10), true, false), ("modifiers", id(2), false, false)] },
            C { id: id(12), name: "jdk.types.Method", fields: vec![("type", id(11), true, false), ("name", id(10), true, false), ("descriptor", id(10), true, false)] },
            C { id: id(13), name: "jdk.types.FrameType", fields: vec![("description", id(5), false, false)] },
            C {
                id: id(14),
                name: "jdk.types.StackFrame",
                fields: vec![("method", id(12), true, false), ("lineNumber", id(2), false, false), ("bytecodeIndex", id(2), false, false), ("type", id(13), true, false)],
            },
            C { id: id(15), name: "jdk.types.StackTrace", fields: vec![("truncated", id(3), false, false), ("frames", id(14), false, true)] },
            C { id: id(101), name: "datadog.ExecutionSample", fields: vec![("startTime", id(1), false, false), ("stackTrace", id(15), true, false), ("weight", id(1), false, false)] },
            C { id: id(102), name: "datadog.MethodSample", fields: vec![("startTime", id(1), false, false), ("stackTrace", id(15), true, false), ("weight", id(1), false, false)] },
            C {
                id: id(103),
                name: "datadog.ObjectSample",
                fields: vec![("startTime", id(1), false, false), ("stackTrace", id(15), true, false), ("size", id(1), false, false), ("weight", id(4), false, false)],
            },
            C { id: id(110), name: "jdk.ActiveSetting", fields: vec![("startTime", id(1), false, false), ("id", id(1), false, false), ("name", id(5), false, false), ("value", id(5), false, false)] },
            C { id: id(120), name: "jdk.SomethingElse", fields: vec![("startTime", id(1), false, false), ("note", id(5), false, false)] },
        ];
        let meta = metadata_event(&classes, shift % 2 == 1);
        let cp = checkpoint(
            &[
                (id(10), vec![(1, sym("Burn")), (2, sym("fib")), (3, sym("main")), (4, sym("java/util/ArrayList")), (5, sym("add")), (6, sym("()V"))]),
                (id(11), vec![(1, refs(&[0, 1, 1])), (2, refs(&[0, 4, 1]))]),
                (id(12), vec![(1, refs(&[1, 2, 6])), (2, refs(&[1, 3, 6])), (3, refs(&[2, 5, 6]))]),
                (id(13), vec![(1, sym("JIT compiled"))]),
                (id(15), vec![(1, stack(&[(1, 3), (1, 3), (2, 10)])), (2, stack(&[(3, 20), (2, 11)]))]),
            ],
            0,
        );
        let ev = |t: u64, fields: &[u64]| record(id(t), &refs(fields));
        let mut setting = refs(&[0, id(101)]);
        utf8(&mut setting, "interval");
        utf8(&mut setting, "10000000");
        let mut alloc = refs(&[0, 2, 1024]);
        alloc.extend_from_slice(&2.5f32.to_be_bytes());
        let mut other = refs(&[0]);
        utf8(&mut other, "nothing kiln reads");
        let events = vec![record(id(110), &setting), ev(101, &[0, 1, 3]), ev(101, &[0, 1, 2]), ev(101, &[0, 2, 1]), record(id(120), &other), ev(102, &[0, 1, 4]), record(id(103), &alloc)];
        chunk_of(&meta, &events, &cp, start_ns)
    }

    /// A profile's flame graph of its type `value`, and each function's (self, total) by name.
    fn graph(bytes: &[u8], value: usize) -> (Flame, std::collections::BTreeMap<String, (i64, i64)>) {
        let p = pprof::read(bytes).unwrap();
        let mut g = Flame::new();
        g.add(&p, value);
        let by = g.by_function().into_iter().map(|(f, o, t)| (g.name(f).0.to_owned(), (o, t))).collect();
        (g, by)
    }

    fn profile<'a>(c: &'a Converted, name: &str) -> &'a [u8] {
        &c.profiles.iter().find(|m| m.name == name).unwrap().pprof
    }

    /// JFR's varint: seven bits a byte for eight, a ninth of eight (a negative long, nine bytes);
    /// padded forms read the same.
    #[test]
    fn jfr_s_varint_is_read_with_its_ninth_byte_and_its_padding() {
        for v in [0u64, 1, 127, 128, 16_383, 1 << 35, 0x8000_0000_0000_0000, u64::MAX] {
            let mut b = Vec::new();
            var(&mut b, v);
            assert!(b.len() <= 9);
            assert_eq!(Cur::new(&b).varint().unwrap(), v, "{v:#x}");
        }
        let mut b = Vec::new();
        var_padded(&mut b, 1, 5);
        assert_eq!((b.as_slice(), Cur::new(&b).varint().unwrap()), (&[0x81, 0x80, 0x80, 0x80, 0x00][..], 1));
        assert_eq!(Cur::new(&[0x80, 0x80]).varint(), Err(JfrError::Truncated));
    }

    /// ddprof's chunk: its CPU samples a weight of its interval (10 ms, its setting's), its wall
    /// samples of the default (50 ms), its allocations a weight of objects and their bytes; frames
    /// labelled `package.Class.method`; an event kiln does not read passed over by its size.
    #[test]
    fn a_ddprof_chunk_becomes_go_s_profiles() {
        let c = to_pprof(&ddprof_chunk(0, 1_792_771_200_000_000_000)).unwrap();
        assert_eq!((c.chunks, c.chunks_failed, c.events), (1, 0, 5));
        assert_eq!(c.profiles.iter().map(|m| m.name).collect::<Vec<_>>(), ["cpu.pprof", "wall.pprof", "alloc.pprof"]);
        let (g, by) = graph(profile(&c, "cpu.pprof"), 1);
        assert_eq!(g.nodes[0].total, 60_000_000, "six intervals of 10 ms");
        assert_eq!(by["Burn.fib"], (50_000_000, 50_000_000), "recursion counted once");
        assert_eq!(by["java.util.ArrayList.add"], (10_000_000, 10_000_000));
        assert_eq!(by["Burn.main"], (0, 60_000_000));
        let (_, samples) = graph(profile(&c, "cpu.pprof"), 0);
        assert_eq!(samples["Burn.main"].1, 6);
        assert_eq!(graph(profile(&c, "wall.pprof"), 1).0.nodes[0].total, 200_000_000, "four of 50 ms");
        let p = pprof::read(profile(&c, "alloc.pprof")).unwrap();
        assert_eq!(p.sample_types.iter().map(|t| (t.kind, t.unit)).collect::<Vec<_>>(), [("alloc_objects", "count"), ("alloc_space", "bytes")]);
        assert_eq!(graph(profile(&c, "alloc.pprof"), 1).0.nodes[0].total, 2_560, "2.5 objects of 1 KiB");
        assert_eq!(p.time_nanos, 1_792_771_200_000_000_000);
    }

    /// Two chunks, their ids each their own (one's metadata UTF-16, the JDK's way): read alone, their
    /// stacks joined by their names.
    #[test]
    fn chunks_of_other_ids_are_read_alone_and_joined_by_name() {
        let mut both = ddprof_chunk(0, 1_000);
        both.extend_from_slice(&ddprof_chunk(1_001, 2_000));
        let c = to_pprof(&both).unwrap();
        assert_eq!((c.chunks, c.events, c.start_ns), (2, 10, 1_000));
        let (g, by) = graph(profile(&c, "cpu.pprof"), 1);
        assert_eq!((g.nodes[0].total, by["Burn.fib"].1), (120_000_000, 100_000_000));
    }

    /// Refused or passed over, never grown past what the bytes allow: not a recording; a chunk past
    /// the file's end (the one before it kept); a checkpoint not read to its size (that chunk
    /// dropped, the others kept); any byte changed anywhere.
    #[test]
    fn a_damaged_recording_loses_what_is_damaged_alone() {
        assert_eq!(to_pprof(b"not a recording").unwrap_err(), JfrError::Magic);
        let good = ddprof_chunk(0, 1);
        let mut cut = good.clone();
        cut.extend_from_slice(&good[..good.len() / 2]);
        let c = to_pprof(&cut).unwrap();
        assert_eq!((c.chunks, c.events), (1, 5), "the whole chunk read, the half one not");
        // A checkpoint whose pools say less than its size: the chunk dropped, the next one kept.
        let mut bad = good.clone();
        let cp_at = u64::from_be_bytes(bad[16..24].try_into().unwrap()) as usize;
        // Its size (five bytes, padded) one larger, a byte more at its end, the chunk's size too.
        let size = Cur::new(&bad[cp_at..]).varint().unwrap() + 1;
        let mut padded = Vec::new();
        var_padded(&mut padded, size, 5);
        bad[cp_at..cp_at + 5].copy_from_slice(&padded);
        bad.push(0);
        let total = bad.len() as u64;
        bad[8..16].copy_from_slice(&total.to_be_bytes());
        bad.extend_from_slice(&good);
        let c = to_pprof(&bad).unwrap();
        assert_eq!((c.chunks, c.chunks_failed, c.events), (1, 1, 5));
        let mut rng = Rng::new(9);
        for _ in 0..3_000 {
            let mut b = good.clone();
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(b.len() as u64) as usize;
                b[i] ^= 1 + rng.below(255) as u8;
            }
            if let Ok(c) = to_pprof(&b) {
                assert!(c.events <= 7 && c.profiles.len() <= 8);
            }
        }
    }

    /// A fixture recording (`fixtures/jfr`) inflated and read.
    fn fixture(zst: &[u8]) -> Converted {
        let mut plain = Vec::new();
        crate::zstd::decompress(zst, &mut plain, 64 << 20).unwrap();
        to_pprof(&plain).unwrap()
    }

    /// Recordings as dd-trace-java 1.67.1 uploads them (`fixtures/jfr`, `Burn.java.txt` says how they
    /// were taken: a recursive Fibonacci; an allocation and an exception every 16th turn of its loop;
    /// two threads taking turns on a monitor), its default (the JDK's chunk, then ddprof's) and the
    /// JDK's alone. Each profile's values summed as jfrsum.py sums them, a reader written apart from
    /// kiln's (from the JDK's and JMC's: `docs/observe.md` §8.11), to the nanosecond and the byte
    /// where each sample's are whole, within half a unit a stack where they are not (Poisson
    /// sampling's weights); its functions alike: its recursive function most of its CPU, its loop
    /// most of its allocations, its two threads its lock waits.
    #[test]
    fn dd_trace_java_s_recordings_read_as_a_reader_written_apart_sums_them() {
        let total = |c: &Converted, name: &str| c.profiles.iter().find(|m| m.name == name).unwrap().totals.clone();
        let near = |got: i64, want: f64, stacks: usize| (got as f64 - want).abs() <= stacks as f64 * 0.5;
        let stacks = |c: &Converted, name: &str| pprof::read(profile(c, name)).unwrap().samples.len();

        let both = fixture(include_bytes!("../fixtures/jfr/dd-trace-java-1.67.1-burn.jfr.zst"));
        assert_eq!((both.chunks, both.chunks_failed), (2, 0));
        assert_eq!(fixture(include_bytes!("../fixtures/jfr/dd-trace-java-1.67.1-burn.jfr.zst")).profiles, both.profiles, "read twice, the same profiles to the byte");
        assert_eq!(both.profiles.iter().map(|m| m.name).collect::<Vec<_>>(), ["cpu.pprof", "wall.pprof", "alloc.pprof", "live.pprof", "lock.pprof", "exceptions.pprof"]);
        // ddprof's CPU samples, a 10 ms interval each (its configuration's: no setting says it).
        assert_eq!(total(&both, "cpu.pprof"), [389, 3_890_000_000]);
        assert_eq!(total(&both, "wall.pprof"), [1_726, 86_300_000_000]);
        assert_eq!(total(&both, "lock.pprof"), [37, 5_866_604_437]);
        assert_eq!(total(&both, "exceptions.pprof"), [1_629]);
        let (alloc, live) = (total(&both, "alloc.pprof"), total(&both, "live.pprof"));
        assert!(near(alloc[0], 27_656.285, stacks(&both, "alloc.pprof")) && near(alloc[1], 58_153_056.825, stacks(&both, "alloc.pprof")), "{alloc:?}");
        assert!(near(live[0], 296_534.039, stacks(&both, "live.pprof")) && near(live[1], 84_151_487.647, stacks(&both, "live.pprof")), "{live:?}");
        let (_, cpu) = graph(profile(&both, "cpu.pprof"), 1);
        assert_eq!((cpu["Burn.fib"], cpu["Burn.main"].1), ((3_500_000_000, 3_500_000_000), 3_580_000_000), "its recursive function 90% of its CPU, counted once");
        let (_, alloc) = graph(profile(&both, "alloc.pprof"), 1);
        assert!(near(alloc["Burn.main"].0, 54_381_127.0, stacks(&both, "alloc.pprof")), "its loop 94% of its allocations: {:?}", alloc["Burn.main"]);
        let (_, lock) = graph(profile(&both, "lock.pprof"), 1);
        assert_eq!((lock["Burn.contend"], lock["Burn$$Lambda.run"].1), ((5_866_604_437, 5_866_604_437), 5_866_604_437), "its threads all of its lock waits");

        let jdk = fixture(include_bytes!("../fixtures/jfr/dd-trace-java-1.67.1-burn-jdk.jfr.zst"));
        assert_eq!((jdk.chunks, jdk.chunks_failed), (1, 0));
        assert_eq!(jdk.profiles.iter().map(|m| m.name).collect::<Vec<_>>(), ["cpu.pprof", "alloc.pprof", "live.pprof", "lock.pprof", "exceptions.pprof"]);
        // The JDK's execution samples, a period each: its setting's, `9 ms` (dd-trace-java's).
        assert_eq!(total(&jdk, "cpu.pprof"), [592, 5_328_000_000]);
        assert_eq!(total(&jdk, "alloc.pprof"), [77, 133_611_656]);
        assert_eq!(total(&jdk, "live.pprof"), [7, 176]);
        assert_eq!(total(&jdk, "lock.pprof"), [233, 54_691_681_172]);
        assert_eq!(total(&jdk, "exceptions.pprof"), [3_851]);
        let (_, cpu) = graph(profile(&jdk, "cpu.pprof"), 1);
        assert_eq!(cpu["Burn.fib"].0, 5_247_000_000);
        let (_, alloc) = graph(profile(&jdk, "alloc.pprof"), 1);
        assert_eq!(alloc["Burn.main"].0, 129_371_096);
        // Its lock waits the JDK's parks too (its executors' threads idle), and its two threads'.
        let (_, lock) = graph(profile(&jdk, "lock.pprof"), 1);
        assert_eq!((lock["jdk.internal.misc.Unsafe.park"].0, lock["Burn.contend"].0), (38_866_749_206, 9_824_727_137));
    }

    /// A string read in place in each of JFR's encodings: written whole (a surrogate pair's
    /// character one, a lone one U+FFFD; Latin-1's bytes their own characters) and compared with no
    /// string made; a pool's resolved through its strings.
    #[test]
    fn a_string_is_read_in_place_in_each_encoding() {
        let mut b = Vec::new();
        utf8(&mut b, "jdk.ExecutionSample");
        chars(&mut b, "ma\u{1F600}n");
        b.push(4);
        var(&mut b, 2);
        var(&mut b, 0xD800);
        var(&mut b, u64::from(b'x'));
        b.push(5);
        var(&mut b, 3);
        b.extend_from_slice(&[b'c', 0xE9, b'!']);
        b.extend_from_slice(&[0, 1, 2, 7]);
        let mut cur = Cur::new(&b);
        let read: Vec<JStr<'_>> = (0..7).map(|_| cur.string().unwrap()).collect();
        assert_eq!(cur.left(), 0);
        let text = |s: &JStr<'_>| {
            let mut out = String::new();
            s.write(&mut out);
            out
        };
        assert_eq!(read.iter().map(text).collect::<Vec<_>>(), ["jdk.ExecutionSample", "ma\u{1F600}n", "\u{FFFD}x", "c\u{e9}!", "", "", ""]);
        assert!(read[0].is("jdk.ExecutionSample") && !read[0].is("jdk.ExecutionSampl") && read[1].is("ma\u{1F600}n") && !read[1].is("man"));
        assert!(read[3].is("c\u{e9}!") && read[4].is("") && read[5].is("") && !read[6].is(""));
        assert_eq!(read[6], JStr::Pool(7));
        let mut p = Pools::new();
        p.strings.insert(7, read[0]);
        assert_eq!(p.text(read[6]), read[0]);
        assert_eq!(p.text(JStr::Pool(8)), JStr::Null, "a key no pool has");
    }

    /// The JDK's settings' periods (`9 ms`, `20 ms`, `1 s`); `everyChunk` none.
    #[test]
    fn a_jdk_period_is_read() {
        assert_eq!(period_ns("9 ms"), Some(9e6));
        assert_eq!(period_ns("20 ms"), Some(20e6));
        assert_eq!(period_ns("1 s"), Some(1e9));
        assert_eq!(period_ns("everyChunk"), None);
    }

    /// Lambdas' and generated accessors' numbers left out of their names; a native frame by its
    /// symbol alone.
    #[test]
    fn frames_are_named_as_profiles_name_them() {
        assert_eq!(label("java/lang/String", "hashCode"), "java.lang.String.hashCode");
        assert_eq!(label("jdk/jfr/internal/PlatformRecorder$$Lambda$295+0x0000000800f78810/307840497", "run"), "jdk.jfr.internal.PlatformRecorder$$Lambda.run");
        assert_eq!(label("jdk/internal/reflect/GeneratedMethodAccessor42", "invoke"), "jdk.internal.reflect.GeneratedMethodAccessor.invoke");
        assert_eq!(label("", "JavaThread::run()"), "JavaThread::run()");
        // A hidden class as a recording names it, in either form: its address left out.
        assert_eq!(label("java.lang.invoke.LambdaForm$DMH/0x00007f3ef4084000", "invokeStatic"), "java.lang.invoke.LambdaForm$DMH.invokeStatic");
        assert_eq!(label("jdk/jfr/internal/instrument/JDKEvents$$Lambda.0x00007f3ef436fe60", "get"), "jdk.jfr.internal.instrument.JDKEvents$$Lambda.get");
        assert_eq!(label("Burn$$Lambda/0x000071d4c8001000", "run"), "Burn$$Lambda.run");
        assert_eq!(label("com/acme/v0x/Handler", "get"), "com.acme.v0x.Handler.get", "a name with 0x in it, not an address");
    }
}
