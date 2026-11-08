//! A session's state as its copy keeps it (goal 9, plan 01a1a87b2cec step 5): each top-level field's JSON, written
//! once from the state its machine sent, rather than a `serde_json::Value` tree.
//!
//! In rt7's calm profile every state an agent's machine sent was made into a tree on its copy's shard (0.45% of the
//! process), compared field by field with the last one its viewers were sent for a patch (0.19%), and the last tree
//! let go (0.25%): 6% of the shards' user time, inside `Registry::apply`. Kept as text, a state is written once (as
//! the tree was written out for its frames anyway), its fields found in that text, compared as bytes, written out as
//! they are, and let go as one buffer. What reads a field (its epoch, its queue) reads that field's text, and what
//! wants the whole as a value (`agents.get`) reads it from the text.

use std::io::{self, Write};
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A session's state as its copy keeps it: none yet (JSON's `null`), an object's fields in order, each its key and its
/// value's JSON (serde_json's compact text, as a value's tree wrote it), or another value's JSON as it is (never a
/// machine's state; what a store might hold).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StateDoc(Kept);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Kept {
    #[default]
    Null,
    Object(Vec<(Arc<str>, Bytes)>),
    Other(Bytes),
}

/// A shared null state: a copy's before it has one (as `super::copies::Catalog::none` is its meta's).
pub fn none() -> Arc<StateDoc> {
    static NONE: std::sync::LazyLock<Arc<StateDoc>> = std::sync::LazyLock::new(|| Arc::new(StateDoc::default()));
    Arc::clone(&NONE)
}

impl StateDoc {
    /// `state` as its JSON: written once (serde_json, compact), its top-level fields found in that text and kept as
    /// slices of it; their keys shared with `was`'s where they're the same (a state's are its type's fields). What
    /// can't be written is `null`, as a tree made of it was.
    pub fn made<T: Serialize + ?Sized>(state: &T, was: &StateDoc) -> Self {
        let mut text = Vec::with_capacity(was.room());
        if serde_json::to_writer(&mut text, state).is_err() {
            return Self::default();
        }
        let text = Bytes::from(text);
        let Some(spans) = fields_of(&text) else {
            return Self(if text.as_ref() == b"null" { Kept::Null } else { Kept::Other(text) });
        };
        let olds = match &was.0 {
            Kept::Object(fields) => fields.as_slice(),
            _ => &[],
        };
        // Both in their type's order: each key looked for past the last one found, shared where `was` has it.
        let mut from = 0;
        let fields = spans
            .into_iter()
            .map(|(key, value)| {
                let at = olds.get(from..).and_then(|rest| rest.iter().position(|(k, _)| k.as_bytes() == key)).map(|p| from + p);
                let key = match at.and_then(|i| olds.get(i).map(|(k, _)| (i, k))) {
                    Some((i, k)) => {
                        from = i + 1;
                        Arc::clone(k)
                    }
                    None => key_of(key),
                };
                (key, text.slice(value))
            })
            .collect();
        Self(Kept::Object(fields))
    }

    /// `v` as a state is kept (a state read back from the store, a test's).
    pub fn of_value(v: &Value) -> Self {
        Self::made(v, &Self::default())
    }

    pub fn is_null(&self) -> bool {
        matches!(self.0, Kept::Null)
    }

    pub fn is_object(&self) -> bool {
        matches!(self.0, Kept::Object(_))
    }

    /// Field `key`'s JSON, if it's an object with one.
    pub fn field(&self, key: &str) -> Option<&[u8]> {
        match &self.0 {
            Kept::Object(fields) => fields.iter().find(|(k, _)| &**k == key).map(|(_, v)| v.as_ref()),
            _ => None,
        }
    }

    /// Field `key` read as a `T` (its epoch, its queue), if it has it and it reads as one.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_slice(self.field(key)?).ok()
    }

    /// Field `key` set to `value`'s JSON, where it's an object: in its place where it has it, else after the rest (as a
    /// JSON object's map takes a key).
    pub fn set(&mut self, key: &str, value: &Value) {
        let Kept::Object(fields) = &mut self.0 else { return };
        let text = Bytes::from(serde_json::to_vec(value).unwrap_or_else(|_| b"null".to_vec()));
        match fields.iter_mut().find(|(k, _)| &**k == key) {
            Some((_, v)) => *v = text,
            None => fields.push((Arc::from(key), text)),
        }
    }

    /// Its JSON, as serde_json writes the value it is: `null`, an object of its fields, or the other value's text.
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        match &self.0 {
            Kept::Null => w.write_all(b"null"),
            Kept::Other(text) => w.write_all(text),
            Kept::Object(fields) => {
                w.write_all(b"{")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 {
                        w.write_all(b",")?;
                    }
                    serde_json::to_writer(&mut *w, &**k)?;
                    w.write_all(b":")?;
                    w.write_all(v)?;
                }
                w.write_all(b"}")
            }
        }
    }

    /// The bytes [`StateDoc::write`] writes, about (a key's escapes aside): the room its frame is written into.
    pub fn room(&self) -> usize {
        match &self.0 {
            Kept::Null => "null".len(),
            Kept::Other(text) => text.len(),
            Kept::Object(fields) => "{}".len() + fields.iter().map(|(k, v)| k.len() + r#""":,"#.len() + v.len()).sum::<usize>(),
        }
    }

    /// The whole as a JSON value, read from its text: a reader's own (`agents.get`'s state).
    pub fn to_value(&self) -> Value {
        self.read().unwrap_or(Value::Null)
    }

    /// The whole read as a `T` (the typed state, for checks before acting on it).
    pub fn read<T: DeserializeOwned>(&self) -> Option<T> {
        let mut text = Vec::with_capacity(self.room());
        self.write(&mut text).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// What changed in it since `was`, for `{"t":"state_patch","set":{..},"gone":[..]}`: its fields whose JSON differs
    /// from `was`'s or that `was` hasn't, in its order, and `was`'s fields it no longer has, in theirs. None when
    /// either isn't an object (nothing to patch against: the state goes whole).
    #[allow(clippy::type_complexity)]
    pub fn patch<'a>(&'a self, was: &'a StateDoc) -> Option<(Vec<(&'a str, &'a [u8])>, Vec<&'a str>)> {
        let (Kept::Object(now), Kept::Object(olds)) = (&self.0, &was.0) else { return None };
        let mut set = Vec::new();
        // Both in the order their state wrote them: each looked for from past the last one found.
        let (mut from, mut found) = (0, 0);
        for (k, v) in now {
            let at = olds.get(from..).and_then(|rest| rest.iter().position(|(o, _)| o == k)).map(|p| from + p).or_else(|| olds.iter().position(|(o, _)| o == k));
            match at.and_then(|i| olds.get(i).map(|(_, o)| (i, o))) {
                Some((i, o)) => {
                    from = i + 1;
                    found += 1;
                    if o != v {
                        set.push((&**k, v.as_ref()));
                    }
                }
                None => set.push((&**k, v.as_ref())),
            }
        }
        let gone = if found == olds.len() { Vec::new() } else { olds.iter().filter(|(o, _)| !now.iter().any(|(k, _)| k == o)).map(|(o, _)| &**o).collect() };
        Some((set, gone))
    }
}

/// A key as its JSON string's bytes have it (no quotes): as it is where nothing in it is escaped, else read.
fn key_of(raw: &[u8]) -> Arc<str> {
    if !raw.contains(&b'\\')
        && let Ok(s) = std::str::from_utf8(raw)
    {
        return Arc::from(s);
    }
    let mut quoted = Vec::with_capacity(raw.len() + 2);
    quoted.push(b'"');
    quoted.extend_from_slice(raw);
    quoted.push(b'"');
    Arc::from(serde_json::from_slice::<String>(&quoted).unwrap_or_default())
}

/// The top-level fields of `text`, serde_json's compact JSON of one value, when it's an object: each one's key (its
/// string's bytes, no quotes) and where its value's JSON lies. None for any other value, or text that isn't so.
fn fields_of(text: &[u8]) -> Option<Vec<(&[u8], Range<usize>)>> {
    if text.first() != Some(&b'{') {
        return None;
    }
    let mut fields = Vec::new();
    if text.get(1) == Some(&b'}') {
        return (text.len() == 2).then_some(fields);
    }
    let mut i = 1;
    loop {
        let key_end = string_end(text, i)?;
        let key = text.get(i + 1..key_end - 1)?;
        if text.get(key_end) != Some(&b':') {
            return None;
        }
        let start = key_end + 1;
        let end = value_end(text, start)?;
        fields.push((key, start..end));
        match text.get(end) {
            Some(b',') => i = end + 1,
            Some(b'}') if end + 1 == text.len() => return Some(fields),
            _ => return None,
        }
    }
}

/// Past the string whose opening quote is at `i`: the index after its closing quote.
fn string_end(text: &[u8], i: usize) -> Option<usize> {
    if text.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    loop {
        match text.get(j)? {
            b'"' => return Some(j + 1),
            b'\\' => j += 2,
            _ => j += 1,
        }
    }
}

/// Where the value starting at `i` ends: past a string's closing quote, past an object's or an array's closing bracket
/// (the strings, objects and arrays within it passed over whole), or at the `,` or `}` after a number, `true`, `false`
/// or `null`.
fn value_end(text: &[u8], i: usize) -> Option<usize> {
    match text.get(i)? {
        b'"' => string_end(text, i),
        b'{' | b'[' => {
            let mut depth = 0_usize;
            let mut j = i;
            loop {
                match text.get(j)? {
                    b'"' => {
                        j = string_end(text, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let len = text.get(i..)?.iter().position(|b| matches!(b, b',' | b'}' | b']'))?;
            (len > 0).then_some(i + len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Goal 9, plan 01a1a87b2cec step 5: a state kept by its fields writes out as its value's tree did, byte for byte
    /// (escapes, nesting, a key with a quote in it, every kind of value), reads back as the value, and finds and sets a
    /// field as the tree's map does; null and a value not an object as they were.
    #[test]
    fn g9_s5_a_state_kept_by_its_fields_writes_and_reads_as_its_tree() -> anyhow::Result<()> {
        let odd = json!({"id": "e\"1\u{2028}", "seq": 3, "text": "line\nnext } ] \\ ☃ ,:", "n": [1, 2.5, -3, null, true, false], "o": {"z": 1, "a": {"b": [], "c": {}}}, "new\"": [5], "e": "", "s": "\\"});
        for v in [odd.clone(), json!({}), json!({"a": 1}), Value::Null, json!([1, {"a": 2}]), json!("x"), json!(7)] {
            let doc = StateDoc::of_value(&v);
            let mut written = Vec::new();
            doc.write(&mut written)?;
            anyhow::ensure!(written == serde_json::to_vec(&v)?, "{v}: written {}", String::from_utf8_lossy(&written));
            anyhow::ensure!(doc.to_value() == v && doc.is_null() == v.is_null() && doc.is_object() == v.is_object(), "{v}: read back {}", doc.to_value());
            anyhow::ensure!(doc.room() >= written.len().saturating_sub(4), "{v}: room {} for {}", doc.room(), written.len());
        }
        let mut doc = StateDoc::of_value(&odd);
        anyhow::ensure!(doc.get::<u64>("seq") == Some(3) && doc.get::<String>("new\"").is_none() && doc.get::<Vec<u64>>("new\"") == Some(vec![5]) && doc.field("missing").is_none());
        let mut tree = odd.clone();
        for (k, v) in [("seq", json!(4)), ("added", json!({"x": "y"})), ("id", json!(null))] {
            doc.set(k, &v);
            if let Some(o) = tree.as_object_mut() {
                o.insert(k.into(), v);
            }
        }
        let mut written = Vec::new();
        doc.write(&mut written)?;
        anyhow::ensure!(written == serde_json::to_vec(&tree)? && doc.to_value() == tree, "set as the tree's map sets: {}", String::from_utf8_lossy(&written));
        let mut null = StateDoc::default();
        null.set("a", &json!(1));
        anyhow::ensure!(null.is_null(), "a null state takes no field");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a state's patch against the last one sent is what the trees' was: the fields
    /// that differ or are new, in its order, and those gone, in the last one's; none where either isn't an object. A
    /// typed state kept again shares its keys with the last one.
    #[test]
    fn g9_s5_a_states_patch_is_the_fields_that_changed() -> anyhow::Result<()> {
        let (was, now) = (json!({"a": 1, "b": {"c": 2}, "gone": 3, "same": [1, 2]}), json!({"new\"": [5], "a": 1, "b": {"c": 4}, "same": [1, 2]}));
        let (w, n) = (StateDoc::of_value(&was), StateDoc::of_value(&now));
        let (set, gone) = n.patch(&w).ok_or_else(|| anyhow::anyhow!("no patch"))?;
        let set: Vec<(&str, Value)> = set.into_iter().map(|(k, v)| Ok::<_, serde_json::Error>((k, serde_json::from_slice(v)?))).collect::<Result<_, _>>()?;
        anyhow::ensure!(set == vec![("new\"", json!([5])), ("b", json!({"c": 4}))] && gone == vec!["gone"], "{set:?} {gone:?}");
        let (set, gone) = n.patch(&n).ok_or_else(|| anyhow::anyhow!("no patch"))?;
        anyhow::ensure!(set.is_empty() && gone.is_empty(), "nothing against itself");
        anyhow::ensure!(n.patch(&StateDoc::default()).is_none() && StateDoc::default().patch(&n).is_none() && StateDoc::of_value(&json!([1])).patch(&n).is_none());
        let state = crate::harness::model::State { summary: "it reads its deadline".into(), ..crate::harness::model::State::default() };
        let first = StateDoc::made(&state, &StateDoc::default());
        let second = StateDoc::made(&crate::harness::model::State { doing: Some("Running cargo test".into()), ..state }, &first);
        let (Kept::Object(a), Kept::Object(b)) = (&first.0, &second.0) else { anyhow::bail!("a state is an object") };
        anyhow::ensure!(b.iter().all(|(k, _)| a.iter().any(|(o, _)| Arc::ptr_eq(o, k)) || &**k == "doing"), "its keys shared with the last one's");
        let (set, gone) = second.patch(&first).ok_or_else(|| anyhow::anyhow!("no patch"))?;
        anyhow::ensure!(set.iter().map(|(k, _)| *k).collect::<Vec<_>>() == vec!["doing"] && gone.is_empty(), "{set:?} {gone:?}");
        Ok(())
    }
}
