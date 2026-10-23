//! pprof's profile format (google/pprof `proto/profile.proto`, `perftools.profiles`), read in place
//! (`docs/observe.md` §8.11): what Datadog's tracers' profilers and Go's `runtime/pprof` write
//! (gzip'd: the caller inflates). Its strings are borrowed from the message; each sample's stack
//! is the functions of its locations, leaf first, a location's inlined functions expanded as its
//! lines say (the last of them the caller); every index and id checked, every size bounded.

use crate::otlp::pb::{Fields, PbError, Wire, varints};

/// Bounds on what one profile may hold (a profile past them is refused, not cut: its totals would
/// be wrong). A minute of a busy service's CPU profile holds some 10,000 samples of 50 frames.
pub const MAX_SAMPLES: usize = 1 << 20;
pub const MAX_LOCATIONS: usize = 1 << 20;
pub const MAX_FUNCTIONS: usize = 1 << 20;
pub const MAX_STRINGS: usize = 1 << 22;
pub const MAX_TYPES: usize = 32;
/// Frames a stack keeps, its leaf's (a deeper stack's root-most frames dropped and said: pprof's
/// own tools cut it so too).
pub const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PprofError {
    #[error(transparent)]
    Pb(#[from] PbError),
    #[error("a string index past the table ({0})")]
    String(u64),
    #[error("the string table does not begin with \"\"")]
    Table,
    #[error("a sample of {got} values where the profile has {want} types")]
    Values { got: usize, want: usize },
    #[error("a location id ({0}) the profile does not define")]
    Location(u64),
    #[error("a function id ({0}) the profile does not define")]
    Function(u64),
    #[error("an id of 0, or one defined twice ({0})")]
    Id(u64),
    #[error("more {0} than a profile may hold")]
    Bound(&'static str),
    #[error("a field of the wrong kind ({0})")]
    Kind(&'static str),
}

/// What a sample's value counts and in what unit (`cpu`/`nanoseconds`, `alloc_space`/`bytes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueType<'a> {
    pub kind: &'a str,
    pub unit: &'a str,
}

/// A function: its name and its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function<'a> {
    pub name: &'a str,
    pub file: &'a str,
}

/// A sample: its stack (`Profile::stacks`' range, function places, leaf first), its values
/// (`Profile::values`' range, one a type), its labels (`Profile::labels`' range).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub stack: (u32, u32),
    pub values: (u32, u32),
    pub labels: (u32, u32),
}

/// A label's value: text, or a number and its unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LabelValue<'a> {
    Str(&'a str),
    Num(i64, &'a str),
}

/// A profile read.
#[derive(Debug, Default)]
pub struct Profile<'a> {
    pub sample_types: Vec<ValueType<'a>>,
    pub period_type: Option<ValueType<'a>>,
    pub period: i64,
    pub time_nanos: i64,
    pub duration_nanos: i64,
    /// The type a reader shows first (its place in `sample_types`; the last where none is said).
    pub default_type: usize,
    pub functions: Vec<Function<'a>>,
    pub samples: Vec<Sample>,
    pub stacks: Vec<u32>,
    pub values: Vec<i64>,
    pub labels: Vec<(&'a str, LabelValue<'a>)>,
    /// Stacks past [`MAX_DEPTH`], cut.
    pub cut: u32,
}

impl<'a> Profile<'a> {
    /// Sample `s`'s stack: its functions, leaf first.
    pub fn stack(&self, s: &Sample) -> impl Iterator<Item = &Function<'a>> {
        self.stacks.get(s.stack.0 as usize..s.stack.1 as usize).unwrap_or_default().iter().filter_map(|&f| self.functions.get(f as usize))
    }

    pub fn values_of(&self, s: &Sample) -> &[i64] {
        self.values.get(s.values.0 as usize..s.values.1 as usize).unwrap_or_default()
    }

    pub fn labels_of(&self, s: &Sample) -> &[(&'a str, LabelValue<'a>)] {
        self.labels.get(s.labels.0 as usize..s.labels.1 as usize).unwrap_or_default()
    }

    /// The place of the type `kind` among `sample_types`.
    pub fn type_of(&self, kind: &str) -> Option<usize> {
        self.sample_types.iter().position(|t| t.kind == kind)
    }
}

/// Ids to places: dense ids (pprof's writers number them 1, 2, …) in a table, others in a map.
#[derive(Default)]
struct Ids {
    dense: Vec<u32>,
    sparse: std::collections::HashMap<u64, u32>,
}

impl Ids {
    const NONE: u32 = u32::MAX;

    fn put(&mut self, id: u64, place: u32) -> Result<(), PprofError> {
        if id == 0 {
            return Err(PprofError::Id(0));
        }
        let fresh = match usize::try_from(id).ok().filter(|&i| i <= (self.dense.len() + 1).max(1 << 16)) {
            Some(i) => {
                if self.dense.len() <= i {
                    self.dense.resize(i + 1, Self::NONE);
                }
                match self.dense.get_mut(i) {
                    Some(slot) if *slot == Self::NONE => {
                        *slot = place;
                        true
                    }
                    _ => false,
                }
            }
            None => self.sparse.insert(id, place).is_none(),
        };
        if fresh { Ok(()) } else { Err(PprofError::Id(id)) }
    }

    fn get(&self, id: u64) -> Option<u32> {
        let dense = usize::try_from(id).ok().and_then(|i| self.dense.get(i)).copied().filter(|&p| p != Self::NONE);
        dense.or_else(|| self.sparse.get(&id).copied())
    }
}

/// `msg` (a `Profile`, inflated) read.
pub fn read(msg: &[u8]) -> Result<Profile<'_>, PprofError> {
    // The string table first: everything else names its strings by index.
    let mut strings: Vec<&str> = Vec::new();
    for f in Fields::new(msg) {
        let (n, w) = f?;
        if n == 6 {
            if strings.len() >= MAX_STRINGS {
                return Err(PprofError::Bound("strings"));
            }
            strings.push(w.str().ok_or(PprofError::Kind("a string"))?);
        }
    }
    if strings.first().is_some_and(|s| !s.is_empty()) {
        return Err(PprofError::Table);
    }
    let s = |i: u64| -> Result<&str, PprofError> { usize::try_from(i).ok().and_then(|i| strings.get(i).copied()).ok_or(PprofError::String(i)) };
    let value_type = |b: &[u8]| -> Result<ValueType<'_>, PprofError> {
        let (mut kind, mut unit) = (0, 0);
        for f in Fields::new(b) {
            match f? {
                (1, w) => kind = w.u64().ok_or(PprofError::Kind("a type"))?,
                (2, w) => unit = w.u64().ok_or(PprofError::Kind("a unit"))?,
                _ => {}
            }
        }
        Ok(ValueType { kind: s(kind)?, unit: s(unit)? })
    };
    let mut p = Profile::default();
    // Functions, then locations (each its functions, inlined ones first, the caller last), then
    // samples: locations name functions by id, samples locations.
    let (mut function_ids, mut location_ids) = (Ids::default(), Ids::default());
    let mut location_functions: Vec<u32> = Vec::new();
    let mut locations: Vec<(u32, u32)> = Vec::new();
    let mut default_type = None;
    for f in Fields::new(msg) {
        let (n, w) = f?;
        match n {
            1 => {
                if p.sample_types.len() >= MAX_TYPES {
                    return Err(PprofError::Bound("sample types"));
                }
                p.sample_types.push(value_type(w.bytes().ok_or(PprofError::Kind("a sample type"))?)?);
            }
            5 => {
                if p.functions.len() >= MAX_FUNCTIONS {
                    return Err(PprofError::Bound("functions"));
                }
                let (mut id, mut name, mut file) = (0, 0, 0);
                for g in Fields::new(w.bytes().ok_or(PprofError::Kind("a function"))?) {
                    match g? {
                        (1, v) => id = v.u64().unwrap_or(0),
                        (2, v) => name = v.u64().unwrap_or(0),
                        (4, v) => file = v.u64().unwrap_or(0),
                        _ => {}
                    }
                }
                function_ids.put(id, u32::try_from(p.functions.len()).map_err(|_| PprofError::Bound("functions"))?)?;
                p.functions.push(Function { name: s(name)?, file: s(file)? });
            }
            9 => p.time_nanos = w.i64().unwrap_or(0),
            10 => p.duration_nanos = w.i64().unwrap_or(0),
            11 => p.period_type = Some(value_type(w.bytes().ok_or(PprofError::Kind("a period type"))?)?),
            12 => p.period = w.i64().unwrap_or(0),
            14 => default_type = Some(s(w.u64().unwrap_or(0))?),
            _ => {}
        }
    }
    for f in Fields::new(msg) {
        let (n, w) = f?;
        if n != 4 {
            continue;
        }
        if locations.len() >= MAX_LOCATIONS {
            return Err(PprofError::Bound("locations"));
        }
        let (mut id, start) = (0, location_functions.len());
        for g in Fields::new(w.bytes().ok_or(PprofError::Kind("a location"))?) {
            match g? {
                (1, v) => id = v.u64().unwrap_or(0),
                (4, Wire::Len(line)) => {
                    let mut function = 0;
                    for h in Fields::new(line) {
                        if let (1, v) = h? {
                            function = v.u64().unwrap_or(0);
                        }
                    }
                    // A line without a function (an address alone) names nothing to show.
                    if function != 0 {
                        location_functions.push(function_ids.get(function).ok_or(PprofError::Function(function))?);
                    }
                }
                _ => {}
            }
        }
        let place = u32::try_from(locations.len()).map_err(|_| PprofError::Bound("locations"))?;
        location_ids.put(id, place)?;
        let range = (u32::try_from(start).map_err(|_| PprofError::Bound("locations"))?, u32::try_from(location_functions.len()).map_err(|_| PprofError::Bound("locations"))?);
        locations.push(range);
    }
    let types = p.sample_types.len();
    let (mut ids, mut vals): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
    for f in Fields::new(msg) {
        let (n, w) = f?;
        if n != 2 {
            continue;
        }
        if p.samples.len() >= MAX_SAMPLES {
            return Err(PprofError::Bound("samples"));
        }
        ids.clear();
        vals.clear();
        let (stack_start, values_start, labels_start) = (p.stacks.len(), p.values.len(), p.labels.len());
        for g in Fields::new(w.bytes().ok_or(PprofError::Kind("a sample"))?) {
            match g? {
                (1, v) => varints(&v, &mut ids)?,
                (2, v) => varints(&v, &mut vals)?,
                (3, Wire::Len(label)) => {
                    let (mut key, mut str_, mut num, mut unit) = (0, 0, None, 0);
                    for h in Fields::new(label) {
                        match h? {
                            (1, v) => key = v.u64().unwrap_or(0),
                            (2, v) => str_ = v.u64().unwrap_or(0),
                            (3, v) => num = v.i64(),
                            (4, v) => unit = v.u64().unwrap_or(0),
                            _ => {}
                        }
                    }
                    let value = match num {
                        Some(n) if str_ == 0 => LabelValue::Num(n, s(unit)?),
                        _ => LabelValue::Str(s(str_)?),
                    };
                    p.labels.push((s(key)?, value));
                }
                _ => {}
            }
        }
        if vals.len() != types {
            return Err(PprofError::Values { got: vals.len(), want: types });
        }
        // int64s, as varints: their two's complement.
        p.values.extend(vals.iter().map(|v| v.cast_signed()));
        // The leaf at location_id[0]: each location's functions in their order (inlined first).
        let mut depth = 0usize;
        'stack: for &id in &ids {
            let place = location_ids.get(id).ok_or(PprofError::Location(id))?;
            let (a, b) = locations.get(place as usize).copied().unwrap_or((0, 0));
            for &f in location_functions.get(a as usize..b as usize).unwrap_or_default() {
                if depth == MAX_DEPTH {
                    p.cut += 1;
                    break 'stack;
                }
                p.stacks.push(f);
                depth += 1;
            }
        }
        let to = |n: usize| u32::try_from(n).map_err(|_| PprofError::Bound("frames"));
        p.samples.push(Sample { stack: (to(stack_start)?, to(p.stacks.len())?), values: (to(values_start)?, to(p.values.len())?), labels: (to(labels_start)?, to(p.labels.len())?) });
    }
    p.default_type = default_type.and_then(|d| p.sample_types.iter().position(|t| t.kind == d)).unwrap_or(types.saturating_sub(1));
    Ok(p)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::otlp::pb::Writer;

    /// A Go CPU profile's shape: `samples`/`count` and `cpu`/`nanoseconds`; `main` calling `work`
    /// calling `hash` (inlined into `work`: one location, its lines `hash` then `work`); a sample
    /// labeled with its span; a second stack `main` → `idle`.
    fn profile() -> Vec<u8> {
        let strings = ["", "samples", "count", "cpu", "nanoseconds", "main", "app/main.go", "work", "hash", "idle", "span id", "1234", "cpu"];
        let mut w = Writer::default();
        for st in [(1, 2), (3, 4)] {
            w.message(1, |m| {
                m.varint(1, st.0);
                m.varint(2, st.1);
            });
        }
        // Sample 1: leaf location 2 (hash inlined in work), then location 1 (main); labeled.
        w.message(2, |m| {
            m.bytes(1, &[2, 1]);
            m.bytes(2, &[1, 10]);
            m.message(3, |l| {
                l.varint(1, 10);
                l.varint(2, 11);
            });
        });
        // Sample 2: location 3 (idle), then main.
        w.message(2, |m| {
            m.bytes(1, &[3, 1]);
            m.bytes(2, &[2, 20]);
        });
        for (id, lines) in [(1u64, vec![1u64]), (2, vec![3, 2]), (3, vec![4])] {
            w.message(4, |m| {
                m.varint(1, id);
                for f in &lines {
                    m.message(4, |l| {
                        l.varint(1, *f);
                        l.varint(2, 7);
                    });
                }
            });
        }
        for (id, name) in [(1u64, 5u64), (2, 7), (3, 8), (4, 9)] {
            w.message(5, |m| {
                m.varint(1, id);
                m.varint(2, name);
                m.varint(4, 6);
            });
        }
        for st in strings {
            w.str(6, st);
        }
        w.varint(9, 1_700_000_000_000_000_000);
        w.varint(10, 10_000_000_000);
        w.message(11, |m| {
            m.varint(1, 3);
            m.varint(2, 4);
        });
        w.varint(12, 10_000_000);
        w.varint(14, 12);
        w.buf
    }

    /// Its types, period and times; each sample's stack leaf first (an inlined function before
    /// its caller), its values, its label; the type shown first the one it names.
    #[test]
    fn a_profile_is_read_as_pprof_writes_it() {
        let bytes = profile();
        let p = read(&bytes).unwrap();
        assert_eq!(p.sample_types, [ValueType { kind: "samples", unit: "count" }, ValueType { kind: "cpu", unit: "nanoseconds" }]);
        assert_eq!((p.period, p.time_nanos, p.duration_nanos, p.default_type), (10_000_000, 1_700_000_000_000_000_000, 10_000_000_000, 1));
        assert_eq!(p.period_type, Some(ValueType { kind: "cpu", unit: "nanoseconds" }));
        let stacks: Vec<Vec<&str>> = p.samples.iter().map(|s| p.stack(s).map(|f| f.name).collect()).collect();
        assert_eq!(stacks, [vec!["hash", "work", "main"], vec!["idle", "main"]]);
        assert_eq!(p.values_of(&p.samples[0]), [1, 10]);
        assert_eq!(p.labels_of(&p.samples[0]), [("span id", LabelValue::Str("1234"))]);
        assert_eq!(p.functions[0], Function { name: "main", file: "app/main.go" });
    }

    fn inflate(gz: &[u8]) -> Vec<u8> {
        use std::io::Read as _;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(gz).read_to_end(&mut out).unwrap();
        out
    }

    /// Go 1.27's own profiles (`fixtures/pprof`, written in a container by `main.go.txt`): a CPU
    /// profile of 1.5 s of a hashing loop, and a heap profile of 10,000 allocations of a KiB
    /// sampled every 4 KiB. Read whole, their totals what Go's own `pprof -raw` sums: 150 samples
    /// of 10 ms, 1.45 s of them in SHA-NI's block function; `main.work` calling `main.hash`; the
    /// heap's four types, their sums, its samples' `bytes` labels.
    #[test]
    fn go_s_own_profiles_are_read() {
        let cpu = inflate(include_bytes!("../fixtures/pprof/go-cpu.pprof"));
        let p = read(&cpu).unwrap();
        assert_eq!(p.sample_types, [ValueType { kind: "samples", unit: "count" }, ValueType { kind: "cpu", unit: "nanoseconds" }]);
        assert_eq!((p.period_type, p.period, p.duration_nanos / 10_000_000, p.default_type), (Some(ValueType { kind: "cpu", unit: "nanoseconds" }), 10_000_000, 150, 1));
        let sum = |p: &Profile<'_>, t: usize| p.samples.iter().map(|s| p.values_of(s)[t]).sum::<i64>();
        assert_eq!((sum(&p, 0), sum(&p, 1)), (150, 1_500_000_000));
        let leaf = |name: &str| p.samples.iter().filter(|s| p.stack(s).next().is_some_and(|f| f.name == name)).map(|s| p.values_of(s)[1]).sum::<i64>();
        assert_eq!(leaf("crypto/internal/fips140/sha256.blockSHANI"), 1_450_000_000);
        assert!(p.samples.iter().any(|s| p.stack(s).map(|f| f.name).collect::<Vec<_>>().windows(2).any(|w| w == ["main.hash", "main.work"])));
        assert!(p.functions.iter().all(|f| !f.file.contains("/home/")), "no one's home in a fixture");
        let heap = inflate(include_bytes!("../fixtures/pprof/go-heap.pprof"));
        let h = read(&heap).unwrap();
        assert_eq!(h.sample_types.iter().map(|t| t.kind).collect::<Vec<_>>(), ["alloc_objects", "alloc_space", "inuse_objects", "inuse_space"]);
        assert_eq!((h.samples.len(), h.period, h.default_type), (48, 4096, 3));
        assert_eq!((sum(&h, 0), sum(&h, 1), sum(&h, 2), sum(&h, 3)), (10_503, 13_105_703, 9_786, 10_278_807));
        assert!(h.samples.iter().any(|s| h.labels_of(s).iter().any(|(k, v)| *k == "bytes" && *v == LabelValue::Num(262_144, ""))));
    }

    /// What a profile may not be: a string index past its table, a sample of too few values, a
    /// location or function it does not define, an id of 0 or one given twice, a table not
    /// beginning with "", a field cut short.
    #[test]
    fn a_malformed_profile_is_refused() {
        let good = profile();
        let with = |f: &dyn Fn(&mut Writer)| {
            let mut w = Writer { buf: good.clone() };
            f(&mut w);
            w.buf
        };
        assert!(matches!(
            read(&with(&|w| w.message(5, |m| {
                m.varint(1, 9);
                m.varint(2, 99)
            }))),
            Err(PprofError::String(99))
        ));
        assert!(matches!(
            read(&with(&|w| w.message(2, |m| {
                m.bytes(1, &[1]);
                m.bytes(2, &[1])
            }))),
            Err(PprofError::Values { got: 1, want: 2 })
        ));
        assert!(matches!(
            read(&with(&|w| w.message(2, |m| {
                m.bytes(1, &[77]);
                m.bytes(2, &[1, 1])
            }))),
            Err(PprofError::Location(77))
        ));
        assert!(matches!(
            read(&with(&|w| w.message(4, |m| {
                m.varint(1, 9);
                m.message(4, |l| l.varint(1, 55))
            }))),
            Err(PprofError::Function(55))
        ));
        assert!(matches!(read(&with(&|w| w.message(5, |m| m.varint(1, 1)))), Err(PprofError::Id(1))));
        assert!(matches!(read(&with(&|w| w.message(4, |m| m.varint(1, 0)))), Err(PprofError::Id(0))));
        let mut bad = Writer::default();
        bad.str(6, "x");
        assert!(matches!(read(&bad.buf), Err(PprofError::Table)));
        assert!(read(&good[..good.len() - 3]).is_err(), "cut short");
    }
}
