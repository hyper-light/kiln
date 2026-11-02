//! Protocol Buffers' wire format, read in place (protobuf "Encoding" guide): what OTLP's binary
//! form is made of. Only reading; every malformed input is an error, never a panic.

use crate::varint;

/// A field's value as the wire carries it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Wire<'a> {
    Varint(u64),
    I64([u8; 8]),
    Len(&'a [u8]),
    I32([u8; 4]),
}

impl<'a> Wire<'a> {
    pub fn u64(&self) -> Option<u64> {
        match self {
            Self::Varint(v) => Some(*v),
            Self::I64(b) => Some(u64::from_le_bytes(*b)),
            Self::I32(b) => Some(u64::from(u32::from_le_bytes(*b))),
            Self::Len(_) => None,
        }
    }

    pub fn f64(&self) -> Option<f64> {
        match self {
            Self::I64(b) => Some(f64::from_le_bytes(*b)),
            Self::I32(b) => Some(f64::from(f32::from_le_bytes(*b))),
            _ => None,
        }
    }

    pub fn i64(&self) -> Option<i64> {
        self.u64().map(|v| v as i64)
    }

    /// A `sint32`/`sint64`: zigzag.
    pub fn sint(&self) -> Option<i64> {
        match self {
            Self::Varint(v) => Some(varint::zag(*v)),
            _ => None,
        }
    }

    /// A length-delimited field's bytes, borrowed from the message.
    pub fn bytes(&self) -> Option<&'a [u8]> {
        match self {
            Self::Len(b) => Some(b),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&'a str> {
        self.bytes().and_then(|b| std::str::from_utf8(b).ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PbError {
    #[error("the message ended inside a field")]
    Truncated,
    #[error("wire type {0} (groups and unknown types are refused)")]
    WireType(u64),
    #[error("field number 0")]
    FieldZero,
}

/// A message's fields, in order.
#[derive(Debug, Clone)]
pub struct Fields<'a> {
    at: &'a [u8],
}

impl<'a> Fields<'a> {
    pub fn new(msg: &'a [u8]) -> Self {
        Self { at: msg }
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Wire<'a>), PbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at.is_empty() {
            return None;
        }
        let r = (|| {
            let key = varint::get(&mut self.at).ok_or(PbError::Truncated)?;
            let field = u32::try_from(key >> 3).map_err(|_| PbError::FieldZero)?;
            if field == 0 {
                return Err(PbError::FieldZero);
            }
            let w = match key & 7 {
                0 => Wire::Varint(varint::get(&mut self.at).ok_or(PbError::Truncated)?),
                1 => {
                    let (b, rest) = self.at.split_first_chunk::<8>().ok_or(PbError::Truncated)?;
                    self.at = rest;
                    Wire::I64(*b)
                }
                2 => {
                    let n = usize::try_from(varint::get(&mut self.at).ok_or(PbError::Truncated)?).map_err(|_| PbError::Truncated)?;
                    let (b, rest) = self.at.split_at_checked(n).ok_or(PbError::Truncated)?;
                    self.at = rest;
                    Wire::Len(b)
                }
                5 => {
                    let (b, rest) = self.at.split_first_chunk::<4>().ok_or(PbError::Truncated)?;
                    self.at = rest;
                    Wire::I32(*b)
                }
                t => return Err(PbError::WireType(t)),
            };
            Ok((field, w))
        })();
        if r.is_err() {
            // Nothing after an error is trusted.
            self.at = &[];
        }
        Some(r)
    }
}

/// A packed repeated `fixed64` field's values (or one unpacked value).
pub fn fixed64s(w: &Wire<'_>, out: &mut Vec<u64>) -> Result<(), PbError> {
    match w {
        Wire::Len(b) => {
            let (words, rest) = b.as_chunks::<8>();
            if !rest.is_empty() {
                return Err(PbError::Truncated);
            }
            out.extend(words.iter().map(|w| u64::from_le_bytes(*w)));
            Ok(())
        }
        Wire::I64(b) => {
            out.push(u64::from_le_bytes(*b));
            Ok(())
        }
        _ => Err(PbError::WireType(0)),
    }
}

/// A packed repeated `double` field's values (or one unpacked value).
pub fn doubles(w: &Wire<'_>, out: &mut Vec<f64>) -> Result<(), PbError> {
    match w {
        Wire::Len(b) => {
            let (words, rest) = b.as_chunks::<8>();
            if !rest.is_empty() {
                return Err(PbError::Truncated);
            }
            out.extend(words.iter().map(|w| f64::from_le_bytes(*w)));
            Ok(())
        }
        Wire::I64(b) => {
            out.push(f64::from_le_bytes(*b));
            Ok(())
        }
        _ => Err(PbError::WireType(0)),
    }
}

/// A packed repeated varint field's values (or one unpacked value).
pub fn varints(w: &Wire<'_>, out: &mut Vec<u64>) -> Result<(), PbError> {
    match w {
        Wire::Len(b) => {
            let mut at = *b;
            while !at.is_empty() {
                out.push(varint::get(&mut at).ok_or(PbError::Truncated)?);
            }
            Ok(())
        }
        Wire::Varint(v) => {
            out.push(*v);
            Ok(())
        }
        _ => Err(PbError::WireType(0)),
    }
}

/// Writing, for tests and for building OTLP to send.
#[derive(Debug, Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn varint(&mut self, field: u32, v: u64) {
        varint::put(&mut self.buf, u64::from(field) << 3);
        varint::put(&mut self.buf, v);
    }

    pub fn fixed64(&mut self, field: u32, v: u64) {
        varint::put(&mut self.buf, (u64::from(field) << 3) | 1);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn double(&mut self, field: u32, v: f64) {
        self.fixed64(field, v.to_bits());
    }

    pub fn bytes(&mut self, field: u32, b: &[u8]) {
        varint::put(&mut self.buf, (u64::from(field) << 3) | 2);
        varint::put(&mut self.buf, b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    pub fn str(&mut self, field: u32, s: &str) {
        self.bytes(field, s.as_bytes());
    }

    /// A nested message, built by `f`.
    pub fn message(&mut self, field: u32, f: impl FnOnce(&mut Writer)) {
        let mut inner = Writer::default();
        f(&mut inner);
        self.bytes(field, &inner.buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_of_every_wire_type_read_back() {
        let mut w = Writer::default();
        w.varint(1, 150);
        w.fixed64(2, 0xdead_beef);
        w.str(3, "hi");
        w.double(4, 1.5);
        w.message(5, |m| m.varint(1, 7));
        let got: Vec<_> = Fields::new(&w.buf).map(Result::unwrap).collect();
        assert_eq!(got[0], (1, Wire::Varint(150)));
        assert_eq!(got[1].1.u64(), Some(0xdead_beef));
        assert_eq!(got[2].1.str(), Some("hi"));
        assert_eq!(got[3].1.f64(), Some(1.5));
        let inner: Vec<_> = Fields::new(got[4].1.bytes().unwrap()).map(Result::unwrap).collect();
        assert_eq!(inner, vec![(1, Wire::Varint(7))]);
        // The canonical example of the encoding guide: field 1, varint 150.
        assert_eq!(&w.buf[..3], &[0x08, 0x96, 0x01]);
    }

    #[test]
    fn malformed_messages_are_errors() {
        assert_eq!(Fields::new(&[0x0a, 0x05, 1]).next(), Some(Err(PbError::Truncated)));
        assert_eq!(Fields::new(&[0x0b]).next(), Some(Err(PbError::WireType(3))));
        assert_eq!(Fields::new(&[0x00, 0x01]).next(), Some(Err(PbError::FieldZero)));
        let mut rng = crate::testkit::Rng::new(2);
        for _ in 0..20_000 {
            let junk: Vec<u8> = (0..rng.below(40)).map(|_| rng.next_u64() as u8).collect();
            for f in Fields::new(&junk) {
                if let Ok((_, w)) = f {
                    let mut v = Vec::new();
                    let _ = fixed64s(&w, &mut v);
                    let mut d = Vec::new();
                    let _ = doubles(&w, &mut d);
                    let _ = varints(&w, &mut v);
                } else {
                    break;
                }
            }
        }
    }
}
