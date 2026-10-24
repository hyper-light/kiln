//! A log record of one buffer (`docs/observe.md` §8.2, §12.17): its service, host, source, message
//! and attributes side by side in one string, each a place in it. A line a pipeline reads is made,
//! changed by its processors and written as its row with no string of its own, its buffer kept from
//! line to line; what a processor makes ([`Attrs`]) it takes in one copy.

use super::record::{Record, put_fields};
use super::search::Fields;

/// A place in a buffer: where it starts and where it ends.
type Span = (u32, u32);

/// Attributes side by side: their text one string, each pair where its key starts, where its value
/// starts (its key's end) and where it ends. What a pipeline's processor makes (a grok rule's
/// matches, a URL's parts, a user agent's) before a record takes it.
#[derive(Debug, Default, Clone)]
pub struct Attrs {
    text: String,
    pairs: Vec<(u32, u32, u32)>,
}

impl Attrs {
    pub fn clear(&mut self) {
        self.text.clear();
        self.pairs.clear();
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// A pair written in place: its key by `key`, then its value by `value` (false: nothing of
    /// the pair kept). Whether it was kept (a buffer past 4 GiB keeps nothing more).
    pub fn push_with(&mut self, key: impl FnOnce(&mut String), value: impl FnOnce(&mut String) -> bool) -> bool {
        let at = self.text.len();
        key(&mut self.text);
        let key_end = self.text.len();
        if !value(&mut self.text) {
            self.text.truncate(at);
            return false;
        }
        match (u32::try_from(at), u32::try_from(key_end), u32::try_from(self.text.len())) {
            (Ok(a), Ok(k), Ok(e)) => {
                self.pairs.push((a, k, e));
                true
            }
            _ => {
                self.text.truncate(at);
                false
            }
        }
    }

    pub fn push(&mut self, key: &str, value: &str) -> bool {
        self.push_with(
            |t| t.push_str(key),
            |t| {
                t.push_str(value);
                true
            },
        )
    }

    /// `{prefix}.{key}`, its value `value`.
    pub fn push_dotted(&mut self, prefix: &str, key: &str, value: &str) -> bool {
        self.push_with(
            |t| {
                t.push_str(prefix);
                t.push('.');
                t.push_str(key);
            },
            |t| {
                t.push_str(value);
                true
            },
        )
    }

    pub fn get(&self, i: usize) -> Option<(&str, &str)> {
        let &(a, k, e) = self.pairs.get(i)?;
        Some((self.text.get(a as usize..k as usize)?, self.text.get(k as usize..e as usize)?))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        (0..self.pairs.len()).filter_map(|i| self.get(i))
    }
}

/// A log record of one buffer: [`Record`]'s fields, its strings places in `text`. Changed in place
/// (a value set is written at the buffer's end, what it replaced left there until the buffer is
/// cleared for the next line: a line's processors are bounded, so is what they leave).
#[derive(Debug, Default, Clone)]
pub struct RecordBuf {
    /// Nanoseconds since the epoch.
    pub ts: i64,
    pub severity: u8,
    pub trace_id: Option<[u8; 16]>,
    pub span_id: Option<[u8; 8]>,
    text: String,
    /// Its service, host and source.
    names: [Span; 3],
    message: Span,
    /// Each attribute's key and value.
    attrs: Vec<(Span, Span)>,
}

const SERVICE: usize = 0;
const HOST: usize = 1;
const SOURCE: usize = 2;

/// Which of its own fields a key names, as a pipeline's processors name them.
enum Field {
    Message,
    Name(usize),
}

fn field_of(key: &str) -> Option<Field> {
    match key {
        "message" => Some(Field::Message),
        "service" => Some(Field::Name(SERVICE)),
        "host" => Some(Field::Name(HOST)),
        "source" => Some(Field::Name(SOURCE)),
        _ => None,
    }
}

impl RecordBuf {
    /// Empty, its room kept.
    pub fn clear(&mut self) {
        self.ts = 0;
        self.severity = 0;
        self.trace_id = None;
        self.span_id = None;
        self.text.clear();
        self.names = [(0, 0); 3];
        self.message = (0, 0);
        self.attrs.clear();
    }

    /// `s` written at the end of the buffer: its place (none past 4 GiB, nothing written).
    fn put(&mut self, s: &str) -> Option<Span> {
        let a = u32::try_from(self.text.len()).ok()?;
        let e = u32::try_from(self.text.len() + s.len()).ok()?;
        self.text.push_str(s);
        Some((a, e))
    }

    fn at(&self, (a, e): Span) -> &str {
        self.text.get(a as usize..e as usize).unwrap_or("")
    }

    fn name(&self, i: usize) -> &str {
        self.names.get(i).map_or("", |&s| self.at(s))
    }

    pub fn service(&self) -> &str {
        self.name(SERVICE)
    }

    pub fn host(&self) -> &str {
        self.name(HOST)
    }

    pub fn source(&self) -> &str {
        self.name(SOURCE)
    }

    pub fn message(&self) -> &str {
        self.at(self.message)
    }

    fn set_name(&mut self, i: usize, v: &str) {
        if let Some(s) = self.put(v)
            && let Some(slot) = self.names.get_mut(i)
        {
            *slot = s;
        }
    }

    pub fn set_service(&mut self, v: &str) {
        self.set_name(SERVICE, v);
    }

    pub fn set_host(&mut self, v: &str) {
        self.set_name(HOST, v);
    }

    pub fn set_source(&mut self, v: &str) {
        self.set_name(SOURCE, v);
    }

    /// Its message: `v` as it is (its row cuts it, as a record's: [`super::record::MAX_MESSAGE_BYTES`]).
    pub fn set_message(&mut self, v: &str) {
        if let Some(s) = self.put(v) {
            self.message = s;
        }
    }

    pub fn attr_count(&self) -> usize {
        self.attrs.len()
    }

    pub fn attr_at(&self, i: usize) -> Option<(&str, &str)> {
        let &(k, v) = self.attrs.get(i)?;
        Some((self.at(k), self.at(v)))
    }

    pub fn attrs(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.attrs.iter().map(|&(k, v)| (self.at(k), self.at(v)))
    }

    /// The first attribute of the name's value.
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|&&(k, _)| self.at(k) == key).map(|&(_, v)| self.at(v))
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.attrs.iter().position(|&(k, _)| self.at(k) == key)
    }

    pub fn reserve_attrs(&mut self, n: usize) {
        self.attrs.reserve(n);
    }

    /// An attribute added, after the rest (one of its name may be there already).
    pub fn push_attr(&mut self, key: &str, value: &str) {
        let at = self.text.len();
        match (self.put(key), self.put(value)) {
            (Some(k), Some(v)) => self.attrs.push((k, v)),
            _ => self.text.truncate(at),
        }
    }

    /// The first attribute of the name given `value`, or one added.
    pub fn set_attr(&mut self, key: &str, value: &str) {
        match self.position(key) {
            Some(i) => self.set_value_at(i, value),
            None => self.push_attr(key, value),
        }
    }

    /// The `i`th attribute's value: `value`.
    pub fn set_value_at(&mut self, i: usize, value: &str) {
        if i < self.attrs.len()
            && let Some(v) = self.put(value)
            && let Some(slot) = self.attrs.get_mut(i)
        {
            slot.1 = v;
        }
    }

    /// The first attribute of the name taken out, the rest in their order.
    pub fn remove_attr(&mut self, key: &str) {
        if let Some(i) = self.position(key) {
            self.attrs.remove(i);
        }
    }

    /// The attributes `keep` says to keep, in their order.
    pub fn retain_attrs(&mut self, mut keep: impl FnMut(&str, &str) -> bool) {
        let text = &self.text;
        let at = |(a, e): Span| text.get(a as usize..e as usize).unwrap_or("");
        self.attrs.retain(|&(k, v)| keep(at(k), at(v)));
    }

    /// Each of `a` set as [`RecordBuf::set_attr`] sets it, in its order, but a key that names one of
    /// its own fields (`message`, `service`, `host`, `source`, as a pipeline's processors name them)
    /// sets that field: its text taken in one copy (a key it sets already is left there unread).
    pub fn put_attrs(&mut self, a: &Attrs) {
        let Ok(base) = u32::try_from(self.text.len()) else { return };
        if u32::try_from(self.text.len() + a.text.len()).is_err() {
            return;
        }
        self.text.push_str(&a.text);
        self.attrs.reserve(a.pairs.len());
        for &(ka, kb, ve) in &a.pairs {
            let (k, v) = ((base + ka, base + kb), (base + kb, base + ve));
            match field_of(self.at(k)) {
                Some(Field::Message) => self.message = v,
                Some(Field::Name(i)) => {
                    if let Some(slot) = self.names.get_mut(i) {
                        *slot = v;
                    }
                }
                None => self.put_attr_span(k, v),
            }
        }
    }

    /// An attribute whose key and value are in the buffer already: the first of its name given
    /// the value, or it added.
    fn put_attr_span(&mut self, k: Span, v: Span) {
        let key = self.at(k);
        match self.attrs.iter().position(|&(x, _)| self.at(x) == key) {
            Some(i) => {
                if let Some(slot) = self.attrs.get_mut(i) {
                    slot.1 = v;
                }
            }
            None => self.attrs.push((k, v)),
        }
    }

    /// Becomes `r` (its room kept).
    pub fn set_from(&mut self, r: &Record) {
        self.clear();
        self.ts = r.ts;
        self.severity = r.severity;
        self.trace_id = r.trace_id;
        self.span_id = r.span_id;
        self.set_service(&r.service);
        self.set_host(&r.host);
        self.set_source(&r.source);
        self.set_message(&r.message);
        self.attrs.reserve(r.attrs.len());
        for (k, v) in &r.attrs {
            self.push_attr(k, v);
        }
    }

    /// A [`Record`] of it.
    pub fn to_record(&self) -> Record {
        Record {
            ts: self.ts,
            severity: self.severity,
            service: self.service().to_owned(),
            host: self.host().to_owned(),
            source: self.source().to_owned(),
            message: self.message().to_owned(),
            attrs: self.attrs().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            trace_id: self.trace_id,
            span_id: self.span_id,
        }
    }

    /// Appends its row to `out`, as [`super::record::put_row`] appends a record's.
    pub fn put_row(&self, out: &mut Vec<u8>, prev_ts: i64) {
        put_fields(
            out,
            self.ts,
            self.severity,
            [self.service(), self.host(), self.source()],
            self.message(),
            self.attrs(),
            self.attrs.len(),
            self.trace_id,
            self.span_id,
            prev_ts,
            crate::batch::MAX_NAME_BYTES,
        );
    }

    /// Bytes its buffer holds (what it leaves of values replaced among them).
    pub fn text_len(&self) -> usize {
        self.text.len()
    }
}

impl Fields for RecordBuf {
    fn message(&self) -> &str {
        RecordBuf::message(self)
    }
    fn service(&self) -> &str {
        RecordBuf::service(self)
    }
    fn host(&self) -> &str {
        RecordBuf::host(self)
    }
    fn source(&self) -> &str {
        RecordBuf::source(self)
    }
    fn severity(&self) -> u8 {
        self.severity
    }
    fn attr(&self, key: &str) -> Option<&str> {
        RecordBuf::attr(self, key)
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        self.trace_id
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        self.span_id
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::record::MAX_MESSAGE_BYTES;
    use super::*;

    /// What a record does, a record of one buffer does the same: fields set, attributes added, set,
    /// replaced, removed and taken from a processor's, a duplicate among them; read back, and its
    /// row the same bytes.
    #[test]
    fn a_record_of_one_buffer_is_a_record() {
        let mut rng = crate::testkit::Rng::new(7);
        let mut b = RecordBuf::default();
        let mut made = Attrs::default();
        for round in 0..2_000 {
            let mut r = Record { ts: round, severity: (round % 24) as u8, service: "web".into(), host: "h1".into(), source: "nginx".into(), message: format!("line {round}"), ..Record::default() };
            b.set_from(&r);
            let keys = ["a", "b", "c", "http.status_code", "é", ""];
            for step in 0..20 {
                let k = keys[rng.below(keys.len() as u64) as usize];
                let v = format!("v{round}.{step}");
                match rng.below(6) {
                    0 => {
                        r.attrs.push((k.to_owned(), v.clone()));
                        b.push_attr(k, &v);
                    }
                    1 => {
                        match r.attrs.iter().position(|(x, _)| x == k) {
                            Some(i) => r.attrs[i].1 = v.clone(),
                            None => r.attrs.push((k.to_owned(), v.clone())),
                        }
                        b.set_attr(k, &v);
                    }
                    2 => {
                        if let Some(i) = r.attrs.iter().position(|(x, _)| x == k) {
                            r.attrs.remove(i);
                        }
                        b.remove_attr(k);
                    }
                    3 => {
                        made.clear();
                        made.push(k, &v);
                        made.push_dotted(k, "x", "1");
                        made.push(k, "again");
                        made.push("service", &format!("{v}.svc"));
                        made.push("message", "remade");
                        for (mk, mv) in made.iter() {
                            match mk {
                                "service" => r.service = mv.to_owned(),
                                "message" => r.message = mv.to_owned(),
                                _ => match r.attrs.iter().position(|(x, _)| x == mk) {
                                    Some(i) => r.attrs[i].1 = mv.to_owned(),
                                    None => r.attrs.push((mk.to_owned(), mv.to_owned())),
                                },
                            }
                        }
                        b.put_attrs(&made);
                    }
                    4 => {
                        r.service = v.clone();
                        b.set_service(&v);
                        r.message = format!("{v} message");
                        b.set_message(&r.message);
                    }
                    _ => {
                        r.attrs.retain(|(x, _)| x != k);
                        b.retain_attrs(|x, _| x != k);
                    }
                }
                assert_eq!(b.to_record(), r, "round {round} step {step}");
                assert_eq!(b.attr(k), r.attrs.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str()));
            }
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            b.put_row(&mut ours, 5);
            super::super::record::put_row(&mut theirs, &r, 5);
            assert_eq!(ours, theirs, "round {round}'s row");
        }
    }

    /// A message past a record's bound is kept whole and cut in its row where a record's is; a pair
    /// whose value says none is not kept, nor its key's text.
    #[test]
    fn what_a_record_cuts_and_drops_it_cuts_and_drops() {
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let r = Record { message: long.clone(), ..Record::default() };
        let mut b = RecordBuf::default();
        b.set_from(&r);
        assert_eq!(b.message(), long);
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        b.put_row(&mut ours, 0);
        super::super::record::put_row(&mut theirs, &r, 0);
        assert_eq!(ours, theirs);
        assert!(ours.len() < long.len());
        let mut a = Attrs::default();
        assert!(!a.push_with(|t| t.push('k'), |_| false));
        assert!(a.is_empty() && a.text.is_empty());
        assert!(a.push("k", "v"));
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![("k", "v")]);
    }
}
