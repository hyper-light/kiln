//! Grok's own matcher (`docs/observe.md` §8.2, §12.17): a rule's expression, as regex's own parser
//! reads it, compiled into a program a backtracking machine runs from the value's start, reading
//! the rule's groups as it goes. One pass where `regex` makes two (its lazy DFA finds the match,
//! then its backtracker reads the groups, after clearing a table of every state at every byte), and
//! the same match: the leftmost-first, its groups where `regex` puts them (a differential test holds
//! them alike).
//!
//! A character is told by its first byte (where that does not tell, a Unicode class's wider
//! members are looked up by their ranges). A class repeated (`\S+`, `[^"]*`, `.*?`) is read as a
//! run: one that nothing after it may begin with what it holds gives nothing back, and one that
//! does gives back only to where what follows may begin; of two ways on, one that cannot begin
//! where the value is, is not tried. Past a first stretch of work, a table of where it has been
//! keeps a search to `regex`'s bound: each instruction at each byte once.
//!
//! What it leaves to `regex`: a loop that may match nothing each round (its empty rounds are
//! `regex`'s to tell), a program past [`INSTS`], and a search past its bound of work or of table.

use std::cmp::Ordering;
use std::collections::HashMap;

use regex_syntax::hir::{Class, Hir, HirKind, Look, Repetition};

/// Instructions a program holds at the most.
const INSTS: usize = 1 << 13;
/// Bits a search's table of where it has been takes at the most (4 MiB).
const MEMO_BITS: usize = 1 << 25;
/// Bytes of room a thread keeps for its table and its ways back between searches (a long value's,
/// past it, is handed back).
const KEPT_ROOM: usize = 64 << 10;
/// A slot not set; a run without end; no loop.
const NONE: u32 = u32::MAX;
/// The longest value a program reads (its places are `u32`s).
const LONGEST: usize = (u32::MAX - 2) as usize;

/// What a set says of a character by its first byte: not a member.
const OUT: u8 = 0;
/// An ASCII member.
const ONE: u8 = 1;
/// Every character this byte begins is a member.
const WHOLE: u8 = 2;
/// Some are: its ranges say.
const ASK: u8 = 3;

/// A class of characters.
#[derive(Debug, Clone)]
struct Set {
    /// By a character's first byte: [`OUT`], [`ONE`], [`WHOLE`] or [`ASK`].
    table: [u8; 256],
    /// Its members past ASCII: ranges of scalar values, sorted, apart.
    wide: Box<[(u32, u32)]>,
}

/// The characters a byte begins (as UTF-8 encodes them): the first and the last.
fn lead_range(lead: u32) -> (u32, u32) {
    match lead {
        0xC2..=0xDF => ((lead & 0x1F) << 6, ((lead & 0x1F) << 6) | 0x3F),
        0xE0 => (0x800, 0xFFF),
        0xE1..=0xEF => ((lead & 0x0F) << 12, ((lead & 0x0F) << 12) | 0xFFF),
        0xF0 => (0x10000, 0x3FFFF),
        _ => ((lead & 0x07) << 18, (((lead & 0x07) << 18) | 0x3FFFF).min(0x10FFFF)),
    }
}

/// The scalar values in `lo..=hi` (no surrogate is one).
fn scalars(lo: u32, hi: u32) -> u64 {
    if hi < lo {
        return 0;
    }
    let all = u64::from(hi - lo) + 1;
    let (s, e) = (lo.max(0xD800), hi.min(0xDFFF));
    all - if s <= e { u64::from(e - s) + 1 } else { 0 }
}

impl Set {
    fn new(ranges: &[(u32, u32)]) -> Self {
        let mut table = [OUT; 256];
        let mut wide = Vec::new();
        for &(s, e) in ranges {
            for c in s..=e.min(0x7F) {
                if let Some(t) = table.get_mut(c as usize) {
                    *t = ONE;
                }
            }
            if e >= 0x80 {
                wide.push((s.max(0x80), e));
            }
        }
        for lead in 0xC2u32..=0xF4 {
            let (lo, hi) = lead_range(lead);
            let held: u64 = wide.iter().map(|&(s, e)| scalars(s.max(lo), e.min(hi))).sum();
            if let Some(t) = table.get_mut(lead as usize) {
                *t = if held == 0 {
                    OUT
                } else if held == scalars(lo, hi) {
                    WHOLE
                } else {
                    ASK
                };
            }
        }
        Self { table, wide: wide.into() }
    }

    /// The bytes the character at `at` takes, when it is a member.
    #[inline]
    fn at(&self, hay: &[u8], at: usize) -> Option<usize> {
        let b = *hay.get(at)?;
        match *self.table.get(usize::from(b))? {
            ONE => Some(1),
            WHOLE => Some(width(b)),
            ASK => {
                let (c, n) = decode(hay, at)?;
                self.wide
                    .binary_search_by(|&(s, e)| {
                        if e < c {
                            Ordering::Less
                        } else if s > c {
                            Ordering::Greater
                        } else {
                            Ordering::Equal
                        }
                    })
                    .is_ok()
                    .then_some(n)
            }
            _ => None,
        }
    }

    /// The bytes its members may begin with.
    fn firsts(&self) -> [u64; 4] {
        let mut out = [0u64; 4];
        for (b, &t) in self.table.iter().enumerate() {
            if t != OUT
                && let Some(w) = out.get_mut(b >> 6)
            {
                *w |= 1 << (b & 63);
            }
        }
        out
    }
}

/// The bytes a character takes, by its first.
#[inline]
fn width(b: u8) -> usize {
    match b {
        0..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// The character at `at` (of valid UTF-8, at a character's start), and the bytes it takes.
#[inline]
fn decode(hay: &[u8], at: usize) -> Option<(u32, usize)> {
    let b = *hay.get(at)?;
    let n = width(b);
    let mut c = match n {
        1 => return Some((u32::from(b), 1)),
        2 => u32::from(b & 0x1F),
        3 => u32::from(b & 0x0F),
        _ => u32::from(b & 0x07),
    };
    for k in 1..n {
        c = (c << 6) | u32::from(*hay.get(at + k)? & 0x3F);
    }
    Some((c, n))
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// The start of the character before `at`, not before `floor`.
#[inline]
fn prev(hay: &[u8], at: usize, floor: usize) -> usize {
    let mut p = at.saturating_sub(1);
    while p > floor && hay.get(p).copied().is_some_and(is_continuation) {
        p -= 1;
    }
    p
}

/// Whether the character at `at` is a word's (`\w`, Unicode's); none past the end.
fn word_at(hay: &[u8], at: usize) -> bool {
    match hay.get(at) {
        None => false,
        Some(&b) if b < 0x80 => regex_syntax::is_word_byte(b),
        Some(_) => decode(hay, at).and_then(|(c, _)| char::from_u32(c)).is_some_and(|c| regex_syntax::try_is_word_character(c).unwrap_or(false)),
    }
}

/// Whether the character before `at` is a word's.
fn word_before(hay: &[u8], at: usize) -> bool {
    at > 0 && word_at(hay, prev(hay, at, 0))
}

/// Whether a look-around holds at `at`, as `regex` reads it (of valid UTF-8, at a character's
/// start: a Unicode boundary's negation then asks nothing more).
fn holds(look: Look, hay: &[u8], at: usize) -> bool {
    let len = hay.len();
    let byte = |i: usize| hay.get(i).copied();
    let ascii_before = || at > 0 && byte(at - 1).is_some_and(regex_syntax::is_word_byte);
    let ascii_after = || byte(at).is_some_and(regex_syntax::is_word_byte);
    match look {
        Look::Start => at == 0,
        Look::End => at == len,
        Look::StartLF => at == 0 || byte(at - 1) == Some(b'\n'),
        Look::EndLF => at == len || byte(at) == Some(b'\n'),
        Look::StartCRLF => at == 0 || byte(at - 1) == Some(b'\n') || (byte(at - 1) == Some(b'\r') && byte(at) != Some(b'\n')),
        Look::EndCRLF => at == len || byte(at) == Some(b'\r') || (byte(at) == Some(b'\n') && (at == 0 || byte(at - 1) != Some(b'\r'))),
        Look::WordAscii => ascii_before() != ascii_after(),
        Look::WordAsciiNegate => ascii_before() == ascii_after(),
        Look::WordUnicode => word_before(hay, at) != word_at(hay, at),
        Look::WordUnicodeNegate => word_before(hay, at) == word_at(hay, at),
        Look::WordStartAscii => !ascii_before() && ascii_after(),
        Look::WordEndAscii => ascii_before() && !ascii_after(),
        Look::WordStartUnicode => !word_before(hay, at) && word_at(hay, at),
        Look::WordEndUnicode => word_before(hay, at) && !word_at(hay, at),
        Look::WordStartHalfAscii => !ascii_before(),
        Look::WordEndHalfAscii => !ascii_after(),
        Look::WordStartHalfUnicode => !word_before(hay, at),
        Look::WordEndHalfUnicode => !word_at(hay, at),
    }
}

/// Where an instruction may begin: the bytes it may first take, and whether it may match with
/// none taken at the end, or anywhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Admit {
    bytes: [u64; 4],
    end: bool,
    any: bool,
}

impl Admit {
    fn of(bytes: [u64; 4]) -> Self {
        Self { bytes, ..Self::default() }
    }

    #[inline]
    fn has(&self, b: u8) -> bool {
        self.bytes.get(usize::from(b >> 6)).is_some_and(|w| (w >> (b & 63)) & 1 == 1)
    }

    /// Whether it may begin at `at`.
    #[inline]
    fn at(&self, hay: &[u8], at: usize) -> bool {
        self.any
            || match hay.get(at) {
                Some(&b) => self.has(b),
                None => self.end,
            }
    }

    fn with(mut self, o: &Self) -> Self {
        for (a, b) in self.bytes.iter_mut().zip(o.bytes) {
            *a |= b;
        }
        self.end |= o.end;
        self.any |= o.any;
        self
    }
}

/// What searches did, counted a thread at a time for the tests' census of a rule's work (grok's
/// tests): each instruction run, by its kind; each way back kept, by its kind; the ways back taken;
/// the bytes runs took. Nothing is counted in a build that is not a test's.
#[cfg(test)]
pub(crate) mod census {
    use std::cell::Cell;

    pub(crate) const NAMES: [&str; 14] = ["literal", "one", "run", "split", "jump", "save", "look", "match", "go kept", "restore kept", "give kept", "take kept", "ways back taken", "bytes runs took"];

    thread_local! {
        static COUNTS: Cell<[u64; 14]> = const { Cell::new([0; 14]) };
    }

    pub(crate) fn add(i: usize, n: u64) {
        COUNTS.with(|c| {
            let mut a = c.get();
            if let Some(x) = a.get_mut(i) {
                *x += n;
            }
            c.set(a);
        });
    }

    /// The counts since they were last taken.
    pub(crate) fn take() -> [u64; 14] {
        COUNTS.with(|c| c.replace([0; 14]))
    }
}

/// A census count ([`census`]): nothing outside a test's build.
#[inline(always)]
fn count(_what: usize, _n: u64) {
    #[cfg(test)]
    census::add(_what, _n);
}

const GO_KEPT: usize = 8;
const RESTORE_KEPT: usize = 9;
const GIVE_KEPT: usize = 10;
const TAKE_KEPT: usize = 11;
const TAKEN: usize = 12;
const RUN_BYTES: usize = 13;

/// Where nothing may begin (one past the last instruction).
const NOWHERE: Admit = Admit { bytes: [0; 4], end: false, any: false };

/// Whether `hay` holds `lit` at `at`, a byte at a time: a rule's literals are a few bytes (a space,
/// a quote and a space, a bracket), fewer than a call to `memcmp` costs (§12.17's profile).
#[inline]
fn holds_lit(hay: &[u8], at: usize, lit: &[u8]) -> bool {
    hay.get(at..).is_some_and(|h| h.len() >= lit.len() && h.iter().zip(lit).all(|(a, b)| a == b))
}

#[derive(Debug, Clone, Copy)]
enum Inst {
    /// These bytes: `lits[at..at + len]`.
    Lit {
        at: u32,
        len: u32,
    },
    /// A character of a set.
    One(u32),
    /// A set's characters, `min` to `max` of them ([`NONE`]: no end), as many as may be first
    /// (greedy) or as few. `keep`: what it took is never given back (none of what may follow could
    /// begin with what it holds, or it takes a count).
    Run {
        set: u32,
        min: u32,
        max: u32,
        greedy: bool,
        keep: bool,
    },
    /// `a` first, then `b`.
    Split {
        a: u32,
        b: u32,
    },
    Jmp(u32),
    Save(u32),
    Look(Look),
    Match,
}

/// A way back: where a search goes on when the way it took fails.
#[derive(Debug, Clone, Copy)]
enum Frame {
    Go {
        pc: u32,
        at: u32,
    },
    /// A slot as it was before a way set it.
    Restore {
        slot: u32,
        was: u32,
    },
    /// A greedy run's: what follows tried where it ends shorter, `hi` down to `lo`.
    Give {
        pc: u32,
        lo: u32,
        hi: u32,
    },
    /// A lazy run's: one more taken from `at`, `n` taken so far.
    Take {
        pc: u32,
        at: u32,
        n: u32,
    },
}

/// What a search found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Found {
    Yes,
    No,
    /// Past its bound: `regex` is asked.
    Unsure,
}

/// What a thread keeps from search to search: its ways back, the groups' slots, the table.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    stack: Vec<Frame>,
    slots: Vec<u32>,
    memo: Vec<u64>,
}

impl Scratch {
    /// Group `i`'s place in the last value found.
    pub(crate) fn group(&self, i: usize) -> Option<(usize, usize)> {
        let s = *self.slots.get(i * 2)?;
        let e = *self.slots.get(i * 2 + 1)?;
        (s != NONE && e != NONE).then_some((s as usize, e as usize))
    }
}

/// The table of where a search has been: off until its first stretch of work is spent.
struct Seen<'a> {
    bits: &'a mut Vec<u64>,
    on: bool,
    stride: usize,
}

impl Seen<'_> {
    /// Whether `(id, at)` is new to the search (and now seen): always, while the table is off.
    #[inline]
    fn first(&mut self, id: usize, at: usize) -> bool {
        if !self.on {
            return true;
        }
        let i = id * self.stride + at;
        match self.bits.get_mut(i >> 6) {
            Some(w) => {
                let bit = 1u64 << (i & 63);
                let new = *w & bit == 0;
                *w |= bit;
                new
            }
            None => true,
        }
    }
}

/// A rule's program.
#[derive(Debug)]
pub(crate) struct Program {
    insts: Vec<Inst>,
    lits: Vec<u8>,
    sets: Vec<Set>,
    /// Where each instruction may begin (one past the last: nowhere).
    admit: Vec<Admit>,
    /// Each run without end, its loop's id in the table (the instructions' own ids come first).
    loops: Vec<u32>,
    ids: usize,
    slots: usize,
    /// What a value it matches may end with (none: anything), and its fewest and most bytes.
    last: Option<Admit>,
    /// Bytes every value it matches holds: the rarest few, each asked for before a search.
    needs: Vec<u8>,
    min_len: usize,
    max_len: Option<usize>,
}

/// Why a program is not made: `regex` reads the rule.
#[derive(Debug)]
struct Refused;

struct Compiler<'a> {
    insts: Vec<Inst>,
    lits: Vec<u8>,
    sets: Vec<Set>,
    set_ids: HashMap<Vec<(u32, u32)>, u32>,
    /// Whether a group is read (one not read is matched, its place not kept).
    read: &'a dyn Fn(u32) -> bool,
    slots: usize,
}

/// A class's ranges, as scalar values (one of bytes holding no more than ASCII, as such).
fn ranges_of(c: &Class) -> Option<Vec<(u32, u32)>> {
    match c {
        Class::Unicode(u) => Some(u.ranges().iter().map(|r| (u32::from(r.start()), u32::from(r.end()))).collect()),
        Class::Bytes(b) => b.ranges().iter().map(|r| (r.end() < 0x80).then_some((u32::from(r.start()), u32::from(r.end())))).collect(),
    }
}

fn split(greedy: bool, body: u32, exit: u32) -> Inst {
    if greedy { Inst::Split { a: body, b: exit } } else { Inst::Split { a: exit, b: body } }
}

impl Compiler<'_> {
    fn push(&mut self, i: Inst) -> Result<u32, Refused> {
        if self.insts.len() >= INSTS {
            return Err(Refused);
        }
        self.insts.push(i);
        Ok(self.here() - 1)
    }

    fn here(&self) -> u32 {
        self.insts.len() as u32
    }

    fn patch(&mut self, at: u32, i: Inst) {
        if let Some(x) = self.insts.get_mut(at as usize) {
            *x = i;
        }
    }

    fn set(&mut self, ranges: Vec<(u32, u32)>) -> u32 {
        if let Some(&id) = self.set_ids.get(&ranges) {
            return id;
        }
        let id = self.sets.len() as u32;
        self.sets.push(Set::new(&ranges));
        self.set_ids.insert(ranges, id);
        id
    }

    /// The ranges of what `h` is when it is one character, whatever it is.
    fn one_char(&self, h: &Hir) -> Option<Vec<(u32, u32)>> {
        match h.kind() {
            HirKind::Class(c) => ranges_of(c),
            HirKind::Literal(l) => {
                let mut cs = std::str::from_utf8(&l.0).ok()?.chars();
                let c = u32::from(cs.next()?);
                cs.next().is_none().then(|| vec![(c, c)])
            }
            HirKind::Capture(cap) if !(self.read)(cap.index) => self.one_char(&cap.sub),
            _ => None,
        }
    }

    fn expr(&mut self, h: &Hir) -> Result<(), Refused> {
        match h.kind() {
            HirKind::Empty => {}
            HirKind::Literal(l) => {
                if !l.0.is_empty() {
                    let at = self.lits.len() as u32;
                    self.lits.extend_from_slice(&l.0);
                    self.push(Inst::Lit { at, len: l.0.len() as u32 })?;
                }
            }
            HirKind::Class(c) => {
                let s = self.set(ranges_of(c).ok_or(Refused)?);
                self.push(Inst::One(s))?;
            }
            HirKind::Look(l) => {
                self.push(Inst::Look(*l))?;
            }
            HirKind::Capture(cap) => {
                if (self.read)(cap.index) {
                    let slot = cap.index.checked_mul(2).ok_or(Refused)?;
                    self.slots = self.slots.max(slot as usize + 2);
                    self.push(Inst::Save(slot))?;
                    self.expr(&cap.sub)?;
                    self.push(Inst::Save(slot + 1))?;
                } else {
                    self.expr(&cap.sub)?;
                }
            }
            HirKind::Concat(v) => {
                for x in v {
                    self.expr(x)?;
                }
            }
            HirKind::Alternation(v) => {
                let mut jumps = Vec::new();
                for (i, x) in v.iter().enumerate() {
                    if i + 1 == v.len() {
                        self.expr(x)?;
                        break;
                    }
                    let s = self.push(Inst::Split { a: 0, b: 0 })?;
                    self.expr(x)?;
                    jumps.push(self.push(Inst::Jmp(0))?);
                    self.patch(s, Inst::Split { a: s + 1, b: self.here() });
                }
                let end = self.here();
                for j in jumps {
                    self.patch(j, Inst::Jmp(end));
                }
            }
            HirKind::Repetition(r) => self.repetition(r)?,
        }
        Ok(())
    }

    fn repetition(&mut self, r: &Repetition) -> Result<(), Refused> {
        if r.min as usize > INSTS || r.max.is_some_and(|m| m as usize > INSTS) {
            return Err(Refused);
        }
        if let Some(ranges) = self.one_char(&r.sub) {
            let set = self.set(ranges);
            self.push(Inst::Run { set, min: r.min, max: r.max.unwrap_or(NONE), greedy: r.greedy, keep: false })?;
            return Ok(());
        }
        // A loop that may match nothing each round: its empty rounds are `regex`'s to tell.
        if r.max.is_none() && r.sub.properties().minimum_len() == Some(0) {
            return Err(Refused);
        }
        for _ in 0..r.min {
            self.expr(&r.sub)?;
        }
        match r.max {
            None => {
                let l = self.push(Inst::Split { a: 0, b: 0 })?;
                self.expr(&r.sub)?;
                self.push(Inst::Jmp(l))?;
                self.patch(l, split(r.greedy, l + 1, self.here()));
            }
            Some(max) => {
                let mut splits = Vec::new();
                for _ in r.min..max {
                    splits.push(self.push(Inst::Split { a: 0, b: 0 })?);
                    self.expr(&r.sub)?;
                }
                let exit = self.here();
                for s in splits {
                    self.patch(s, split(r.greedy, s + 1, exit));
                }
            }
        }
        Ok(())
    }
}

/// The bytes every match of `h` holds.
fn required(h: &Hir) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut put = |b: u8| {
        if let Some(w) = out.get_mut(usize::from(b >> 6)) {
            *w |= 1 << (b & 63);
        }
    };
    match h.kind() {
        HirKind::Literal(l) => l.0.iter().for_each(|&b| put(b)),
        HirKind::Class(c) => {
            if let Some([(lo, hi)]) = ranges_of(c).as_deref()
                && lo == hi
                && *lo < 0x80
            {
                put(*lo as u8);
            }
        }
        HirKind::Repetition(r) if r.min > 0 => return required(&r.sub),
        HirKind::Capture(c) => return required(&c.sub),
        HirKind::Concat(v) => {
            for x in v {
                for (a, b) in out.iter_mut().zip(required(x)) {
                    *a |= b;
                }
            }
        }
        HirKind::Alternation(v) => {
            let mut all = [u64::MAX; 4];
            for x in v {
                for (a, b) in all.iter_mut().zip(required(x)) {
                    *a &= b;
                }
            }
            if !v.is_empty() {
                out = all;
            }
        }
        _ => {}
    }
    out
}

/// How rare a byte is in log lines, roughly (lower, rarer): what a value is asked for first.
fn rarity(b: u8) -> u8 {
    match b {
        b' ' => 5,
        b'a'..=b'z' | b'A'..=b'Z' => 4,
        b'0'..=b'9' => 3,
        b'.' | b'-' | b'/' | b':' | b',' | b'_' => 2,
        0x80.. => 3,
        _ => 1,
    }
}

/// What a match of `h` may end with: its last bytes, and whether it may be empty.
fn lasts(h: &Hir) -> ([u64; 4], bool) {
    let bit = |b: u8| {
        let mut s = [0u64; 4];
        if let Some(w) = s.get_mut(usize::from(b >> 6)) {
            *w |= 1 << (b & 63);
        }
        s
    };
    let or = |a: &mut [u64; 4], b: [u64; 4]| a.iter_mut().zip(b).for_each(|(x, y)| *x |= y);
    match h.kind() {
        HirKind::Empty | HirKind::Look(_) => ([0; 4], true),
        HirKind::Literal(l) => l.0.last().map_or(([0; 4], true), |&b| (bit(b), false)),
        HirKind::Class(c) => {
            let mut s = [0u64; 4];
            for (lo, hi) in ranges_of(c).unwrap_or_default() {
                for b in lo..=hi.min(0x7F) {
                    or(&mut s, bit(b as u8));
                }
                if hi >= 0x80 {
                    // A wider character ends with a continuation byte.
                    for b in 0x80u8..=0xBF {
                        or(&mut s, bit(b));
                    }
                }
            }
            (s, false)
        }
        HirKind::Repetition(r) => {
            let (s, empty) = lasts(&r.sub);
            (s, empty || r.min == 0)
        }
        HirKind::Capture(c) => lasts(&c.sub),
        HirKind::Concat(v) => {
            let mut acc = [0u64; 4];
            for x in v.iter().rev() {
                let (s, empty) = lasts(x);
                or(&mut acc, s);
                if !empty {
                    return (acc, false);
                }
            }
            (acc, true)
        }
        HirKind::Alternation(v) => {
            let mut acc = [0u64; 4];
            let mut empty = false;
            for x in v {
                let (s, e) = lasts(x);
                or(&mut acc, s);
                empty |= e;
            }
            (acc, empty)
        }
    }
}

impl Program {
    /// `hir` compiled (a value matched whole is anchored by its `^` and `$`, which it holds), the
    /// groups `read` says kept; none when it is `regex`'s to read.
    pub(crate) fn compile(hir: &Hir, read: &dyn Fn(u32) -> bool) -> Option<Self> {
        let mut c = Compiler { insts: Vec::new(), lits: Vec::new(), sets: Vec::new(), set_ids: HashMap::new(), read, slots: 0 };
        c.expr(hir).ok()?;
        c.push(Inst::Match).ok()?;
        let Compiler { mut insts, lits, sets, slots, .. } = c;
        let firsts: Vec<[u64; 4]> = sets.iter().map(Set::firsts).collect();
        let first_of = |s: u32| firsts.get(s as usize).copied().unwrap_or_default();
        // Where each instruction may begin, to a fixed point (loops lead back).
        let mut admit = vec![Admit::default(); insts.len() + 1];
        loop {
            let mut changed = false;
            for pc in (0..insts.len()).rev() {
                let at = |t: usize| admit.get(t).copied().unwrap_or_default();
                let new = match insts.get(pc).copied().unwrap_or(Inst::Match) {
                    Inst::Lit { at: l, .. } => {
                        let mut a = Admit::default();
                        if let Some(&b) = lits.get(l as usize)
                            && let Some(w) = a.bytes.get_mut(usize::from(b >> 6))
                        {
                            *w |= 1 << (b & 63);
                        }
                        a
                    }
                    Inst::One(s) => Admit::of(first_of(s)),
                    Inst::Run { set, min, .. } => {
                        let a = Admit::of(first_of(set));
                        if min == 0 { a.with(&at(pc + 1)) } else { a }
                    }
                    Inst::Split { a, b } => at(a as usize).with(&at(b as usize)),
                    Inst::Jmp(t) => at(t as usize),
                    Inst::Save(_) => at(pc + 1),
                    Inst::Look(Look::End) => Admit { end: true, ..Admit::default() },
                    Inst::Look(_) => at(pc + 1),
                    Inst::Match => Admit { any: true, ..Admit::default() },
                };
                if let Some(old) = admit.get_mut(pc) {
                    let merged = old.with(&new);
                    if merged != *old {
                        *old = merged;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        // Runs that give nothing back, and each run without end its loop's id.
        let mut ids = insts.len();
        let mut loops = vec![NONE; insts.len()];
        for (pc, inst) in insts.iter_mut().enumerate() {
            if let Inst::Run { set, min, max, greedy, keep } = inst {
                let next = admit.get(pc + 1).copied().unwrap_or_default();
                let overlap = first_of(*set).iter().zip(next.bytes).any(|(a, b)| a & b != 0);
                *keep = *max == *min || (*greedy && !next.any && !overlap);
                if *max == NONE {
                    if let Some(l) = loops.get_mut(pc) {
                        *l = ids as u32;
                    }
                    ids += 1;
                }
            }
        }
        let props = hir.properties();
        let (last_bytes, empty) = lasts(hir);
        let need = required(hir);
        let mut needs: Vec<u8> = (0..=255u8).filter(|&b| need.get(usize::from(b >> 6)).is_some_and(|w| (w >> (b & 63)) & 1 == 1)).collect();
        needs.sort_by_key(|&b| rarity(b));
        needs.truncate(2);
        Some(Self {
            insts,
            lits,
            sets,
            admit,
            loops,
            ids,
            slots,
            last: (!empty).then_some(Admit::of(last_bytes)),
            needs,
            min_len: props.minimum_len().unwrap_or(usize::MAX),
            max_len: props.maximum_len(),
        })
    }

    /// Where instruction `pc` may begin (read in place: a split asks two of them a step).
    #[inline]
    fn admit(&self, pc: usize) -> &Admit {
        self.admit.get(pc).unwrap_or(&NOWHERE)
    }

    /// Whether `value` matches from its start (to its end, as the program's `$` says), its groups
    /// in `s` when it does.
    pub(crate) fn search(&self, value: &str, s: &mut Scratch) -> Found {
        let found = self.find(value, s);
        // Room a long value's search took is not kept for every value after it.
        if s.memo.capacity() > KEPT_ROOM / 8 {
            s.memo = Vec::new();
        }
        if s.stack.capacity() > KEPT_ROOM / std::mem::size_of::<Frame>() {
            s.stack = Vec::new();
        }
        found
    }

    fn find(&self, value: &str, s: &mut Scratch) -> Found {
        let hay = value.as_bytes();
        let len = hay.len();
        if len > LONGEST {
            return Found::Unsure;
        }
        if len < self.min_len || self.max_len.is_some_and(|m| len > m) {
            return Found::No;
        }
        if let (Some(last), Some(&b)) = (&self.last, hay.last())
            && !last.has(b)
        {
            return Found::No;
        }
        if self.needs.iter().any(|&b| memchr::memchr(b, hay).is_none()) {
            return Found::No;
        }
        let Scratch { stack, slots, memo } = s;
        stack.clear();
        slots.clear();
        slots.resize(self.slots, NONE);
        let stride = len + 1;
        let mut seen = Seen { bits: memo, on: false, stride };
        // Work a search does before it keeps its table (a value read straight through does a
        // fraction of it), and past which `regex` takes it.
        let budget = stride.saturating_mul(4).saturating_add(256);
        let bound = budget.saturating_add(self.ids.saturating_mul(stride).saturating_mul(64));
        let mut steps = 0usize;
        let mut next = Some((0usize, 0usize));
        loop {
            let (pc, at) = match next {
                Some(x) => x,
                None => match self.back(hay, stack, slots, &mut seen, &mut steps) {
                    Some(x) => x,
                    None => return Found::No,
                },
            };
            steps += 1;
            if steps > budget {
                if !seen.on {
                    let bits = self.ids.saturating_mul(stride);
                    if bits > MEMO_BITS {
                        return Found::Unsure;
                    }
                    seen.bits.clear();
                    seen.bits.resize(bits.div_ceil(64), 0);
                    seen.on = true;
                } else if steps > bound {
                    return Found::Unsure;
                }
            }
            if !seen.first(pc, at) {
                next = None;
                continue;
            }
            let Some(&inst) = self.insts.get(pc) else { return Found::No };
            count(
                match inst {
                    Inst::Lit { .. } => 0,
                    Inst::One(_) => 1,
                    Inst::Run { .. } => 2,
                    Inst::Split { .. } => 3,
                    Inst::Jmp(_) => 4,
                    Inst::Save(_) => 5,
                    Inst::Look(_) => 6,
                    Inst::Match => 7,
                },
                1,
            );
            next = match inst {
                Inst::Lit { at: l, len: n } => {
                    let lit = self.lits.get(l as usize..(l + n) as usize).unwrap_or_default();
                    holds_lit(hay, at, lit).then_some((pc + 1, at + lit.len()))
                }
                Inst::One(set) => self.sets.get(set as usize).and_then(|s| s.at(hay, at)).map(|n| (pc + 1, at + n)),
                Inst::Run { set, min, max, greedy, keep } => {
                    let Some(set) = self.sets.get(set as usize) else { return Found::No };
                    self.run(pc, set, (min, max, greedy, keep), hay, at, stack, &mut seen, &mut steps)
                }
                Inst::Split { a, b } => {
                    let (a, b) = (a as usize, b as usize);
                    match (self.admit(a).at(hay, at), self.admit(b).at(hay, at)) {
                        (true, true) => {
                            stack.push(Frame::Go { pc: b as u32, at: at as u32 });
                            count(GO_KEPT, 1);
                            Some((a, at))
                        }
                        (true, false) => Some((a, at)),
                        (false, true) => Some((b, at)),
                        (false, false) => None,
                    }
                }
                Inst::Jmp(t) => Some((t as usize, at)),
                Inst::Save(slot) => {
                    if let Some(x) = slots.get_mut(slot as usize) {
                        // Nothing to go back to: nothing to restore.
                        if !stack.is_empty() {
                            stack.push(Frame::Restore { slot, was: *x });
                            count(RESTORE_KEPT, 1);
                        }
                        *x = at as u32;
                    }
                    Some((pc + 1, at))
                }
                Inst::Look(l) => holds(l, hay, at).then_some((pc + 1, at)),
                Inst::Match => return Found::Yes,
            };
        }
    }

    /// A run at `at`: where the search goes on (`pc + 1` where the run ends), its ways back kept.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn run(
        &self,
        pc: usize,
        set: &Set,
        (min, max, greedy, keep): (u32, u32, bool, bool),
        hay: &[u8],
        at: usize,
        stack: &mut Vec<Frame>,
        seen: &mut Seen<'_>,
        steps: &mut usize,
    ) -> Option<(usize, usize)> {
        // The fewest it takes.
        let mut q = at;
        for _ in 0..min {
            q += set.at(hay, q)?;
        }
        *steps += q - at;
        let lo = q;
        let looped = self.loops.get(pc).copied().unwrap_or(NONE);
        if !greedy {
            return self.lazy(pc, set, max, looped, hay, q, min, stack, seen, steps);
        }
        // As many as it may: past a loop's place seen before, all of what follows from there was
        // tried (and failed).
        let mut n = min;
        let mut stopped = false;
        if looped != NONE && seen.on {
            while n != max {
                if !seen.first(looped as usize, q) {
                    stopped = true;
                    break;
                }
                let Some(w) = set.at(hay, q) else { break };
                q += w;
                n += 1;
            }
        } else if max == NONE {
            // No count to keep: as far as its characters go.
            while let Some(&b) = hay.get(q) {
                match set.table.get(usize::from(b)).copied().unwrap_or(OUT) {
                    ONE => q += 1,
                    WHOLE => q += width(b),
                    ASK => match set.at(hay, q) {
                        Some(w) => q += w,
                        None => break,
                    },
                    _ => break,
                }
            }
        } else {
            while n != max {
                let Some(w) = set.at(hay, q) else { break };
                q += w;
                n += 1;
            }
        }
        *steps += q - lo;
        count(RUN_BYTES, (q - at) as u64);
        if keep {
            return (!stopped).then_some((pc + 1, q));
        }
        let top = if stopped {
            if q == lo {
                return None;
            }
            prev(hay, q, lo)
        } else {
            q
        };
        let t = self.highest(pc + 1, hay, lo, top, steps)?;
        if t > lo {
            stack.push(Frame::Give { pc: pc as u32, lo: lo as u32, hi: prev(hay, t, lo) as u32 });
            count(GIVE_KEPT, 1);
        }
        Some((pc + 1, t))
    }

    /// Where, from `hi` down to `lo`, what follows a run may begin first.
    fn highest(&self, follow: usize, hay: &[u8], lo: usize, hi: usize, steps: &mut usize) -> Option<usize> {
        let next = self.admit(follow);
        let mut t = hi;
        loop {
            if next.at(hay, t) {
                return Some(t);
            }
            if t <= lo {
                return None;
            }
            t = prev(hay, t, lo);
            *steps += 1;
        }
    }

    /// A lazy run with `n` taken to `q`: the first place from `q` on where what follows may begin
    /// (taking one more each place it may not), a way back to take more from there.
    #[allow(clippy::too_many_arguments)]
    fn lazy(&self, pc: usize, set: &Set, max: u32, looped: u32, hay: &[u8], mut q: usize, mut n: u32, stack: &mut Vec<Frame>, seen: &mut Seen<'_>, steps: &mut usize) -> Option<(usize, usize)> {
        let next = self.admit(pc + 1);
        loop {
            if looped != NONE && !seen.first(looped as usize, q) {
                return None;
            }
            if next.at(hay, q) {
                stack.push(Frame::Take { pc: pc as u32, at: q as u32, n });
                count(TAKE_KEPT, 1);
                return Some((pc + 1, q));
            }
            if n == max {
                return None;
            }
            q += set.at(hay, q)?;
            n += 1;
            *steps += 1;
        }
    }

    /// The next way back from the top of `stack`: where the search goes on, none when there is no
    /// way left.
    fn back(&self, hay: &[u8], stack: &mut Vec<Frame>, slots: &mut [u32], seen: &mut Seen<'_>, steps: &mut usize) -> Option<(usize, usize)> {
        while let Some(f) = stack.pop() {
            *steps += 1;
            count(TAKEN, 1);
            match f {
                Frame::Go { pc, at } => return Some((pc as usize, at as usize)),
                Frame::Restore { slot, was } => {
                    if let Some(x) = slots.get_mut(slot as usize) {
                        *x = was;
                    }
                }
                Frame::Give { pc, lo, hi } => {
                    let (pc, lo) = (pc as usize, lo as usize);
                    if let Some(t) = self.highest(pc + 1, hay, lo, hi as usize, steps) {
                        if t > lo {
                            stack.push(Frame::Give { pc: pc as u32, lo: lo as u32, hi: prev(hay, t, lo) as u32 });
                            count(GIVE_KEPT, 1);
                        }
                        return Some((pc + 1, t));
                    }
                }
                Frame::Take { pc, at, n } => {
                    let (pc, at) = (pc as usize, at as usize);
                    let Some(Inst::Run { set, max, .. }) = self.insts.get(pc).copied() else { continue };
                    let Some(set) = self.sets.get(set as usize) else { continue };
                    if n == max {
                        continue;
                    }
                    let Some(w) = set.at(hay, at) else { continue };
                    let looped = self.loops.get(pc).copied().unwrap_or(NONE);
                    if let Some(x) = self.lazy(pc, set, max, looped, hay, at + w, n + 1, stack, seen, steps) {
                        return Some(x);
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A pattern as a rule's is (`^(?:…)$`), compiled both ways: its program (every group read)
    /// and `regex`'s.
    fn both(p: &str) -> Option<(Program, regex::Regex)> {
        let whole = format!("^(?:{p})$");
        let re = regex::Regex::new(&whole).ok()?;
        let hir = regex_syntax::ParserBuilder::new().build().parse(&whole).ok()?;
        Some((Program::compile(&hir, &|_| true)?, re))
    }

    /// Every group as the program and as `regex` find them, and whether either matched.
    fn compare(prog: &Program, re: &regex::Regex, hay: &str, s: &mut Scratch) -> Option<bool> {
        let mut locs = re.capture_locations();
        let theirs = re.captures_read(&mut locs, hay).is_some();
        let ours = match prog.search(hay, s) {
            Found::Yes => true,
            Found::No => false,
            Found::Unsure => return None,
        };
        assert_eq!(ours, theirs, "{re} on {hay:?}");
        if ours {
            for i in 1..locs.len() {
                assert_eq!(s.group(i), locs.get(i), "{re} on {hay:?}: group {i}");
            }
        }
        Some(ours)
    }

    /// A random pattern's parts: an atom (its text, and texts it matches), or a group of
    /// alternatives (non-capturing, capturing, named), each perhaps repeated.
    enum Node {
        Atom(usize),
        Group(usize, Vec<Vec<(Node, Option<usize>)>>),
    }

    /// Atoms of what rules are made of: literal text, classes Unicode's and ASCII's, word
    /// boundaries and anchors; each with texts it matches (none for what takes none).
    const ATOMS: &[(&str, &[&str])] = &[
        ("a", &["a"]),
        ("b", &["b"]),
        ("c", &["c"]),
        (" ", &[" "]),
        ("-", &["-"]),
        (r"\.", &["."]),
        (":", &[":"]),
        ("/", &["/"]),
        ("\"", &["\""]),
        ("é", &["é"]),
        ("日", &["日"]),
        ("1", &["1"]),
        (r"\d", &["1", "7", "\u{0663}"]),
        (r"\w", &["a", "é", "日", "_", "1", "α"]),
        (r"\s", &[" ", "\u{2003}", "\n"]),
        (r"\S", &["a", "é", "-", "😀", "\""]),
        (r"\D", &["a", " ", "日"]),
        (r"\W", &[" ", "-", "😀"]),
        (".", &["a", "é", " ", "\""]),
        ("[a-c]", &["a", "b", "c"]),
        ("[^\"]", &["a", " ", "é"]),
        ("[^ ]", &["a", "\"", "日"]),
        ("[[:alpha:]]", &["a", "K"]),
        (r"\p{Greek}", &["α"]),
        ("[0-9A-Fa-f]", &["1", "a", "F"]),
        ("(?i:k)", &["k", "K", "\u{212A}"]),
        (r"(?-u:\w)", &["a", "_", "7"]),
        ("(?s:.)", &["\n", "a"]),
        ("ab", &["ab"]),
        ("a b", &["a b"]),
        (r"\b", &[]),
        (r"\B", &[]),
        ("^", &[]),
        ("$", &[]),
        ("(?m:^)", &[]),
        ("(?m:$)", &[]),
        (r"\b{start}", &[]),
        (r"\b{end}", &[]),
        (r"\b{start-half}", &[]),
        (r"\b{end-half}", &[]),
        (r"(?-u:\b)", &[]),
    ];
    /// Repetitions, and how many times a text of one repeats.
    const REPS: &[(&str, usize, usize)] =
        &[("?", 0, 1), ("*", 0, 3), ("+", 1, 3), ("{2}", 2, 2), ("{1,}", 1, 3), ("{0,3}", 0, 3), ("{1,2}", 1, 2), ("??", 0, 1), ("*?", 0, 3), ("+?", 1, 3), ("{0,2}?", 0, 2), ("{2,}?", 2, 3)];

    fn seq_of(rng: &mut kiln_obs::testkit::Rng, depth: usize) -> Vec<(Node, Option<usize>)> {
        let mut pick = |n: usize| rng.below(n as u64) as usize;
        let n = 1 + pick(3);
        let mut out = Vec::new();
        for _ in 0..n {
            let node = if depth < 3 && rng.below(4) == 0 {
                let kind = rng.below(3) as usize;
                let alts = (0..1 + rng.below(3)).map(|_| seq_of(rng, depth + 1)).collect();
                Node::Group(kind, alts)
            } else {
                Node::Atom(rng.below(ATOMS.len() as u64) as usize)
            };
            let rep = (rng.below(3) == 0).then(|| rng.below(REPS.len() as u64) as usize);
            out.push((node, rep));
        }
        out
    }

    fn render(seq: &[(Node, Option<usize>)], group: &mut usize, out: &mut String) {
        for (node, rep) in seq {
            match node {
                Node::Atom(i) => out.push_str(ATOMS[*i].0),
                Node::Group(kind, alts) => {
                    out.push_str(match kind {
                        0 => "(?:",
                        1 => "(",
                        _ => "",
                    });
                    if *kind == 2 {
                        *group += 1;
                        out.push_str(&format!("(?P<g{group}>"));
                    }
                    for (k, alt) in alts.iter().enumerate() {
                        if k > 0 {
                            out.push('|');
                        }
                        render(alt, group, out);
                    }
                    out.push(')');
                }
            }
            if let Some(r) = rep {
                out.push_str(REPS[*r].0);
            }
        }
    }

    /// A text the sequence would match but for its look-arounds.
    fn witness(seq: &[(Node, Option<usize>)], rng: &mut kiln_obs::testkit::Rng, out: &mut String) {
        for (node, rep) in seq {
            let times = rep.map_or(1, |r| REPS[r].1 + rng.below((REPS[r].2 - REPS[r].1 + 1) as u64) as usize);
            for _ in 0..times {
                match node {
                    Node::Atom(i) => {
                        let texts = ATOMS[*i].1;
                        if !texts.is_empty() {
                            out.push_str(texts[rng.below(texts.len() as u64) as usize]);
                        }
                    }
                    Node::Group(_, alts) => witness(&alts[rng.below(alts.len() as u64) as usize], rng, out),
                }
            }
        }
    }

    /// Random patterns of what rules are made of (literal text, classes Unicode's and ASCII's,
    /// runs greedy and lazy, counted repetitions, alternatives, optional and nested groups, word
    /// boundaries and anchors), on texts each would match, those changed a little, and random
    /// texts of the characters they name (Unicode's spaces, digits and letters among them): the
    /// program finds what `regex` finds, every group where it does.
    #[test]
    fn the_program_finds_what_regex_finds() {
        const ALPHABET: &[&str] = &["a", "b", "c", " ", "-", ".", ":", "/", "\"", "é", "日", "1", "7", "k", "K", "\u{212A}", "\u{2003}", "\u{0663}", "α", "_", "\n", "\r", "😀"];
        let mut rng = kiln_obs::testkit::Rng::new(13);
        let mut s = Scratch::default();
        let (mut patterns, mut refused, mut cases, mut matched, mut unsure) = (0, 0, 0, 0, 0);
        while patterns < 3_000 {
            let seq = seq_of(&mut rng, 0);
            let mut p = String::new();
            render(&seq, &mut 0, &mut p);
            let Ok(re) = regex::Regex::new(&format!("^(?:{p})$")) else { continue };
            patterns += 1;
            let Some((prog, _)) = both(&p) else {
                refused += 1;
                continue;
            };
            for k in 0..32 {
                let hay: String = match k % 4 {
                    // A text it would match, as it is, or a character in it changed, taken out or put in.
                    0..=2 => {
                        let mut w = String::new();
                        witness(&seq, &mut rng, &mut w);
                        if k % 4 > 0 && !w.is_empty() {
                            let mut chars: Vec<char> = w.chars().collect();
                            let i = rng.below(chars.len() as u64) as usize;
                            let c = ALPHABET[rng.below(ALPHABET.len() as u64) as usize].chars().next().unwrap();
                            match rng.below(3) {
                                0 => chars[i] = c,
                                1 => {
                                    chars.remove(i);
                                }
                                _ => chars.insert(i, c),
                            }
                            w = chars.into_iter().collect();
                        }
                        w
                    }
                    _ => (0..rng.below(12)).map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize]).collect(),
                };
                cases += 1;
                match compare(&prog, &re, &hay, &mut s) {
                    Some(true) => matched += 1,
                    Some(false) => {}
                    None => unsure += 1,
                }
            }
        }
        // Enough of them matching, and few left to `regex`, for the comparison to say something.
        assert!(matched > cases / 4 && refused < patterns / 10 && unsure == 0, "{matched} of {cases} matched; {refused} of {patterns} refused; {unsure} unsure");
    }

    /// Values made of a rule's own pieces match: the program and `regex` read Datadog's access
    /// rule's groups alike on lines whole, cut short and changed.
    #[test]
    fn an_access_rule_reads_alike() {
        let ip = r"(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)";
        let ip6 = r"(?:[0-9A-Fa-f]{0,4}:){2,7}(?:[0-9A-Fa-f]{0,4}|(?:\d{1,3}\.){3}\d{1,3})";
        let host = r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*\.?";
        let rule = format!(
            r#"(?P<ip>(?:{ip}|{ip6}|{host})) (?P<ident>\S+) (?P<auth>\S+) \[(?P<date>\d{{1,2}}/[A-Za-z]{{3}}/\d{{4}}:\d{{1,2}}:\d{{1,2}}:\d{{1,2}} (?:Z|[+-]\d{{4}}|[+-]\d{{2}}))\] "(?:(?P<method>\b\w+\b) |)(?P<url>\S+)(?: HTTP\/(?P<version>\d+\.\d+)|)" (?P<status>[+-]?\d+) (?:(?P<bytes>[+-]?\d+)|-)(?: (?:(?P<duration>[+-]?(?:\d+(?:\.\d*)?|\.\d+)) )?"(?P<referer>\S+)" "(?P<agent>[^\"]*)"(?: "(?P<xff>[^\"]*)")?.*)?"#
        );
        let (prog, re) = both(&rule).unwrap();
        let lines = [
            r#"172.17.0.1 - - [06/Jan/2017:16:16:37 +0000] "GET /datadoghq/company?test=var1%20Pl HTTP/1.1" 200 612 "http://www.perdu.com/" "Mozilla/5.0 (X11; Linux x86_64) Chrome/55.0.2883.87 Safari/537.36" "-""#,
            r#"10.0.0.2 - bob [06/Jan/2017:16:16:37 -0500] "POST /api HTTP/2.0" 404 -"#,
            r#"2001:db8::1 - - [18/Oct/2026:12:00:00 +0000] "GET / HTTP/1.1" 200 5 "-" "curl/8.5.0""#,
            r#"web-1.example.com - - [18/Oct/2026:12:00:00 +0000] "-" 400 0 "-" "-""#,
            r#"10.1.2.3 - - [18/Oct/2026:12:00:00 +0000] "GET /a"b HTTP/1.1" 200 5 "-" "x""#,
            r#"10.1.2.3 - - [18/Oct/2026:12:00:00 +0000] "GET /é?q=日本 HTTP/1.1" 200 5 0.004 "-" "Mozilla" "203.0.113.9, 10.0.0.1" trailing"#,
        ];
        let mut rng = kiln_obs::testkit::Rng::new(5);
        let mut s = Scratch::default();
        let mut matched = 0;
        for line in lines {
            assert_eq!(compare(&prog, &re, line, &mut s), Some(true), "{line}");
            // Cut short, a byte changed, and a piece doubled: matched or not, alike.
            for _ in 0..400 {
                let mut v = line.to_owned();
                let at = |rng: &mut kiln_obs::testkit::Rng, v: &str| {
                    let mut i = rng.below(v.len() as u64 + 1) as usize;
                    while !v.is_char_boundary(i) {
                        i -= 1;
                    }
                    i
                };
                match rng.below(3) {
                    0 => v.truncate(at(&mut rng, &v)),
                    1 => {
                        let i = at(&mut rng, &v);
                        let c = [" ", "\"", "-", "1", "x", "[", "é", ":"][rng.below(8) as usize];
                        v.insert_str(i, c);
                    }
                    _ => {
                        let (i, j) = (at(&mut rng, &v), at(&mut rng, &v));
                        let (i, j) = (i.min(j), i.max(j));
                        let piece = v[i..j].to_owned();
                        v.insert_str(j, &piece);
                    }
                }
                matched += usize::from(compare(&prog, &re, &v, &mut s).unwrap());
            }
        }
        assert!(matched > 200, "{matched}");
    }

    /// Work that would grow as a value's square (a run tried at every place, then again) is kept to
    /// the table's bound: a long value that fails fails at once.
    #[test]
    fn a_search_that_backtracks_is_bounded() {
        let mut s = Scratch::default();
        // (Each ends with what the value does, so it is searched, not refused by its last byte.)
        for (p, hay) in [(r"(?:a+)+b.", "a".repeat(20_000)), (r"(?:\S+ )*\S+x.", format!("{}ab", "ab ".repeat(5_000))), (r".*?.*?.*?z.", "y".repeat(5_000)), (r"(?:a|aa)+c.", "a".repeat(10_000))] {
            let (prog, re) = both(p).unwrap();
            let t = std::time::Instant::now();
            assert_eq!(prog.search(&hay, &mut s), Found::No, "{p}");
            assert!(t.elapsed() < std::time::Duration::from_secs(2), "{p}: {:?}", t.elapsed());
            assert!(!re.is_match(&hay));
        }
        // A loop that may match nothing is `regex`'s.
        assert!(both(r"(?:a*)*b").is_none());
    }

    /// A literal is held where its bytes are, whole: not past the value's end, not cut short.
    #[test]
    fn a_literal_is_held_where_its_bytes_are() {
        let hay = b"GET /x HTTP/1.1";
        assert!(holds_lit(hay, 0, b"GET ") && holds_lit(hay, 7, b"HTTP/") && holds_lit(hay, 15, b"") && holds_lit(hay, 14, b"1"));
        assert!(!holds_lit(hay, 7, b"HTTP/1.1x") && !holds_lit(hay, 14, b"1.") && !holds_lit(hay, 16, b"") && !holds_lit(hay, 1, b"GET"));
    }
}
