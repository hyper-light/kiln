//! Values and how they convert, GitHub-exact (`github.com/actions/languageservices`,
//! `expressions/src/result.ts` and `data/`): JavaScript's number parsing and printing,
//! truthiness, loose equality and ordering with case-insensitive strings, and JSON as
//! `JSON.parse` / `JSON.stringify` read and write it (key order kept).

use std::sync::Arc;

/// An object's members, in insertion order. Lookup is case-insensitive (W01-R03) and the first
/// member of a name wins, as GitHub's dictionaries do; the written case is kept for `toJSON`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map(pub Vec<(Arc<str>, Value)>);

impl Map {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| ci_eq(k, name)).map(|(_, v)| v)
    }

    /// Adds a member unless one of the same name (ignoring case) exists.
    pub fn add(&mut self, name: &str, value: Value) {
        if self.get(name).is_none() {
            self.0.push((Arc::from(name), value));
        }
    }

    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.0.iter().map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Case-insensitive equality of names, as `toLowerCase()` compares them.
pub fn ci_eq(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.to_lowercase() == b.to_lowercase()
}

/// A value: JSON's types, shared cheaply. `Send + Sync`.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(Arc<str>),
    Array(Arc<Vec<Value>>),
    /// The result of a `*` filter: an array whose index access applies to each of its elements.
    Filtered(Arc<Vec<Value>>),
    Object(Arc<Map>),
}

/// Values compare structurally here (for tests and the planner); the expression language's own
/// `==` is [`equals`], where arrays and objects equal only themselves.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Number(a), Self::Number(b)) => a.to_bits() == b.to_bits() || a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a) | Self::Filtered(a), Self::Array(b) | Self::Filtered(b)) => a == b,
            (Self::Object(a), Self::Object(b)) => a == b,
            _ => false,
        }
    }
}

/// The kinds GitHub names in its errors and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Boolean => "Boolean",
            Self::Number => "Number",
            Self::String => "String",
            Self::Array => "Array",
            Self::Object => "Object",
        }
    }
}

impl Value {
    pub fn str(s: &str) -> Self {
        Self::String(s.into())
    }

    pub fn array(items: Vec<Value>) -> Self {
        Self::Array(Arc::new(items))
    }

    pub fn object(members: impl IntoIterator<Item = (String, Value)>) -> Self {
        let mut m = Map::default();
        for (k, v) in members {
            m.add(&k, v);
        }
        Self::Object(Arc::new(m))
    }

    pub fn kind(&self) -> Kind {
        match self {
            Self::Null => Kind::Null,
            Self::Bool(_) => Kind::Boolean,
            Self::Number(_) => Kind::Number,
            Self::String(_) => Kind::String,
            Self::Array(_) | Self::Filtered(_) => Kind::Array,
            Self::Object(_) => Kind::Object,
        }
    }

    /// Null, booleans, numbers and strings.
    pub fn is_primitive(&self) -> bool {
        matches!(self, Self::Null | Self::Bool(_) | Self::Number(_) | Self::String(_))
    }

    /// The elements of an array (filtered or not).
    pub fn items(&self) -> Option<&[Value]> {
        match self {
            Self::Array(a) | Self::Filtered(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    /// As a number, the way GitHub coerces (W01-R04).
    pub fn number(&self) -> f64 {
        match self {
            Self::Null => 0.0,
            Self::Bool(b) => f64::from(u8::from(*b)),
            Self::Number(n) => *n,
            Self::String(s) => js_string_to_number(s),
            Self::Array(_) | Self::Filtered(_) | Self::Object(_) => f64::NAN,
        }
    }

    /// As a string, the way GitHub coerces and interpolates (W01-R07).
    pub fn coerce_string(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Bool(true) => "true".to_owned(),
            Self::Bool(false) => "false".to_owned(),
            Self::Number(n) => number_to_display(*n),
            Self::String(s) => s.to_string(),
            Self::Array(_) | Self::Filtered(_) => "Array".to_owned(),
            Self::Object(_) => "Object".to_owned(),
        }
    }

    pub fn truthy(&self) -> bool {
        !self.falsy()
    }

    /// `false`, `0`, `-0`, `""`, `null` and NaN (W01-R05).
    pub fn falsy(&self) -> bool {
        match self {
            Self::Null => true,
            Self::Bool(b) => !b,
            Self::Number(n) => *n == 0.0 || n.is_nan(),
            Self::String(s) => s.is_empty(),
            _ => false,
        }
    }

    /// From a `serde_json` value (object key order is whatever `serde_json` kept).
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => Self::Number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => Self::String(s.as_str().into()),
            serde_json::Value::Array(a) => Self::array(a.iter().map(Self::from_json).collect()),
            serde_json::Value::Object(o) => {
                let mut m = Map::default();
                for (k, v) in o {
                    m.add(k, Self::from_json(v));
                }
                Self::Object(Arc::new(m))
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Number(n) => serde_json::Number::from_f64(*n).map_or(serde_json::Value::Null, serde_json::Value::Number),
            Self::String(s) => serde_json::Value::String(s.to_string()),
            Self::Array(a) | Self::Filtered(a) => serde_json::Value::Array(a.iter().map(Self::to_json).collect()),
            Self::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, v) in &o.0 {
                    m.insert(k.to_string(), v.to_json());
                }
                serde_json::Value::Object(m)
            }
        }
    }

    /// An estimate of the memory a value holds, for the evaluation memory bound (strings count
    /// two bytes per character, as the runner's UTF-16 strings do).
    pub fn mem_size(&self) -> usize {
        match self {
            Self::Null | Self::Bool(_) | Self::Number(_) => 8,
            Self::String(s) => 16 + 2 * s.len(),
            Self::Array(a) | Self::Filtered(a) => 16 + a.iter().map(Value::mem_size).sum::<usize>(),
            Self::Object(o) => 16 + o.0.iter().map(|(k, v)| 16 + 2 * k.len() + v.mem_size()).sum::<usize>(),
        }
    }
}

// ------------------------------------------------------------------ comparisons (result.ts)

/// JavaScript's abstract equality, except that arrays and objects are not coerced and strings
/// compare ignoring case.
fn coerce_types(l: &Value, r: &Value) -> (Value, Value) {
    let (lk, rk) = (l.kind(), r.kind());
    if lk == rk {
        return (l.clone(), r.clone());
    }
    match lk {
        Kind::Number if rk == Kind::String => return (l.clone(), Value::Number(r.number())),
        Kind::String if rk == Kind::Number => return (Value::Number(l.number()), r.clone()),
        Kind::Null | Kind::Boolean => return coerce_types(&Value::Number(l.number()), r),
        _ => {}
    }
    match rk {
        Kind::Null | Kind::Boolean => coerce_types(l, &Value::Number(r.number())),
        _ => (l.clone(), r.clone()),
    }
}

pub fn equals(l: &Value, r: &Value) -> bool {
    let (l, r) = coerce_types(l, r);
    match (&l, &r) {
        (Value::Null, Value::Null) => true,
        (Value::Number(a), Value::Number(b)) => !a.is_nan() && !b.is_nan() && a == b,
        (Value::String(a), Value::String(b)) => upper_special(a) == upper_special(b),
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Array(a) | Value::Filtered(a), Value::Array(b) | Value::Filtered(b)) => Arc::ptr_eq(a, b),
        (Value::Object(a), Value::Object(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

pub fn greater_than(l: &Value, r: &Value) -> bool {
    let (l, r) = coerce_types(l, r);
    match (&l, &r) {
        (Value::Number(a), Value::Number(b)) => !a.is_nan() && !b.is_nan() && a > b,
        (Value::String(a), Value::String(b)) => utf16_cmp(&upper_special(a), &upper_special(b)) == std::cmp::Ordering::Greater,
        (Value::Bool(a), Value::Bool(b)) => *a && !*b,
        _ => false,
    }
}

pub fn less_than(l: &Value, r: &Value) -> bool {
    let (l, r) = coerce_types(l, r);
    match (&l, &r) {
        (Value::Number(a), Value::Number(b)) => !a.is_nan() && !b.is_nan() && a < b,
        (Value::String(a), Value::String(b)) => utf16_cmp(&upper_special(a), &upper_special(b)) == std::cmp::Ordering::Less,
        (Value::Bool(a), Value::Bool(b)) => !*a && *b,
        _ => false,
    }
}

/// Strings compare as JavaScript compares them: by UTF-16 code units.
fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Upper case, except the small dotless i (`ı`), which GitHub leaves alone.
pub fn upper_special(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_uppercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == 'ı' {
            out.push(c);
        } else {
            out.extend(c.to_uppercase());
        }
    }
    out
}

// ------------------------------------------------------------------ numbers (JavaScript)

/// JavaScript's whitespace (`StrWhiteSpaceChar`: WhiteSpace and LineTerminator).
pub fn js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{b}'
            | '\u{c}'
            | '\r'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `Number(s)`: JavaScript's StringToNumber (decimal, `0x`/`0o`/`0b`, `Infinity`, whitespace
/// trimmed, the empty string is 0, anything else NaN).
pub fn js_string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(js_whitespace);
    if t.is_empty() {
        return 0.0;
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let lower2 = t.get(..2).map(str::to_ascii_lowercase);
    let radix = match lower2.as_deref() {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => 0,
    };
    if radix != 0 {
        let digits = t.get(2..).unwrap_or_default();
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut v = 0.0f64;
        for c in digits.chars() {
            match c.to_digit(radix) {
                Some(d) => v = v * f64::from(radix) + f64::from(d),
                None => return f64::NAN,
            }
        }
        return v;
    }
    if !is_decimal_literal(t) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `[+-]? (digits [. digits?] | . digits) ([eE] [+-]? digits)?`
fn is_decimal_literal(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = 0usize;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0usize;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let f = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        frac_digits = i - f;
    }
    if int_digits + frac_digits == 0 {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let e = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

/// The shortest round-trip digits of a positive, finite, non-zero number, and `n` such that the
/// value is `0.digits × 10^n` (ECMAScript Number::toString's `k`, `n`, `s`).
fn shortest_digits(x: f64) -> (String, i32) {
    let e = format!("{x:e}");
    let (mantissa, exp) = e.split_once('e').unwrap_or((e.as_str(), "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0".to_owned() } else { digits.to_owned() };
    (digits, exp + 1)
}

/// JavaScript's `Number.prototype.toString()` for a number.
pub fn js_number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x == 0.0 {
        return "0".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".to_owned() } else { "-Infinity".to_owned() };
    }
    if x < 0.0 {
        return format!("-{}", js_number_to_string(-x));
    }
    let (s, n) = shortest_digits(x);
    let k = i32::try_from(s.len()).unwrap_or(i32::MAX);
    if k <= n && n <= 21 {
        let zeros = usize::try_from(n - k).unwrap_or(0);
        return format!("{s}{}", "0".repeat(zeros));
    }
    if 0 < n && n <= 21 {
        let at = usize::try_from(n).unwrap_or(0);
        return format!("{}.{}", s.get(..at).unwrap_or_default(), s.get(at..).unwrap_or_default());
    }
    if -6 < n && n <= 0 {
        let zeros = usize::try_from(-n).unwrap_or(0);
        return format!("0.{}{s}", "0".repeat(zeros));
    }
    let e = n - 1;
    let sign = if e < 0 { '-' } else { '+' };
    let first = s.get(..1).unwrap_or_default();
    let rest = s.get(1..).unwrap_or_default();
    if rest.is_empty() { format!("{first}e{sign}{}", e.abs()) } else { format!("{first}.{rest}e{sign}{}", e.abs()) }
}

/// `(+x.toFixed(15)).toString()`, which is how GitHub prints numbers so that `1.2` never shows as
/// `1.199999…` (`data/number.ts`).
pub fn number_to_display(x: f64) -> String {
    if x == 0.0 {
        return "0".to_owned();
    }
    js_number_to_string(js_round_fixed(x, 15))
}

/// `+x.toFixed(digits)`: rounds to `digits` decimals exactly as JavaScript does (ties away from
/// zero, on the exact binary value), for `digits` ≤ 18.
pub fn js_round_fixed(x: f64, digits: u32) -> f64 {
    if !x.is_finite() || x.abs() >= 1e21 {
        return x;
    }
    let neg = x < 0.0;
    let a = x.abs();
    let bits = a.to_bits();
    let exp_bits = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    let frac = bits & ((1u64 << 52) - 1);
    let (mant, exp) = if exp_bits == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp_bits - 1075) };
    let scale = 10u128.pow(digits.min(18));
    let n: u128 = if exp >= 0 {
        u128::from(mant).checked_shl(exp.unsigned_abs()).unwrap_or(u128::MAX).saturating_mul(scale)
    } else {
        let shift = exp.unsigned_abs();
        let num = u128::from(mant) * scale;
        if shift >= 127 {
            0
        } else {
            let q = num >> shift;
            let r = num & ((1u128 << shift) - 1);
            let half = 1u128 << (shift - 1);
            if r >= half { q + 1 } else { q }
        }
    };
    let int = n / scale;
    let fracpart = n % scale;
    let text = format!("{int}.{fracpart:0width$}", width = usize::try_from(digits.min(18)).unwrap_or(15));
    let v: f64 = text.parse().unwrap_or(a);
    if neg { -v } else { v }
}

// ------------------------------------------------------------------ JSON (JSON.parse / stringify)

/// `JSON.parse`, into values with object keys in document order (a repeated key keeps its first
/// position and its last value; a key differing only in case is dropped, as GitHub's dictionaries
/// drop it).
pub fn json_parse(text: &str) -> Result<Value, String> {
    let mut p = JsonParser { b: text.as_bytes(), i: 0, depth: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("unexpected text at {}", p.i));
    }
    Ok(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
    depth: u32,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn lit(&mut self, word: &str, v: Value) -> Result<Value, String> {
        let end = self.i + word.len();
        if self.b.get(self.i..end) == Some(word.as_bytes()) {
            self.i = end;
            Ok(v)
        } else {
            Err(format!("unexpected token at {}", self.i))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.depth += 1;
        if self.depth > 512 {
            return Err("JSON nested too deeply".to_owned());
        }
        let v = match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(|s| Value::String(s.into())),
            Some(b't') => self.lit("true", Value::Bool(true)),
            Some(b'f') => self.lit("false", Value::Bool(false)),
            Some(b'n') => self.lit("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("unexpected token at {}", self.i)),
        };
        self.depth -= 1;
        v
    }

    fn object(&mut self) -> Result<Value, String> {
        self.i += 1;
        let mut members: Vec<(Arc<str>, Value)> = Vec::new();
        self.ws();
        if self.eat(b'}') {
            return Ok(Value::Object(Arc::new(Map(members))));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(format!("expected a string key at {}", self.i));
            }
            let key = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return Err(format!("expected ':' at {}", self.i));
            }
            self.ws();
            let v = self.value()?;
            if let Some(slot) = members.iter_mut().find(|(k, _)| k.as_ref() == key) {
                slot.1 = v;
            } else if !members.iter().any(|(k, _)| ci_eq(k, &key)) {
                members.push((Arc::from(key.as_str()), v));
            }
            self.ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Ok(Value::Object(Arc::new(Map(members))));
            }
            return Err(format!("expected ',' or '}}' at {}", self.i));
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.eat(b']') {
            return Ok(Value::array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                return Ok(Value::array(items));
            }
            return Err(format!("expected ',' or ']' at {}", self.i));
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.i..self.i + 4).ok_or("truncated \\u escape")?;
        let s = std::str::from_utf8(s).map_err(|_| "bad \\u escape")?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "bad \\u escape")?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            let chunk = self.b.get(start..self.i).unwrap_or_default();
            out.push_str(std::str::from_utf8(chunk).map_err(|_| "invalid UTF-8 in string")?);
            match self.b.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let e = *self.b.get(self.i).ok_or("truncated escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) && self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                                } else {
                                    self.i = save;
                                    0xfffd
                                }
                            } else {
                                hi
                            };
                            out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err(format!("bad escape at {}", self.i)),
                    }
                }
                _ => return Err(format!("unterminated string at {}", self.i)),
            }
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        self.eat(b'-');
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                    self.i += 1;
                }
            }
            _ => return Err(format!("bad number at {start}")),
        }
        if self.eat(b'.') {
            let f = self.i;
            while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
            if self.i == f {
                return Err(format!("bad number at {start}"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            let e = self.i;
            while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
            if self.i == e {
                return Err(format!("bad number at {start}"));
            }
        }
        let text = std::str::from_utf8(self.b.get(start..self.i).unwrap_or_default()).unwrap_or("0");
        Ok(Value::Number(text.parse::<f64>().unwrap_or(f64::NAN)))
    }
}

/// `JSON.stringify(v)` (`indent` 0) or `JSON.stringify(v, null, "  ")` (`indent` 2).
pub fn json_stringify(v: &Value, indent: usize) -> String {
    let mut out = String::new();
    write_json(v, indent, 0, &mut out);
    out
}

fn write_json(v: &Value, indent: usize, level: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_finite() {
                out.push_str(&js_number_to_string(*n));
            } else {
                out.push_str("null");
            }
        }
        Value::String(s) => quote_json(s, out),
        Value::Array(a) | Value::Filtered(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(indent, level + 1, out);
                write_json(item, indent, level + 1, out);
            }
            newline(indent, level, out);
            out.push(']');
        }
        Value::Object(o) => {
            if o.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in o.0.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(indent, level + 1, out);
                quote_json(k, out);
                out.push(':');
                if indent > 0 {
                    out.push(' ');
                }
                write_json(item, indent, level + 1, out);
            }
            newline(indent, level, out);
            out.push('}');
        }
    }
}

fn newline(indent: usize, level: usize, out: &mut String) {
    if indent > 0 {
        out.push('\n');
        out.push_str(&" ".repeat(indent * level));
    }
}

fn quote_json(s: &str, out: &mut String) {
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
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_as_javascript_prints_them() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (1e21, "1e+21"),
            (1.5e-7, "1.5e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.000001, "0.000001"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (-0.0, "0"),
        ];
        for (x, want) in cases {
            assert_eq!(js_number_to_string(*x), *want, "{x}");
        }
        assert_eq!(number_to_display(1.2), "1.2");
        assert_eq!(number_to_display(0.1 + 0.2), "0.3");
        assert_eq!(number_to_display(2.0 / 3.0), "0.666666666666667");
    }

    #[test]
    fn strings_become_numbers_as_javascript_number_does() {
        let cases: &[(&str, f64)] = &[("", 0.0), ("  12  ", 12.0), ("0x1F", 31.0), ("0b101", 5.0), ("1e3", 1000.0), (".5", 0.5), ("5.", 5.0), ("-Infinity", f64::NEG_INFINITY)];
        for (s, want) in cases {
            assert_eq!(js_string_to_number(s), *want, "{s:?}");
        }
        for s in ["abc", "1e", "-0x10", "1_000", "inf", "nan", "0x"] {
            assert!(js_string_to_number(s).is_nan(), "{s:?}");
        }
    }

    #[test]
    fn json_round_trips_in_document_order() -> Result<(), String> {
        let v = json_parse(r#"{"b": 1, "a": [true, null, "x\n"], "B": 2, "b": 3}"#)?;
        assert_eq!(json_stringify(&v, 0), r#"{"b":3,"a":[true,null,"x\n"]}"#);
        assert_eq!(json_stringify(&v, 2), "{\n  \"b\": 3,\n  \"a\": [\n    true,\n    null,\n    \"x\\n\"\n  ]\n}");
        assert!(json_parse("[1,]").is_err());
        assert!(json_parse("01").is_err());
        Ok(())
    }
}
