//! Bounded control views and demand reads of runner-owned state. Deferred fields are references,
//! never empty authoritative collections; reading a page never assembles the state on its receiver.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};

use serde::ser::{self, Impossible, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple, SerializeTupleStruct, SerializeTupleVariant};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest as _;

use super::model::State;

/// The existing agent control frame limit; callers reserve their actual outer envelope from it.
pub const FRAME: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateKind"))]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Array,
    Object,
    Text,
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateDescriptor"))]
pub struct Descriptor {
    pub kind: Kind,
    /// Array/object entries, UTF-8 bytes for text, or one scalar value.
    pub len: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateCounts"))]
pub struct Counts {
    pub queue: usize,
    pub approvals: usize,
    pub tasks: usize,
    pub tasks_running: usize,
    pub todos: usize,
    pub todos_done: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateMetadata"))]
pub struct Metadata {
    pub revision: String,
    #[serde(default)]
    pub revisions: BTreeMap<String, String>,
    #[serde(default)]
    pub deferred: BTreeMap<String, Descriptor>,
    #[serde(default)]
    pub counts: Counts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_index: Option<QueueIndex>,
}

#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub state: Value,
    pub revision: String,
    pub revisions: BTreeMap<String, String>,
    pub deferred: BTreeMap<String, Descriptor>,
    pub counts: Counts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_index: Option<QueueIndex>,
    #[serde(skip)]
    budget: usize,
}

#[derive(Serialize)]
struct MetadataRef<'a> {
    revision: &'a str,
    revisions: &'a BTreeMap<String, String>,
    deferred: &'a BTreeMap<String, Descriptor>,
    counts: &'a Counts,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_index: Option<&'a QueueIndex>,
}

struct WireState<'a>(&'a View);
impl Serialize for WireState<'_> {
    fn serialize<S: ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fields = self.0.state.as_object().ok_or_else(|| ser::Error::custom("state is not an object"))?;
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in fields {
            if key != "view" {
                map.serialize_entry(key, value)?;
            }
        }
        map.serialize_entry(
            "view",
            &MetadataRef {
                revision: &self.0.revision,
                revisions: &self.0.revisions,
                deferred: &self.0.deferred,
                counts: &self.0.counts,
                queue_index: self.0.queue_index.as_ref(),
            },
        )?;
        map.end()
    }
}
fn bounded_view(view: &View, budget: usize) -> Result<Vec<u8>, Error> {
    encoded(&WireState(view), budget)
}

impl View {
    /// Moves the bounded projection into the existing State wire shape without another state copy.
    pub fn into_state(self) -> Result<Value, Error> {
        let mut state = self.state;
        let metadata = Metadata {
            revision: self.revision,
            revisions: self.revisions,
            deferred: self.deferred,
            counts: self.counts,
            queue_index: self.queue_index,
        };
        state.as_object_mut().ok_or(Error::Unreadable)?.insert("view".to_owned(), value(&metadata, self.budget)?);
        encoded(&state, self.budget)?;
        Ok(state)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateQueueEntry"))]
pub struct QueueEntry {
    pub id: String,
    pub digest: String,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateQueueIndex"))]
pub struct QueueIndex {
    pub entries: Vec<QueueEntry>,
    /// True only when this index includes every source entry, not merely a final suffix.
    pub complete: bool,
    pub next: Option<usize>,
}

#[derive(Serialize)]
struct QueueEntryRef<'a> {
    id: &'a str,
    digest: String,
    len: usize,
}
impl<'a> QueueEntryRef<'a> {
    fn of(q: &'a super::model::QueueItem) -> Self {
        Self {
            id: &q.id,
            digest: format!("{:016x}", super::queued::digest(&q.text)),
            len: q.text.len(),
        }
    }
}

pub fn queue_index(queue: &[super::model::QueueItem], start: usize, budget: usize) -> Result<QueueIndex, Error> {
    let remaining = queue.get(start..).ok_or(Error::InvalidCursor)?;
    let mut result = QueueIndex {
        entries: Vec::new(),
        complete: start == 0 && remaining.is_empty(),
        next: (!remaining.is_empty()).then_some(start),
    };
    let mut entries_bytes = 0usize;
    for (relative, item) in remaining.iter().enumerate() {
        let index = start.checked_add(relative).ok_or(Error::InvalidCursor)?;
        let entry = match encoded(&QueueEntryRef::of(item), budget) {
            Ok(entry) => entry,
            Err(Error::Budget) if !result.entries.is_empty() => break,
            Err(e) => return Err(e),
        };
        let end = index.checked_add(1).ok_or(Error::InvalidCursor)?;
        let next = (end < queue.len()).then_some(end);
        let complete = start == 0 && next.is_none();
        let wrapper = QueueIndex { entries: Vec::new(), complete, next };
        let candidate_bytes = entries_bytes
            .checked_add(entry.len())
            .and_then(|n| n.checked_add(usize::from(!result.entries.is_empty())))
            .ok_or(Error::Budget)?;
        // Compact JSON replaces the empty array's interior with these exact entry bytes and
        // separators. Only the constant-size wrapper is remeasured when its cursor changes.
        let total = encoded(&wrapper, budget)?.len().checked_add(candidate_bytes).ok_or(Error::Budget)?;
        if total > budget.min(FRAME) {
            if result.entries.is_empty() {
                return Err(Error::Budget);
            }
            break;
        }
        result.entries.push(serde_json::from_slice(&entry).map_err(|_| Error::Unreadable)?);
        entries_bytes = candidate_bytes;
        result.next = next;
        result.complete = complete;
    }
    encoded(&result, budget)?;
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateStep"))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    Index { index: usize },
    Key { key: String },
    Id { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateRead"))]
pub struct Read {
    pub revision: String,
    pub field: String,
    #[serde(default)]
    pub path: Vec<Step>,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateEntry"))]
pub struct Entry {
    pub key: Step,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateReply"))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Page { revision: String, entries: Vec<Entry>, next: Option<usize> },
    Text { revision: String, text: String, next: Option<usize> },
    Value { revision: String, value: Value },
    Node { revision: String, node: Descriptor },
}

#[derive(Serialize)]
struct PageRef<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    revision: &'a str,
    entries: &'a [Entry],
    next: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "StateError"))]
#[serde(rename_all = "snake_case")]
pub enum Error {
    Budget,
    Stale,
    InvalidPath,
    InvalidCursor,
    Unreadable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Budget => "the reply does not fit its byte budget",
            Self::Stale => "the state changed; read its current revision",
            Self::InvalidPath => "no state value at this path",
            Self::InvalidCursor => "the cursor is not at a value boundary",
            Self::Unreadable => "the state could not be serialized",
        })
    }
}
impl std::error::Error for Error {}
impl ser::Error for Error {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self::Unreadable
    }
}

/// A writer whose allocation is never derived from an advertised length or a whole state.
struct Bounded {
    bytes: Vec<u8>,
    budget: usize,
}
impl Bounded {
    fn target(len: usize, capacity: usize, incoming: usize, budget: usize) -> Option<usize> {
        let next = len.checked_add(incoming).filter(|next| *next <= budget)?;
        if capacity > budget {
            return None;
        }
        // Standard amortized doubling, clamped before requesting memory: reserve_exact must not
        // choose its own geometric growth beyond the control message's byte budget.
        Some(if next <= capacity {
            capacity
        } else {
            capacity.checked_mul(2).unwrap_or(budget).max(next).min(budget)
        })
    }
}
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let target = Self::target(self.bytes.len(), self.bytes.capacity(), bytes.len(), self.budget).ok_or_else(|| io::Error::other("state view byte budget"))?;
        if target > self.bytes.capacity() {
            let additional = target.checked_sub(self.bytes.len()).ok_or_else(|| io::Error::other("state view byte budget"))?;
            self.bytes.try_reserve_exact(additional).map_err(io::Error::other)?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Exact borrowed serialization for the ordinary control path, before any view or revision work.
pub(crate) fn encode<T: Serialize + ?Sized>(value: &T, budget: usize) -> Result<Vec<u8>, Error> {
    let mut writer = Bounded {
        bytes: Vec::new(),
        budget: budget.min(FRAME),
    };
    serde_json::to_writer(&mut writer, value).map_err(|e| if e.is_io() { Error::Budget } else { Error::Unreadable })?;
    Ok(writer.bytes)
}

fn encoded<T: Serialize + ?Sized>(value: &T, budget: usize) -> Result<Vec<u8>, Error> {
    encode(value, budget)
}

fn value<T: Serialize + ?Sized>(v: &T, budget: usize) -> Result<Value, Error> {
    serde_json::from_slice(&encoded(v, budget)?).map_err(|_| Error::Unreadable)
}

trait FieldSink {
    fn field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error>;
}

/// Intercepts the derived top-level struct, including future fields, without cloning the state.
struct Fields<S>(S);
macro_rules! not_fields {
    ($($method:ident($($arg:ident:$ty:ty),*));* $(;)?) => { $(
        fn $method(self, $($arg:$ty),*) -> Result<Self::Ok, Self::Error> { $(let _ = $arg;)* Err(Error::Unreadable) }
    )* };
}
impl<S: FieldSink> ser::Serializer for Fields<S> {
    type Ok = S;
    type Error = Error;
    type SerializeSeq = Impossible<S, Error>;
    type SerializeTuple = Impossible<S, Error>;
    type SerializeTupleStruct = Impossible<S, Error>;
    type SerializeTupleVariant = Impossible<S, Error>;
    type SerializeMap = Impossible<S, Error>;
    type SerializeStruct = Self;
    type SerializeStructVariant = Impossible<S, Error>;
    not_fields! {
        serialize_bool(v:bool); serialize_i8(v:i8); serialize_i16(v:i16); serialize_i32(v:i32); serialize_i64(v:i64); serialize_i128(v:i128);
        serialize_u8(v:u8); serialize_u16(v:u16); serialize_u32(v:u32); serialize_u64(v:u64); serialize_u128(v:u128);
        serialize_f32(v:f32); serialize_f64(v:f64); serialize_char(v:char); serialize_str(v:&str); serialize_bytes(v:&[u8]);
        serialize_none(); serialize_unit(); serialize_unit_struct(name:&'static str); serialize_unit_variant(name:&'static str,index:u32,variant:&'static str)
    }
    fn serialize_some<T: Serialize + ?Sized>(self, _: &T) -> Result<S, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, _: &T) -> Result<S, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, _: &T) -> Result<S, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Error::Unreadable)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, Error> {
        Ok(self)
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error::Unreadable)
    }
}
impl<S: FieldSink> SerializeStruct for Fields<S> {
    type Ok = S;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, value: &T) -> Result<(), Error> {
        if key == "view" {
            return Ok(());
        }
        self.0.field(key, value)
    }
    fn end(self) -> Result<S, Error> {
        Ok(self.0)
    }
}

struct HashFields {
    hash: sha2::Sha256,
    first: bool,
}
struct HashWriter<'a>(&'a mut sha2::Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl FieldSink for HashFields {
    fn field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        if !self.first {
            self.hash.update(b",");
        }
        self.first = false;
        serde_json::to_writer(HashWriter(&mut self.hash), key).map_err(|_| Error::Unreadable)?;
        self.hash.update(b":");
        serde_json::to_writer(HashWriter(&mut self.hash), v).map_err(|_| Error::Unreadable)
    }
}
pub fn revision(state: &State) -> Result<String, Error> {
    let mut hash = sha2::Sha256::new();
    hash.update(b"{");
    let mut fields = state.serialize(Fields(HashFields { hash, first: true }))?;
    fields.hash.update(b"}");
    Ok(hex::encode(fields.hash.finalize()))
}

fn field_hash<T: Serialize + ?Sized>(epoch: u64, field: &str, v: &T) -> Result<String, Error> {
    let mut hash = sha2::Sha256::new();
    serde_json::to_writer(HashWriter(&mut hash), &(epoch, field, v)).map_err(|_| Error::Unreadable)?;
    Ok(hex::encode(hash.finalize()))
}

struct Revisions<'a> {
    epoch: u64,
    field: Option<&'a str>,
    hashes: BTreeMap<String, String>,
}
impl FieldSink for Revisions<'_> {
    fn field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        if self.field.is_none_or(|wanted| wanted == key) {
            self.hashes.insert(key.to_owned(), field_hash(self.epoch, key, v)?);
        }
        Ok(())
    }
}
pub fn field_revision(state: &State, field: &str) -> Result<String, Error> {
    let field = if field == "$queue_index" { "queue" } else { field };
    let mut found = state.serialize(Fields(Revisions {
        epoch: state.epoch,
        field: Some(field),
        hashes: BTreeMap::new(),
    }))?;
    found.hashes.remove(field).ok_or(Error::InvalidPath)
}

struct Descriptors {
    pinned: &'static [&'static str],
    fields: BTreeMap<String, Descriptor>,
}
impl FieldSink for Descriptors {
    fn field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        if !self.pinned.contains(&key) {
            self.fields.insert(key.to_owned(), v.serialize(Describe)?.node);
        }
        Ok(())
    }
}

struct Project {
    view: View,
    budget: usize,
    pinned: &'static [&'static str],
}
impl FieldSink for Project {
    fn field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        if self.pinned.contains(&key) {
            return Ok(());
        }
        // Every possible descriptor was reserved first. An inline admission can only free that
        // metadata, so a later omitted field cannot push the control view over its budget.
        if let Ok(v) = value(v, self.budget) {
            if let Some(map) = self.view.state.as_object_mut() {
                let old = map.insert(key.to_owned(), v);
                let descriptor = self.view.deferred.remove(key);
                let index = if key == "queue" { self.view.queue_index.take() } else { None };
                if bounded_view(&self.view, self.budget).is_ok() {
                    return Ok(());
                }
                if let Some(descriptor) = descriptor {
                    self.view.deferred.insert(key.to_owned(), descriptor);
                }
                if key == "queue" {
                    self.view.queue_index = index;
                }
                if let Some(map) = self.view.state.as_object_mut() {
                    match old {
                        Some(v) => {
                            map.insert(key.to_owned(), v);
                        }
                        None => {
                            map.remove(key);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn view(state: &State, budget: usize) -> Result<View, Error> {
    let budget = budget.min(FRAME);
    let counts = Counts {
        queue: state.queue.len(),
        approvals: state.approvals.len(),
        tasks: state.tasks.len(),
        tasks_running: state.tasks.iter().filter(|t| t.live()).count(),
        todos: state.todos.len(),
        todos_done: state.todos.iter().filter(|t| t.status == "completed").count(),
    };
    let revision = revision(state)?;
    let revisions = state
        .serialize(Fields(Revisions {
            epoch: state.epoch,
            field: None,
            hashes: BTreeMap::new(),
        }))?
        .hashes;
    if let Ok(mut whole) = value(state, budget) {
        whole.as_object_mut().ok_or(Error::Unreadable)?.remove("view");
        let complete = View {
            state: whole,
            revision: revision.clone(),
            revisions: revisions.clone(),
            deferred: BTreeMap::new(),
            counts: counts.clone(),
            queue_index: None,
            budget,
        };
        if bounded_view(&complete, budget).is_ok() {
            return Ok(complete);
        }
    }
    // Only schema defaults are materialized here; omitted required fields keep valid defaults.
    let mut defaults = value(&State::default(), budget)?;
    let map = defaults.as_object_mut().ok_or(Error::Unreadable)?;
    map.remove("view");
    map.insert("status".to_owned(), value(&state.status, budget)?);
    map.insert("epoch".to_owned(), Value::from(state.epoch));
    let pinned = &["status", "epoch"];
    let deferred = state.serialize(Fields(Descriptors { pinned, fields: BTreeMap::new() }))?.fields;
    let mut projection = Project {
        view: View {
            state: defaults,
            revision,
            revisions,
            deferred,
            counts,
            queue_index: None,
            budget,
        },
        budget,
        pinned,
    };
    bounded_view(&projection.view, budget)?;
    if !state.queue.is_empty() {
        projection.view.queue_index = Some(QueueIndex {
            entries: Vec::new(),
            complete: false,
            next: Some(0),
        });
        bounded_view(&projection.view, budget)?;
    }
    // Control fields get the available space before a queue index grows. Only its empty partial
    // marker was reserved above; index entries must not crowd out an actionable approval or task.
    let mut projection = state.serialize(Fields(projection))?;
    if projection.view.deferred.contains_key("queue")
        && let Some(index) = projection.view.queue_index.as_ref()
    {
        let empty_len = encoded(index, budget)?.len();
        let with_index = bounded_view(&projection.view, budget)?;
        let overhead = with_index.len().checked_sub(empty_len).ok_or(Error::Budget)?;
        if let Ok(index) = queue_index(&state.queue, 0, budget.saturating_sub(overhead)) {
            projection.view.queue_index = Some(index);
        }
    }
    bounded_view(&projection.view, budget)?;
    Ok(projection.view)
}

/// Open's display projection. Its descriptors explain omitted fields; the following State view
/// supplies read tokens. Info's own count fields are preserved without inventing other totals.
pub fn info_view(info: &super::session::Info, budget: usize) -> Result<Value, Error> {
    let budget = budget.min(FRAME);
    if let Ok(mut whole) = value(info, budget) {
        whole.as_object_mut().ok_or(Error::Unreadable)?.insert("queue_partial".to_owned(), Value::from(info.queued > 0));
        if encoded(&whole, budget).is_ok() {
            return Ok(whole);
        }
    }
    let mut defaults = value(&super::session::Info::default(), budget)?;
    let map = defaults.as_object_mut().ok_or(Error::Unreadable)?;
    map.remove("view");
    map.remove("deferred");
    map.insert("id".to_owned(), value(&info.id, budget)?);
    map.insert("kind".to_owned(), value(&info.kind, budget)?);
    map.insert("kind_name".to_owned(), value(&info.kind_name, budget)?);
    map.insert("target".to_owned(), value(&info.target, budget)?);
    map.insert("status".to_owned(), value(&info.status, budget)?);
    map.insert("epoch".to_owned(), Value::from(info.epoch));
    map.insert("queued".to_owned(), Value::from(info.queued));
    map.insert("queue_partial".to_owned(), Value::from(info.queued > 0));
    map.insert("tasks_running".to_owned(), Value::from(info.tasks_running));
    map.insert("todos_done".to_owned(), Value::from(info.todos_done));
    map.insert("todos_total".to_owned(), Value::from(info.todos_total));
    let pinned = &[
        "id",
        "kind",
        "kind_name",
        "target",
        "status",
        "epoch",
        "queued",
        "queue_partial",
        "tasks_running",
        "todos_done",
        "todos_total",
        "deferred",
    ];
    let deferred = info.serialize(Fields(Descriptors { pinned, fields: BTreeMap::new() }))?.fields;
    let project = Project {
        view: View {
            state: defaults,
            revision: String::new(),
            revisions: BTreeMap::new(),
            deferred,
            counts: Counts::default(),
            queue_index: None,
            budget,
        },
        budget,
        pinned,
    };
    bounded_view(&project.view, budget)?;
    let projected = info.serialize(Fields(project))?.view;
    let mut state = projected.state;
    state.as_object_mut().ok_or(Error::Unreadable)?.insert("deferred".to_owned(), value(&projected.deferred, budget)?);
    encoded(&state, budget)?;
    Ok(state)
}

struct Pick<'a> {
    read: &'a Read,
    budget: usize,
    found: Option<Reply>,
}
impl FieldSink for Pick<'_> {
    fn field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        if key == self.read.field {
            self.found = Some(v.serialize(Walk {
                path: &self.read.path,
                offset: self.read.offset,
                revision: &self.read.revision,
                budget: self.budget,
            })?);
        }
        Ok(())
    }
}

pub fn read(state: &State, request: &Read, budget: usize) -> Result<Reply, Error> {
    if request.revision != field_revision(state, &request.field)? {
        return Err(Error::Stale);
    }
    if request.field == "queue"
        && let Some((Step::Id { id }, tail)) = request.path.split_first()
    {
        if tail.iter().any(|step| matches!(step, Step::Id { .. })) {
            return Err(Error::InvalidPath);
        }
        // Select the owner's entry exactly as its existing queue edits and removals do.
        let index = state.queue.iter().position(|q| &q.id == id).ok_or(Error::InvalidPath)?;
        let mut path = Vec::new();
        path.try_reserve_exact(request.path.len()).map_err(|_| Error::Budget)?;
        path.push(Step::Index { index });
        path.extend_from_slice(tail);
        return state.queue.serialize(Walk {
            path: &path,
            offset: request.offset,
            revision: &request.revision,
            budget: budget.min(FRAME),
        });
    }
    if request.field == "$queue_index" {
        if let [Step::Id { id }] = request.path.as_slice() {
            if request.offset != 0 {
                return Err(Error::InvalidCursor);
            }
            let entry = state.queue.iter().find(|q| &q.id == id).ok_or(Error::InvalidPath)?;
            let reply = Reply::Value {
                revision: request.revision.clone(),
                value: value(&QueueEntryRef::of(entry), budget)?,
            };
            encoded(&reply, budget)?;
            return Ok(reply);
        }
        if request.path.iter().any(|step| matches!(step, Step::Id { .. })) {
            return Err(Error::InvalidPath);
        }
        return QueueEntries(&state.queue).serialize(Walk {
            path: &request.path,
            offset: request.offset,
            revision: &request.revision,
            budget: budget.min(FRAME),
        });
    }
    if request.path.iter().any(|step| matches!(step, Step::Id { .. })) {
        return Err(Error::InvalidPath);
    }
    let pick = state.serialize(Fields(Pick {
        read: request,
        budget: budget.min(FRAME),
        found: None,
    }))?;
    let reply = pick.found.ok_or(Error::InvalidPath)?;
    encoded(&reply, budget)?;
    Ok(reply)
}

struct QueueEntries<'a>(&'a [super::model::QueueItem]);
impl Serialize for QueueEntries<'_> {
    fn serialize<S: ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for entry in self.0 {
            seq.serialize_element(&QueueEntryRef::of(entry))?;
        }
        seq.end()
    }
}

struct Shape {
    node: Descriptor,
}
struct Describe;
struct Description {
    kind: Kind,
    len: usize,
}
impl Description {
    fn new(kind: Kind, _: bool) -> Self {
        Self { kind, len: 0 }
    }
    fn next(&mut self) -> Result<(), Error> {
        self.len = self.len.checked_add(1).ok_or(Error::Budget)?;
        Ok(())
    }
    fn done(self) -> Shape {
        Shape {
            node: Descriptor { kind: self.kind, len: self.len },
        }
    }
}
macro_rules! describe_scalars {
    ($($method:ident($ty:ty));* $(;)?) => { $(fn $method(self, _: $ty) -> Result<Shape, Error> { self.serialize_unit() })* };
}
impl ser::Serializer for Describe {
    type Ok = Shape;
    type Error = Error;
    type SerializeSeq = Description;
    type SerializeTuple = Description;
    type SerializeTupleStruct = Description;
    type SerializeTupleVariant = Description;
    type SerializeMap = Description;
    type SerializeStruct = Description;
    type SerializeStructVariant = Description;
    describe_scalars! { serialize_bool(bool); serialize_i8(i8); serialize_i16(i16); serialize_i32(i32); serialize_i64(i64); serialize_i128(i128); serialize_u8(u8); serialize_u16(u16); serialize_u32(u32); serialize_u64(u64); serialize_u128(u128); serialize_f32(f32); serialize_f64(f64); serialize_char(char) }
    fn serialize_str(self, v: &str) -> Result<Shape, Error> {
        Ok(Shape {
            node: Descriptor { kind: Kind::Text, len: v.len() },
        })
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Shape, Error> {
        Ok(Shape {
            node: Descriptor { kind: Kind::Array, len: v.len() },
        })
    }
    fn serialize_none(self) -> Result<Shape, Error> {
        self.serialize_unit()
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<Shape, Error> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<Shape, Error> {
        Ok(Shape {
            node: Descriptor { kind: Kind::Value, len: 1 },
        })
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Shape, Error> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, v: &'static str) -> Result<Shape, Error> {
        self.serialize_str(v)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<Shape, Error> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, _: &T) -> Result<Shape, Error> {
        Ok(Shape {
            node: Descriptor { kind: Kind::Object, len: 1 },
        })
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Description, Error> {
        Ok(Description::new(Kind::Array, true))
    }
    fn serialize_tuple(self, _: usize) -> Result<Description, Error> {
        self.serialize_seq(None)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Description, Error> {
        self.serialize_seq(None)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Description, Error> {
        Ok(Description::new(Kind::Object, false))
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Description, Error> {
        Ok(Description::new(Kind::Object, true))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Description, Error> {
        Ok(Description::new(Kind::Object, false))
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Description, Error> {
        Ok(Description::new(Kind::Object, false))
    }
}
macro_rules! describe_sequence {
    ($trait:ident,$method:ident) => {
        impl $trait for Description {
            type Ok = Shape;
            type Error = Error;
            fn $method<T: Serialize + ?Sized>(&mut self, _: &T) -> Result<(), Error> {
                self.next()
            }
            fn end(self) -> Result<Shape, Error> {
                Ok(self.done())
            }
        }
    };
}
describe_sequence!(SerializeSeq, serialize_element);
describe_sequence!(SerializeTuple, serialize_element);
describe_sequence!(SerializeTupleStruct, serialize_field);
describe_sequence!(SerializeTupleVariant, serialize_field);
impl SerializeMap for Description {
    type Ok = Shape;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, _: &T) -> Result<(), Error> {
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, _: &T) -> Result<(), Error> {
        self.next()
    }
    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(&mut self, _: &K, _: &V) -> Result<(), Error> {
        self.next()
    }
    fn end(self) -> Result<Shape, Error> {
        Ok(self.done())
    }
}
impl SerializeStruct for Description {
    type Ok = Shape;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, _: &T) -> Result<(), Error> {
        self.next()
    }
    fn end(self) -> Result<Shape, Error> {
        Ok(self.done())
    }
}
impl SerializeStructVariant for Description {
    type Ok = Shape;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, _: &T) -> Result<(), Error> {
        self.next()
    }
    fn end(self) -> Result<Shape, Error> {
        Ok(self.done())
    }
}

struct Walk<'a> {
    path: &'a [Step],
    offset: usize,
    revision: &'a str,
    budget: usize,
}
impl Walk<'_> {
    fn scalar<T: Serialize + ?Sized>(self, v: &T) -> Result<Reply, Error> {
        if !self.path.is_empty() {
            return Err(Error::InvalidPath);
        }
        if self.offset != 0 {
            return Err(Error::InvalidCursor);
        }
        let reply = match value(v, self.budget) {
            Ok(value) => Reply::Value {
                revision: self.revision.to_owned(),
                value,
            },
            Err(Error::Budget) => Reply::Node {
                revision: self.revision.to_owned(),
                node: v.serialize(Describe)?.node,
            },
            Err(e) => return Err(e),
        };
        encoded(&reply, self.budget)?;
        Ok(reply)
    }
    fn text(self, text: &str) -> Result<Reply, Error> {
        if !self.path.is_empty() {
            return Err(Error::InvalidPath);
        }
        if !text.is_char_boundary(self.offset) {
            return Err(Error::InvalidCursor);
        }
        let start = self.offset;
        let remaining = text.get(start..).ok_or(Error::InvalidCursor)?;
        let mut low = 0;
        let mut high = remaining.len().min(self.budget);
        let mut best = None;
        while low <= high {
            let mid = low + (high - low) / 2;
            let mut end = start.checked_add(mid).ok_or(Error::InvalidCursor)?;
            while !text.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            let reply = Reply::Text {
                revision: self.revision.to_owned(),
                text: text.get(start..end).ok_or(Error::InvalidCursor)?.to_owned(),
                next: (end < text.len()).then_some(end),
            };
            if encoded(&reply, self.budget).is_ok() {
                best = Some(reply);
                low = mid.checked_add(1).ok_or(Error::Budget)?;
            } else {
                let Some(next) = mid.checked_sub(1) else { break };
                high = next;
            }
        }
        match best {
            Some(Reply::Text { text: part, .. }) if part.is_empty() && !remaining.is_empty() => Err(Error::Budget),
            Some(reply) => Ok(reply),
            None => Err(Error::Budget),
        }
    }
}

macro_rules! walk_scalars {
    ($($method:ident($ty:ty));* $(;)?) => { $(fn $method(self, v: $ty) -> Result<Reply, Error> { self.scalar(&v) })* };
}
impl<'a> ser::Serializer for Walk<'a> {
    type Ok = Reply;
    type Error = Error;
    type SerializeSeq = Scan<'a>;
    type SerializeTuple = Scan<'a>;
    type SerializeTupleStruct = Scan<'a>;
    type SerializeTupleVariant = Scan<'a>;
    type SerializeMap = Scan<'a>;
    type SerializeStruct = Scan<'a>;
    type SerializeStructVariant = Scan<'a>;
    walk_scalars! { serialize_bool(bool); serialize_i8(i8); serialize_i16(i16); serialize_i32(i32); serialize_i64(i64); serialize_i128(i128); serialize_u8(u8); serialize_u16(u16); serialize_u32(u32); serialize_u64(u64); serialize_u128(u128); serialize_f32(f32); serialize_f64(f64); serialize_char(char) }
    fn serialize_str(self, v: &str) -> Result<Reply, Error> {
        self.text(v)
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Reply, Error> {
        v.serialize(self)
    }
    fn serialize_none(self) -> Result<Reply, Error> {
        self.scalar(&())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<Reply, Error> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<Reply, Error> {
        self.scalar(&())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Reply, Error> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, variant: &'static str) -> Result<Reply, Error> {
        self.text(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<Reply, Error> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, variant: &'static str, v: &T) -> Result<Reply, Error> {
        let mut scan = Scan::new(self);
        scan.entry(Step::Key { key: variant.to_owned() }, v)?;
        scan.done()
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Scan<'a>, Error> {
        Ok(Scan::new(self))
    }
    fn serialize_tuple(self, _: usize) -> Result<Scan<'a>, Error> {
        self.serialize_seq(None)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Scan<'a>, Error> {
        self.serialize_seq(None)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, variant: &'static str, len: usize) -> Result<Scan<'a>, Error> {
        Scan::variant(self, variant, Kind::Array, len)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Scan<'a>, Error> {
        Ok(Scan::new(self))
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Scan<'a>, Error> {
        Ok(Scan::new(self))
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, variant: &'static str, len: usize) -> Result<Scan<'a>, Error> {
        Scan::variant(self, variant, Kind::Object, len)
    }
}

struct Scan<'a> {
    walk: Walk<'a>,
    at: usize,
    entries: Vec<Entry>,
    next: Option<usize>,
    found: Option<Reply>,
    key: Option<Value>,
    wrapped: Option<(&'static str, Descriptor)>,
}
impl<'a> Scan<'a> {
    fn new(walk: Walk<'a>) -> Self {
        Self {
            walk,
            at: 0,
            entries: Vec::new(),
            next: None,
            found: None,
            key: None,
            wrapped: None,
        }
    }
    fn variant(mut walk: Walk<'a>, name: &'static str, kind: Kind, len: usize) -> Result<Self, Error> {
        match walk.path.split_first() {
            Some((Step::Key { key }, tail)) if key == name => {
                walk.path = tail;
                Ok(Self::new(walk))
            }
            Some(_) => Err(Error::InvalidPath),
            None if walk.offset == 0 => {
                let mut scan = Self::new(walk);
                scan.wrapped = Some((name, Descriptor { kind, len }));
                Ok(scan)
            }
            None => Err(Error::InvalidCursor),
        }
    }
    fn entry<T: Serialize + ?Sized>(&mut self, key: Step, v: &T) -> Result<(), Error> {
        if self.wrapped.is_some() {
            return Ok(());
        }
        let index = self.at;
        self.at = self.at.checked_add(1).ok_or(Error::Budget)?;
        if let Some((head, tail)) = self.walk.path.split_first() {
            if head == &key {
                self.found = Some(v.serialize(Walk {
                    path: tail,
                    offset: self.walk.offset,
                    revision: self.walk.revision,
                    budget: self.walk.budget,
                })?);
            }
            return Ok(());
        }
        if index < self.walk.offset || self.next.is_some() {
            return Ok(());
        }
        let inline = value(v, self.walk.budget);
        let entry = match inline {
            Ok(value) => Entry { key, value: Some(value), node: None },
            Err(Error::Budget) => Entry {
                key,
                value: None,
                node: Some(v.serialize(Describe)?.node),
            },
            Err(e) => return Err(e),
        };
        self.entries.push(entry);
        let revision = self.walk.revision;
        let next = Some(self.at);
        let budget = self.walk.budget;
        let fits = |entries: &[Entry]| {
            encoded(
                &PageRef {
                    kind: "page",
                    revision,
                    entries,
                    next,
                },
                budget,
            )
            .is_ok()
        };
        if fits(&self.entries) {
            return Ok(());
        }
        let mut entry = self.entries.pop().ok_or(Error::Unreadable)?;
        // A value fitting alone may leave no room for its page envelope; expose its node instead.
        if self.entries.is_empty() && entry.value.is_some() {
            entry.value = None;
            entry.node = Some(v.serialize(Describe)?.node);
            self.entries.push(entry);
            if fits(&self.entries) {
                return Ok(());
            }
            self.entries.pop();
            return Err(Error::Budget);
        }
        self.next = Some(index);
        Ok(())
    }
    fn done(self) -> Result<Reply, Error> {
        if let Some((name, node)) = self.wrapped {
            let reply = Reply::Page {
                revision: self.walk.revision.to_owned(),
                entries: vec![Entry {
                    key: Step::Key { key: name.to_owned() },
                    value: None,
                    node: Some(node),
                }],
                next: None,
            };
            encoded(&reply, self.walk.budget)?;
            return Ok(reply);
        }
        if !self.walk.path.is_empty() {
            return self.found.ok_or(Error::InvalidPath);
        }
        if self.walk.offset > self.at {
            return Err(Error::InvalidCursor);
        }
        let reply = Reply::Page {
            revision: self.walk.revision.to_owned(),
            entries: self.entries,
            next: self.next,
        };
        encoded(&reply, self.walk.budget)?;
        Ok(reply)
    }
}
macro_rules! scan_sequence {
    ($trait:ident,$method:ident) => {
        impl $trait for Scan<'_> {
            type Ok = Reply;
            type Error = Error;
            fn $method<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
                self.entry(Step::Index { index: self.at }, v)
            }
            fn end(self) -> Result<Reply, Error> {
                self.done()
            }
        }
    };
}
scan_sequence!(SerializeSeq, serialize_element);
scan_sequence!(SerializeTuple, serialize_element);
scan_sequence!(SerializeTupleStruct, serialize_field);
scan_sequence!(SerializeTupleVariant, serialize_field);
impl SerializeStruct for Scan<'_> {
    type Ok = Reply;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        self.entry(Step::Key { key: key.to_owned() }, v)
    }
    fn end(self) -> Result<Reply, Error> {
        self.done()
    }
}
impl SerializeStructVariant for Scan<'_> {
    type Ok = Reply;
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, key: &'static str, v: &T) -> Result<(), Error> {
        self.entry(Step::Key { key: key.to_owned() }, v)
    }
    fn end(self) -> Result<Reply, Error> {
        self.done()
    }
}
#[derive(Serialize)]
struct MapEntry<'a, K: ?Sized, V: ?Sized> {
    key: &'a K,
    value: &'a V,
}
impl SerializeMap for Scan<'_> {
    type Ok = Reply;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        self.key = Some(value(key, self.walk.budget)?);
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), Error> {
        let key = self.key.take().ok_or(Error::Unreadable)?;
        self.serialize_entry(&key, v)
    }
    fn serialize_entry<K: Serialize + ?Sized, V: Serialize + ?Sized>(&mut self, key: &K, v: &V) -> Result<(), Error> {
        if let Some(Step::Key { key: wanted }) = self.walk.path.first() {
            if value(key, self.walk.budget).ok().and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(wanted) {
                let tail = self.walk.path.get(1..).ok_or(Error::InvalidPath)?;
                self.found = Some(v.serialize(Walk {
                    path: tail,
                    offset: self.walk.offset,
                    revision: self.walk.revision,
                    budget: self.walk.budget,
                })?);
            }
            self.at = self.at.checked_add(1).ok_or(Error::Budget)?;
            return Ok(());
        }
        // Ordinal map entries expose both the key and value as child nodes, including huge keys.
        self.entry(Step::Index { index: self.at }, &MapEntry { key, value: v })
    }
    fn end(self) -> Result<Reply, Error> {
        self.done()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{QueueItem, Status};

    fn queued(text: String, i: usize) -> QueueItem {
        QueueItem {
            id: format!("message-{i}"),
            text,
            at: i as i64,
            attachments: Vec::new(),
            overrides: None,
            reply_to: None,
        }
    }
    fn request(s: &State, field: &str, path: Vec<Step>, offset: usize) -> Read {
        Read {
            revision: field_revision(s, field).unwrap_or_default(),
            field: field.to_owned(),
            path,
            offset,
        }
    }

    #[test]
    fn a_large_queue_is_referenced_and_each_page_keeps_its_exact_values() {
        let s = State {
            status: Status::Running,
            queue: (0..564).map(|i| queued("é\"\\\n".repeat(700), i)).collect(),
            ..State::default()
        };
        assert!(serde_json::to_vec(&s).unwrap().len() > FRAME);
        let head = view(&s, FRAME - 512).unwrap();
        assert_eq!(head.state.get("status"), Some(&Value::from("running")));
        assert_eq!(head.counts.queue, 564);
        assert_eq!(head.deferred.get("queue").unwrap().len, 564);
        assert!(bounded_view(&head, FRAME - 512).is_ok());
        let index = head.queue_index.as_ref().unwrap();
        assert!(index.complete);
        assert_eq!(index.entries.len(), s.queue.len());
        assert_eq!(index.entries.first().unwrap().id, s.queue.first().unwrap().id);
        assert_eq!(index.entries.first().unwrap().len, s.queue.first().unwrap().text.len());
        let mut at = 0;
        let mut got = Vec::new();
        loop {
            let reply = read(&s, &request(&s, "queue", Vec::new(), at), FRAME - 512).unwrap();
            assert!(encoded(&reply, FRAME - 512).is_ok());
            let Reply::Page { entries, next, .. } = reply else { panic!("not a queue page") };
            for e in entries {
                got.push(serde_json::from_value::<QueueItem>(e.value.unwrap()).unwrap());
            }
            match next {
                Some(next) => {
                    assert!(next > at);
                    at = next;
                }
                None => break,
            }
        }
        assert_eq!(got, s.queue);
    }

    #[test]
    fn one_oversized_item_is_inspected_as_nodes_and_utf8_text_ranges() {
        let text = "🙂\"\\é\n".repeat(FRAME / 8);
        let s = State {
            queue: vec![queued(text.clone(), 0)],
            ..State::default()
        };
        let Reply::Page { entries, .. } = read(&s, &request(&s, "queue", Vec::new(), 0), 4096).unwrap() else {
            panic!()
        };
        assert!(entries.first().unwrap().value.is_none());
        assert_eq!(entries.first().unwrap().node.as_ref().unwrap().kind, Kind::Object);
        let path = vec![Step::Index { index: 0 }, Step::Key { key: "text".to_owned() }];
        let mut bytes = String::new();
        let mut at = 0;
        loop {
            let reply = read(&s, &request(&s, "queue", path.clone(), at), 4096).unwrap();
            assert!(encoded(&reply, 4096).is_ok());
            let Reply::Text { text, next, .. } = reply else { panic!() };
            bytes.push_str(&text);
            match next {
                Some(next) => {
                    assert!(next > at);
                    at = next;
                }
                None => break,
            }
        }
        assert_eq!(bytes, text);
    }

    #[test]
    fn stale_and_invalid_reads_do_not_produce_partial_answers() {
        let mut s = State {
            queue: vec![queued("é".to_owned(), 0)],
            ..State::default()
        };
        let old = request(&s, "queue", Vec::new(), 0);
        s.queue.clear();
        assert_eq!(read(&s, &old, FRAME), Err(Error::Stale));
        assert_eq!(read(&s, &request(&s, "queue", Vec::new(), 1), FRAME), Err(Error::InvalidCursor));
        assert_eq!(read(&s, &request(&s, "absent", Vec::new(), 0), FRAME), Err(Error::InvalidPath));
        s.queue.push(queued("é".to_owned(), 0));
        assert_eq!(read(&s, &request(&s, "queue", vec![Step::Index { index: 2 }], 0), FRAME), Err(Error::InvalidPath));
        assert_eq!(
            read(&s, &request(&s, "queue", vec![Step::Index { index: 0 }, Step::Key { key: "text".to_owned() }], 1), FRAME),
            Err(Error::InvalidCursor)
        );
        assert!(matches!(view(&s, 1), Err(Error::Budget)));
    }

    #[test]
    fn map_entries_keep_large_keys_and_values_accessible_without_assembling_them() {
        let key = "long".repeat(4096);
        let mut s = State::default();
        s.delivery.insert(key.clone(), crate::harness::model::Delivery::Queued);
        let req = request(&s, "delivery", vec![Step::Index { index: 0 }, Step::Key { key: "key".to_owned() }], 0);
        let Reply::Text { text, next, .. } = read(&s, &req, 1024).unwrap() else { panic!() };
        assert!(key.starts_with(&text));
        assert!(next.is_some());
    }

    #[test]
    fn an_id_lookup_reads_only_the_matching_item_and_absence_is_typed() {
        let s = State {
            queue: (0..564).map(|i| queued(format!("message {i}"), i)).collect(),
            ..State::default()
        };
        let path = vec![Step::Id { id: "message-563".to_owned() }, Step::Key { key: "text".to_owned() }];
        let Reply::Text { text, next, .. } = read(&s, &request(&s, "queue", path, 0), 4096).unwrap() else {
            panic!()
        };
        assert_eq!(text, "message 563");
        assert!(next.is_none());
        assert_eq!(read(&s, &request(&s, "queue", vec![Step::Id { id: "absent".to_owned() }], 0), 4096), Err(Error::InvalidPath));
    }

    #[test]
    fn ordinary_states_keep_exact_nonempty_queues_and_approvals_inline() {
        use crate::harness::model::{Approval, ToolKind};
        let approval = Approval {
            id: "approval-one".to_owned(),
            tool_id: None,
            tool_name: "shell".to_owned(),
            tool: ToolKind::Shell,
            title: "Run the exact command".to_owned(),
            detail: None,
            input: serde_json::json!({"command":"printf 'é\\n'"}),
            command: Some("printf 'é\\n'".to_owned()),
            cwd: "/work".to_owned(),
            diffs: Vec::new(),
            reason: None,
            always: Vec::new(),
            default_to_no: false,
            questions: Vec::new(),
            plan: None,
            agent: None,
            at: 1,
        };
        let s = State {
            status: Status::Waiting,
            queue: vec![queued("a small message".to_owned(), 0)],
            approvals: vec![approval],
            ..State::default()
        };
        let projection = view(&s, FRAME).unwrap();
        assert!(projection.deferred.is_empty());
        assert!(projection.queue_index.is_none());
        let state = projection.into_state().unwrap();
        assert!(encoded(&state, FRAME).is_ok());
        let parsed: State = serde_json::from_value(state).unwrap();
        assert_eq!(parsed.queue, s.queue);
        assert_eq!(parsed.approvals, s.approvals);
        assert_eq!(parsed.status, Status::Waiting);
    }

    #[test]
    fn a_large_queue_index_cannot_crowd_out_a_small_actionable_approval() {
        use crate::harness::model::{Approval, ToolKind};
        let approval = Approval {
            id: "approve-exact-command".to_owned(),
            tool_id: None,
            tool_name: "shell".to_owned(),
            tool: ToolKind::Shell,
            title: "Run echo actual".to_owned(),
            detail: None,
            input: serde_json::json!({"command":"echo actual"}),
            command: Some("echo actual".to_owned()),
            cwd: "/work".to_owned(),
            diffs: Vec::new(),
            reason: None,
            always: Vec::new(),
            default_to_no: false,
            questions: Vec::new(),
            plan: None,
            agent: None,
            at: 1,
        };
        let s = State {
            status: Status::Waiting,
            queue: (0..2048).map(|i| queued("b".repeat(512), i)).collect(),
            approvals: vec![approval],
            ..State::default()
        };
        let budget = 16 * 1024;
        let projection = view(&s, budget).unwrap();
        assert!(projection.deferred.contains_key("queue"));
        assert!(!projection.deferred.contains_key("approvals"));
        let index = projection.queue_index.as_ref().unwrap();
        assert!(!index.complete);
        assert!(!index.entries.is_empty());
        assert!(index.next.is_some());
        assert!(index.entries.len() < s.queue.len());
        let wire = projection.into_state().unwrap();
        assert!(encoded(&wire, budget).is_ok());
        let parsed: State = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed.approvals, s.approvals);
        assert_eq!(parsed.status, Status::Waiting);
    }

    #[test]
    fn all_descriptor_space_is_reserved_before_optional_fields_can_fill_the_budget() {
        let s = State {
            status: Status::Running,
            model: "m".repeat(12000),
            goal: "g".repeat(12000),
            summary: "s".repeat(12000),
            queue: vec![queued("body".repeat(FRAME), 0)],
            ..State::default()
        };
        let budget = 16 * 1024;
        let projection = view(&s, budget).unwrap();
        assert_eq!(projection.state.get("status"), Some(&Value::from("running")));
        assert_eq!(projection.counts.queue, 1);
        assert!(projection.deferred.contains_key("queue"));
        assert!(projection.deferred.contains_key("model") || projection.deferred.contains_key("goal"));
        assert!(encoded(&projection.into_state().unwrap(), budget).is_ok());
    }

    #[test]
    fn compact_queue_indexes_are_exact_and_partial_indexes_never_claim_absence() {
        let queue: Vec<_> = (0..564).map(|i| queued("é\"\\\n".repeat(700), i)).collect();
        let complete = queue_index(&queue, 0, FRAME).unwrap();
        assert!(complete.complete);
        assert!(complete.next.is_none());
        assert_eq!(complete.entries.len(), queue.len());
        for (entry, item) in complete.entries.iter().zip(&queue) {
            assert_eq!(entry.id, item.id);
            assert_eq!(entry.len, item.text.len());
            assert_eq!(u64::from_str_radix(&entry.digest, 16).unwrap(), super::super::queued::digest(&item.text));
        }
        let partial = queue_index(&queue, 0, 4096).unwrap();
        assert!(!partial.complete);
        assert_eq!(partial.next, Some(partial.entries.len()));
        assert!(!partial.entries.is_empty());
        assert!(encoded(&partial, 4096).is_ok());
        let suffix = queue_index(&queue, partial.next.unwrap(), FRAME).unwrap();
        assert!(!suffix.complete);
        assert!(suffix.next.is_none());
        assert_eq!(suffix.entries.first().unwrap().id, queue.get(partial.entries.len()).unwrap().id);
        assert!(queue_index(&[], 0, 4096).unwrap().complete);
        assert_eq!(queue_index(&queue, queue.len() + 1, FRAME), Err(Error::InvalidCursor));
    }

    #[test]
    fn queue_reads_survive_status_and_usage_changes_but_not_queue_or_epoch_changes() {
        let mut s = State {
            queue: vec![queued("unchanged".to_owned(), 0)],
            ..State::default()
        };
        let body = request(&s, "queue", vec![Step::Id { id: "message-0".to_owned() }, Step::Key { key: "text".to_owned() }], 0);
        let index = request(&s, "$queue_index", vec![Step::Id { id: "message-0".to_owned() }], 0);
        assert_eq!(body.revision, index.revision);
        s.status = Status::Running;
        s.usage.input_tokens = 400;
        assert!(matches!(read(&s, &body, 4096), Ok(Reply::Text { .. })));
        let Reply::Value { value, .. } = read(&s, &index, 4096).unwrap() else { panic!() };
        assert_eq!(value.get("id").and_then(Value::as_str), Some("message-0"));
        assert_eq!(value.get("len").and_then(Value::as_u64), Some(9));
        let missing = request(&s, "$queue_index", vec![Step::Id { id: "absent".to_owned() }], 0);
        assert_eq!(read(&s, &missing, 4096), Err(Error::InvalidPath));
        s.queue.first_mut().unwrap().text.push('!');
        assert_eq!(read(&s, &body, 4096), Err(Error::Stale));
        assert_eq!(read(&s, &index, 4096), Err(Error::Stale));
        let updated = request(&s, "queue", Vec::new(), 0);
        s.epoch = 1;
        assert_eq!(read(&s, &updated, 4096), Err(Error::Stale));
    }

    #[test]
    fn open_keeps_exact_identity_counts_and_marks_its_untransmitted_index_partial() {
        let info = super::super::session::Info {
            id: "session-one".to_owned(),
            kind: "codex".to_owned(),
            target: "agent:box".to_owned(),
            status: Status::Running,
            queued: 564,
            tasks_running: 3,
            limits: vec![crate::harness::model::Limit::default(); 10000],
            ..Default::default()
        };
        let projected = info_view(&info, 4096).unwrap();
        assert_eq!(projected.get("id").and_then(Value::as_str), Some("session-one"));
        assert_eq!(projected.get("target").and_then(Value::as_str), Some("agent:box"));
        assert_eq!(projected.get("queued").and_then(Value::as_u64), Some(564));
        assert_eq!(projected.get("queue_partial"), Some(&Value::Bool(true)));
        assert!(projected.get("queue").is_none());
        assert!(projected.get("deferred").and_then(|v| v.get("limits")).is_some());
        assert!(encoded(&projected, 4096).is_ok());
    }

    #[test]
    fn short_writes_never_reserve_capacity_above_the_budget() {
        for budget in [0, 1, 3, 17, 1023, FRAME - 1] {
            let mut writer = Bounded { bytes: Vec::new(), budget };
            for _ in 0..budget {
                writer.write_all(b"x").unwrap();
                assert!(writer.bytes.capacity() <= budget);
            }
            assert_eq!(writer.bytes.len(), budget);
            let capacity = writer.bytes.capacity();
            assert!(writer.write_all(b"x").is_err());
            assert_eq!(writer.bytes.len(), budget);
            assert_eq!(writer.bytes.capacity(), capacity);
        }
        assert!(Bounded::target(usize::MAX, usize::MAX, 1, usize::MAX).is_none());
        assert!(Bounded::target(1, 1, usize::MAX, usize::MAX).is_none());
        let over_half = usize::MAX / 2 + 1;
        assert_eq!(Bounded::target(over_half, over_half, 1, usize::MAX), Some(usize::MAX));
    }

    #[test]
    fn exact_encoding_keeps_ordinary_state_bytes_and_refuses_over_budget_values() {
        let s = State {
            queue: vec![queued("exact é\"\\\n bytes".to_owned(), 0)],
            ..State::default()
        };
        let expected = serde_json::to_vec(&s).unwrap();
        assert_eq!(encode(&s, expected.len()).unwrap(), expected);
        assert_eq!(encode(&s, expected.len() - 1), Err(Error::Budget));
        assert_eq!(encode(&"x".repeat(FRAME), FRAME), Err(Error::Budget));
    }

    #[test]
    fn queue_body_and_metadata_reads_select_the_existing_owner_action_entry() {
        let mut s = State {
            queue: vec![queued("first owner entry".to_owned(), 0), queued("selected owner entry".to_owned(), 1)],
            ..State::default()
        };
        let id = "message-1";
        let owner = s.queue.iter().position(|q| q.id == id).unwrap();
        let body_request = request(&s, "queue", vec![Step::Id { id: id.to_owned() }, Step::Key { key: "text".to_owned() }], 0);
        let metadata_request = request(&s, "$queue_index", vec![Step::Id { id: id.to_owned() }], 0);
        assert_eq!(body_request.revision, metadata_request.revision);
        let Reply::Text { text, next, .. } = read(&s, &body_request, 4096).unwrap() else { panic!() };
        assert!(next.is_none());
        assert_eq!(text, s.queue.get(owner).unwrap().text);
        let Reply::Value { value, .. } = read(&s, &metadata_request, 4096).unwrap() else { panic!() };
        assert_eq!(value.get("id").and_then(Value::as_str), Some(id));
        assert_eq!(value.get("len").and_then(Value::as_u64), Some(text.len() as u64));
        let removed = s.queue.remove(owner);
        assert_eq!(removed.id, id);
        assert_eq!(removed.text, text);
        assert_eq!(read(&s, &body_request, 4096), Err(Error::Stale));
        assert_eq!(read(&s, &request(&s, "tasks", vec![Step::Id { id: id.to_owned() }], 0), 4096), Err(Error::InvalidPath));
    }

    #[test]
    fn queue_index_size_accounts_exact_entry_bytes_and_cursor_digit_changes() {
        let queue: Vec<_> = (0..12).map(|i| queued(format!("body {i}"), i)).collect();
        let all = queue_index(&queue, 0, FRAME).unwrap();
        let expected = QueueIndex {
            entries: all.entries.iter().take(10).cloned().collect(),
            complete: false,
            next: Some(10),
        };
        let boundary = encoded(&expected, FRAME).unwrap().len();
        let exact = queue_index(&queue, 0, boundary).unwrap();
        assert_eq!(exact, expected);
        assert_eq!(encoded(&exact, boundary).unwrap().len(), boundary);
        let below = queue_index(&queue, 0, boundary - 1).unwrap();
        assert_eq!(below.entries.len(), 9);
        assert_eq!(below.next, Some(9));
        assert!(!below.complete);
        assert!(encoded(&below, boundary - 1).is_ok());
        let one = QueueIndex {
            entries: all.entries.iter().skip(9).take(1).cloned().collect(),
            complete: false,
            next: Some(10),
        };
        let one_boundary = encoded(&one, FRAME).unwrap().len();
        assert_eq!(queue_index(&queue, 9, one_boundary).unwrap(), one);
        assert_eq!(queue_index(&queue, 9, one_boundary - 1), Err(Error::Budget));
        let complete_boundary = encoded(&all, FRAME).unwrap().len();
        assert_eq!(queue_index(&queue, 0, complete_boundary).unwrap(), all);
        assert!(!queue_index(&queue, 0, complete_boundary - 1).unwrap().complete);
    }

    #[test]
    fn borrowed_page_envelopes_encode_exactly_the_owned_wire_shape() {
        let entries = vec![Entry {
            key: Step::Index { index: 9 },
            value: Some(Value::from("é\"\\\n".repeat(1024))),
            node: None,
        }];
        let owned = Reply::Page {
            revision: "revision".to_owned(),
            entries: entries.clone(),
            next: Some(10),
        };
        let borrowed = PageRef {
            kind: "page",
            revision: "revision",
            entries: &entries,
            next: Some(10),
        };
        let bytes = encoded(&owned, FRAME).unwrap();
        assert_eq!(encoded(&borrowed, bytes.len()).unwrap(), bytes);
        assert_eq!(encoded(&borrowed, bytes.len() - 1), Err(Error::Budget));
    }
}
