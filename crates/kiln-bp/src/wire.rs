//! The protocol's encoding (docs/browser-protocol.md, "Encoding"): postcard's wire format for the
//! types the protocol uses, read under its rules. Every length and count is checked against the
//! bytes left before anything is allocated, and nothing here panics on any bytes.

use crate::rng::Rng;
use crate::schema::{Registry, Ty};

/// The largest integer JavaScript holds exactly: `u53` and `i53` fields stay within it.
pub const MAX_SAFE: u64 = (1 << 53) - 1;

/// Why a frame couldn't be read (docs/browser-protocol.md, "Refusals"). The index is the wire's:
/// appended only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Why {
    CutShort = 0,
    TooLarge = 1,
    BadVarint = 2,
    BadValue = 3,
    TooDeep = 4,
    UnknownKind = 5,
    UnknownVariant = 6,
    LeftOver = 7,
    OutOfOrder = 8,
}

/// Every reason, in index order.
pub const WHYS: [Why; 9] = [Why::CutShort, Why::TooLarge, Why::BadVarint, Why::BadValue, Why::TooDeep, Why::UnknownKind, Why::UnknownVariant, Why::LeftOver, Why::OutOfOrder];

impl Why {
    /// docs/wire.md's code for it.
    pub fn code(self) -> Code {
        match self {
            Why::UnknownKind | Why::UnknownVariant => Code::Unknown,
            Why::CutShort | Why::TooLarge | Why::BadVarint | Why::BadValue | Why::TooDeep | Why::LeftOver | Why::OutOfOrder => Code::Malformed,
        }
    }

    pub fn index(self) -> u32 {
        self as u32
    }

    pub fn of_index(i: u64) -> Option<Why> {
        WHYS.get(usize::try_from(i).ok()?).copied()
    }

    /// Its name, as the docs and the TypeScript side say it.
    pub fn name(self) -> &'static str {
        match self {
            Why::CutShort => "cut_short",
            Why::TooLarge => "too_large",
            Why::BadVarint => "bad_varint",
            Why::BadValue => "bad_value",
            Why::TooDeep => "too_deep",
            Why::UnknownKind => "unknown_kind",
            Why::UnknownVariant => "unknown_variant",
            Why::LeftOver => "left_over",
            Why::OutOfOrder => "out_of_order",
        }
    }
}

/// docs/wire.md's codes: a newer build's kind (`unknown`), or bytes that don't read (`malformed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    Unknown = 0,
    Malformed = 1,
}

impl Code {
    pub fn name(self) -> &'static str {
        match self {
            Code::Unknown => "unknown",
            Code::Malformed => "malformed",
        }
    }
}

/// A refusal: why, and nothing of what the bytes said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub why: Why,
}

impl Refusal {
    pub fn code(self) -> Code {
        self.why.code()
    }
}

impl From<Why> for Refusal {
    fn from(why: Why) -> Self {
        Self { why }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.why.name(), self.code().name())
    }
}

impl std::error::Error for Refusal {}

/// Where encoded bytes go.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Self { buf: Vec::with_capacity(n) }
    }

    pub fn byte(&mut self, b: u8) {
        self.buf.push(b);
    }

    pub fn varint(&mut self, mut v: u64) {
        loop {
            let low = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(low);
                return;
            }
            self.buf.push(low | 0x80);
        }
    }

    /// A length or count.
    pub fn len(&mut self, n: usize) {
        self.varint(u64::try_from(n).unwrap_or(u64::MAX));
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.buf.extend_from_slice(b);
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// How many bytes `v`'s varint takes.
pub fn varint_len(v: u64) -> usize {
    let bits = 64 - (v | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// A cursor over bytes being read.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes not yet read.
    pub fn left(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn is_empty(&self) -> bool {
        self.left() == 0
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn byte(&mut self) -> Result<u8, Refusal> {
        let b = *self.buf.get(self.pos).ok_or(Why::CutShort)?;
        self.pos = self.pos.saturating_add(1);
        Ok(b)
    }

    /// An unsigned varint of up to 64 bits: at most 10 bytes, the tenth at most 1.
    pub fn varint(&mut self) -> Result<u64, Refusal> {
        let mut v = 0u64;
        for i in 0..10u32 {
            let b = self.byte()?;
            let low = u64::from(b & 0x7f);
            if i == 9 && b > 1 {
                return Err(Why::BadVarint.into());
            }
            v |= low << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Why::BadVarint.into())
    }

    /// A count of things each at least one byte long: never past the bytes left.
    pub fn count(&mut self) -> Result<usize, Refusal> {
        let n = self.varint()?;
        match usize::try_from(n) {
            Ok(n) if n <= self.left() => Ok(n),
            _ => Err(Why::CutShort.into()),
        }
    }

    /// `n` bytes, checked against what's left before anything is taken.
    pub fn slice(&mut self, n: usize) -> Result<&'a [u8], Refusal> {
        if n > self.left() {
            return Err(Why::CutShort.into());
        }
        let end = self.pos.saturating_add(n);
        let s = self.buf.get(self.pos..end).ok_or(Why::CutShort)?;
        self.pos = end;
        Ok(s)
    }

    /// A length-prefixed run of bytes.
    pub fn bytes(&mut self) -> Result<&'a [u8], Refusal> {
        let n = self.count()?;
        self.slice(n)
    }

    /// What's left, all of it.
    pub fn rest(&mut self) -> &'a [u8] {
        let s = self.buf.get(self.pos..).unwrap_or(&[]);
        self.pos = self.buf.len();
        s
    }
}

/// A type the protocol carries: how it's written, read, named in the schema, and shown as JSON
/// in the shared test vectors.
pub trait Wire: Sized {
    fn put(&self, w: &mut Writer);
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal>;
    /// Reads past one without making it: refuses exactly what `take` refuses, allocating
    /// nothing, so a frame is checked whole before any of it is built.
    fn skip(r: &mut Reader<'_>) -> Result<(), Refusal> {
        Self::take(r).map(|_| ())
    }
    /// Its type in the schema.
    fn ty() -> Ty;
    /// Adds its shape (and what it holds) to the registry.
    fn visit(_reg: &mut Registry) {}
    /// The canonical JSON the shared vectors hold (docs/browser-protocol.md: JSON.stringify's
    /// escapes for strings, `bits` as a decimal string, `bytes` as lowercase hex).
    fn json(&self, out: &mut String);
    /// A value drawn from `g`, for structure-aware fuzzing: the TypeScript side draws the same
    /// value from the same generator (docs/browser-protocol.md, "Testing").
    fn arb(g: &mut Rng) -> Self;
}

/// The characters drawn strings are made of: ASCII, what JSON escapes, and two, three and four
/// byte UTF-8.
pub const ALPHABET: [char; 13] = ['a', 'b', 'z', '0', '9', ' ', '"', '\\', '\n', '\u{1}', 'é', '中', '😀'];

/// A record a topic carries field by field (docs/browser-protocol.md, "Changes"): each field its
/// own encoding, so the generic core merges and keeps them without the type, and a delta carries
/// only the fields that changed.
pub trait Fields: Sized {
    fn fields(&self) -> Vec<Bytes>;
    /// The record from its fields; a missing or empty one is its type's default.
    fn from_fields(fields: &[Bytes]) -> Result<Self, Refusal>;
}

/// What changed from `old` to `new`, field by field: the mask of the fields that differ and their
/// new encodings (a `Patch`'s or `Set`'s).
pub fn diff(old: &[Bytes], new: &[Bytes]) -> (Bits, Vec<Bytes>) {
    let mut mask = 0u64;
    let mut out = Vec::new();
    for (i, f) in new.iter().enumerate().take(64) {
        if old.get(i) != Some(f) {
            mask |= 1u64 << i;
            out.push(f.clone());
        }
    }
    (Bits(mask), out)
}

/// A frame's body: read tolerantly (missing trailing fields default, newer ones are counted),
/// written for the version the peer speaks (fields newer than it left off).
pub trait Body: Wire + Default {
    fn put_body(&self, w: &mut Writer, version: u32);
    fn take_body(r: &mut Reader<'_>) -> Result<Self, Refusal>;
    fn skip_body(r: &mut Reader<'_>) -> Result<(), Refusal>;
}

macro_rules! uint {
    ($t:ty, $ty:ident, $arb:ident) => {
        impl Wire for $t {
            fn put(&self, w: &mut Writer) {
                w.varint(u64::from(*self));
            }
            fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
                <$t>::try_from(r.varint()?).map_err(|_| Why::BadVarint.into())
            }
            fn ty() -> Ty {
                Ty::$ty
            }
            fn json(&self, out: &mut String) {
                out.push_str(&self.to_string());
            }
            fn arb(g: &mut Rng) -> Self {
                $arb(g)
            }
        }
    };
}

fn arb_u16(g: &mut Rng) -> u16 {
    let v = if g.coin() { g.below(128) } else { g.below(65_536) };
    u16::try_from(v).unwrap_or(0)
}

fn arb_u32(g: &mut Rng) -> u32 {
    let v = match g.below(3) {
        0 => g.below(128),
        1 => g.below(16_384),
        _ => g.below(1 << 32),
    };
    u32::try_from(v).unwrap_or(0)
}

fn arb_u53(g: &mut Rng) -> u64 {
    match g.below(4) {
        0 => g.below(128),
        1 => g.below(16_384),
        2 => g.below(1 << 32),
        _ => g.next_u64() & MAX_SAFE,
    }
}

uint!(u16, U16, arb_u16);
uint!(u32, U32, arb_u32);

impl Wire for u8 {
    fn put(&self, w: &mut Writer) {
        w.byte(*self);
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        r.byte()
    }
    fn ty() -> Ty {
        Ty::U8
    }
    fn json(&self, out: &mut String) {
        out.push_str(&self.to_string());
    }
    fn arb(g: &mut Rng) -> Self {
        g.byte()
    }
}

/// `u53`: an unsigned integer JavaScript holds exactly.
impl Wire for u64 {
    fn put(&self, w: &mut Writer) {
        w.varint(*self);
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        let v = r.varint()?;
        if v > MAX_SAFE {
            return Err(Why::BadVarint.into());
        }
        Ok(v)
    }
    fn ty() -> Ty {
        Ty::U53
    }
    fn json(&self, out: &mut String) {
        out.push_str(&self.to_string());
    }
    fn arb(g: &mut Rng) -> Self {
        arb_u53(g)
    }
}

/// `i53`: a signed integer JavaScript holds exactly, zigzag encoded.
impl Wire for i64 {
    fn put(&self, w: &mut Writer) {
        w.varint(((*self << 1) ^ (*self >> 63)) as u64);
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        let v = r.varint()?;
        // zigzag(±(2^53 - 1)) is at most 2^54 - 2.
        if v > (1u64 << 54) - 2 {
            return Err(Why::BadVarint.into());
        }
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
    fn ty() -> Ty {
        Ty::I53
    }
    fn json(&self, out: &mut String) {
        out.push_str(&self.to_string());
    }
    fn arb(g: &mut Rng) -> Self {
        let m = arb_u53(g) as i64;
        if g.coin() { -m } else { m }
    }
}

/// `bits`: all 64 bits (capabilities, field masks, hashes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, PartialOrd, Ord)]
pub struct Bits(pub u64);

impl Wire for Bits {
    fn put(&self, w: &mut Writer) {
        w.varint(self.0);
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        Ok(Bits(r.varint()?))
    }
    fn ty() -> Ty {
        Ty::Bits
    }
    fn json(&self, out: &mut String) {
        out.push('"');
        out.push_str(&self.0.to_string());
        out.push('"');
    }
    fn arb(g: &mut Rng) -> Self {
        Bits(match g.below(3) {
            0 => g.below(256),
            1 => g.next_u64(),
            _ => 1u64 << g.below(64),
        })
    }
}

impl Wire for bool {
    fn put(&self, w: &mut Writer) {
        w.byte(u8::from(*self));
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        match r.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Why::BadValue.into()),
        }
    }
    fn ty() -> Ty {
        Ty::Bool
    }
    fn json(&self, out: &mut String) {
        out.push_str(if *self { "true" } else { "false" });
    }
    fn arb(g: &mut Rng) -> Self {
        g.coin()
    }
}

impl Wire for String {
    fn put(&self, w: &mut Writer) {
        w.bytes(self.as_bytes());
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        let b = r.bytes()?;
        std::str::from_utf8(b).map(str::to_owned).map_err(|_| Why::BadValue.into())
    }
    fn skip(r: &mut Reader<'_>) -> Result<(), Refusal> {
        std::str::from_utf8(r.bytes()?).map(|_| ()).map_err(|_| Why::BadValue.into())
    }
    fn ty() -> Ty {
        Ty::Str
    }
    fn json(&self, out: &mut String) {
        json_str(self, out);
    }
    fn arb(g: &mut Rng) -> Self {
        let n = g.below(12);
        (0..n).map(|_| ALPHABET.get(g.index(ALPHABET.len())).copied().unwrap_or('a')).collect()
    }
}

/// `bytes`: raw bytes, length first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Bytes(pub Vec<u8>);

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Self(v.to_vec())
    }
}

impl From<&str> for Bytes {
    fn from(v: &str) -> Self {
        Self(v.as_bytes().to_vec())
    }
}

impl Wire for Bytes {
    fn put(&self, w: &mut Writer) {
        w.bytes(&self.0);
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        Ok(Bytes(r.bytes()?.to_vec()))
    }
    fn skip(r: &mut Reader<'_>) -> Result<(), Refusal> {
        r.bytes().map(|_| ())
    }
    fn ty() -> Ty {
        Ty::Bytes
    }
    fn json(&self, out: &mut String) {
        out.push('"');
        hex(&self.0, out);
        out.push('"');
    }
    fn arb(g: &mut Rng) -> Self {
        let n = g.below(16);
        Bytes((0..n).map(|_| g.byte()).collect())
    }
}

impl<T: Wire> Wire for Option<T> {
    fn put(&self, w: &mut Writer) {
        match self {
            None => w.byte(0),
            Some(v) => {
                w.byte(1);
                v.put(w);
            }
        }
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        match r.byte()? {
            0 => Ok(None),
            1 => Ok(Some(T::take(r)?)),
            _ => Err(Why::BadValue.into()),
        }
    }
    fn skip(r: &mut Reader<'_>) -> Result<(), Refusal> {
        match r.byte()? {
            0 => Ok(()),
            1 => T::skip(r),
            _ => Err(Why::BadValue.into()),
        }
    }
    fn ty() -> Ty {
        Ty::Opt(Box::new(T::ty()))
    }
    fn visit(reg: &mut Registry) {
        T::visit(reg);
    }
    fn json(&self, out: &mut String) {
        match self {
            None => out.push_str("null"),
            Some(v) => v.json(out),
        }
    }
    fn arb(g: &mut Rng) -> Self {
        if g.coin() { Some(T::arb(g)) } else { None }
    }
}

impl<T: Wire> Wire for Vec<T> {
    fn put(&self, w: &mut Writer) {
        w.len(self.len());
        for v in self {
            v.put(w);
        }
    }
    fn take(r: &mut Reader<'_>) -> Result<Self, Refusal> {
        // Each item takes a byte at least, so a count past the bytes left is refused before any
        // room is made; and room grows only as items are read.
        let n = r.count()?;
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(T::take(r)?);
        }
        Ok(v)
    }
    fn skip(r: &mut Reader<'_>) -> Result<(), Refusal> {
        let n = r.count()?;
        for _ in 0..n {
            T::skip(r)?;
        }
        Ok(())
    }
    fn ty() -> Ty {
        Ty::Vec(Box::new(T::ty()))
    }
    fn visit(reg: &mut Registry) {
        T::visit(reg);
    }
    fn json(&self, out: &mut String) {
        out.push('[');
        for (i, v) in self.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            v.json(out);
        }
        out.push(']');
    }
    fn arb(g: &mut Rng) -> Self {
        let n = g.below(4);
        (0..n).map(|_| T::arb(g)).collect()
    }
}

/// A string as JSON.stringify writes it.
pub fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Lowercase hex.
pub fn hex(b: &[u8], out: &mut String) {
    const D: &[u8; 16] = b"0123456789abcdef";
    for x in b {
        out.push(char::from(D.get(usize::from(x >> 4)).copied().unwrap_or(b'0')));
        out.push(char::from(D.get(usize::from(x & 15)).copied().unwrap_or(b'0')));
    }
}

/// Hex back to bytes (the vectors' form); `None` for what isn't.
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    b.chunks(2).map(|p| Some((nib(*p.first()?)? << 4) | nib(*p.get(1)?)?)).collect()
}

/// Encodes one value alone.
pub fn to_bytes<T: Wire>(v: &T) -> Vec<u8> {
    let mut w = Writer::new();
    v.put(&mut w);
    w.buf
}

/// Reads one value that must take every byte.
pub fn from_bytes<T: Wire>(b: &[u8]) -> Result<T, Refusal> {
    let mut r = Reader::new(b);
    let v = T::take(&mut r)?;
    if !r.is_empty() {
        return Err(Why::LeftOver.into());
    }
    Ok(v)
}

/// A value as the vectors' JSON.
pub fn json_of<T: Wire>(v: &T) -> String {
    let mut s = String::new();
    v.json(&mut s);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_read_back_and_refuse_past_their_type() {
        for v in [0u64, 1, 127, 128, 300, 16_383, 16_384, MAX_SAFE] {
            let b = to_bytes(&v);
            assert_eq!(b.len(), varint_len(v));
            assert_eq!(from_bytes::<u64>(&b), Ok(v));
        }
        assert_eq!(from_bytes::<u64>(&to_bytes(&Bits(MAX_SAFE + 1))), Err(Why::BadVarint.into()));
        assert_eq!(from_bytes::<u32>(&to_bytes(&Bits(1 << 32))), Err(Why::BadVarint.into()));
        assert_eq!(from_bytes::<Bits>(&to_bytes(&Bits(u64::MAX))), Ok(Bits(u64::MAX)));
        // Eleven bytes, or a tenth past 1, is no varint.
        assert_eq!(from_bytes::<Bits>(&[0xff; 11]), Err(Why::BadVarint.into()));
        assert_eq!(from_bytes::<Bits>(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]), Err(Why::BadVarint.into()));
        assert_eq!(from_bytes::<Bits>(&[0x80]), Err(Why::CutShort.into()));
    }

    #[test]
    fn zigzag_holds_javascripts_range() {
        for v in [0i64, -1, 1, -64, 64, MAX_SAFE as i64, -(MAX_SAFE as i64)] {
            assert_eq!(from_bytes::<i64>(&to_bytes(&v)), Ok(v));
        }
        assert_eq!(from_bytes::<i64>(&to_bytes(&Bits((1 << 54) - 1))), Err(Why::BadVarint.into()));
    }

    #[test]
    fn lengths_past_the_bytes_left_are_refused_before_allocating() {
        let mut w = Writer::new();
        w.varint(u64::MAX >> 1);
        assert_eq!(from_bytes::<String>(&w.buf), Err(Why::CutShort.into()));
        assert_eq!(from_bytes::<Vec<String>>(&w.buf), Err(Why::CutShort.into()));
        assert_eq!(from_bytes::<String>(&[2, 0xc3, 0x28]), Err(Why::BadValue.into()));
        assert_eq!(from_bytes::<bool>(&[2]), Err(Why::BadValue.into()));
        assert_eq!(from_bytes::<Option<u8>>(&[7]), Err(Why::BadValue.into()));
    }

    #[test]
    fn json_is_javascripts() {
        let mut s = String::new();
        json_str("a\"\\\n\u{1}é", &mut s);
        assert_eq!(s, "\"a\\\"\\\\\\n\\u0001é\"");
        assert_eq!(json_of(&Bytes(vec![0, 255, 16])), "\"00ff10\"");
        assert_eq!(unhex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(json_of(&Bits(u64::MAX)), "\"18446744073709551615\"");
    }
}
