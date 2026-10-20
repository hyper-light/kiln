//! MessagePack (the msgpack.org specification, 2017), read in place: what Datadog's trace agent
//! sends its stats in (`/api/v0.2/stats`, `crate::ddtrace`). A [`Reader`] gives each value's head
//! (a scalar whole, a string or bytes borrowed from the payload, an array's or a map's length, its
//! items following); nothing is allocated, and [`Reader::skip`] passes a value whole without
//! recursion, so no nesting a payload holds can exhaust the stack.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MsgpackError {
    #[error("the payload ends inside a value")]
    Truncated,
    #[error("a string that is not UTF-8")]
    Utf8,
    #[error("{found} where {wanted} was expected")]
    Unexpected { found: &'static str, wanted: &'static str },
    #[error("type byte 0xc1, which the specification never uses")]
    Reserved,
}

/// A value's head.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Nil,
    Bool(bool),
    /// A negative fixint or an int 8 to 64.
    Int(i64),
    /// A positive fixint or a uint 8 to 64.
    Uint(u64),
    F32(f32),
    F64(f64),
    Str(&'a str),
    Bin(&'a [u8]),
    /// An array of this many items, which follow.
    Array(u32),
    /// A map of this many pairs, which follow (each key, then its value).
    Map(u32),
    Ext(i8, &'a [u8]),
}

impl Value<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Nil => "nil",
            Self::Bool(_) => "a bool",
            Self::Int(_) | Self::Uint(_) => "an integer",
            Self::F32(_) | Self::F64(_) => "a float",
            Self::Str(_) => "a string",
            Self::Bin(_) => "bytes",
            Self::Array(_) => "an array",
            Self::Map(_) => "a map",
            Self::Ext(..) => "an extension",
        }
    }
}

/// Reads values from a payload, one after another.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// A reader of the same payload from `pos` (one [`Reader::pos`] gave).
    pub fn at(&self, pos: usize) -> Self {
        Self { buf: self.buf, pos }
    }

    /// Where the next value begins.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Whether the payload is read to its end.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], MsgpackError> {
        let end = self.pos.checked_add(n).ok_or(MsgpackError::Truncated)?;
        let b = self.buf.get(self.pos..end).ok_or(MsgpackError::Truncated)?;
        self.pos = end;
        Ok(b)
    }

    fn be<const N: usize>(&mut self) -> Result<[u8; N], MsgpackError> {
        let b = self.take(N)?;
        <[u8; N]>::try_from(b).map_err(|_| MsgpackError::Truncated)
    }

    /// A length of `w` bytes, big-endian.
    fn len(&mut self, w: usize) -> Result<usize, MsgpackError> {
        Ok(match w {
            1 => usize::from(u8::from_be_bytes(self.be()?)),
            2 => usize::from(u16::from_be_bytes(self.be()?)),
            _ => u32::from_be_bytes(self.be()?) as usize,
        })
    }

    /// A container's length, at most what the bytes left could hold (an item takes one at least).
    fn count(&mut self, n: usize, per: usize) -> Result<u32, MsgpackError> {
        if n.saturating_mul(per) > self.buf.len() - self.pos.min(self.buf.len()) {
            return Err(MsgpackError::Truncated);
        }
        u32::try_from(n).map_err(|_| MsgpackError::Truncated)
    }

    fn str_of(&mut self, n: usize) -> Result<Value<'a>, MsgpackError> {
        let b = self.take(n)?;
        std::str::from_utf8(b).map(Value::Str).map_err(|_| MsgpackError::Utf8)
    }

    /// The next value's head.
    pub fn value(&mut self) -> Result<Value<'a>, MsgpackError> {
        let [c] = self.be::<1>()?;
        Ok(match c {
            0x00..=0x7f => Value::Uint(u64::from(c)),
            0x80..=0x8f => Value::Map(self.count(usize::from(c & 0x0f), 2)?),
            0x90..=0x9f => Value::Array(self.count(usize::from(c & 0x0f), 1)?),
            0xa0..=0xbf => self.str_of(usize::from(c & 0x1f))?,
            0xc0 => Value::Nil,
            0xc1 => return Err(MsgpackError::Reserved),
            0xc2 => Value::Bool(false),
            0xc3 => Value::Bool(true),
            0xc4..=0xc6 => {
                let n = self.len(1 << (c - 0xc4))?;
                Value::Bin(self.take(n)?)
            }
            0xc7..=0xc9 => {
                let n = self.len(1 << (c - 0xc7))?;
                let [t] = self.be::<1>()?;
                Value::Ext(i8::from_be_bytes([t]), self.take(n)?)
            }
            0xca => Value::F32(f32::from_be_bytes(self.be()?)),
            0xcb => Value::F64(f64::from_be_bytes(self.be()?)),
            0xcc => Value::Uint(u64::from(u8::from_be_bytes(self.be()?))),
            0xcd => Value::Uint(u64::from(u16::from_be_bytes(self.be()?))),
            0xce => Value::Uint(u64::from(u32::from_be_bytes(self.be()?))),
            0xcf => Value::Uint(u64::from_be_bytes(self.be()?)),
            0xd0 => Value::Int(i64::from(i8::from_be_bytes(self.be()?))),
            0xd1 => Value::Int(i64::from(i16::from_be_bytes(self.be()?))),
            0xd2 => Value::Int(i64::from(i32::from_be_bytes(self.be()?))),
            0xd3 => Value::Int(i64::from_be_bytes(self.be()?)),
            0xd4..=0xd8 => {
                let [t] = self.be::<1>()?;
                Value::Ext(i8::from_be_bytes([t]), self.take(1 << (c - 0xd4))?)
            }
            0xd9..=0xdb => {
                let n = self.len(1 << (c - 0xd9))?;
                self.str_of(n)?
            }
            0xdc | 0xdd => {
                let n = self.len(if c == 0xdc { 2 } else { 4 })?;
                Value::Array(self.count(n, 1)?)
            }
            0xde | 0xdf => {
                let n = self.len(if c == 0xde { 2 } else { 4 })?;
                Value::Map(self.count(n, 2)?)
            }
            0xe0..=0xff => Value::Int(i64::from(i8::from_be_bytes([c]))),
        })
    }

    /// Passes the next value whole, its items too: a count of items left, not a recursion (each
    /// item takes a byte at the least, so the count is bounded by the payload).
    pub fn skip(&mut self) -> Result<(), MsgpackError> {
        let mut left: u64 = 1;
        while left > 0 {
            left -= 1;
            match self.value()? {
                Value::Array(n) => left += u64::from(n),
                Value::Map(n) => left += 2 * u64::from(n),
                _ => {}
            }
        }
        Ok(())
    }

    fn wrong<T>(found: Value<'_>, wanted: &'static str) -> Result<T, MsgpackError> {
        Err(MsgpackError::Unexpected { found: found.name(), wanted })
    }

    /// A string (nil: empty).
    pub fn str(&mut self) -> Result<&'a str, MsgpackError> {
        match self.value()? {
            Value::Str(s) => Ok(s),
            Value::Nil => Ok(""),
            v => Self::wrong(v, "a string"),
        }
    }

    /// Bytes, or a string's (nil: none).
    pub fn bin(&mut self) -> Result<&'a [u8], MsgpackError> {
        match self.value()? {
            Value::Bin(b) => Ok(b),
            Value::Str(s) => Ok(s.as_bytes()),
            Value::Nil => Ok(&[]),
            v => Self::wrong(v, "bytes"),
        }
    }

    /// An integer at or above zero (nil: zero); a negative one is zero.
    pub fn u64(&mut self) -> Result<u64, MsgpackError> {
        match self.value()? {
            Value::Uint(u) => Ok(u),
            Value::Int(i) => Ok(u64::try_from(i).unwrap_or(0)),
            Value::Nil => Ok(0),
            v => Self::wrong(v, "an integer"),
        }
    }

    /// An integer (nil: zero); one past `i64::MAX` is it.
    pub fn i64(&mut self) -> Result<i64, MsgpackError> {
        match self.value()? {
            Value::Int(i) => Ok(i),
            Value::Uint(u) => Ok(i64::try_from(u).unwrap_or(i64::MAX)),
            Value::Nil => Ok(0),
            v => Self::wrong(v, "an integer"),
        }
    }

    /// A number, of either width or an integer (nil: zero).
    #[allow(clippy::cast_precision_loss)]
    pub fn f64(&mut self) -> Result<f64, MsgpackError> {
        match self.value()? {
            Value::F64(f) => Ok(f),
            Value::F32(f) => Ok(f64::from(f)),
            Value::Int(i) => Ok(i as f64),
            Value::Uint(u) => Ok(u as f64),
            Value::Nil => Ok(0.0),
            v => Self::wrong(v, "a number"),
        }
    }

    /// A bool (nil: false; an integer: whether it is not zero).
    pub fn bool(&mut self) -> Result<bool, MsgpackError> {
        match self.value()? {
            Value::Bool(b) => Ok(b),
            Value::Uint(u) => Ok(u != 0),
            Value::Int(i) => Ok(i != 0),
            Value::Nil => Ok(false),
            v => Self::wrong(v, "a bool"),
        }
    }

    /// An array's length (nil: an empty one).
    pub fn array(&mut self) -> Result<u32, MsgpackError> {
        match self.value()? {
            Value::Array(n) => Ok(n),
            Value::Nil => Ok(0),
            v => Self::wrong(v, "an array"),
        }
    }

    /// A map's length (nil: an empty one).
    pub fn map(&mut self) -> Result<u32, MsgpackError> {
        match self.value()? {
            Value::Map(n) => Ok(n),
            Value::Nil => Ok(0),
            v => Self::wrong(v, "a map"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer of the forms the tests read, the widths the specification gives each.
    #[derive(Default)]
    struct W(Vec<u8>);

    impl W {
        fn map(&mut self, n: u32) -> &mut Self {
            match n {
                0..=15 => self.0.push(0x80 | n as u8),
                16..=0xffff => {
                    self.0.push(0xde);
                    self.0.extend_from_slice(&(n as u16).to_be_bytes());
                }
                _ => {
                    self.0.push(0xdf);
                    self.0.extend_from_slice(&n.to_be_bytes());
                }
            }
            self
        }
        fn array(&mut self, n: u32) -> &mut Self {
            match n {
                0..=15 => self.0.push(0x90 | n as u8),
                16..=0xffff => {
                    self.0.push(0xdc);
                    self.0.extend_from_slice(&(n as u16).to_be_bytes());
                }
                _ => {
                    self.0.push(0xdd);
                    self.0.extend_from_slice(&n.to_be_bytes());
                }
            }
            self
        }
        fn str(&mut self, s: &str) -> &mut Self {
            let n = s.len();
            match n {
                0..=31 => self.0.push(0xa0 | n as u8),
                32..=0xff => self.0.extend_from_slice(&[0xd9, n as u8]),
                0x100..=0xffff => {
                    self.0.push(0xda);
                    self.0.extend_from_slice(&(n as u16).to_be_bytes());
                }
                _ => {
                    self.0.push(0xdb);
                    self.0.extend_from_slice(&(n as u32).to_be_bytes());
                }
            }
            self.0.extend_from_slice(s.as_bytes());
            self
        }
        fn uint(&mut self, v: u64) -> &mut Self {
            match v {
                0..=0x7f => self.0.push(v as u8),
                0x80..=0xff => self.0.extend_from_slice(&[0xcc, v as u8]),
                0x100..=0xffff => {
                    self.0.push(0xcd);
                    self.0.extend_from_slice(&(v as u16).to_be_bytes());
                }
                0x1_0000..=0xffff_ffff => {
                    self.0.push(0xce);
                    self.0.extend_from_slice(&(v as u32).to_be_bytes());
                }
                _ => {
                    self.0.push(0xcf);
                    self.0.extend_from_slice(&v.to_be_bytes());
                }
            }
            self
        }
        fn raw(&mut self, b: &[u8]) -> &mut Self {
            self.0.extend_from_slice(b);
            self
        }
    }

    #[test]
    fn every_form_reads_as_the_specification_has_it() -> Result<(), MsgpackError> {
        let mut w = W::default();
        w.array(20);
        w.uint(5).uint(200).uint(60_000).uint(4_000_000_000).uint(u64::MAX);
        w.raw(&[0xff]).raw(&[0xd0, 0x80]).raw(&[0xd1, 0x80, 0x00]).raw(&[0xd2, 0x80, 0, 0, 0]).raw(&[0xd3, 0x80, 0, 0, 0, 0, 0, 0, 0]);
        w.raw(&[0xca]).raw(&1.5f32.to_be_bytes()).raw(&[0xcb]).raw(&(-2.25f64).to_be_bytes());
        w.raw(&[0xc0, 0xc2, 0xc3]);
        w.str("hé").str(&"x".repeat(40)).str(&"y".repeat(300));
        w.raw(&[0xc4, 3, 1, 2, 3]).raw(&[0xd4, 7, 9]);
        let mut r = Reader::new(&w.0);
        assert_eq!(r.value()?, Value::Array(20));
        for want in [5, 200, 60_000, 4_000_000_000, u64::MAX] {
            assert_eq!(r.value()?, Value::Uint(want));
        }
        for want in [-1, -128, -32_768, i64::from(i32::MIN), i64::MIN] {
            assert_eq!(r.value()?, Value::Int(want));
        }
        assert_eq!(r.value()?, Value::F32(1.5));
        assert_eq!(r.value()?, Value::F64(-2.25));
        assert_eq!([r.value()?, r.value()?, r.value()?], [Value::Nil, Value::Bool(false), Value::Bool(true)]);
        assert_eq!(r.value()?, Value::Str("hé"));
        assert!(matches!(r.value()?, Value::Str(s) if s.len() == 40));
        assert!(matches!(r.value()?, Value::Str(s) if s.len() == 300));
        assert_eq!(r.value()?, Value::Bin(&[1, 2, 3]));
        assert_eq!(r.value()?, Value::Ext(7, &[9]));
        assert!(r.is_empty());
        Ok(())
    }

    #[test]
    fn a_value_is_passed_whole_however_deep_and_a_short_payload_is_refused() -> Result<(), MsgpackError> {
        // A map of an array nested ten thousand deep, then a value after it.
        let mut w = W::default();
        w.map(1).str("deep");
        for _ in 0..10_000 {
            w.array(1);
        }
        w.uint(1);
        w.str("after");
        let mut r = Reader::new(&w.0);
        r.skip()?;
        assert_eq!(r.str()?, "after");
        // Cut anywhere, a payload is refused, never read past its end.
        let mut whole = W::default();
        whole.map(2).str("Stats").array(2).uint(1).str("two").str("Hits").uint(70_000);
        for cut in 0..whole.0.len() {
            let mut r = Reader::new(whole.0.get(..cut).unwrap_or_default());
            assert!(r.skip().is_err(), "cut at {cut}");
        }
        Reader::new(&whole.0).skip()?;
        // A container longer than the bytes left could hold is refused before it is walked.
        assert_eq!(Reader::new(&[0xdd, 0xff, 0xff, 0xff, 0xff]).value(), Err(MsgpackError::Truncated));
        assert_eq!(Reader::new(&[0xc1]).value(), Err(MsgpackError::Reserved));
        assert_eq!(Reader::new(&[0xa2, 0xff, 0xfe]).value(), Err(MsgpackError::Utf8));
        Ok(())
    }

    #[test]
    fn typed_reads_take_what_go_s_msgp_writes_for_a_field_and_refuse_the_rest() -> Result<(), MsgpackError> {
        let mut w = W::default();
        w.raw(&[0xc0]).raw(&[0xc0]).raw(&[0xc0]).uint(3).raw(&[0xff]).str("x");
        let mut r = Reader::new(&w.0);
        assert_eq!((r.str()?, r.u64()?, r.array()?), ("", 0, 0));
        assert_eq!(r.f64()?, 3.0);
        assert_eq!(r.u64()?, 0);
        assert!(matches!(r.u64(), Err(MsgpackError::Unexpected { found: "a string", .. })));
        Ok(())
    }
}
