//! JSON (RFC 8259), read in place: a log line's object read field by field with no tree of it made
//! (`docs/observe.md` §12.17). [`object`] gives an object's fields, each key and value borrowed from
//! the text (a string as it lies between its quotes, decoded only when asked, and only when it holds
//! an escape; a nested array or object as its own text), in their order, a key given twice its last
//! value at its first place, as `serde_json`'s map with `preserve_order` keeps them. What it takes is
//! what `serde_json` takes: every value is checked whole (an escape, a surrogate pair, a number's
//! grammar, no control character in a string), nested at most [`MAX_DEPTH`] deep (its recursion
//! limit), and nothing but whitespace after the object. Nesting is walked with a stack of its own,
//! never by recursion; an object's keys past a few are found by a keyed hash (the process's own,
//! `crate::hash`), so no line can be made whose keys collide.

use hashbrown::HashTable;

/// Containers nested at the most, the object read among them: `serde_json`'s (its recursion limit
/// of 128 refuses a container 128 deep).
pub const MAX_DEPTH: usize = 127;

/// Fields an object holds before its keys are found by their hash rather than looked through.
const LINEAR: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JsonError {
    #[error("not JSON at byte {0}")]
    Syntax(usize),
    #[error("nested past {MAX_DEPTH}")]
    Deep,
}

/// A string as the text holds it, between its quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Str<'a> {
    pub raw: &'a str,
    /// Whether it holds an escape (so [`Str::text`] decodes it).
    pub escaped: bool,
}

impl<'a> Str<'a> {
    /// Its text: itself, or decoded into `buf` when it holds an escape.
    pub fn text<'b>(&self, buf: &'b mut String) -> &'b str
    where
        'a: 'b,
    {
        if !self.escaped {
            return self.raw;
        }
        buf.clear();
        unescape(self.raw, buf);
        buf.as_str()
    }

    /// Its text, owned.
    pub fn owned(&self) -> String {
        let mut s = String::new();
        if self.escaped {
            unescape(self.raw, &mut s);
        } else {
            s.push_str(self.raw);
        }
        s
    }

    /// Whether its text is `s` (decoded if it must be).
    pub fn is(&self, s: &str) -> bool {
        if self.escaped {
            let mut b = String::new();
            unescape(self.raw, &mut b);
            b == s
        } else {
            self.raw == s
        }
    }
}

/// A value: a string, a number as written, a literal, or an array's or an object's whole text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Val<'a> {
    Str(Str<'a>),
    Num(&'a str),
    Bool(bool),
    Null,
    Array(&'a str),
    Object(&'a str),
}

/// Room an [`object`] reading keeps: its containers' stack, and its keys' table.
#[derive(Debug, Default)]
pub struct Scratch {
    stack: Vec<u8>,
    keys: HashTable<u32>,
    hash: crate::hash::Keyed,
    key: String,
    other: String,
}

struct Cursor<'a> {
    text: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, b: text.as_bytes(), i: 0 }
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn bad<T>(&self) -> Result<T, JsonError> {
        Err(JsonError::Syntax(self.i))
    }

    fn eat(&mut self, c: u8) -> Result<(), JsonError> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.bad()
        }
    }

    fn slice(&self, from: usize, to: usize) -> &'a str {
        self.text.get(from..to).unwrap_or("")
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.peek() {
                Some(c @ b'0'..=b'9') => c - b'0',
                Some(c @ b'a'..=b'f') => c - b'a' + 10,
                Some(c @ b'A'..=b'F') => c - b'A' + 10,
                _ => return self.bad(),
            };
            v = v * 16 + u32::from(d);
            self.i += 1;
        }
        Ok(v)
    }

    /// A string at the cursor (its opening quote), checked: its escapes, its surrogate pairs, no
    /// control character.
    fn string(&mut self) -> Result<Str<'a>, JsonError> {
        self.eat(b'"')?;
        let start = self.i;
        let mut escaped = false;
        loop {
            // The run to the next quote, escape or control character, at once.
            let rest = self.b.get(self.i..).unwrap_or_default();
            let n = rest.iter().position(|&c| c == b'"' || c == b'\\' || c < 0x20).unwrap_or(rest.len());
            self.i += n;
            match self.peek() {
                Some(b'"') => {
                    let raw = self.slice(start, self.i);
                    self.i += 1;
                    return Ok(Str { raw, escaped });
                }
                Some(b'\\') => {
                    escaped = true;
                    self.i += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 1,
                        Some(b'u') => {
                            self.i += 1;
                            let u = self.hex4()?;
                            match u {
                                0xD800..=0xDBFF => {
                                    // A high surrogate: its low one must follow.
                                    if self.b.get(self.i..self.i + 2) != Some(b"\\u") {
                                        return self.bad();
                                    }
                                    self.i += 2;
                                    if !(0xDC00..=0xDFFF).contains(&self.hex4()?) {
                                        return self.bad();
                                    }
                                }
                                0xDC00..=0xDFFF => return self.bad(),
                                _ => {}
                            }
                        }
                        _ => return self.bad(),
                    }
                }
                _ => return self.bad(),
            }
        }
    }

    /// A number at the cursor, as JSON's grammar has one: `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`.
    fn number(&mut self) -> Result<&'a str, JsonError> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let digits = |c: &mut Self| {
            let from = c.i;
            while let Some(b'0'..=b'9') = c.peek() {
                c.i += 1;
            }
            c.i - from
        };
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return self.bad(),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return self.bad();
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.i += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if digits(self) == 0 {
                return self.bad();
            }
        }
        Ok(self.slice(start, self.i))
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), JsonError> {
        if self.b.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Ok(())
        } else {
            self.bad()
        }
    }

    /// A key and its colon.
    fn key(&mut self) -> Result<Str<'a>, JsonError> {
        self.ws();
        let k = self.string()?;
        self.ws();
        self.eat(b':')?;
        Ok(k)
    }

    /// A value at the cursor, whole: a scalar read, an array or an object checked to its end (its
    /// nesting on `stack`, `depth` deep already).
    fn value(&mut self, stack: &mut Vec<u8>, depth: usize) -> Result<Val<'a>, JsonError> {
        self.ws();
        let start = self.i;
        match self.peek() {
            Some(b'"') => return Ok(Val::Str(self.string()?)),
            Some(b't') => {
                self.literal(b"true")?;
                return Ok(Val::Bool(true));
            }
            Some(b'f') => {
                self.literal(b"false")?;
                return Ok(Val::Bool(false));
            }
            Some(b'n') => {
                self.literal(b"null")?;
                return Ok(Val::Null);
            }
            Some(b'-' | b'0'..=b'9') => return Ok(Val::Num(self.number()?)),
            Some(b'{' | b'[') => {}
            _ => return self.bad(),
        }
        let object = self.peek() == Some(b'{');
        stack.clear();
        loop {
            // One value: a scalar, or a container opened (and closed at once if empty).
            self.ws();
            match self.peek() {
                Some(c @ (b'{' | b'[')) => {
                    if depth + stack.len() + 1 > MAX_DEPTH {
                        return Err(JsonError::Deep);
                    }
                    self.i += 1;
                    self.ws();
                    let close = if c == b'{' { b'}' } else { b']' };
                    if self.peek() == Some(close) {
                        self.i += 1;
                    } else {
                        stack.push(c);
                        if c == b'{' {
                            self.key()?;
                        }
                        continue;
                    }
                }
                Some(b'"') => {
                    self.string()?;
                }
                Some(b't') => self.literal(b"true")?,
                Some(b'f') => self.literal(b"false")?,
                Some(b'n') => self.literal(b"null")?,
                Some(b'-' | b'0'..=b'9') => {
                    self.number()?;
                }
                _ => return self.bad(),
            }
            // After it: the containers it closes, or the next of its container's.
            loop {
                let Some(&top) = stack.last() else {
                    let raw = self.slice(start, self.i);
                    return Ok(if object { Val::Object(raw) } else { Val::Array(raw) });
                };
                self.ws();
                match (top, self.peek()) {
                    (_, Some(b',')) => {
                        self.i += 1;
                        if top == b'{' {
                            self.key()?;
                        }
                        break;
                    }
                    (b'{', Some(b'}')) | (b'[', Some(b']')) => {
                        self.i += 1;
                        stack.pop();
                    }
                    _ => return self.bad(),
                }
            }
        }
    }
}

/// The fields of the JSON object `text` is (whitespace around it, nothing else) into `out`
/// (cleared): each key and value in their order, a key given twice its last value at its first
/// place; an error for what is not one.
pub fn object<'a>(text: &'a str, out: &mut Vec<(Str<'a>, Val<'a>)>, scratch: &mut Scratch) -> Result<(), JsonError> {
    object_at(text, 1, out, scratch)
}

/// [`object`] of an object nested `depth` deep (1, the outermost).
pub fn object_at<'a>(text: &'a str, depth: usize, out: &mut Vec<(Str<'a>, Val<'a>)>, scratch: &mut Scratch) -> Result<(), JsonError> {
    out.clear();
    scratch.keys.clear();
    if depth > MAX_DEPTH {
        return Err(JsonError::Deep);
    }
    let mut c = Cursor::new(text);
    c.ws();
    c.eat(b'{')?;
    c.ws();
    if c.peek() == Some(b'}') {
        c.i += 1;
    } else {
        loop {
            let key = c.key()?;
            let val = c.value(&mut scratch.stack, depth)?;
            put(out, scratch, key, val);
            c.ws();
            match c.peek() {
                Some(b',') => c.i += 1,
                Some(b'}') => {
                    c.i += 1;
                    break;
                }
                _ => return c.bad(),
            }
        }
    }
    c.ws();
    if c.i != c.b.len() {
        return c.bad();
    }
    Ok(())
}

/// A key's hash: of its text (decoded, which only an escaped key's needs room for).
fn key_hash(h: &crate::hash::Keyed, key: &Str<'_>) -> u64 {
    if key.escaped { h.bytes(key.owned().as_bytes()) } else { h.bytes(key.raw.as_bytes()) }
}

/// A field into `out`: a key there already has its value replaced, in its place.
fn put<'a>(out: &mut Vec<(Str<'a>, Val<'a>)>, scratch: &mut Scratch, key: Str<'a>, val: Val<'a>) {
    let Scratch { keys, hash, key: kb, other, .. } = scratch;
    let k = key.text(kb);
    if out.len() < LINEAR {
        let found = out.iter().position(|(o, _)| if !o.escaped && !key.escaped { o.raw == key.raw } else { o.text(other) == k });
        match found.and_then(|i| out.get_mut(i)) {
            Some(slot) => slot.1 = val,
            None => out.push((key, val)),
        }
        return;
    }
    let rehash = |out: &Vec<(Str<'a>, Val<'a>)>, j: u32| out.get(j as usize).map_or(0, |(o, _)| key_hash(hash, o));
    if keys.is_empty() {
        // Past the linear ones: the table made of every key so far.
        for (i, (o, _)) in out.iter().enumerate() {
            keys.insert_unique(key_hash(hash, o), u32::try_from(i).unwrap_or(u32::MAX), |&j| rehash(out, j));
        }
    }
    let h = key_hash(hash, &key);
    let found = keys.find(h, |&i| out.get(i as usize).is_some_and(|(o, _)| o.text(other) == k)).copied();
    match found.and_then(|i| out.get_mut(i as usize)) {
        Some(slot) => slot.1 = val,
        None => {
            let i = u32::try_from(out.len()).unwrap_or(u32::MAX);
            out.push((key, val));
            keys.insert_unique(h, i, |&j| rehash(out, j));
        }
    }
}

/// A checked string's escapes decoded into `out`.
fn unescape(raw: &str, out: &mut String) {
    let mut chars = raw.chars();
    let hex = |chars: &mut std::str::Chars<'_>| -> u32 { (0..4).filter_map(|_| chars.next().and_then(|c| c.to_digit(16))).fold(0, |v, d| v * 16 + d) };
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let u = hex(&mut chars);
                let c = if (0xD800..=0xDBFF).contains(&u) {
                    chars.next();
                    chars.next();
                    let low = hex(&mut chars);
                    char::from_u32(0x10000 + ((u - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF))
                } else {
                    char::from_u32(u)
                };
                out.push(c.unwrap_or('\u{FFFD}'));
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
}

/// A checked value's text without the whitespace between its tokens (a string's own kept), into
/// `out`: an array's or an object's as compact JSON.
pub fn compact(raw: &str, out: &mut String) {
    let mut in_string = false;
    let mut escape = false;
    for c in raw.chars() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            ' ' | '\t' | '\n' | '\r' => {}
            '"' => {
                in_string = true;
                out.push(c);
            }
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    fn fields(text: &str) -> Result<Vec<(String, String)>, JsonError> {
        let (mut out, mut s) = (Vec::new(), Scratch::default());
        object(text, &mut out, &mut s)?;
        Ok(out
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Val::Str(s) => format!("s:{}", s.owned()),
                    Val::Num(n) => format!("n:{n}"),
                    Val::Bool(b) => format!("b:{b}"),
                    Val::Null => "null".into(),
                    Val::Array(a) => format!("a:{a}"),
                    Val::Object(o) => format!("o:{o}"),
                };
                (k.owned(), v)
            })
            .collect())
    }

    #[test]
    fn an_object_s_fields_come_in_order_borrowed_and_checked() {
        let f = fields(r#" {"a":"x","b":12.5e-3,"c":true,"d":null,"e":[1, {"z":2}],"f":{"g":"h"}, "\u0067":"\ud83d\ude00\n"} "#).unwrap();
        assert_eq!(
            f,
            vec![
                ("a".into(), "s:x".into()),
                ("b".into(), "n:12.5e-3".into()),
                ("c".into(), "b:true".into()),
                ("d".into(), "null".into()),
                ("e".into(), "a:[1, {\"z\":2}]".into()),
                ("f".into(), "o:{\"g\":\"h\"}".into()),
                ("g".into(), "s:😀\n".into()),
            ]
        );
        // A key twice: its last value, at its first place, its escaped spelling the same key.
        assert_eq!(fields(r#"{"a":1,"b":2,"\u0061":3}"#).unwrap(), vec![("a".into(), "n:3".into()), ("b".into(), "n:2".into())]);
        assert_eq!(fields("{}").unwrap(), vec![]);
        for bad in [
            "",
            "[]",
            "{",
            "{\"a\"}",
            "{\"a\":}",
            "{\"a\":1,}",
            "{\"a\":01}",
            "{\"a\":1.}",
            "{\"a\":-}",
            "{\"a\":.5}",
            "{\"a\":1e}",
            "{\"a\":\"\\x\"}",
            "{\"a\":\"\\ud800\"}",
            "{\"a\":\"\\udc00\"}",
            "{\"a\":\"\u{1}\"}",
            "{\"a\":tru}",
            "{\"a\":[1,]}",
            "{\"a\":[1 2]}",
            "{\"a\":{\"b\" 1}}",
            "{\"a\":1} x",
            "{\"a\":1}}",
            "{'a':1}",
        ] {
            assert!(object(bad, &mut Vec::new(), &mut Scratch::default()).is_err(), "{bad:?}");
        }
        // Nesting at the limit and past it.
        let deep = |n: usize| format!("{{\"a\":{}{}}}", "[".repeat(n), "]".repeat(n));
        assert!(object(&deep(MAX_DEPTH - 1), &mut Vec::new(), &mut Scratch::default()).is_ok());
        assert_eq!(object(&deep(MAX_DEPTH), &mut Vec::new(), &mut Scratch::default()), Err(JsonError::Deep));
        assert_eq!(object(&deep(100_000), &mut Vec::new(), &mut Scratch::default()), Err(JsonError::Deep));
    }

    #[test]
    fn many_keys_are_found_by_their_hash_twice_or_not() {
        let mut text = String::from("{");
        for i in 0..5_000 {
            text.push_str(&format!("\"k{i}\":{i},"));
        }
        text.push_str("\"k17\":\"again\",\"k4999\":\"last\"}");
        let f = fields(&text).unwrap();
        assert_eq!(f.len(), 5_000);
        assert_eq!(f.get(17), Some(&("k17".into(), "s:again".into())));
        assert_eq!(f.last(), Some(&("k4999".into(), "s:last".into())));
    }

    /// A random JSON value (`depth` deep at the most): strings with every kind of escape, numbers
    /// of every form, nesting, duplicates.
    fn value(rng: &mut Rng, depth: u32, out: &mut String) {
        let pick = if depth == 0 { rng.below(5) } else { rng.below(7) };
        match pick {
            0 => string(rng, out),
            1 => out.push_str(["0", "-0", "12", "-7.25", "1e3", "2.5E-4", "123456789012345678901234", "1.0", "0.1"][rng.below(9) as usize]),
            2 => out.push_str(["true", "false"][rng.below(2) as usize]),
            3 => out.push_str("null"),
            4 => string(rng, out),
            5 => {
                out.push('[');
                for i in 0..rng.below(4) {
                    if i > 0 {
                        out.push_str([",", " , ", ",\n"][rng.below(3) as usize]);
                    }
                    value(rng, depth - 1, out);
                }
                out.push(']');
            }
            _ => {
                out.push('{');
                for i in 0..rng.below(5) {
                    if i > 0 {
                        out.push(',');
                    }
                    if rng.below(4) == 0 {
                        out.push_str("\"dup\"");
                    } else {
                        string(rng, out);
                    }
                    out.push_str([":", " : "][rng.below(2) as usize]);
                    value(rng, depth - 1, out);
                }
                out.push('}');
            }
        }
    }

    fn string(rng: &mut Rng, out: &mut String) {
        out.push('"');
        for _ in 0..rng.below(8) {
            out.push_str(["a", "é", "😀", "\\n", "\\\"", "\\\\", "\\/", "\\u00e9", "\\ud83d\\ude00", "\\t", " ", "k"][rng.below(12) as usize]);
        }
        out.push('"');
    }

    /// The same as `serde_json` reads it: what it takes is taken, and the fields alike.
    #[test]
    fn it_reads_what_serde_json_reads_and_refuses_what_it_refuses() {
        let mut rng = Rng::new(1);
        let mut s = Scratch::default();
        let mut buf = String::new();
        // Whether this build's serde_json keeps an object's keys in order (`preserve_order`, as
        // kiln's has it), or sorts them.
        let ordered = matches!(serde_json::from_str::<serde_json::Value>(r#"{"b":1,"a":2}"#), Ok(serde_json::Value::Object(o)) if o.keys().next().map(String::as_str) == Some("b"));
        for round in 0..20_000 {
            let mut text = String::new();
            value(&mut rng, 4, &mut text);
            let text = if text.starts_with('{') { text } else { format!("{{\"v\":{text}}}") };
            // Now and then a byte broken: both refuse it, or both take it alike.
            let text = if round % 5 == 0 && !text.is_empty() {
                let mut b = text.into_bytes();
                let at = rng.below(b.len() as u64) as usize;
                b[at] = b"{}[]\":,1a\\ "[rng.below(11) as usize];
                match String::from_utf8(b) {
                    Ok(t) => t,
                    Err(_) => continue,
                }
            } else {
                text
            };
            let theirs = serde_json::from_str::<serde_json::Value>(&text);
            let mut out = Vec::new();
            let mine = object(&text, &mut out, &mut s);
            match (&theirs, &mine) {
                (Ok(serde_json::Value::Object(o)), Ok(())) => {
                    assert_eq!(o.len(), out.len(), "{text}");
                    let mine: Vec<(String, serde_json::Value)> = out
                        .iter()
                        .map(|(mk, mv)| {
                            let v = match mv {
                                Val::Str(x) => serde_json::Value::String(x.owned()),
                                Val::Num(n) | Val::Array(n) | Val::Object(n) => serde_json::from_str(n).unwrap(),
                                Val::Bool(b) => serde_json::Value::Bool(*b),
                                Val::Null => serde_json::Value::Null,
                            };
                            (mk.text(&mut buf).to_owned(), v)
                        })
                        .collect();
                    let mut theirs: Vec<(String, serde_json::Value)> = o.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    let mut mine = mine;
                    if !ordered {
                        mine.sort_by(|a, b| a.0.cmp(&b.0));
                        theirs.sort_by(|a, b| a.0.cmp(&b.0));
                    }
                    assert_eq!(theirs, mine, "{text}");
                }
                (Ok(_), Ok(())) => panic!("an object read where serde_json reads another value: {text}"),
                (Err(_), Err(_)) | (Ok(_), Err(_)) if !matches!(theirs, Ok(serde_json::Value::Object(_))) => {}
                _ => panic!("{text}: serde_json {:?}, here {mine:?}", theirs.map(|_| ())),
            }
        }
    }

    #[test]
    fn compact_takes_out_the_spaces_between_tokens_and_keeps_strings_whole() {
        let mut out = String::new();
        compact("[ 1 , {\"a b\" : \"c \\\" d\"} ,\n true ]", &mut out);
        assert_eq!(out, "[1,{\"a b\":\"c \\\" d\"},true]");
    }
}
